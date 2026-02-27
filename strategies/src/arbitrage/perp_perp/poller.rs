use std::time::Duration;

use boldtrax_core::CoreApi;
use boldtrax_core::types::{InstrumentKey, OrderEvent};
use boldtrax_core::zmq::client::ZmqEventSubscriber;
use boldtrax_core::zmq::protocol::ZmqEvent;
use boldtrax_core::zmq::router::ZmqRouter;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tokio::sync::{mpsc, watch};
use tracing::{debug, error, info, warn};

use crate::arbitrage::perp_perp::decider::PerpPerpDecider;
use crate::arbitrage::perp_perp::execution::PerpPerpExecutionEngine;
use crate::arbitrage::perp_perp::types::PerpPerpPair;
use crate::arbitrage::policy::{DecisionPolicy, ExecutionPolicy, MarginPolicy, PriceSource};
use crate::arbitrage::recovery::{RecoveryContext, RecoveryDecision, assess_recovery};
use crate::arbitrage::types::{
    DEFAULT_UNWIND_CHUNKS, DeciderAction, PairState, PairStatus, StrategyCommand, StrategyResponse,
    StrategyStatus, TrackedOrderStatus, UnwindSchedule, next_unwind_action,
};

/// Orders older than this are considered stale and will be auto-cancelled.
const STALE_ORDER_TIMEOUT: Duration = Duration::from_secs(10);

pub struct PerpPerpPoller<M, P>
where
    M: MarginPolicy,
    P: PriceSource,
{
    pub pair: PerpPerpPair,
    pub decider: PerpPerpDecider,
    pub execution: PerpPerpExecutionEngine,
    pub margin: M,
    pub oracle: P,
    pub router: ZmqRouter,
    pub poll_interval: Duration,
    pub strategy_id: String,
    paused: bool,
    status_tx: watch::Sender<StrategyResponse>,
    /// When the current recovery process started (None when not Recovering).
    recovery_started: Option<DateTime<Utc>>,
}

impl<M, P> PerpPerpPoller<M, P>
where
    M: MarginPolicy,
    P: PriceSource,
{
    pub fn new(
        pair: PerpPerpPair,
        decider: PerpPerpDecider,
        execution: PerpPerpExecutionEngine,
        margin: M,
        oracle: P,
        router: ZmqRouter,
        poll_interval: Duration,
    ) -> Self {
        let (status_tx, _) = watch::channel(StrategyResponse::Status(StrategyStatus {
            strategy_id: String::new(),
            pair_status: format!("{:?}", pair.status()),
            paused: false,
            pending_orders: 0,
        }));
        Self {
            pair,
            decider,
            execution,
            margin,
            oracle,
            router,
            poll_interval,
            strategy_id: String::new(),
            paused: false,
            status_tx,
            recovery_started: None,
        }
    }

    pub fn with_strategy_id(mut self, id: impl Into<String>) -> Self {
        self.strategy_id = id.into();
        self
    }

    /// Returns a watch receiver for the command server to read
    /// the latest `StrategyStatus` when `GetStatus` arrives.
    pub fn status_watch(&self) -> watch::Receiver<StrategyResponse> {
        self.status_tx.subscribe()
    }

    /// Production entry point with command channel.
    pub async fn run(
        self,
        long_subscriber: ZmqEventSubscriber,
        short_subscriber: ZmqEventSubscriber,
        cmd_rx: mpsc::Receiver<StrategyCommand>,
    ) {
        let (tx, rx) = mpsc::channel(1024);

        let tx2 = tx.clone();
        let _long_listener = long_subscriber.spawn_event_listener(tx);
        let _short_listener = short_subscriber.spawn_event_listener(tx2);

        info!(
            poll_interval_secs = self.poll_interval.as_secs(),
            long_key = %self.pair.long_leg.key,
            short_key = %self.pair.short_leg.key,
            "Starting PerpPerp Poller"
        );

        self.run_with_commands(rx, cmd_rx).await;
    }

    /// For testing, accepts a raw `mpsc::Receiver` (no command channel).
    pub async fn run_loop(
        self,
        rx: mpsc::Receiver<ZmqEvent>,
    ) -> (PerpPerpPair, PerpPerpExecutionEngine) {
        let (_tx, cmd_rx) = mpsc::channel(1);
        self.run_with_commands(rx, cmd_rx).await
    }

    /// Core event loop — listens for ZMQ events, poll ticks, and commands.
    pub async fn run_with_commands(
        mut self,
        mut rx: mpsc::Receiver<ZmqEvent>,
        mut cmd_rx: mpsc::Receiver<StrategyCommand>,
    ) -> (PerpPerpPair, PerpPerpExecutionEngine) {
        let mut interval = tokio::time::interval(self.poll_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.poll_and_evaluate().await;
                }
                event = rx.recv() => {
                    match event {
                        Some(evt) => self.handle_event(evt).await,
                        None => {
                            info!("Event channel closed, stopping poller");
                            break;
                        }
                    }
                }
                cmd = cmd_rx.recv() => {
                    if let Some(c) = cmd {
                        self.handle_command(c).await
                    }
                }
            }
        }

        (self.pair, self.execution)
    }

    async fn poll_and_evaluate(&mut self) {
        if self.paused {
            debug!("Poll skipped — strategy is paused");
            return;
        }

        // 1. Refresh funding rates from cache or API.
        let long_key = self.pair.long_leg.key;
        let short_key = self.pair.short_leg.key;
        self.refresh_funding(&long_key).await;
        self.refresh_funding(&short_key).await;

        // 2. Refresh prices from oracle.
        self.pair
            .refresh_prices(&|key| self.oracle.get_mid_price(key));

        // 2b. Refresh orderbook levels for post-only pricing.
        //     Use level 2 (3rd level) with fallback to level 1, then level 0.
        //     This places maker orders 2-3 levels deep for better fill probability.
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

        // 3. Sync positions from exchange (optional — positions also arrive via events).
        self.sync_positions().await;

        // 3a. Cancel stale orders before evaluating.
        //     Skip during Entering — entry orders need time to fill on illiquid
        //     pairs; if entry truly fails, the order goes terminal and
        //     check_tracker_transitions moves us to Recovering.
        //     Skip during Recovering — recovery assessment handles its own orders.
        if !matches!(
            *self.pair.status(),
            PairStatus::Entering | PairStatus::Recovering
        ) {
            self.cancel_stale_orders().await;
        }

        // 3b. Transitional statuses with dedicated retry loops.
        //     These run instead of normal decider evaluation because the
        //     decider intentionally skips transitional statuses.
        match *self.pair.status() {
            PairStatus::Unwinding => {
                self.continue_unwind().await;
                return;
            }
            PairStatus::Recovering => {
                self.continue_recovery().await;
                return;
            }
            PairStatus::Exiting => {
                self.continue_exit().await;
                return;
            }
            _ => {}
        }

        // 4. Margin checks.
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
                    self.execution.cancel_pending_orders(&mut self.pair).await;

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

        // 5. Decision + Execution.
        let action = self.decider.evaluate(&self.pair);

        // 5b. Depth gate (entries & rebalances only).
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
            _ => {}
        }

        if let Err(e) = self.execution.execute(&action, &mut self.pair).await {
            error!(error = %e, "Execution error");
        } else {
            debug!(
                spread = %self.pair.funding_spread(),
                status = ?self.pair.status(),
                "Poll cycle complete"
            );
        }
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
                self.publish_status();
            }
            StrategyCommand::Pause => {
                self.paused = true;
                info!("Strategy paused — evaluation suspended");
                self.publish_status();
            }
            StrategyCommand::Resume => {
                self.paused = false;
                info!("Strategy resumed — evaluation re-enabled");
                self.publish_status();
            }
            StrategyCommand::GetStatus => {
                self.publish_status();
            }
        }
    }

    fn publish_status(&self) {
        let status = StrategyStatus {
            strategy_id: self.strategy_id.clone(),
            pair_status: format!("{:?}", self.pair.status()),
            paused: self.paused,
            pending_orders: self.pair.order_tracker().pending_count(),
        };
        let _ = self.status_tx.send(StrategyResponse::Status(status));
    }

    async fn refresh_funding(&mut self, key: &InstrumentKey) {
        if let Some(cached) = self.pair.funding_cache.get_if_fresh(key) {
            let rate = cached.funding_rate;
            self.pair.leg_mut(key).funding_rate = rate;
            debug!(key = %key, rate = %rate, source = "cache", "Funding rate from cache");
            return;
        }

        let result = self.router.get_funding_rate(*key).await;

        match result {
            Ok(snapshot) => {
                info!(
                    key = %key,
                    rate = %snapshot.funding_rate,
                    mark_price = %snapshot.mark_price,
                    source = "api",
                    "Funding rate fetched from exchange"
                );
                let leg = self.pair.leg_mut(key);
                leg.funding_rate = snapshot.funding_rate;
                if leg.current_price.is_zero() && !snapshot.mark_price.is_zero() {
                    leg.current_price = snapshot.mark_price;
                }
                self.pair.funding_cache.update(*key, snapshot);
            }
            Err(e) => {
                warn!(
                    key = %key,
                    error = %e,
                    "Failed to fetch funding rate from exchange, using last known"
                );
            }
        }
    }

    async fn sync_positions(&mut self) {
        let long_key = self.pair.long_leg.key;
        let short_key = self.pair.short_leg.key;

        let (long_res, short_res) = tokio::join!(
            self.router.get_position(long_key),
            self.router.get_position(short_key),
        );

        match long_res {
            Ok(Some(pos)) => {
                self.pair.long_leg.position_size = pos.size;
                self.pair.long_leg.entry_price = pos.entry_price;
                self.pair.position_long = Some(pos);
            }
            Ok(None) => {}
            Err(e) => {
                debug!(key = %long_key, error = %e, "Failed to sync long position");
            }
        }

        match short_res {
            Ok(Some(pos)) => {
                self.pair.short_leg.position_size = pos.size;
                self.pair.short_leg.entry_price = pos.entry_price;
                self.pair.position_short = Some(pos);
            }
            Ok(None) => {}
            Err(e) => {
                debug!(key = %short_key, error = %e, "Failed to sync short position");
            }
        }

        self.pair.maybe_transition_to_active();
    }

    /// Handle events from the merged event stream.
    async fn handle_event(&mut self, event: ZmqEvent) {
        match event {
            // Market data → oracle (same as ArbitrageEngine)
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
                self.update_tracker(evt);
                self.pair.apply_event(event);
                let continuation = self.check_tracker_transitions();
                if let Some(action) = continuation
                    && let Err(e) = self.execution.execute(&action, &mut self.pair).await
                {
                    error!(error = %e, "Continuation execution failed");
                }
            }
            // Pair-specific events
            event => {
                self.pair.apply_event(event);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Tracker + lifecycle transitions (mirrors ArbitrageEngine logic)
    // -----------------------------------------------------------------------

    fn update_tracker(&mut self, evt: &OrderEvent) {
        let order = evt.inner();
        let status = TrackedOrderStatus::from_order_event(evt);
        let tracker = self.pair.order_tracker_mut();

        if !tracker.update_fill(&order.client_order_id, order.filled_size, status.clone())
            && !tracker.update_fill(&order.internal_id, order.filled_size, status)
        {
            debug!(
                client_order_id = %order.client_order_id,
                internal_id = %order.internal_id,
                "OrderUpdate for untracked order — probably from another strategy"
            );
        }
    }

    fn check_tracker_transitions(&mut self) -> Option<DeciderAction> {
        if self.pair.order_tracker().has_pending() {
            return None;
        }

        let current = self.pair.status().clone();
        if !current.is_transitional() {
            return None;
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
                    // Immediately dispatch recovery for the missing leg.
                    let action = self.compute_recovery_action();
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
                    self.recovery_started = None;
                    (PairStatus::Active, None)
                } else {
                    warn!("Recovery orders terminal but legs still imbalanced — retrying recovery");
                    let action = self.compute_recovery_action();
                    (PairStatus::Recovering, action)
                }
            }
            PairStatus::Unwinding => {
                if self.pair.is_fully_exited() {
                    info!("Unwind complete — both legs closed, transitioning to Inactive");
                    self.pair.order_tracker_mut().unwind_schedule = None;
                    (PairStatus::Inactive, None)
                } else {
                    let action = {
                        let (rl, rs) = self.pair.close_sizes();
                        let tracker = self.pair.order_tracker_mut();
                        match tracker.unwind_schedule.as_mut() {
                            Some(sched) if !sched.is_complete() => {
                                next_unwind_action(sched, rl, rs)
                            }
                            Some(_) => {
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
            _ => return None,
        };

        self.pair.set_status(next);
        self.pair.order_tracker_mut().remove_terminal();
        self.publish_status();
        continuation
    }

    /// Compute the recovery action for the missing leg.
    /// Returns `None` if prices aren't available yet or both/neither legs are present.
    fn compute_recovery_action(&self) -> Option<DeciderAction> {
        self.pair.recovery_sizes().map(|(size_long, size_short)| {
            info!(
                recovery_long = %size_long,
                recovery_short = %size_short,
                "Computing recovery for missing leg"
            );
            DeciderAction::Recover {
                size_long,
                size_short,
            }
        })
    }

    /// Smart recovery: assess economics each tick and act accordingly.
    ///
    /// Instead of blindly retrying limit orders, this evaluates the filled
    /// leg's P&L, the resting order's competitiveness, and elapsed time
    /// to decide between Rest / Reprice / Chase / AbortAndReset.
    async fn continue_recovery(&mut self) {
        // 1. Check if recovery is done — both legs in position.
        if self.pair.is_fully_entered() {
            info!("Recovery complete — transitioning to Active");
            self.pair.set_status(PairStatus::Active);
            self.pair.order_tracker_mut().remove_terminal();
            self.recovery_started = None;
            self.publish_status();
            return;
        }

        // 2. Defensive: if both legs are zero, nothing to recover from.
        if self.pair.is_fully_exited() {
            warn!("Both legs empty during Recovering — transitioning to Inactive");
            self.pair.set_status(PairStatus::Inactive);
            self.pair.order_tracker_mut().remove_terminal();
            self.recovery_started = None;
            self.publish_status();
            return;
        }

        // 3. Ensure recovery clock is running.
        let started = *self.recovery_started.get_or_insert_with(Utc::now);

        // 4. Gather all read-only inputs before any mutations.
        let (filled_pnl, filled_notional) = self.filled_leg_economics();
        let (order_is_stale, has_resting) = self.recovery_order_staleness();
        let elapsed = (Utc::now() - started).to_std().unwrap_or_default();

        // 5. Assess.
        let ctx = RecoveryContext {
            filled_pnl,
            filled_notional,
            order_is_stale,
            has_resting_order: has_resting,
            elapsed,
        };
        let decision = assess_recovery(&ctx);

        info!(
            ?decision,
            filled_pnl = %filled_pnl,
            filled_notional = %filled_notional,
            order_stale = order_is_stale,
            has_resting = has_resting,
            elapsed_secs = elapsed.as_secs(),
            "Recovery assessment"
        );

        // 6. Execute decision.
        match decision {
            RecoveryDecision::Rest => {
                debug!("Recovery order resting — no action needed");
            }
            RecoveryDecision::Reprice => {
                self.cancel_pending_recovery_orders().await;
                self.dispatch_recovery_limit().await;
            }
            RecoveryDecision::Chase => {
                self.cancel_pending_recovery_orders().await;
                self.dispatch_recovery_market().await;
            }
            RecoveryDecision::AbortAndReset => {
                warn!(
                    filled_pnl = %filled_pnl,
                    elapsed_secs = elapsed.as_secs(),
                    "Recovery abort — closing filled leg and resetting"
                );
                self.cancel_pending_recovery_orders().await;
                self.recovery_started = None;
                // Exit closes the filled leg; missing leg has zero size → no-op.
                let action = DeciderAction::Exit;
                if let Err(e) = self.execution.execute(&action, &mut self.pair).await {
                    error!(error = %e, "Recovery abort exit failed");
                }
            }
        }
    }

    /// Compute unrealized P&L and absolute notional of the filled leg.
    ///
    /// The filled leg is whichever has a non-zero position_size.
    /// P&L is signed: `position_size × (current_price - entry_price)`.
    fn filled_leg_economics(&self) -> (Decimal, Decimal) {
        let filled = if !self.pair.long_leg.position_size.is_zero() {
            &self.pair.long_leg
        } else {
            &self.pair.short_leg
        };
        let pnl = filled.position_size * (filled.current_price - filled.entry_price);
        let notional = filled.position_size.abs() * filled.current_price;
        (pnl, notional)
    }

    /// Check whether the resting recovery order is stale vs current market.
    ///
    /// Returns `(is_stale, has_resting_order)`.
    /// A buy order is stale if best_bid moved above its price (deeper in book).
    /// A sell order is stale if best_ask moved below its price (deeper in book).
    fn recovery_order_staleness(&self) -> (bool, bool) {
        let missing = if self.pair.long_leg.position_size.is_zero() {
            &self.pair.long_leg
        } else {
            &self.pair.short_leg
        };

        let pending = self.pair.order_tracker().pending_for_leg(&missing.key);
        if pending.is_empty() {
            return (false, false);
        }

        let resting_price = match pending[0].order_price {
            Some(p) => p,
            None => return (false, true), // has order but no price info (market order?)
        };

        // Missing long leg → buy order → stale if best_bid moved above our price.
        // Missing short leg → sell order → stale if best_ask moved below our price.
        let is_stale = if missing.key == self.pair.long_leg.key {
            missing.best_bid.is_some_and(|bid| bid > resting_price)
        } else {
            missing.best_ask.is_some_and(|ask| ask < resting_price)
        };

        (is_stale, true)
    }

    /// Cancel all pending recovery orders: optimistic tracker update, then
    /// fire the actual cancel to the exchange.
    async fn cancel_pending_recovery_orders(&mut self) {
        let pending: Vec<_> = self
            .pair
            .order_tracker()
            .all_pending()
            .iter()
            .map(|o| (o.key.exchange, o.order_id.clone()))
            .collect();

        if pending.is_empty() {
            return;
        }

        // Optimistic: mark cancelled in tracker immediately so new orders
        // won't see these as "has_pending" on the same tick.
        for (_, order_id) in &pending {
            self.pair.order_tracker_mut().force_cancel(order_id);
        }
        self.pair.order_tracker_mut().remove_terminal();

        // Fire actual cancels to the exchange.
        for (exchange, order_id) in pending {
            info!(%exchange, %order_id, "Cancelling recovery order");
            if let Err(e) = self.execution.cancel_order(exchange, order_id).await {
                error!(error = %e, "Failed to cancel recovery order");
            }
        }
    }

    /// Place a fresh limit order for the missing leg at current best price.
    async fn dispatch_recovery_limit(&mut self) {
        self.pair.order_tracker_mut().remove_terminal();

        if let Some(action) = self.compute_recovery_action()
            && let Err(e) = self.execution.execute(&action, &mut self.pair).await
        {
            error!(error = %e, "Recovery limit order failed");
        }
    }

    /// Market order the missing leg for immediate fill (chase).
    async fn dispatch_recovery_market(&mut self) {
        self.pair.order_tracker_mut().remove_terminal();

        if let Some((size_long, size_short)) = self.pair.recovery_sizes() {
            info!(
                long = %size_long, short = %size_short,
                "Chasing missing leg with market order"
            );
            if let Err(e) = self
                .execution
                .chase_missing_leg(&mut self.pair, size_long, size_short)
                .await
            {
                error!(error = %e, "Recovery chase market order failed");
            }
        }
    }

    /// Continue exiting: retry close orders for remaining positions.
    /// Called from `poll_and_evaluate` when status is `Exiting`.
    async fn continue_exit(&mut self) {
        if self.pair.is_fully_exited() {
            info!("Exit complete — transitioning to Inactive");
            self.pair.set_status(PairStatus::Inactive);
            self.pair.order_tracker_mut().remove_terminal();
            self.publish_status();
            return;
        }

        if self.pair.order_tracker().has_pending() {
            debug!("Exit orders still in flight — waiting");
            return;
        }

        self.pair.order_tracker_mut().remove_terminal();

        let action = DeciderAction::Exit;
        if let Err(e) = self.execution.execute(&action, &mut self.pair).await {
            error!(error = %e, "Exit retry execution failed");
        }
    }

    /// Continue unwinding: dispatch the next chunk or finalize.
    /// Called from `poll_and_evaluate` when status is already `Unwinding`.
    async fn continue_unwind(&mut self) {
        if self.pair.is_fully_exited() {
            info!("Unwind complete — transitioning to Inactive");
            self.pair.set_status(PairStatus::Inactive);
            self.pair.order_tracker_mut().unwind_schedule = None;
            self.publish_status();
            return;
        }

        // Don't dispatch a new chunk if previous orders are still in flight.
        if self.pair.order_tracker().has_pending() {
            debug!("Unwind chunk still in flight — waiting");
            return;
        }

        let action = {
            let (rl, rs) = self.pair.close_sizes();
            let tracker = self.pair.order_tracker_mut();
            tracker.remove_terminal();
            match tracker.unwind_schedule.as_mut() {
                Some(sched) if !sched.is_complete() => next_unwind_action(sched, rl, rs),
                Some(_) => {
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

        if let Some(action) = action
            && let Err(e) = self.execution.execute(&action, &mut self.pair).await
        {
            error!(error = %e, "Unwind chunk execution failed");
        }
    }

    /// Cancel orders that have been pending/open longer than [`STALE_ORDER_TIMEOUT`].
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
}
