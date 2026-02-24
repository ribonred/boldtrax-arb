use std::time::Duration;

use boldtrax_core::CoreApi;
use boldtrax_core::types::InstrumentKey;
use boldtrax_core::zmq::client::ZmqEventSubscriber;
use boldtrax_core::zmq::protocol::ZmqEvent;
use boldtrax_core::zmq::router::ZmqRouter;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::arbitrage::perp_perp::decider::PerpPerpDecider;
use crate::arbitrage::perp_perp::execution::PerpPerpExecutionEngine;
use crate::arbitrage::perp_perp::types::PerpPerpPair;
use crate::arbitrage::policy::{DecisionPolicy, ExecutionPolicy, MarginPolicy, PriceSource};
use crate::arbitrage::types::PairState;

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
}

impl<M, P> PerpPerpPoller<M, P>
where
    M: MarginPolicy,
    P: PriceSource,
{
    pub async fn run(
        self,
        long_subscriber: ZmqEventSubscriber,
        short_subscriber: ZmqEventSubscriber,
    ) {
        let (tx, rx) = mpsc::channel(1024);

        // Merge both exchange event streams into one channel.
        let tx2 = tx.clone();
        let _long_listener = long_subscriber.spawn_event_listener(tx);
        let _short_listener = short_subscriber.spawn_event_listener(tx2);

        info!(
            poll_interval_secs = self.poll_interval.as_secs(),
            long_key = %self.pair.long_leg.key,
            short_key = %self.pair.short_leg.key,
            "Starting PerpPerp Poller"
        );

        self.run_loop(rx).await;
    }

    /// For testing, accepts a raw `mpsc::Receiver` so tests can feed
    /// synthetic events without ZMQ.
    pub async fn run_loop(
        mut self,
        mut rx: mpsc::Receiver<ZmqEvent>,
    ) -> (PerpPerpPair, PerpPerpExecutionEngine) {
        let mut interval = tokio::time::interval(self.poll_interval);
        // Don't burst-fire ticks that accumulated while we were busy.
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
            }
        }

        (self.pair, self.execution)
    }

    async fn poll_and_evaluate(&mut self) {
        // 1. Refresh funding rates from cache or API.
        let long_key = self.pair.long_leg.key;
        let short_key = self.pair.short_leg.key;
        self.refresh_funding(&long_key).await;
        self.refresh_funding(&short_key).await;

        // 2. Refresh prices from oracle.
        self.pair
            .refresh_prices(&|key| self.oracle.get_mid_price(key));

        // 3. Sync positions from exchange (optional — positions also arrive via events).
        self.sync_positions().await;

        // 4. Margin checks.
        for (pos, price) in self.pair.positions_for_margin_check() {
            if let Err(violation) = self.margin.check_position(pos, price) {
                warn!(%violation, "Position margin gate blocked evaluation");
                return;
            }
        }

        // 5. Decision + Execution.
        let action = self.decider.evaluate(&self.pair);
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
            // Pair-specific events
            event => {
                self.pair.apply_event(event);
            }
        }
    }
}
