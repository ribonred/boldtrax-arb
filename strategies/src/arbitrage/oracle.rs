use boldtrax_core::CoreApi;
use boldtrax_core::types::{InstrumentKey, OrderBookSnapshot, Ticker};
use boldtrax_core::zmq::router::ZmqRouter;
use rust_decimal::Decimal;
use std::collections::HashMap;
use tracing::debug;

use crate::arbitrage::policy::PriceSource;

const DEPTH_LEVELS: usize = 3;

#[derive(Default)]
pub struct PriceOracle {
    snapshots: HashMap<InstrumentKey, OrderBookSnapshot>,
    tickers: HashMap<InstrumentKey, Ticker>,
}

impl PriceOracle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fetch the reference price via ZMQ if it's not available locally.
    pub async fn fetch_reference_price(
        &mut self,
        router: &ZmqRouter,
        key: InstrumentKey,
    ) -> anyhow::Result<Decimal> {
        if let Some(mid) = self.get_mid_price_inner(&key) {
            return Ok(mid);
        }

        debug!(?key, "Fetching reference price via ZMQ");
        let price = router.get_reference_price(key).await?;
        Ok(price)
    }
}

impl PriceSource for PriceOracle {
    fn name(&self) -> &'static str {
        "price_oracle"
    }

    fn update_snapshot(&mut self, snapshot: OrderBookSnapshot) {
        self.snapshots.insert(snapshot.key, snapshot);
    }

    fn update_ticker(&mut self, ticker: Ticker) {
        self.tickers.insert(ticker.key, ticker);
    }

    fn get_mid_price_inner(&self, key: &InstrumentKey) -> Option<Decimal> {
        self.snapshots.get(key).and_then(|s| s.mid)
    }

    fn get_best_bid_inner(&self, key: &InstrumentKey) -> Option<Decimal> {
        self.snapshots.get(key).and_then(|s| s.best_bid)
    }

    fn get_best_ask_inner(&self, key: &InstrumentKey) -> Option<Decimal> {
        self.snapshots.get(key).and_then(|s| s.best_ask)
    }

    fn get_ask_depth(&self, key: &InstrumentKey) -> Option<Decimal> {
        self.snapshots
            .get(key)
            .map(|s| s.asks.iter().take(DEPTH_LEVELS).map(|l| l.quantity).sum())
    }

    fn get_bid_depth(&self, key: &InstrumentKey) -> Option<Decimal> {
        self.snapshots
            .get(key)
            .map(|s| s.bids.iter().take(DEPTH_LEVELS).map(|l| l.quantity).sum())
    }
}
