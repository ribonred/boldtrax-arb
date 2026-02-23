use crate::arbitrage::policy::{DecisionPolicy, ExecutionPolicy, MarginPolicy, PriceSource};
use crate::arbitrage::types::PairState;
use boldtrax_core::AccountSnapshot;
use boldtrax_core::zmq::client::ZmqEventSubscriber;
use boldtrax_core::zmq::protocol::ZmqEvent;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

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
        Self {
            pair,
            oracle,
            decider,
            margin,
            execution,
            account: None,
        }
    }

    /// Production entry point — driven by a live ZMQ subscriber.
    pub async fn run(self, subscriber: ZmqEventSubscriber) {
        let (tx, rx) = mpsc::channel(1024);
        let _listener_handle = subscriber.spawn_event_listener(tx);
        info!("Starting Arbitrage Engine");
        self.run_with_stream(rx).await;
    }

    pub async fn run_with_stream(mut self, mut rx: mpsc::Receiver<ZmqEvent>) -> (Pair, E) {
        while let Some(event) = rx.recv().await {
            self.handle_event(event).await;
        }

        (self.pair, self.execution)
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
                // Delegate to pair so spot fills update spot.quantity
                self.pair.apply_event(event);
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

    async fn evaluate_and_execute(&mut self) {
        // --- Account-level margin gate (skip if no snapshot yet) ---
        if let Some(snap) = self.account.as_ref()
            && let Err(violation) = self.margin.check_account(snap)
        {
            warn!(%violation, "Account margin gate blocked evaluation");
            return;
        }

        // --- Refresh prices from oracle ---
        self.pair
            .refresh_prices(&|key| self.oracle.get_mid_price(key));

        // --- Refresh orderbook bid/ask for smart execution ---
        self.pair.refresh_orderbook(
            &|key| self.oracle.get_best_bid(key),
            &|key| self.oracle.get_best_ask(key),
            &|key| self.oracle.get_ask_depth(key),
            &|key| self.oracle.get_bid_depth(key),
        );

        // --- Position-level margin gates ---
        for (pos, price) in self.pair.positions_for_margin_check() {
            if let Err(violation) = self.margin.check_position(pos, price) {
                warn!(%violation, "Position margin gate blocked evaluation");
                return;
            }
        }

        // --- Decision + Execution ---
        let action = self.decider.evaluate(&self.pair);
        if let Err(e) = self.execution.execute(&action, &mut self.pair).await {
            error!(error = %e, "Execution error");
        } else {
            debug!("evaluate_and_execute complete");
        }
    }
}
