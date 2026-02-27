use std::time::Duration;

use crate::arbitrage::policy::{DecisionPolicy, ExecutionPolicy, MarginPolicy, PriceSource};
use crate::arbitrage::types::{
    DEFAULT_UNWIND_CHUNKS, DeciderAction, PairState, PairStatus, StrategyCommand, StrategyResponse,
    StrategyStatus, TrackedOrderStatus, UnwindSchedule, next_unwind_action,
};
use boldtrax_core::AccountSnapshot;
use boldtrax_core::types::OrderEvent;
use boldtrax_core::zmq::client::ZmqEventSubscriber;
use boldtrax_core::zmq::protocol::ZmqEvent;
use tokio::sync::{mpsc, watch};
use tracing::{debug, error, info, warn};

/// Orders older than this are considered stale and will be auto-cancelled.
const STALE_ORDER_TIMEOUT: Duration = Duration::from_secs(60);
/// How often the engine checks for stale orders.
const STALE_CHECK_INTERVAL: Duration = Duration::from_secs(30);
/// Periodic evaluation interval — ensures the engine re-evaluates even
/// when no funding or position events arrive (e.g. orderbook-only periods).
const EVAL_INTERVAL: Duration = Duration::from_secs(60);

pub struct ArbitrageEngine<Pair, D, E, M, P>
where
    Pair: PairState,
    D: DecisionPolicy<Pair>,
    E: ExecutionPolicy<Pair>,
    M: MarginPolicy,
    P: PriceSource,
{
    pub pair: Pair,
    pub oracle: P,
    pub decider: D,
    pub margin: M,
    pub execution: E,
    /// Most recent account snapshot received (if any).
    pub account: Option<AccountSnapshot>,
    /// When `true`, `evaluate_and_execute` becomes a no-op.
    paused: bool,
    /// Strategy identity for status reporting.
    strategy_id: String,
    /// Publishes the latest `StrategyStatus` so the command server
    /// can reply to `GetStatus` without blocking the event loop.
    status_tx: watch::Sender<StrategyResponse>,
}

impl<Pair, D, E, M, P> ArbitrageEngine<Pair, D, E, M, P>
where
    Pair: PairState,
    D: DecisionPolicy<Pair>,
    E: ExecutionPolicy<Pair>,
    M: MarginPolicy,
    P: PriceSource,
{
    pub fn new(pair: Pair, oracle: P, decider: D, margin: M, execution: E) -> Self {
        let (status_tx, _) = watch::channel(StrategyResponse::Status(StrategyStatus {
            strategy_id: String::new(),
            pair_status: format!("{:?}", pair.status()),
            paused: false,
            pending_orders: 0,
        }));
        Self {
            pair,
            oracle,
            decider,
            margin,
            execution,
            account: None,
            paused: false,
            strategy_id: String::new(),
            status_tx,
        }
    }

    /// Set strategy identity. Call before `run` so status reports
    /// include the correct id.
    pub fn with_strategy_id(mut self, id: impl Into<String>) -> Self {
        self.strategy_id = id.into();
        self
    }

    /// Returns a watch receiver for the command server to read
    /// the latest `StrategyStatus` when `GetStatus` arrives.
    pub fn status_watch(&self) -> watch::Receiver<StrategyResponse> {
        self.status_tx.subscribe()
    }

    /// Production entry point — driven by a live ZMQ subscriber + command channel.
    pub async fn run(
        self,
        subscriber: ZmqEventSubscriber,
        cmd_rx: mpsc::Receiver<StrategyCommand>,
    ) {
        let (tx, rx) = mpsc::channel(1024);
        let _listener_handle = subscriber.spawn_event_listener(tx);
        info!("Starting Arbitrage Engine");
        self.run_with_commands(rx, cmd_rx).await;
    }

    /// Test entry point — no command channel required.
    pub async fn run_with_stream(self, rx: mpsc::Receiver<ZmqEvent>) -> (Pair, E) {
        // Keep _tx alive so cmd_rx never returns None prematurely.
        let (_tx, cmd_rx) = mpsc::channel(1);
        self.run_with_commands(rx, cmd_rx).await
    }

    /// Core event loop — listens for ZMQ events, strategy commands, stale
    /// order checks, and periodic evaluation.
    pub async fn run_with_commands(
        mut self,
        mut rx: mpsc::Receiver<ZmqEvent>,
        mut cmd_rx: mpsc::Receiver<StrategyCommand>,
    ) -> (Pair, E) {
        let mut stale_timer = tokio::time::interval(STALE_CHECK_INTERVAL);
        stale_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut eval_timer = tokio::time::interval(EVAL_INTERVAL);
        eval_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                event = rx.recv() => {
                    match event {
                        Some(e) => self.handle_event(e).await,
                        None => break,
                    }
                }
                cmd = cmd_rx.recv() => {
                    if let Some(c) = cmd {
                        self.handle_command(c).await;
                    }
                }
                _ = stale_timer.tick() => {
                    // Skip during Entering — entry orders need time to fill;
                    // if entry fails, tracker transitions handle it.
                    // Skip during Recovering — recovery assessment manages its own orders.
                    if !matches!(
                        *self.pair.status(),
                        PairStatus::Entering | PairStatus::Recovering
                    ) {
                        self.cancel_stale_orders().await;
                    }
                }
                _ = eval_timer.tick() => {
                    self.evaluate_and_execute().await;
                }
            }
        }

        (self.pair, self.execution)
    }

    async fn handle_command(&mut self, cmd: StrategyCommand) {
        info!(command = ?cmd, status = ?self.pair.status(), "Received strategy command");

        match cmd {
            StrategyCommand::GracefulExit => {
                if *self.pair.status() != PairStatus::Active {
                    warn!(
                        status = ?self.pair.status(),
                        "GracefulExit ignored — only valid when Active"
                    );
                    return;
                }
                let (remaining_long, remaining_short) = self.pair.close_sizes();
                let mut schedule = UnwindSchedule::new(DEFAULT_UNWIND_CHUNKS);
                let action = next_unwind_action(&mut schedule, remaining_long, remaining_short);
                self.pair.order_tracker_mut().unwind_schedule = Some(schedule);
                if let Some(action) = action
                    && let Err(e) = self.execution.execute(&action, &mut self.pair).await
                {
                    error!(error = %e, "GracefulExit first unwind chunk failed");
                }
                self.publish_status();
            }
            StrategyCommand::ForceExit => {
                if self.pair.is_fully_exited() {
                    info!("ForceExit ignored — already fully exited");
                    return;
                }
                let action = DeciderAction::Exit;
                if let Err(e) = self.execution.execute(&action, &mut self.pair).await {
                    error!(error = %e, "ForceExit failed");
                }
            }
            StrategyCommand::Pause => {
                self.paused = true;
                info!("Strategy paused — evaluation suspended");
            }
            StrategyCommand::Resume => {
                self.paused = false;
                info!("Strategy resumed — evaluation re-enabled");
                self.publish_status();
                self.evaluate_and_execute().await;
            }
            StrategyCommand::GetStatus => {
                // Handled at the server level via watch channel.
                // If it arrives here, just refresh the status.
                self.publish_status();
            }
        }
    }

    /// Push the current status snapshot into the watch channel so the
    /// command server can reply to `GetStatus` without blocking.
    fn publish_status(&self) {
        let status = StrategyStatus {
            strategy_id: self.strategy_id.clone(),
            pair_status: format!("{:?}", self.pair.status()),
            paused: self.paused,
            pending_orders: self.pair.order_tracker().pending_count(),
        };
        let _ = self.status_tx.send(StrategyResponse::Status(status));
    }

    async fn handle_event(&mut self, event: ZmqEvent) {
        match event {
            // --- Universal: market data → oracle ---
            ZmqEvent::OrderBook(snapshot) => {
                self.oracle.update_snapshot(snapshot);
            }
            ZmqEvent::OrderBookUpdate(update) => {
                self.oracle.update_snapshot(update.snapshot);
            }
            ZmqEvent::Ticker(ticker) => {
                self.oracle.update_ticker(ticker);
            }
            ZmqEvent::OrderUpdate(ref evt) => {
                info!("Order update: {:?}", evt);
                // 1. Feed the event into the tracker
                self.update_tracker(evt);
                // 2. Delegate to pair so position state is updated (e.g. spot fills)
                self.pair.apply_event(event);
                // 3. Now that positions reflect the latest fill, check for
                //    tracker-driven status transitions (Entering → Active, etc.)
                //    Returns an optional continuation action (e.g. next unwind chunk).
                let continuation = self.check_tracker_transitions();
                if let Some(action) = continuation
                    && let Err(e) = self.execution.execute(&action, &mut self.pair).await
                {
                    error!(error = %e, "Continuation execution failed");
                }
            }

            // --- Pair-specific: delegated to the pair ---
            event => {
                if self.pair.apply_event(event) {
                    info!("Pair state updated, evaluating...");
                    self.evaluate_and_execute().await;
                }
            }
        }
    }

    /// Feed an [`OrderEvent`] into the [`OrderTracker`], updating
    /// the fill quantity and status of the tracked order.
    fn update_tracker(&mut self, evt: &OrderEvent) {
        let order = evt.inner();
        let status = TrackedOrderStatus::from_order_event(evt);
        let tracker = self.pair.order_tracker_mut();

        // Orders are tracked by client_order_id (the key set in track_from_order).
        if !tracker.update_fill(&order.client_order_id, order.filled_size, status.clone()) {
            // Might also be keyed by internal_id — try that as a fallback.
            if !tracker.update_fill(&order.internal_id, order.filled_size, status) {
                debug!(
                    client_order_id = %order.client_order_id,
                    internal_id = %order.internal_id,
                    "OrderUpdate for untracked order — probably from another strategy"
                );
            }
        }
    }

    /// Cancel orders that have been pending/open longer than [`STALE_ORDER_TIMEOUT`].
    ///
    /// Sends cancel requests via the execution engine. Exchange confirmations
    /// arrive as `OrderUpdate(Canceled)` events, feeding back into the tracker
    /// and triggering normal lifecycle transitions.
    async fn cancel_stale_orders(&mut self) {
        let stale: Vec<_> = self
            .pair
            .order_tracker()
            .stale_orders(STALE_ORDER_TIMEOUT)
            .iter()
            .map(|o| (o.key.exchange, o.order_id.clone()))
            .collect();

        if stale.is_empty() {
            return;
        }

        warn!(count = stale.len(), "Cancelling stale orders");
        for (exchange, order_id) in stale {
            info!(%exchange, %order_id, "Cancelling stale order");
            if let Err(e) = self.execution.cancel_order(exchange, order_id).await {
                error!(error = %e, "Failed to cancel stale order");
            }
        }
    }

    /// After all tracked orders reach a terminal state, transition the
    /// pair's lifecycle status based on actual position sizes.
    ///
    /// Returns an optional continuation action — currently only used when
    /// `Unwinding` and the schedule has more chunks to dispatch.
    fn check_tracker_transitions(&mut self) -> Option<DeciderAction> {
        if self.pair.order_tracker().has_pending() {
            return None; // Orders still in flight — wait.
        }

        let current = self.pair.status().clone();
        if !current.is_transitional() {
            return None; // Nothing to transition.
        }

        let (next, continuation) = match current {
            PairStatus::Entering => {
                if self.pair.is_fully_entered() {
                    info!("All entry orders filled — transitioning to Active");
                    (PairStatus::Active, None)
                } else {
                    warn!(
                        "Entry incomplete after all orders terminal — transitioning to Recovering"
                    );
                    let action =
                        self.pair
                            .recovery_sizes()
                            .map(|(sl, ss)| DeciderAction::Recover {
                                size_long: sl,
                                size_short: ss,
                            });
                    (PairStatus::Recovering, action)
                }
            }
            PairStatus::Exiting => {
                if self.pair.is_fully_exited() {
                    info!("All exit orders filled — transitioning to Inactive");
                    (PairStatus::Inactive, None)
                } else {
                    warn!("Exit incomplete — retrying exit");
                    (PairStatus::Exiting, Some(DeciderAction::Exit))
                }
            }
            PairStatus::Recovering => {
                if self.pair.is_fully_entered() {
                    info!("Recovery complete — both legs restored, transitioning to Active");
                    (PairStatus::Active, None)
                } else {
                    warn!("Recovery orders terminal but legs still imbalanced — retrying recovery");
                    let action =
                        self.pair
                            .recovery_sizes()
                            .map(|(sl, ss)| DeciderAction::Recover {
                                size_long: sl,
                                size_short: ss,
                            });
                    (PairStatus::Recovering, action)
                }
            }
            PairStatus::Unwinding => {
                if self.pair.is_fully_exited() {
                    info!("Unwind complete — both legs closed, transitioning to Inactive");
                    self.pair.order_tracker_mut().unwind_schedule = None;
                    (PairStatus::Inactive, None)
                } else {
                    // Try dispatching the next unwind chunk.
                    let action = {
                        let (rl, rs) = self.pair.close_sizes();
                        let tracker = self.pair.order_tracker_mut();
                        match tracker.unwind_schedule.as_mut() {
                            Some(sched) if !sched.is_complete() => {
                                next_unwind_action(sched, rl, rs)
                            }
                            Some(_) => {
                                // Schedule exhausted but position remains.
                                warn!(
                                    "All unwind chunks dispatched but position remains — retrying full close"
                                );
                                Some(DeciderAction::Unwind {
                                    size_long: rl,
                                    size_short: rs,
                                })
                            }
                            None => {
                                warn!("Unwinding without schedule — dispatching full close");
                                Some(DeciderAction::Unwind {
                                    size_long: rl,
                                    size_short: rs,
                                })
                            }
                        }
                    };
                    (PairStatus::Unwinding, action)
                }
            }
            // Active/Inactive are non-transitional — caught by the guard above.
            _ => return None,
        };

        self.pair.set_status(next);
        self.pair.order_tracker_mut().remove_terminal();
        self.publish_status();
        continuation
    }

    async fn evaluate_and_execute(&mut self) {
        if self.paused {
            debug!("Evaluation skipped — strategy is paused");
            return;
        }

        // --- Account-level margin gate (skip if no snapshot yet) ---
        if let Some(snap) = self.account.as_ref()
            && let Err(violation) = self.margin.check_account(snap)
        {
            warn!(%violation, "Account margin gate blocked evaluation");
            return;
        }

        // --- Transitional statuses with dedicated retry loops ---
        // These run instead of normal decider evaluation because the
        // decider intentionally skips transitional statuses.
        match *self.pair.status() {
            PairStatus::Recovering => {
                if self.pair.is_fully_entered() {
                    info!("Recovery complete — transitioning to Active");
                    self.pair.set_status(PairStatus::Active);
                    self.pair.order_tracker_mut().remove_terminal();
                    self.publish_status();
                } else if !self.pair.order_tracker().has_pending() {
                    self.pair.order_tracker_mut().remove_terminal();
                    if let Some((sl, ss)) = self.pair.recovery_sizes() {
                        let action = DeciderAction::Recover {
                            size_long: sl,
                            size_short: ss,
                        };
                        if let Err(e) = self.execution.execute(&action, &mut self.pair).await {
                            error!(error = %e, "Recovery execution failed");
                        }
                    }
                }
                return;
            }
            PairStatus::Exiting => {
                if self.pair.is_fully_exited() {
                    info!("Exit complete — transitioning to Inactive");
                    self.pair.set_status(PairStatus::Inactive);
                    self.pair.order_tracker_mut().remove_terminal();
                    self.publish_status();
                } else if !self.pair.order_tracker().has_pending() {
                    self.pair.order_tracker_mut().remove_terminal();
                    let action = DeciderAction::Exit;
                    if let Err(e) = self.execution.execute(&action, &mut self.pair).await {
                        error!(error = %e, "Exit retry execution failed");
                    }
                }
                return;
            }
            PairStatus::Unwinding => {
                // Unwinding is already handled via check_tracker_transitions
                // continuation actions. No poll-based retry needed beyond that.
                return;
            }
            _ => {}
        }

        // --- Refresh prices from oracle ---
        self.pair
            .refresh_prices(&|key| self.oracle.get_mid_price(key));

        // --- Refresh orderbook bid/ask for smart execution ---
        // Use level 2 (3rd level) with fallback to level 1, then level 0.
        // This places maker orders 2-3 levels deep for better fill probability.
        self.pair.refresh_orderbook(
            &|key| {
                self.oracle
                    .get_bid_at_level(key, 2)
                    .or_else(|| self.oracle.get_bid_at_level(key, 1))
                    .or_else(|| self.oracle.get_best_bid(key))
            },
            &|key| {
                self.oracle
                    .get_ask_at_level(key, 2)
                    .or_else(|| self.oracle.get_ask_at_level(key, 1))
                    .or_else(|| self.oracle.get_best_ask(key))
            },
            &|key| self.oracle.get_ask_depth(key),
            &|key| self.oracle.get_bid_depth(key),
        );

        // --- Position-level margin gates ---
        // If any position is near liquidation, force an emergency exit and
        // pause the strategy so it doesn't re-enter while the account is at risk.
        for (pos, price) in self.pair.positions_for_margin_check() {
            if let Err(violation) = self.margin.check_position(pos, price) {
                if matches!(
                    *self.pair.status(),
                    PairStatus::Active | PairStatus::Entering
                ) {
                    error!(
                        %violation,
                        status = ?self.pair.status(),
                        "EMERGENCY: Near-liquidation detected — forcing exit + pause"
                    );
                    // Cancel any in-flight orders to free margin.
                    let pending: Vec<_> = self
                        .pair
                        .order_tracker()
                        .all_pending()
                        .iter()
                        .map(|o| (o.key.exchange, o.order_id.clone()))
                        .collect();
                    for (exchange, order_id) in pending {
                        let _ = self.execution.cancel_order(exchange, order_id).await;
                    }
                    self.pair.order_tracker_mut().remove_terminal();

                    // Force exit + pause (same as ForceExit + Pause commands).
                    let action = DeciderAction::Exit;
                    if let Err(e) = self.execution.execute(&action, &mut self.pair).await {
                        error!(error = %e, "Emergency exit execution failed");
                    }
                    self.paused = true;
                    warn!("Strategy PAUSED after emergency margin exit — manual resume required");
                    self.publish_status();
                } else {
                    warn!(%violation, "Position margin gate blocked evaluation");
                }
                return;
            }
        }

        // --- Decision + Execution ---
        let action = self.decider.evaluate(&self.pair);

        // --- Depth gate (entries & rebalances only) ---
        match &action {
            DeciderAction::Enter {
                size_long,
                size_short,
            }
            | DeciderAction::Rebalance {
                size_long,
                size_short,
            }
            | DeciderAction::Recover {
                size_long,
                size_short,
            } => {
                if !self.pair.has_sufficient_depth(*size_long, *size_short) {
                    warn!(
                        action = ?action,
                        "Insufficient orderbook depth — skipping action"
                    );
                    return;
                }
            }
            // Exit / Unwind / DoNothing — never depth-gated.
            _ => {}
        }

        if let Err(e) = self.execution.execute(&action, &mut self.pair).await {
            error!(error = %e, "Execution error");
        } else {
            debug!("evaluate_and_execute complete");
        }
    }
}
