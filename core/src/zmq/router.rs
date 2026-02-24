use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use rust_decimal::Decimal;
use tracing::info;

use super::client::{ZmqClient, ZmqCommandClient};
use crate::AccountSnapshot;
use crate::traits::CoreApi;
use crate::types::{
    Exchange, FundingRateSnapshot, Instrument, InstrumentKey, Order, OrderRequest, Position,
};

/// Smart router: resolves `InstrumentKey.exchange` → `ZmqCommandClient`
/// automatically. Cloneable — every clone shares the same connections.
#[derive(Clone)]
pub struct ZmqRouter {
    inner: Arc<RouterInner>,
}

struct RouterInner {
    clients: HashMap<Exchange, ZmqCommandClient>,
}

impl ZmqRouter {
    pub fn new(clients: HashMap<Exchange, ZmqCommandClient>) -> Self {
        Self {
            inner: Arc::new(RouterInner { clients }),
        }
    }

    /// Connect to every exchange in the list via Redis service discovery.
    ///
    /// Creates one DEALER connection per unique exchange. Subscribers are
    /// NOT created — use [`ZmqClient::connect`] directly when you need a
    /// [`ZmqEventSubscriber`].
    pub async fn connect_all(redis_url: &str, exchanges: &[Exchange]) -> Result<Self> {
        let mut clients = HashMap::new();
        for &ex in exchanges {
            if clients.contains_key(&ex) {
                continue;
            }
            info!(exchange = %ex, "Connecting ZMQ command client for router");
            let zmq = ZmqClient::connect(redis_url, ex).await?;
            let (cmd, _sub) = zmq.split();
            clients.insert(ex, cmd);
        }
        Ok(Self::new(clients))
    }

    pub fn exchanges(&self) -> Vec<Exchange> {
        self.inner.clients.keys().copied().collect()
    }

    pub fn has_exchange(&self, exchange: Exchange) -> bool {
        self.inner.clients.contains_key(&exchange)
    }

    fn client(&self, exchange: Exchange) -> Result<&ZmqCommandClient> {
        self.inner
            .clients
            .get(&exchange)
            .ok_or_else(|| anyhow!("no ZMQ client registered for exchange: {}", exchange))
    }
}

#[async_trait::async_trait]
impl CoreApi for ZmqRouter {
    async fn submit_order(&self, req: OrderRequest) -> Result<Order> {
        self.client(req.key.exchange)?.submit_order(req).await
    }

    async fn get_position(&self, key: InstrumentKey) -> Result<Option<Position>> {
        self.client(key.exchange)?.get_position(key).await
    }

    async fn get_instrument(&self, key: InstrumentKey) -> Result<Option<Instrument>> {
        self.client(key.exchange)?.get_instrument(key).await
    }

    async fn get_reference_price(&self, key: InstrumentKey) -> Result<Decimal> {
        self.client(key.exchange)?.get_reference_price(key).await
    }

    async fn get_funding_rate(&self, key: InstrumentKey) -> Result<FundingRateSnapshot> {
        self.client(key.exchange)?.get_funding_rate(key).await
    }

    async fn set_leverage(&self, key: InstrumentKey, leverage: Decimal) -> Result<Decimal> {
        self.client(key.exchange)?.set_leverage(key, leverage).await
    }

    async fn cancel_order(&self, exchange: Exchange, id: String) -> Result<Order> {
        self.client(exchange)?.cancel_order(exchange, id).await
    }

    async fn get_all_positions(&self, exchange: Exchange) -> Result<Vec<Position>> {
        self.client(exchange)?.get_all_positions(exchange).await
    }

    async fn get_account_snapshot(&self, exchange: Exchange) -> Result<AccountSnapshot> {
        self.client(exchange)?.get_account_snapshot(exchange).await
    }

    async fn get_all_instruments(&self, exchange: Exchange) -> Result<Vec<Instrument>> {
        self.client(exchange)?.get_all_instruments(exchange).await
    }
}
