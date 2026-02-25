//! Binance WebSocket policy implementations.
//!
//! Three policies cover the different stream types:
//!
//! | Policy | Stream | Auth |
//! |---|---|---|
//! | [`BinanceDepthPolicy`] | Combined-stream order-book depth | None (public) |
//! | [`BinanceFuturesUserDataPolicy`] | Futures user-data listen-key stream | REST listen-key + 30min PUT heartbeat |
//! | [`BinanceSpotUserDataPolicy`] | Spot WS API user-data | HMAC `userDataStream.subscribe.signature` |

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use boldtrax_core::http::TracedHttpClient;
use boldtrax_core::manager::types::WsPositionPatch;
use boldtrax_core::registry::InstrumentRegistry;
use boldtrax_core::types::{Exchange, InstrumentKey, InstrumentType, OrderBookUpdate, OrderEvent};
use boldtrax_core::utils::hmac_sha256_hex;
use boldtrax_core::ws::WsPolicy;
use boldtrax_core::ws::policy::WsStream;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, trace, warn};
use uuid::Uuid;

use crate::binance::mappers::{
    futures_execution_to_order_event, spot_execution_to_order_event, ws_depth_event_to_order_book,
    ws_position_to_patch,
};
use crate::binance::types::{
    BinanceCombinedStreamMsg, BinanceFuturesUserDataEvent, BinanceSpotUserDataEvent,
    BinanceWsApiEnvelope, BinanceWsApiResponse, BinanceWsDepthEvent,
};

// ─── BinanceDepthPolicy ───────────────────────────────────────────────────────

/// WebSocket policy for Binance combined-stream depth updates.
///
/// Connects to `{base_ws_url}/stream?streams=sym1@depth5@100ms/sym2@...`
/// and forwards [`OrderBookUpdate`] messages to the provided channel.
pub struct BinanceDepthPolicy {
    /// Base WebSocket URL: e.g. `wss://fstream.binance.com` or
    /// `wss://stream.binance.com:9443`.
    base_url: String,
    /// Maps lowercase exchange symbol (e.g. `"btcusdt"`) to its [`InstrumentKey`].
    symbol_to_key: HashMap<String, InstrumentKey>,
    tx: mpsc::Sender<OrderBookUpdate>,
}

impl BinanceDepthPolicy {
    pub fn new(
        base_url: String,
        symbol_to_key: HashMap<String, InstrumentKey>,
        tx: mpsc::Sender<OrderBookUpdate>,
    ) -> Self {
        Self {
            base_url,
            symbol_to_key,
            tx,
        }
    }
}

#[async_trait]
impl WsPolicy for BinanceDepthPolicy {
    async fn prepare(&mut self) -> anyhow::Result<String> {
        let streams: Vec<String> = self
            .symbol_to_key
            .keys()
            .map(|s| format!("{s}@depth5@100ms"))
            .collect();
        let stream_list = streams.join("/");
        let url = format!("{}/stream?streams={}", self.base_url, stream_list);
        debug!(url = %url, streams = ?streams, "BinanceDepthPolicy prepared");
        Ok(url)
    }

    async fn parse_message(&mut self, raw: Message) -> anyhow::Result<()> {
        let text = match raw {
            Message::Text(t) => t,
            Message::Binary(b) => String::from_utf8(b.to_vec())?,
            _ => return Ok(()),
        };

        let combined: BinanceCombinedStreamMsg = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "Failed to parse combined stream message; skipping");
                return Ok(());
            }
        };

        let symbol_lower = combined.stream.split('@').next().unwrap_or("").to_string();

        let key = match self.symbol_to_key.get(&symbol_lower) {
            Some(&k) => k,
            None => {
                warn!(symbol = %symbol_lower, "WS depth event for untracked symbol; skipping");
                return Ok(());
            }
        };

        let event: BinanceWsDepthEvent = match serde_json::from_value(combined.data) {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, symbol = %symbol_lower, "Failed to parse depth event");
                return Ok(());
            }
        };

        trace!(
            symbol = %symbol_lower,
            bids = event.bids.len(),
            asks = event.asks.len(),
            event_time_ms = event.event_time_ms,
            "Depth WS event received"
        );

        if let Some(snapshot) = ws_depth_event_to_order_book(&event, key) {
            match self.tx.try_send(OrderBookUpdate { snapshot }) {
                Ok(_) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(
                        symbol = %symbol_lower,
                        "Depth channel full; dropping order book update"
                    );
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return Err(anyhow::anyhow!("depth channel closed"));
                }
            }
        }

        Ok(())
    }
}

// ─── BinanceFuturesUserDataPolicy ─────────────────────────────────────────────

/// WebSocket policy for the Binance **Futures** user-data stream.
///
/// Handles both `ORDER_TRADE_UPDATE` (order executions) and `ACCOUNT_UPDATE`
/// (real-time position changes) from the same listen-key stream.
///
/// ### Lifecycle
/// - **`prepare()`**: POSTs `/fapi/v1/listenKey` to obtain a fresh key and
///   embeds it in `wss://fstream.binance.com/ws/<key>`.
/// - **`heartbeat_interval()`**: 30 minutes.
/// - **`send_heartbeat()`**: PUTs `/fapi/v1/listenKey?listenKey=…` to keep the
///   key alive.  A `410` or `401` response causes the supervisor to reconnect
///   (which calls `prepare()` again for a new key).
pub struct BinanceFuturesUserDataPolicy {
    client: TracedHttpClient,
    base_ws_url: String,
    listen_key: String,
    registry: InstrumentRegistry,
    /// Order execution sink — always present; events are dropped if no consumer.
    order_tx: mpsc::Sender<OrderEvent>,
    /// Position update sink — always present; events are dropped if no consumer.
    pos_tx: mpsc::Sender<WsPositionPatch>,
}

/// Receivers returned by [`BinanceFuturesUserDataPolicy::new`].
///
/// Each receiver corresponds to one event type dispatched by the policy.
/// Consumers (trait methods) take their receiver and relay into the
/// caller-provided `mpsc::Sender`.
pub struct FuturesUserDataChannels {
    pub order_rx: mpsc::Receiver<OrderEvent>,
    pub pos_rx: mpsc::Receiver<WsPositionPatch>,
}

impl BinanceFuturesUserDataPolicy {
    /// Create a new policy that owns its internal channels.
    ///
    /// Returns `(policy, channels)`.  The policy keeps the tx halves and
    /// dispatches both `ORDER_TRADE_UPDATE` and `ACCOUNT_UPDATE` via
    /// `try_send`.  Callers take the rx halves from `channels` and relay
    /// events into the trait-provided senders.
    pub fn new(
        client: TracedHttpClient,
        base_ws_url: String,
        registry: InstrumentRegistry,
    ) -> (Self, FuturesUserDataChannels) {
        let (order_tx, order_rx) = mpsc::channel(256);
        let (pos_tx, pos_rx) = mpsc::channel(64);
        let policy = Self {
            client,
            base_ws_url,
            listen_key: String::new(),
            registry,
            order_tx,
            pos_tx,
        };
        let channels = FuturesUserDataChannels { order_rx, pos_rx };
        (policy, channels)
    }
}

#[async_trait]
impl WsPolicy for BinanceFuturesUserDataPolicy {
    async fn prepare(&mut self) -> anyhow::Result<String> {
        let response = self.client.post_empty("/fapi/v1/listenKey").await?;
        let data: crate::binance::types::BinanceListenKeyResponse = response
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to parse listenKey response: {e}"))?;
        self.listen_key = data.listen_key;
        let url = format!("{}/ws/{}", self.base_ws_url, self.listen_key);
        info!(listen_key = %self.listen_key, "Futures listen key obtained");
        Ok(url)
    }

    fn heartbeat_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs(30 * 60))
    }

    async fn send_heartbeat(
        &mut self,
        _sink: &mut futures_util::stream::SplitSink<WsStream, Message>,
    ) -> anyhow::Result<()> {
        let path = format!("/fapi/v1/listenKey?listenKey={}", self.listen_key);
        let result = self.client.put_empty(&path).await;

        match result {
            Ok(_) => {
                debug!(listen_key = %self.listen_key, "Futures listen key kept alive");
                Ok(())
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("410") || msg.contains("401") {
                    Err(anyhow::anyhow!(
                        "Futures listen key expired ({}); forcing reconnect to obtain a new key",
                        msg
                    ))
                } else {
                    warn!(error = %msg, "Futures listen key keep-alive failed (non-fatal)");
                    Ok(())
                }
            }
        }
    }

    async fn parse_message(&mut self, raw: Message) -> anyhow::Result<()> {
        let text = match raw {
            Message::Text(t) => t,
            Message::Binary(b) => String::from_utf8(b.to_vec())?,
            _ => return Ok(()),
        };

        match serde_json::from_str::<BinanceFuturesUserDataEvent>(&text) {
            Ok(BinanceFuturesUserDataEvent::OrderTradeUpdate { order: report }) => {
                info!(
                    symbol = %report.symbol,
                    client_order_id = %report.client_order_id,
                    side = %report.side,
                    order_type = %report.order_type,
                    order_status = %report.order_status,
                    filled_qty = %report.order_filled_accumulated_quantity,
                    avg_price = %report.average_price,
                    "Futures ORDER_TRADE_UPDATE received"
                );
                if let Some(event) = self
                    .registry
                    .get_by_exchange_symbol(Exchange::Binance, &report.symbol, InstrumentType::Swap)
                    .and_then(|inst| futures_execution_to_order_event(&report, inst.key))
                {
                    match self.order_tx.try_send(event) {
                        Ok(_) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            warn!(symbol = %report.symbol, "Order event channel full; dropping");
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            debug!(symbol = %report.symbol, "Order event channel closed; no consumer");
                        }
                    }
                } else {
                    warn!(symbol = %report.symbol, "Futures execution for untracked symbol or parse failed");
                }
            }
            Ok(BinanceFuturesUserDataEvent::AccountUpdate { update }) => {
                for ws_pos in &update.positions {
                    if let Some(instrument) = self.registry.get_by_exchange_symbol(
                        Exchange::Binance,
                        &ws_pos.symbol,
                        InstrumentType::Swap,
                    ) {
                        match ws_position_to_patch(ws_pos, instrument.key) {
                            Some(patch) => {
                                debug!(
                                    symbol = %ws_pos.symbol,
                                    size = %patch.size,
                                    entry_price = %patch.entry_price,
                                    unrealized_pnl = %patch.unrealized_pnl,
                                    "ACCOUNT_UPDATE position patch"
                                );
                                match self.pos_tx.try_send(patch) {
                                    Ok(_) => {}
                                    Err(mpsc::error::TrySendError::Full(_)) => {
                                        warn!(symbol = %ws_pos.symbol, "Position channel full; dropping patch");
                                    }
                                    Err(mpsc::error::TrySendError::Closed(_)) => {
                                        debug!(symbol = %ws_pos.symbol, "Position channel closed; no consumer");
                                    }
                                }
                            }
                            None => {
                                warn!(symbol = %ws_pos.symbol, "Failed to parse ACCOUNT_UPDATE position fields");
                            }
                        }
                    }
                }
            }
            Ok(BinanceFuturesUserDataEvent::Other) => {}
            Err(e) => {
                warn!(error = %e, text = %text, "Failed to parse futures user-data event");
            }
        }

        Ok(())
    }
}

// ─── BinanceSpotUserDataPolicy ────────────────────────────────────────────────

/// WebSocket policy for the Binance **Spot** user-data stream using the new
/// WebSocket API endpoint (`wss://ws-api.binance.com:443/ws-api/v3`).
///
/// ### Lifecycle
/// - **`prepare()`**: no-op — returns the static WS API URL.
/// - **`on_connected()`**: sends `userDataStream.subscribe.signature` with an
///   HMAC-SHA256 signature, then reads and verifies the ack frame.
/// - **`heartbeat_interval()`**: `None` — no token to renew.
/// - **`parse_message()`**: unwraps the `{"subscriptionId": N, "event": {...}}`
///   envelope before routing the inner event.
pub struct BinanceSpotUserDataPolicy {
    ws_api_url: String,
    api_key: String,
    api_secret: String,
    subscription_id: Option<u64>,
    registry: InstrumentRegistry,
    tx: mpsc::Sender<OrderEvent>,
}

impl BinanceSpotUserDataPolicy {
    pub fn new(
        ws_api_url: String,
        api_key: String,
        api_secret: String,
        registry: InstrumentRegistry,
        tx: mpsc::Sender<OrderEvent>,
    ) -> Self {
        Self {
            ws_api_url,
            api_key,
            api_secret,
            subscription_id: None,
            registry,
            tx,
        }
    }
}

#[async_trait]
impl WsPolicy for BinanceSpotUserDataPolicy {
    async fn prepare(&mut self) -> anyhow::Result<String> {
        Ok(self.ws_api_url.clone())
    }

    async fn on_connected(
        &mut self,
        sink: &mut futures_util::stream::SplitSink<WsStream, Message>,
        stream: &mut futures_util::stream::SplitStream<WsStream>,
    ) -> anyhow::Result<()> {
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .map_err(|_| anyhow::anyhow!("system clock is before UNIX epoch"))?;

        // Binance WS API signs: "apiKey=<key>&timestamp=<ts>"
        let payload = format!("apiKey={}&timestamp={}", self.api_key, ts_ms);
        let signature = hmac_sha256_hex(&self.api_secret, &payload);
        let request_id = Uuid::new_v4().to_string();

        let subscribe_msg = serde_json::json!({
            "id": request_id,
            "method": "userDataStream.subscribe.signature",
            "params": {
                "apiKey": self.api_key,
                "signature": signature,
                "timestamp": ts_ms
            }
        });

        sink.send(Message::Text(subscribe_msg.to_string()))
            .await
            .map_err(|e| anyhow::anyhow!("failed to send subscription frame: {e}"))?;

        // Read the ack — Binance responds with {"id":"...","status":200,"result":{"subscriptionId":0}}
        let ack_raw = stream
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("WS closed before subscription ack"))??;

        let ack_text = match ack_raw {
            Message::Text(t) => t,
            other => {
                return Err(anyhow::anyhow!("unexpected ack frame type: {:?}", other));
            }
        };

        let ack: BinanceWsApiResponse = serde_json::from_str(&ack_text).map_err(|e| {
            anyhow::anyhow!("failed to parse subscription ack: {e}\nraw: {ack_text}")
        })?;

        if ack.status != 200 {
            return Err(anyhow::anyhow!(
                "subscription rejected — status {} — raw: {}",
                ack.status,
                ack_text
            ));
        }

        self.subscription_id = ack.result.map(|r| r.subscription_id);
        info!(
            subscription_id = ?self.subscription_id,
            "Binance Spot WS API user-data subscription confirmed"
        );

        Ok(())
    }

    async fn parse_message(&mut self, raw: Message) -> anyhow::Result<()> {
        let text = match raw {
            Message::Text(t) => t,
            Message::Binary(b) => String::from_utf8(b.to_vec())?,
            _ => return Ok(()),
        };

        // Unwrap the WS API envelope: {"subscriptionId": N, "event": {...}}
        let envelope: BinanceWsApiEnvelope = match serde_json::from_str(&text) {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, text = %text, "Failed to parse Spot WS API envelope");
                return Ok(());
            }
        };

        // Parse the inner event using the same type as the legacy stream
        match serde_json::from_value::<BinanceSpotUserDataEvent>(envelope.event) {
            Ok(BinanceSpotUserDataEvent::ExecutionReport(report)) => {
                info!(
                    symbol = %report.symbol,
                    client_order_id = %report.client_order_id,
                    side = %report.side,
                    order_type = %report.order_type,
                    order_status = %report.order_status,
                    filled_qty = %report.cumulative_filled_quantity,
                    last_price = %report.last_executed_price,
                    subscription_id = ?self.subscription_id,
                    "Spot executionReport received"
                );
                if let Some(event) = self
                    .registry
                    .get_by_exchange_symbol(Exchange::Binance, &report.symbol, InstrumentType::Spot)
                    .and_then(|inst| spot_execution_to_order_event(&report, inst.key))
                {
                    if self.tx.send(event).await.is_err() {
                        return Err(anyhow::anyhow!("spot execution channel closed"));
                    }
                } else {
                    warn!(symbol = %report.symbol, "Spot execution for untracked symbol or parse failed");
                }
            }
            Ok(BinanceSpotUserDataEvent::Other) => {
                // outboundAccountPosition etc — not handled yet
            }
            Err(e) => {
                warn!(error = %e, "Failed to parse Spot WS API inner event");
            }
        }

        Ok(())
    }
}
