//! WebSocket policies for Bybit V5.
//!
//! Bybit V5 WS differs from Binance/Aster:
//! - Subscribe via JSON message: `{"op":"subscribe","args":["orderbook.1.BTCUSDT"]}`
//! - Heartbeat: send `{"op":"ping"}` every 20 seconds
//! - Messages have `topic`, `type`, `ts`, `data` fields
//! - Level 1 orderbook has snapshot-only messages (no delta)

use crate::bybit::mappers::{order_book_to_snapshot, ws_order_to_order_event, ws_position_to_patch};
use crate::bybit::types::{
    BybitOrderBookResult, BybitWsControl, BybitWsOrderUpdate, BybitWsPositionUpdate,
    BybitWsPrivateMessage,
};
use async_trait::async_trait;
use boldtrax_core::manager::types::WsPositionPatch;
use boldtrax_core::registry::InstrumentRegistry;
use boldtrax_core::types::{Exchange, InstrumentKey, InstrumentType, OrderBookUpdate, OrderEvent};
use boldtrax_core::ws::WsPolicy;
use futures_util::SinkExt;
use futures_util::stream::SplitStream;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

/// Top-level WS message — we first check if it's a data message or a control
/// response (pong, subscription ack).
#[derive(Debug, Deserialize)]
struct BybitWsRawMessage {
    #[serde(default)]
    topic: Option<String>,
    #[serde(default, rename = "type")]
    #[allow(dead_code)]
    msg_type: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    ts: Option<u64>,
    #[serde(default)]
    data: Option<BybitOrderBookResult>,
    // Control fields
    #[serde(default)]
    op: Option<String>,
    #[serde(default)]
    success: Option<bool>,
}

pub struct BybitDepthPolicy {
    /// The WS URL (e.g. `wss://stream.bybit.com/v5/public/linear`)
    pub url: String,
    /// Map of `BTCUSDT` -> InstrumentKey (uppercase symbol to key)
    pub symbol_map: HashMap<String, InstrumentKey>,
    /// Channel to send parsed snapshots
    pub tx: mpsc::Sender<OrderBookUpdate>,
    /// Depth level to subscribe to (1, 50, 200, 1000)
    pub depth: u32,
}

#[async_trait]
impl WsPolicy for BybitDepthPolicy {
    async fn prepare(&mut self) -> anyhow::Result<String> {
        Ok(self.url.clone())
    }

    async fn on_connected(
        &mut self,
        sink: &mut futures_util::stream::SplitSink<boldtrax_core::ws::policy::WsStream, Message>,
        _stream: &mut SplitStream<boldtrax_core::ws::policy::WsStream>,
    ) -> anyhow::Result<()> {
        // Subscribe to orderbook topics for all tracked symbols
        let args: Vec<String> = self
            .symbol_map
            .keys()
            .map(|symbol| format!("orderbook.{}.{}", self.depth, symbol))
            .collect();

        if args.is_empty() {
            return Ok(());
        }

        let subscribe = BybitWsControl {
            op: "subscribe".to_string(),
            args: Some(args.clone()),
        };

        let msg = serde_json::to_string(&subscribe)?;
        debug!(topics = ?args, "Bybit WS subscribing to depth topics");
        sink.send(Message::Text(msg)).await?;
        Ok(())
    }

    fn heartbeat_interval(&self) -> Option<Duration> {
        // Bybit recommend sending ping every 20 seconds
        Some(Duration::from_secs(20))
    }

    async fn send_heartbeat(
        &mut self,
        sink: &mut futures_util::stream::SplitSink<boldtrax_core::ws::policy::WsStream, Message>,
    ) -> anyhow::Result<()> {
        let ping = BybitWsControl {
            op: "ping".to_string(),
            args: None,
        };
        let msg = serde_json::to_string(&ping)?;
        sink.send(Message::Text(msg)).await?;
        Ok(())
    }

    async fn parse_message(&mut self, raw: Message) -> anyhow::Result<()> {
        let text = match raw {
            Message::Text(t) => t,
            Message::Binary(b) => String::from_utf8(b.to_vec())?,
            _ => return Ok(()),
        };

        let msg: BybitWsRawMessage = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                debug!(text = %text, error = %e, "Bybit WS unrecognised message");
                return Ok(());
            }
        };

        // Control messages (pong, subscribe ack)
        if msg.op.is_some() {
            if let Some(false) = msg.success {
                warn!(text = %text, "Bybit WS control message failed");
            }
            return Ok(());
        }

        // Data message — must have topic + data
        let topic = match msg.topic.as_deref() {
            Some(t) => t,
            None => return Ok(()),
        };
        let data = match msg.data {
            Some(d) => d,
            None => return Ok(()),
        };

        // topic format: "orderbook.{depth}.{SYMBOL}"
        let symbol = match topic.split('.').nth(2) {
            Some(s) => s,
            None => return Ok(()),
        };

        if let Some(key) = self.symbol_map.get(symbol) {
            if let Some(snapshot) = order_book_to_snapshot(&data, *key) {
                let update = OrderBookUpdate { snapshot };
                if self.tx.try_send(update).is_err() {
                    warn!("Bybit depth channel full or closed");
                }
            }
        } else {
            debug!(symbol = symbol, "Bybit WS symbol not found in map");
        }

        Ok(())
    }
}

// ──────────────────────────────────────────────────────────────────
// BybitUserDataPolicy — private WS (order + position updates)
// ──────────────────────────────────────────────────────────────────

/// WebSocket policy for the Bybit V5 **private** user-data stream.
///
/// Handles both `order` (order/execution updates) and `position`
/// (real-time position changes) from the same private WS connection.
///
/// ### Lifecycle
/// - **`prepare()`**: returns the private WS URL.
/// - **`on_connected()`**: sends auth + subscribe messages.
/// - **`heartbeat_interval()`**: 20 seconds.
/// - **`send_heartbeat()`**: sends `{"op":"ping"}`.
/// - **`parse_message()`**: handles auth/subscribe acks and dispatches
///   `order` topic → `order_tx`, `position` topic → `pos_tx`.
pub struct BybitUserDataPolicy {
    url: String,
    api_key: String,
    api_secret: String,
    registry: InstrumentRegistry,
    order_tx: mpsc::Sender<OrderEvent>,
    pos_tx: mpsc::Sender<WsPositionPatch>,
}

/// Receivers returned by [`BybitUserDataPolicy::new`].
pub struct BybitUserDataChannels {
    pub order_rx: mpsc::Receiver<OrderEvent>,
    pub pos_rx: mpsc::Receiver<WsPositionPatch>,
}

impl BybitUserDataPolicy {
    /// Create a new policy that owns internal channels.
    ///
    /// Returns `(policy, channels)`.  The policy keeps the tx halves;
    /// callers take the rx halves and relay events into trait-provided senders.
    pub fn new(
        url: String,
        api_key: String,
        api_secret: String,
        registry: InstrumentRegistry,
    ) -> (Self, BybitUserDataChannels) {
        let (order_tx, order_rx) = mpsc::channel(256);
        let (pos_tx, pos_rx) = mpsc::channel(64);
        let policy = Self {
            url,
            api_key,
            api_secret,
            registry,
            order_tx,
            pos_tx,
        };
        let channels = BybitUserDataChannels { order_rx, pos_rx };
        (policy, channels)
    }
}

#[async_trait]
impl WsPolicy for BybitUserDataPolicy {
    async fn prepare(&mut self) -> anyhow::Result<String> {
        Ok(self.url.clone())
    }

    async fn on_connected(
        &mut self,
        sink: &mut futures_util::stream::SplitSink<boldtrax_core::ws::policy::WsStream, Message>,
        _stream: &mut SplitStream<boldtrax_core::ws::policy::WsStream>,
    ) -> anyhow::Result<()> {
        use std::time::{SystemTime, UNIX_EPOCH};

        // ── Auth ─────────────────────────────────────────────────
        let expires = {
            let now_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (now_secs + 10) * 1000
        };
        let sign_payload = format!("GET/realtime{expires}");
        let signature = boldtrax_core::utils::hmac_sha256_hex(&self.api_secret, &sign_payload);

        let auth_msg = serde_json::json!({
            "op": "auth",
            "args": [self.api_key, expires, signature]
        });
        let auth_text = serde_json::to_string(&auth_msg)?;
        debug!("Bybit private WS sending auth");
        sink.send(Message::Text(auth_text)).await?;

        // ── Subscribe ────────────────────────────────────────────
        // Sent right after auth; Bybit processes messages in order so
        // auth will be evaluated before the subscribe.
        let subscribe = BybitWsControl {
            op: "subscribe".to_string(),
            args: Some(vec!["order".to_string(), "position".to_string()]),
        };
        let sub_text = serde_json::to_string(&subscribe)?;
        debug!("Bybit private WS subscribing to order+position");
        sink.send(Message::Text(sub_text)).await?;

        Ok(())
    }

    fn heartbeat_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs(20))
    }

    async fn send_heartbeat(
        &mut self,
        sink: &mut futures_util::stream::SplitSink<boldtrax_core::ws::policy::WsStream, Message>,
    ) -> anyhow::Result<()> {
        let ping = BybitWsControl {
            op: "ping".to_string(),
            args: None,
        };
        let msg = serde_json::to_string(&ping)?;
        sink.send(Message::Text(msg)).await?;
        Ok(())
    }

    async fn parse_message(&mut self, raw: Message) -> anyhow::Result<()> {
        let text = match raw {
            Message::Text(t) => t,
            Message::Binary(b) => String::from_utf8(b.to_vec())?,
            _ => return Ok(()),
        };

        let msg: BybitWsPrivateMessage = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                debug!(text = %text, error = %e, "Bybit private WS unrecognised message");
                return Ok(());
            }
        };

        // Control messages (auth ack, subscribe ack, pong)
        if let Some(ref op) = msg.op {
            match (op.as_str(), msg.success) {
                ("auth", Some(true)) => {
                    info!("Bybit private WS auth success");
                }
                ("auth", _) => {
                    let reason = msg.ret_msg.as_deref().unwrap_or("unknown");
                    warn!(reason = reason, "Bybit private WS auth failed; supervisor will reconnect");
                    return Err(anyhow::anyhow!("Bybit private WS auth failed: {reason}"));
                }
                ("subscribe", Some(false)) => {
                    let reason = msg.ret_msg.as_deref().unwrap_or("unknown");
                    warn!(reason = reason, "Bybit private WS subscribe failed");
                }
                ("subscribe", _) => {
                    debug!("Bybit private WS subscribe ack");
                }
                _ => {
                    // pong or other control
                    if msg.success == Some(false) {
                        warn!(text = %text, "Bybit private WS control message failed");
                    }
                }
            }
            return Ok(());
        }

        let topic = match msg.topic.as_deref() {
            Some(t) => t,
            None => return Ok(()),
        };
        let data = match msg.data {
            Some(d) => d,
            None => return Ok(()),
        };

        match topic {
            "order" => {
                let updates: Vec<BybitWsOrderUpdate> = match serde_json::from_value(data) {
                    Ok(u) => u,
                    Err(e) => {
                        warn!(error = %e, "Failed to parse Bybit WS order update");
                        return Ok(());
                    }
                };

                for update in &updates {
                    let instrument = match self.registry.get_by_exchange_symbol(
                        Exchange::Bybit,
                        &update.symbol,
                        InstrumentType::Swap,
                    ) {
                        Some(i) => i,
                        None => {
                            debug!(symbol = %update.symbol, "Bybit WS order for untracked symbol");
                            continue;
                        }
                    };

                    if let Some(event) = ws_order_to_order_event(update, instrument.key) {
                        info!(
                            symbol = %update.symbol,
                            order_id = %update.order_id,
                            status = %update.order_status,
                            side = %update.side,
                            "Bybit WS order update"
                        );
                        match self.order_tx.try_send(event) {
                            Ok(_) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                warn!(symbol = %update.symbol, "Order event channel full; dropping");
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                debug!(symbol = %update.symbol, "Order event channel closed");
                            }
                        }
                    }
                }
            }
            "position" => {
                let updates: Vec<BybitWsPositionUpdate> = match serde_json::from_value(data) {
                    Ok(u) => u,
                    Err(e) => {
                        warn!(error = %e, "Failed to parse Bybit WS position update");
                        return Ok(());
                    }
                };

                for update in &updates {
                    let instrument = match self.registry.get_by_exchange_symbol(
                        Exchange::Bybit,
                        &update.symbol,
                        InstrumentType::Swap,
                    ) {
                        Some(i) => i,
                        None => {
                            debug!(symbol = %update.symbol, "Bybit WS position for untracked symbol");
                            continue;
                        }
                    };

                    if let Some(patch) = ws_position_to_patch(update, instrument.key) {
                        debug!(
                            symbol = %update.symbol,
                            size = %patch.size,
                            entry_price = %patch.entry_price,
                            "Bybit WS position update"
                        );
                        match self.pos_tx.try_send(patch) {
                            Ok(_) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                warn!(symbol = %update.symbol, "Position channel full; dropping");
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                debug!(symbol = %update.symbol, "Position channel closed");
                            }
                        }
                    }
                }
            }
            _ => {
                debug!(topic = topic, "Bybit private WS unknown topic");
            }
        }

        Ok(())
    }
}
