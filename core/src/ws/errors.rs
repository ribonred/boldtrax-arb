use thiserror::Error;

#[derive(Error, Debug)]
pub enum WsError {
    #[error("WebSocket prepare phase failed: {0}")]
    PrepareFailed(String),

    #[error("WebSocket connect failed: {0}")]
    ConnectFailed(String),

    #[error("WebSocket handshake/on_connected failed: {0}")]
    HandshakeFailed(String),

    #[error("WebSocket heartbeat failed: {0}")]
    HeartbeatFailed(String),

    #[error("WebSocket message receiver dropped")]
    ReceiverDropped,
}
