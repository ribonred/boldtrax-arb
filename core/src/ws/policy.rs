use async_trait::async_trait;
use futures_util::stream::{SplitSink, SplitStream};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

/// Convenience alias for a Binance/generic TLS-capable WebSocket stream.
pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Lifecycle policy for a managed WebSocket connection.
///
/// Implement this trait to describe how a single WebSocket endpoint should
/// be prepared, authenticated, kept alive, and how its messages should be
/// parsed.  The [`ws_supervisor`](super::supervisor::ws_supervisor_spawn)
/// drives the reconnect loop and calls these hooks at the right moment.
///
/// # Phases
///
/// | Phase | Hook | Called when |
/// |-------|------|-------------|
/// | 1 | [`prepare`](Self::prepare) | Before **every** connect attempt (mutable — may fetch a fresh listen-key) |
/// | 2 | [`on_connected`](Self::on_connected) | Once the TCP+TLS handshake completes |
/// | 3 | [`send_heartbeat`](Self::send_heartbeat) | On each tick of [`heartbeat_interval`](Self::heartbeat_interval) |
/// | 4 | [`parse_message`](Self::parse_message) | For every incoming text/binary frame |
#[async_trait]
pub trait WsPolicy: Send + 'static {
    /// Return the WebSocket URL to connect to.
    ///
    /// Called before **every** connection attempt so implementations that embed
    /// a short-lived token in the URL (e.g. Binance Futures listen-key) always
    /// produce a fresh URL on reconnect.
    async fn prepare(&mut self) -> anyhow::Result<String>;

    /// Post-connect hook.
    ///
    /// Called after the TCP/TLS upgrade succeeds, before entering the message
    /// loop.  Implementations can send authentication frames (e.g. Binance Spot
    /// `userDataStream.subscribe.signature`) and read back the acknowledgement.
    ///
    /// Both `sink` (write) and `stream` (read) are provided so you can do a
    /// request/response handshake before the main loop takes over.
    ///
    /// The default implementation is a no-op.
    async fn on_connected(
        &mut self,
        _sink: &mut SplitSink<WsStream, Message>,
        _stream: &mut SplitStream<WsStream>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// How often to call [`send_heartbeat`](Self::send_heartbeat).
    ///
    /// Return `None` (the default) when the exchange uses native WebSocket
    /// ping/pong, which `tokio-tungstenite` handles automatically.  Return
    /// `Some(interval)` when the exchange requires application-level heartbeats
    /// (e.g. Binance Futures listen-key `PUT` every 30 minutes).
    fn heartbeat_interval(&self) -> Option<Duration> {
        None
    }

    /// Send an application-level heartbeat / keepalive.
    ///
    /// Called on each tick of [`heartbeat_interval`](Self::heartbeat_interval).
    /// Return `Err` to signal that the connection is unhealthy and should be
    /// torn down and reconnected (the supervisor will call `prepare()` again to
    /// obtain a fresh URL/token).
    ///
    /// The default implementation is a no-op.
    async fn send_heartbeat(
        &mut self,
        _sink: &mut SplitSink<WsStream, Message>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Parse and dispatch a single incoming WebSocket frame.
    ///
    /// Only `Text` and `Binary` messages are passed here; `Close`, `Ping`, and
    /// `Pong` frames are handled by the supervisor.  Return `Err` to log a
    /// parse/dispatch warning; the supervisor will continue rather than
    /// reconnect.
    async fn parse_message(&mut self, raw: Message) -> anyhow::Result<()>;
}
