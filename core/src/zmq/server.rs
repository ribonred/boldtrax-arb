use crate::manager::account::{AccountManager, AccountManagerHandle};
use crate::order::OrderManagerHandle;
use crate::position::PositionManagerHandle;
use crate::registry::InstrumentRegistry;
use crate::types::Exchange;
use crate::zmq::discovery::{DiscoveryClient, ServiceType};
use crate::zmq::protocol::{ZmqCommand, ZmqEvent, ZmqResponse};
use rkyv::{Deserialize, Infallible};
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};
use zeromq::{PubSocket, RouterSocket, Socket, SocketRecv, SocketSend, ZmqMessage};

#[derive(Debug, Clone)]
pub struct ZmqServerConfig {
    pub exchange: Exchange,
    pub redis_url: String,
    pub ttl_secs: u64,
}

pub struct ZmqServer {
    config: ZmqServerConfig,
    order_handle: Option<OrderManagerHandle>,
    position_handle: Option<PositionManagerHandle>,
    account_handle: Option<AccountManagerHandle>,
    registry: InstrumentRegistry,
    event_rx: broadcast::Receiver<ZmqEvent>,
}

impl ZmqServer {
    pub fn new(
        config: ZmqServerConfig,
        order_handle: Option<OrderManagerHandle>,
        position_handle: Option<PositionManagerHandle>,
        account_handle: Option<AccountManagerHandle>,
        registry: InstrumentRegistry,
        event_rx: broadcast::Receiver<ZmqEvent>,
    ) -> Self {
        Self {
            config,
            order_handle,
            position_handle,
            account_handle,
            registry,
            event_rx,
        }
    }

    pub async fn run(self) -> anyhow::Result<()> {
        let Self {
            config,
            order_handle,
            position_handle,
            account_handle,
            registry,
            mut event_rx,
        } = self;

        let discovery = DiscoveryClient::new(&config.redis_url)?;

        // 1. Bind PUB socket for market data and events
        let mut pub_socket = PubSocket::new();
        let pub_endpoint = pub_socket.bind("tcp://0.0.0.0:0").await?;
        let pub_endpoint_str = pub_endpoint.to_string();
        info!(endpoint = %pub_endpoint_str, "Bound PUB socket");

        discovery
            .register_and_heartbeat(
                config.exchange,
                ServiceType::Pub,
                pub_endpoint_str,
                config.ttl_secs,
            )
            .await?;

        // 2. Bind ROUTER socket for commands
        let mut router_socket = RouterSocket::new();
        let router_endpoint = router_socket.bind("tcp://0.0.0.0:0").await?;
        let router_endpoint_str = router_endpoint.to_string();
        info!(endpoint = %router_endpoint_str, "Bound ROUTER socket");

        discovery
            .register_and_heartbeat(
                config.exchange,
                ServiceType::Router,
                router_endpoint_str,
                config.ttl_secs,
            )
            .await?;

        // 3. Spawn PUB broadcast loop
        let pub_socket = Arc::new(tokio::sync::Mutex::new(pub_socket));
        let pub_clone = Arc::clone(&pub_socket);

        tokio::spawn(async move {
            loop {
                match event_rx.recv().await {
                    Ok(zmq_event) => {
                        let topic = zmq_event.topic();
                        let payload = rkyv::to_bytes::<_, 1024>(&zmq_event).unwrap();

                        let mut msg = ZmqMessage::from(topic);
                        msg.push_back(payload.into_vec().into());

                        let mut socket = pub_clone.lock().await;
                        if let Err(e) = socket.send(msg).await {
                            error!(error = %e, "Failed to publish event");
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("ZMQ PUB loop lagged by {} messages", n);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("Event broadcast channel closed, stopping PUB loop");
                        break;
                    }
                }
            }
        });

        // 4. Run ROUTER command loop
        info!("ZMQ Server started, listening for commands");
        loop {
            match router_socket.recv().await {
                Ok(msg) => {
                    // ROUTER messages: [Identity, Empty, Payload]
                    if msg.len() < 3 {
                        warn!("Received malformed ROUTER message (too few frames)");
                        continue;
                    }

                    let identity = msg.get(0).unwrap().clone();
                    let payload_bytes = msg.get(2).unwrap();

                    // Deserialize command
                    let command_result =
                        match rkyv::check_archived_root::<ZmqCommand>(payload_bytes) {
                            Ok(a) => Ok(a.deserialize(&mut Infallible).unwrap()),
                            Err(e) => Err(format!("Failed to validate rkyv payload: {:?}", e)),
                        };

                    let command: ZmqCommand = match command_result {
                        Ok(cmd) => cmd,
                        Err(err_msg) => {
                            error!("{}", err_msg);
                            Self::send_error(
                                &mut router_socket,
                                identity,
                                "Invalid payload format",
                            )
                            .await;
                            continue;
                        }
                    };
                    debug!(?command, "Received ZMQ command");

                    // Process command
                    let response = Self::process_command(
                        config.exchange,
                        &order_handle,
                        &position_handle,
                        &account_handle,
                        &registry,
                        command,
                    )
                    .await;

                    // Serialize and send response
                    let response_bytes = rkyv::to_bytes::<_, 1024>(&response).unwrap();
                    let mut reply_msg = ZmqMessage::from(identity);
                    reply_msg.push_back(bytes::Bytes::new()); // Empty frame
                    reply_msg.push_back(response_bytes.into_vec().into());

                    if let Err(e) = router_socket.send(reply_msg).await {
                        error!(error = %e, "Failed to send ROUTER reply");
                    }
                }
                Err(e) => {
                    error!(error = %e, "Failed to receive from ROUTER socket");
                }
            }
        }
    }

    async fn process_command(
        exchange: Exchange,
        order_handle: &Option<OrderManagerHandle>,
        position_handle: &Option<PositionManagerHandle>,
        account_handle: &Option<AccountManagerHandle>,
        registry: &InstrumentRegistry,
        command: ZmqCommand,
    ) -> ZmqResponse {
        match command {
            ZmqCommand::SubmitOrder(req) => {
                if let Some(handle) = order_handle {
                    match handle.submit_order(req).await {
                        Ok(order) => ZmqResponse::SubmitAck(order),
                        Err(e) => ZmqResponse::Error(e.to_string()),
                    }
                } else {
                    ZmqResponse::Error(
                        "Order management not implemented for this exchange".to_string(),
                    )
                }
            }
            ZmqCommand::CancelOrder(id) => {
                if let Some(handle) = order_handle {
                    match handle.cancel_order(id).await {
                        Ok(order) => ZmqResponse::CancelAck(order),
                        Err(e) => ZmqResponse::Error(e.to_string()),
                    }
                } else {
                    ZmqResponse::Error(
                        "Order management not implemented for this exchange".to_string(),
                    )
                }
            }
            ZmqCommand::GetPosition(key) => {
                if let Some(handle) = position_handle {
                    let pos = handle.get_position(key).await;
                    ZmqResponse::Position(pos)
                } else {
                    ZmqResponse::Error(
                        "Position management not implemented for this exchange".to_string(),
                    )
                }
            }
            ZmqCommand::GetAllPositions => {
                if let Some(handle) = position_handle {
                    let positions = handle.get_all_positions().await;
                    ZmqResponse::AllPositions(positions)
                } else {
                    ZmqResponse::Error(
                        "Position management not implemented for this exchange".to_string(),
                    )
                }
            }
            ZmqCommand::GetAccountSnapshot => {
                if let Some(handle) = account_handle {
                    match handle.get_snapshot(exchange).await {
                        Ok(snap) => ZmqResponse::AccountSnapshot(snap),
                        Err(e) => ZmqResponse::Error(e.to_string()),
                    }
                } else {
                    ZmqResponse::Error(
                        "Account management not implemented for this exchange".to_string(),
                    )
                }
            }
            ZmqCommand::GetInstrument(key) => ZmqResponse::Instrument(registry.get(&key)),
            ZmqCommand::GetAllInstruments => ZmqResponse::AllInstruments(registry.get_all()),
        }
    }

    async fn send_error(socket: &mut RouterSocket, identity: bytes::Bytes, err: &str) {
        let response = ZmqResponse::Error(err.to_string());
        let response_bytes = rkyv::to_bytes::<_, 1024>(&response).unwrap();

        let mut reply_msg = ZmqMessage::from(identity);
        reply_msg.push_back(bytes::Bytes::new());
        reply_msg.push_back(response_bytes.into_vec().into());

        let _ = socket.send(reply_msg).await;
    }
}
