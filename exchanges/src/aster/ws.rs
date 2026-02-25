use crate::aster::mappers::{
    aster_execution_to_order_event, partial_depth_to_order_book, ws_position_to_patch,
};
use crate::aster::types::{AsterFuturesUserDataEvent, AsterListenKeyResponse, AsterPartialDepth};
use async_trait::async_trait;
use boldtrax_core::http::TracedHttpClient;
use boldtrax_core::manager::types::WsPositionPatch;
use boldtrax_core::registry::InstrumentRegistry;
use boldtrax_core::types::{Exchange, InstrumentKey, InstrumentType, OrderBookUpdate, OrderEvent};
use boldtrax_core::ws::WsPolicy;
use chrono::Utc;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

#[derive(Debug, Deserialize)]
struct AsterWsStreamMessage {
    stream: String,
    data: AsterPartialDepth,
}

pub struct AsterDepthPolicy {
    pub url: String,
    pub symbol_map: HashMap<String, InstrumentKey>,
    pub tx: mpsc::Sender<OrderBookUpdate>,
}

#[async_trait]
impl WsPolicy for AsterDepthPolicy {
    async fn prepare(&mut self) -> anyhow::Result<String> {
        Ok(self.url.clone())
    }

    async fn parse_message(&mut self, raw: Message) -> anyhow::Result<()> {
        if let Message::Text(text) = raw {
            if let Ok(stream_msg) = serde_json::from_str::<AsterWsStreamMessage>(&text) {
                let parts: Vec<&str> = stream_msg.stream.split('@').collect();
                if parts.is_empty() {
                    return Ok(());
                }
                let symbol_lower = parts[0];

                if let Some(key) = self.symbol_map.get(symbol_lower) {
                    let timestamp_ms = Utc::now().timestamp_millis();
                    if let Some(snapshot) =
                        partial_depth_to_order_book(&stream_msg.data, *key, timestamp_ms)
                    {
                        let update = OrderBookUpdate { snapshot };
                        if self.tx.try_send(update).is_err() {
                            warn!("Aster depth channel full or closed");
                        }
                    }
                } else {
                    debug!("Aster WS symbol not found in map: {}", symbol_lower);
                }
            } else {
                debug!("Aster WS unhandled message: {}", text);
            }
        }
        Ok(())
    }
}
// ─── AsterUserDataPolicy ──────────────────────────────────────────────────────

/// WebSocket policy for the Aster Futures user-data stream.
///
/// ### Lifecycle
/// - **`prepare()`**: POSTs `/fapi/v1/listenKey` to obtain a fresh key.
/// - **`heartbeat_interval()`**: 30 minutes.
/// - **`send_heartbeat()`**: PUTs `/fapi/v1/listenKey?listenKey=…` to keep the
///   key alive.
/// - **`parse_message()`**: Routes `ORDER_TRADE_UPDATE` events to the order sink
///   and `ACCOUNT_UPDATE` position entries to the position sink.
pub struct AsterUserDataPolicy {
    client: TracedHttpClient,
    base_ws_url: String,
    listen_key: String,
    registry: InstrumentRegistry,
    order_tx: mpsc::Sender<OrderEvent>,
    pos_tx: mpsc::Sender<WsPositionPatch>,
}

pub struct UserDataChannels {
    pub order_rx: mpsc::Receiver<OrderEvent>,
    pub pos_rx: mpsc::Receiver<WsPositionPatch>,
}

impl AsterUserDataPolicy {
    pub fn new(
        client: TracedHttpClient,
        base_ws_url: String,
        registry: InstrumentRegistry,
    ) -> (Self, UserDataChannels) {
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
        let channels = UserDataChannels { order_rx, pos_rx };
        (policy, channels)
    }
}

#[async_trait]
impl WsPolicy for AsterUserDataPolicy {
    async fn prepare(&mut self) -> anyhow::Result<String> {
        let response = self.client.post_empty("/fapi/v1/listenKey").await?;
        let data: AsterListenKeyResponse = response
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to parse Aster listenKey response: {e}"))?;
        self.listen_key = data.listen_key;
        let url = format!("{}/ws/{}", self.base_ws_url, self.listen_key);
        info!(listen_key = %self.listen_key, "Aster listen key obtained");
        Ok(url)
    }

    fn heartbeat_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs(30 * 60))
    }

    async fn send_heartbeat(
        &mut self,
        _sink: &mut futures_util::stream::SplitSink<boldtrax_core::ws::policy::WsStream, Message>,
    ) -> anyhow::Result<()> {
        let path = format!("/fapi/v1/listenKey?listenKey={}", self.listen_key);
        let result = self.client.put_empty(&path).await;

        match result {
            Ok(_) => {
                debug!(listen_key = %self.listen_key, "Aster listen key kept alive");
                Ok(())
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("410") || msg.contains("401") {
                    Err(anyhow::anyhow!(
                        "Aster listen key expired ({}); forcing reconnect",
                        msg
                    ))
                } else {
                    warn!(error = %msg, "Aster listen key keep-alive failed (non-fatal)");
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

        match serde_json::from_str::<AsterFuturesUserDataEvent>(&text) {
            Ok(AsterFuturesUserDataEvent::OrderTradeUpdate { order: report }) => {
                info!(
                    symbol = %report.symbol,
                    client_order_id = %report.client_order_id,
                    side = %report.side,
                    order_type = %report.order_type,
                    order_status = %report.order_status,
                    filled_qty = %report.order_filled_accumulated_quantity,
                    avg_price = %report.average_price,
                    "Aster ORDER_TRADE_UPDATE received"
                );
                if let Some(event) = self
                    .registry
                    .get_by_exchange_symbol(Exchange::Aster, &report.symbol, InstrumentType::Swap)
                    .and_then(|inst| aster_execution_to_order_event(&report, inst.key))
                {
                    match self.order_tx.try_send(event) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            warn!("Aster order channel full — dropping event");
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            debug!("Aster order channel closed");
                        }
                    }
                } else {
                    warn!(symbol = %report.symbol, "Aster execution for untracked symbol or parse failed");
                }
            }
            Ok(AsterFuturesUserDataEvent::AccountUpdate { update }) => {
                for ws_pos in &update.positions {
                    if let Some(inst) = self.registry.get_by_exchange_symbol(
                        Exchange::Aster,
                        &ws_pos.symbol,
                        InstrumentType::Swap,
                    ) && let Some(patch) = ws_position_to_patch(ws_pos, inst.key)
                    {
                        debug!(
                            key = %inst.key,
                            size = %patch.size,
                            entry = %patch.entry_price,
                            "Aster ACCOUNT_UPDATE position"
                        );
                        match self.pos_tx.try_send(patch) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                warn!("Aster position channel full — dropping patch");
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                debug!("Aster position channel closed");
                            }
                        }
                    }
                }
            }
            Ok(AsterFuturesUserDataEvent::Other) => {}
            Err(e) => {
                warn!(error = %e, text = %text, "Failed to parse Aster user-data event");
            }
        }

        Ok(())
    }
}
