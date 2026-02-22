use anyhow::Result;
use boldtrax_core::AccountSnapshot;
use boldtrax_core::types::{Exchange, Instrument, InstrumentKey, Order, OrderRequest, Position};
use boldtrax_core::zmq::client::{ZmqClient, ZmqCommandClient};
use boldtrax_core::zmq::protocol::ZmqEvent;
use tokio::sync::{broadcast, mpsc};

pub struct ManualStrategy {
    client: ZmqCommandClient,
    event_tx: broadcast::Sender<ZmqEvent>,
}

impl ManualStrategy {
    pub async fn new(redis_url: &str, exchange: Exchange) -> Result<Self> {
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

        Ok(Self {
            client: command_client,
            event_tx,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ZmqEvent> {
        self.event_tx.subscribe()
    }

    pub async fn submit_order(&mut self, req: OrderRequest) -> Result<Order> {
        self.client.submit_order(req).await
    }

    pub async fn cancel_order(&mut self, id: String) -> Result<Order> {
        self.client.cancel_order(id).await
    }

    pub async fn get_position(&mut self, key: InstrumentKey) -> Result<Option<Position>> {
        self.client.get_position(key).await
    }

    pub async fn get_all_positions(&mut self) -> Result<Vec<Position>> {
        self.client.get_all_positions().await
    }

    pub async fn get_account_snapshot(&mut self) -> Result<AccountSnapshot> {
        self.client.get_account_snapshot().await
    }

    #[allow(dead_code)]
    pub async fn get_instrument(&mut self, key: InstrumentKey) -> Result<Option<Instrument>> {
        self.client.get_instrument(key).await
    }

    pub async fn get_all_instruments(&mut self) -> Result<Vec<Instrument>> {
        self.client.get_all_instruments().await
    }
}
