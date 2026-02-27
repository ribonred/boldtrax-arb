//! ZMQ DEALER client for sending commands to strategy ROUTER sockets.
//!
//! Used by the `stratctl` CLI to send [`StrategyCommand`] messages and
//! receive [`StrategyResponse`] replies.

use crate::zmq::discovery::DiscoveryClient;
use crate::zmq::protocol::{StrategyCommand, StrategyResponse};
use anyhow::Context;
use rkyv::{Deserialize, Infallible};
use std::time::Duration;
use zeromq::{DealerSocket, Socket, SocketRecv, SocketSend, ZmqMessage};

/// A one-shot client that connects to a strategy's ROUTER socket,
/// sends a command, and waits for the response.
pub struct StrategyCommandClient {
    socket: DealerSocket,
}

impl StrategyCommandClient {
    /// Connect to a strategy by its known endpoint (e.g. `tcp://127.0.0.1:5678`).
    pub async fn connect(endpoint: &str) -> anyhow::Result<Self> {
        let mut socket = DealerSocket::new();
        socket
            .connect(endpoint)
            .await
            .with_context(|| format!("Failed to connect DEALER to {}", endpoint))?;

        // Small settle time for ZMQ socket connection
        tokio::time::sleep(Duration::from_millis(100)).await;

        Ok(Self { socket })
    }

    /// Discover a strategy's endpoint via Redis and connect.
    pub async fn discover_and_connect(redis_url: &str, strategy_id: &str) -> anyhow::Result<Self> {
        let discovery = DiscoveryClient::new(redis_url)
            .with_context(|| "Failed to create Redis discovery client")?;

        let endpoint = discovery
            .discover_strategy(strategy_id)
            .await
            .with_context(|| format!("Redis lookup failed for strategy '{}'", strategy_id))?
            .with_context(|| {
                format!(
                    "Strategy '{}' not found in Redis — is it running?",
                    strategy_id
                )
            })?;

        Self::connect(&endpoint).await
    }

    /// Send a command and wait for the response with a timeout.
    pub async fn send_command(
        &mut self,
        cmd: StrategyCommand,
        timeout: Duration,
    ) -> anyhow::Result<StrategyResponse> {
        let payload = rkyv::to_bytes::<_, 256>(&cmd)
            .map_err(|e| anyhow::anyhow!("Failed to serialize command: {}", e))?;

        // DEALER sends: [empty frame][payload]
        let mut msg = ZmqMessage::from(bytes::Bytes::new());
        msg.push_back(payload.into_vec().into());
        self.socket
            .send(msg)
            .await
            .context("Failed to send command")?;

        // Wait for reply with timeout
        let reply = tokio::time::timeout(timeout, self.socket.recv())
            .await
            .context("Timed out waiting for strategy response")?
            .context("Failed to receive response")?;

        // DEALER receives: [empty frame][payload]
        let payload_frame = if reply.len() >= 2 {
            reply.get(1).unwrap()
        } else if reply.len() == 1 {
            reply.get(0).unwrap()
        } else {
            anyhow::bail!("Empty response from strategy");
        };

        let mut aligned = rkyv::AlignedVec::with_capacity(payload_frame.len());
        aligned.extend_from_slice(payload_frame);

        let archived = rkyv::check_archived_root::<StrategyResponse>(&aligned)
            .map_err(|e| anyhow::anyhow!("Invalid response: {:?}", e))?;
        let response: StrategyResponse = archived.deserialize(&mut Infallible).unwrap();

        Ok(response)
    }
}
