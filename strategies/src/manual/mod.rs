use anyhow::Result;
use boldtrax_core::AccountSnapshot;
use boldtrax_core::CoreApi;
use boldtrax_core::types::{Exchange, Instrument, InstrumentKey, Order, OrderRequest, Position};
use boldtrax_core::zmq::client::ZmqClient;
use boldtrax_core::zmq::protocol::ZmqEvent;
use boldtrax_core::zmq::router::ZmqRouter;
use tokio::sync::{broadcast, mpsc};

pub struct ManualStrategy {
    router: ZmqRouter,
    exchange: Exchange,
    event_tx: broadcast::Sender<ZmqEvent>,
}

impl ManualStrategy {
    pub async fn new(redis_url: &str, exchange: Exchange) -> Result<Self> {
        // SUB socket needs its own connection — router only manages DEALER sockets.
        let client = ZmqClient::connect(redis_url, exchange).await?;
        let (command_client, event_subscriber) = client.split();
        let (event_tx, _) = broadcast::channel(1024);

        let (mpsc_tx, mut mpsc_rx) = mpsc::channel(1024);
        event_subscriber.spawn_event_listener(mpsc_tx);

        let tx_clone = event_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = mpsc_rx.recv().await {
                let _ = tx_clone.send(event);
            }
        });

        let mut clients = std::collections::HashMap::new();
        clients.insert(exchange, command_client);
        let router = ZmqRouter::new(clients);

        Ok(Self {
            router,
            exchange,
            event_tx,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ZmqEvent> {
        self.event_tx.subscribe()
    }

    pub async fn submit_order(&self, req: OrderRequest) -> Result<Order> {
        self.router.submit_order(req).await
    }

    pub async fn cancel_order(&self, id: String) -> Result<Order> {
        self.router.cancel_order(self.exchange, id).await
    }

    pub async fn get_position(&self, key: InstrumentKey) -> Result<Option<Position>> {
        self.router.get_position(key).await
    }

    pub async fn get_all_positions(&self) -> Result<Vec<Position>> {
        self.router.get_all_positions(self.exchange).await
    }

    pub async fn get_account_snapshot(&self) -> Result<AccountSnapshot> {
        self.router.get_account_snapshot(self.exchange).await
    }

    #[allow(dead_code)]
    pub async fn get_instrument(&self, key: InstrumentKey) -> Result<Option<Instrument>> {
        self.router.get_instrument(key).await
    }

    pub async fn get_all_instruments(&self) -> Result<Vec<Instrument>> {
        self.router.get_all_instruments(self.exchange).await
    }
}
