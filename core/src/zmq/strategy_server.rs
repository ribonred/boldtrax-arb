use crate::zmq::discovery::DiscoveryClient;
use crate::zmq::protocol::{StrategyCommand, StrategyResponse};
use rkyv::{Deserialize, Infallible};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use zeromq::{RouterSocket, Socket, SocketRecv, SocketSend, ZmqMessage};

/// Configuration for the strategy command server.
#[derive(Debug, Clone)]
pub struct StrategyServerConfig {
    /// Unique identity — derived from strategy config (e.g. "spot_perp_btc_binance").
    pub strategy_id: String,
    /// Redis URL for service discovery.
    pub redis_url: String,
    /// TTL for the discovery heartbeat (seconds).
    pub ttl_secs: u64,
}

/// A ROUTER socket that deserializes [`StrategyCommand`] messages
/// and forwards them to an `mpsc::Sender`. Replies are generated
/// by sending a [`StrategyResponse`] back through a oneshot channel.
pub struct StrategyCommandServer {
    config: StrategyServerConfig,
    cmd_tx: mpsc::Sender<StrategyCommand>,
    /// Receive pre-built responses from the engine for `GetStatus`.
    /// For simple ack/reject we reply inline.
    status_provider: Option<tokio::sync::watch::Receiver<StrategyResponse>>,
}

impl StrategyCommandServer {
    /// Create a new server. The returned `mpsc::Receiver` should be
    /// wired into the engine/poller's `select!` loop.
    pub fn new(config: StrategyServerConfig) -> (Self, mpsc::Receiver<StrategyCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let server = Self {
            config,
            cmd_tx,
            status_provider: None,
        };
        (server, cmd_rx)
    }

    /// Attach a watch channel that the engine updates with the latest
    /// `StrategyResponse::Status(...)`. When a `GetStatus` command arrives
    /// the server reads the current value and replies immediately.
    pub fn with_status_watch(mut self, rx: tokio::sync::watch::Receiver<StrategyResponse>) -> Self {
        self.status_provider = Some(rx);
        self
    }

    /// Bind the ROUTER socket, register in Redis, and start the command loop.
    /// This is a long-running task — spawn it with `tokio::spawn`.
    pub async fn run(self) -> anyhow::Result<()> {
        let discovery = DiscoveryClient::new(&self.config.redis_url)?;

        let mut router_socket = RouterSocket::new();
        let endpoint = router_socket.bind("tcp://0.0.0.0:0").await?;
        let endpoint_str = endpoint.to_string();
        info!(
            strategy_id = %self.config.strategy_id,
            endpoint = %endpoint_str,
            "Strategy command server bound"
        );

        discovery
            .register_strategy(&self.config.strategy_id, endpoint_str, self.config.ttl_secs)
            .await?;

        self.command_loop(router_socket).await;
        Ok(())
    }

    async fn command_loop(self, mut router_socket: RouterSocket) {
        loop {
            match router_socket.recv().await {
                Ok(msg) => {
                    if msg.len() < 3 {
                        warn!("Malformed ROUTER message (too few frames)");
                        continue;
                    }

                    let identity = msg.get(0).unwrap().clone();
                    let payload_bytes = msg.get(2).unwrap();

                    // Deserialize in a sync block so bytecheck's non-Send error
                    // doesn't live across await boundaries.
                    let parse_result: Result<StrategyCommand, String> = {
                        let mut aligned = rkyv::AlignedVec::with_capacity(payload_bytes.len());
                        aligned.extend_from_slice(payload_bytes);

                        match rkyv::check_archived_root::<StrategyCommand>(&aligned) {
                            Ok(archived) => Ok(archived.deserialize(&mut Infallible).unwrap()),
                            Err(e) => Err(format!("{e:?}")),
                        }
                    };

                    let command = match parse_result {
                        Ok(cmd) => cmd,
                        Err(err_msg) => {
                            error!(error = %err_msg, "Invalid StrategyCommand payload");
                            Self::send_response(
                                &mut router_socket,
                                identity,
                                &StrategyResponse::Error("Invalid payload format".to_string()),
                            )
                            .await;
                            continue;
                        }
                    };

                    debug!(command = ?command, strategy_id = %self.config.strategy_id, "Received strategy command");

                    let response = self.handle(&command).await;
                    Self::send_response(&mut router_socket, identity, &response).await;
                }
                Err(e) => {
                    error!(error = %e, "Failed to receive from strategy ROUTER socket");
                }
            }
        }
    }

    /// Decide the response and optionally forward the command to the engine.
    async fn handle(&self, cmd: &StrategyCommand) -> StrategyResponse {
        match cmd {
            StrategyCommand::GetStatus => {
                if let Some(rx) = &self.status_provider {
                    rx.borrow().clone()
                } else {
                    StrategyResponse::Error("Status provider not configured".to_string())
                }
            }
            // All other commands are forwarded to the engine.
            _ => {
                if self.cmd_tx.try_send(cmd.clone()).is_err() {
                    StrategyResponse::Rejected("Command channel full or closed".to_string())
                } else {
                    StrategyResponse::Ack
                }
            }
        }
    }

    async fn send_response(
        socket: &mut RouterSocket,
        identity: bytes::Bytes,
        response: &StrategyResponse,
    ) {
        let response_bytes = rkyv::to_bytes::<_, 1024>(response).unwrap();
        let mut reply = ZmqMessage::from(identity);
        reply.push_back(bytes::Bytes::new()); // Empty frame
        reply.push_back(response_bytes.into_vec().into());

        if let Err(e) = socket.send(reply).await {
            error!(error = %e, "Failed to send strategy command response");
        }
    }
}
