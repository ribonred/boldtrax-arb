use crate::AccountSnapshot;
use crate::types::{Exchange, Instrument, InstrumentKey, Order, OrderRequest, Position};
use crate::zmq::discovery::{DiscoveryClient, ServiceType};
use crate::zmq::protocol::{ZmqCommand, ZmqEvent, ZmqResponse};
use anyhow::{Context, Result};
use rkyv::{Deserialize, Infallible};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info};
use zeromq::{DealerSocket, Socket, SocketRecv, SocketSend, SubSocket, ZmqMessage};

pub struct ZmqClient {
    dealer: DealerSocket,
    sub: SubSocket,
}

pub struct ZmqCommandClient {
    dealer: DealerSocket,
}

pub struct ZmqEventSubscriber {
    sub: SubSocket,
}

impl ZmqClient {
    pub async fn connect(redis_url: &str, exchange: Exchange) -> Result<Self> {
        let discovery = DiscoveryClient::new(redis_url)?;

        // Discover ROUTER endpoint
        let router_endpoint = discovery
            .discover_service(exchange, ServiceType::Router)
            .await?
            .context("ROUTER service not found for exchange")?;

        // Discover PUB endpoint
        let pub_endpoint = discovery
            .discover_service(exchange, ServiceType::Pub)
            .await?
            .context("PUB service not found for exchange")?;

        info!(
            exchange = ?exchange,
            router = %router_endpoint,
            pub_endpoint = %pub_endpoint,
            "Discovered ZMQ endpoints"
        );

        let mut dealer = DealerSocket::new();
        dealer.connect(&router_endpoint).await?;

        let mut sub = SubSocket::new();
        sub.connect(&pub_endpoint).await?;
        // Subscribe to all events for now
        sub.subscribe("").await?;

        Ok(Self { dealer, sub })
    }

    pub fn split(self) -> (ZmqCommandClient, ZmqEventSubscriber) {
        (
            ZmqCommandClient {
                dealer: self.dealer,
            },
            ZmqEventSubscriber { sub: self.sub },
        )
    }
}

impl ZmqCommandClient {
    pub async fn submit_order(&mut self, req: OrderRequest) -> Result<Order> {
        let response = self.send_command(ZmqCommand::SubmitOrder(req)).await?;
        match response {
            ZmqResponse::SubmitAck(order) => Ok(order),
            ZmqResponse::Error(e) => anyhow::bail!("Exchange error: {}", e),
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    pub async fn cancel_order(&mut self, id: String) -> Result<Order> {
        let response = self.send_command(ZmqCommand::CancelOrder(id)).await?;
        match response {
            ZmqResponse::CancelAck(order) => Ok(order),
            ZmqResponse::Error(e) => anyhow::bail!("Exchange error: {}", e),
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    pub async fn get_position(&mut self, key: InstrumentKey) -> Result<Option<Position>> {
        let response = self.send_command(ZmqCommand::GetPosition(key)).await?;
        match response {
            ZmqResponse::Position(pos) => Ok(pos),
            ZmqResponse::Error(e) => anyhow::bail!("Exchange error: {}", e),
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    pub async fn get_all_positions(&mut self) -> Result<Vec<Position>> {
        let response = self.send_command(ZmqCommand::GetAllPositions).await?;
        match response {
            ZmqResponse::AllPositions(positions) => Ok(positions),
            ZmqResponse::Error(e) => anyhow::bail!("Exchange error: {}", e),
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    pub async fn get_account_snapshot(&mut self) -> Result<AccountSnapshot> {
        let response = self.send_command(ZmqCommand::GetAccountSnapshot).await?;
        match response {
            ZmqResponse::AccountSnapshot(snapshot) => Ok(snapshot),
            ZmqResponse::Error(e) => anyhow::bail!("Exchange error: {}", e),
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    pub async fn get_instrument(&mut self, key: InstrumentKey) -> Result<Option<Instrument>> {
        let response = self.send_command(ZmqCommand::GetInstrument(key)).await?;
        match response {
            ZmqResponse::Instrument(instrument) => Ok(instrument),
            ZmqResponse::Error(e) => anyhow::bail!("Exchange error: {}", e),
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    pub async fn get_all_instruments(&mut self) -> Result<Vec<Instrument>> {
        let response = self.send_command(ZmqCommand::GetAllInstruments).await?;
        match response {
            ZmqResponse::AllInstruments(instruments) => Ok(instruments),
            ZmqResponse::Error(e) => anyhow::bail!("Exchange error: {}", e),
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    pub async fn get_reference_price(
        &mut self,
        key: InstrumentKey,
    ) -> Result<rust_decimal::Decimal> {
        let response = self
            .send_command(ZmqCommand::GetReferencePrice(key))
            .await?;
        match response {
            ZmqResponse::ReferencePrice(price) => Ok(price),
            ZmqResponse::Error(e) => anyhow::bail!("Exchange error: {}", e),
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    pub async fn get_funding_rate(
        &mut self,
        key: InstrumentKey,
    ) -> Result<crate::types::FundingRateSnapshot> {
        let response = self.send_command(ZmqCommand::GetFundingRate(key)).await?;
        match response {
            ZmqResponse::FundingRate(snapshot) => Ok(snapshot),
            ZmqResponse::Error(e) => anyhow::bail!("Exchange error: {}", e),
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    pub async fn set_leverage(
        &mut self,
        key: InstrumentKey,
        leverage: rust_decimal::Decimal,
    ) -> Result<rust_decimal::Decimal> {
        let response = self
            .send_command(ZmqCommand::SetLeverage(key, leverage))
            .await?;
        match response {
            ZmqResponse::SetLeverageAck(actual) => Ok(actual),
            ZmqResponse::Error(e) => anyhow::bail!("Exchange error: {}", e),
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    async fn send_command(&mut self, command: ZmqCommand) -> Result<ZmqResponse> {
        let payload = rkyv::to_bytes::<_, 1024>(&command).unwrap();

        let mut msg = ZmqMessage::from(bytes::Bytes::new()); // Empty frame for DEALER
        msg.push_back(payload.into_vec().into());

        self.dealer.send(msg).await?;

        // Wait for response with timeout
        let response_msg = tokio::time::timeout(Duration::from_secs(5), self.dealer.recv())
            .await
            .context("Timeout waiting for ZMQ response")??;

        if response_msg.len() < 2 {
            anyhow::bail!("Received malformed DEALER message");
        }

        let payload_bytes = response_msg.get(1).unwrap();

        let mut aligned_payload = rkyv::AlignedVec::with_capacity(payload_bytes.len());
        aligned_payload.extend_from_slice(payload_bytes);

        let archived = rkyv::check_archived_root::<ZmqResponse>(&aligned_payload)
            .map_err(|e| anyhow::anyhow!("Failed to validate response payload: {}", e))?;

        let response: ZmqResponse = archived.deserialize(&mut Infallible).unwrap();
        Ok(response)
    }
}

impl ZmqEventSubscriber {
    pub fn spawn_event_listener(
        mut self,
        tx: mpsc::Sender<ZmqEvent>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match self.sub.recv().await {
                    Ok(msg) => {
                        // SUB receives: [Topic, Payload]
                        if msg.len() < 2 {
                            continue;
                        }

                        let payload_bytes = msg.get(1).unwrap();

                        let mut aligned_payload =
                            rkyv::AlignedVec::with_capacity(payload_bytes.len());
                        aligned_payload.extend_from_slice(payload_bytes);

                        let archived = match rkyv::check_archived_root::<ZmqEvent>(&aligned_payload)
                        {
                            Ok(a) => a,
                            Err(e) => {
                                error!(error = %e, "Failed to validate event payload");
                                continue;
                            }
                        };

                        let event: ZmqEvent = archived.deserialize(&mut Infallible).unwrap();
                        if tx.send(event).await.is_err() {
                            debug!("Event listener channel closed");
                            break;
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "Failed to receive from SUB socket");
                    }
                }
            }
        })
    }
}
