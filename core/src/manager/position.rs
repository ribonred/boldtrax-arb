use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::manager::types::WsPositionPatch;
use crate::traits::PositionProvider;
use crate::types::{InstrumentKey, Position};

// ── PositionStore ─────────────────────────────────────────────────────────────

/// Auto-removing position store.
///
/// `update` removes the entry when `size == 0` (position closed), keeping the
/// map free of ghost positions.  The inner `HashMap` is never exposed publicly;
/// all callers receive `Position` values, never a reference to the store.
struct PositionStore(HashMap<InstrumentKey, Position>);

impl PositionStore {
    fn new() -> Self {
        Self(HashMap::new())
    }

    /// Insert or remove depending on size.  Returns the previous value if any.
    fn update(&mut self, pos: Position) -> Option<Position> {
        if pos.size.is_zero() {
            self.0.remove(&pos.key)
        } else {
            self.0.insert(pos.key, pos)
        }
    }

    fn get(&self, key: &InstrumentKey) -> Option<&Position> {
        self.0.get(key)
    }

    fn get_mut(&mut self, key: &InstrumentKey) -> Option<&mut Position> {
        self.0.get_mut(key)
    }

    fn all(&self) -> Vec<Position> {
        self.0.values().cloned().collect()
    }

    /// Remove entries whose key is absent from `keys` (stale REST cleanup).
    fn retain_keys(&mut self, keys: &HashSet<InstrumentKey>) {
        self.0.retain(|k, _| keys.contains(k));
    }
}

#[derive(Debug, Clone)]
pub struct PositionManagerConfig {
    pub mailbox_capacity: usize,
    pub event_broadcast_capacity: usize,
    pub reconcile_interval: Duration,
}

impl Default for PositionManagerConfig {
    fn default() -> Self {
        Self {
            mailbox_capacity: 1024,
            event_broadcast_capacity: 1024,
            reconcile_interval: Duration::from_secs(30),
        }
    }
}

#[derive(Debug)]
pub enum PositionCommand {
    GetPosition {
        key: InstrumentKey,
        reply_to: tokio::sync::oneshot::Sender<Option<Position>>,
    },
    GetAllPositions {
        reply_to: tokio::sync::oneshot::Sender<Vec<Position>>,
    },
    SyncPositions(Vec<Position>),
}

#[derive(Clone)]
pub struct PositionManagerHandle {
    tx: mpsc::Sender<PositionCommand>,
    event_rx: broadcast::Sender<Position>,
}

impl PositionManagerHandle {
    /// Subscribe to all position change events (upsert and close).
    /// Zero-size positions signal a close event.
    pub fn subscribe_events(&self) -> broadcast::Receiver<Position> {
        self.event_rx.subscribe()
    }

    pub async fn get_position(&self, key: InstrumentKey) -> Option<Position> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if self
            .tx
            .send(PositionCommand::GetPosition {
                key,
                reply_to: reply_tx,
            })
            .await
            .is_ok()
        {
            reply_rx.await.unwrap_or(None)
        } else {
            None
        }
    }

    pub async fn get_all_positions(&self) -> Vec<Position> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if self
            .tx
            .send(PositionCommand::GetAllPositions { reply_to: reply_tx })
            .await
            .is_ok()
        {
            reply_rx.await.unwrap_or_default()
        } else {
            Vec::new()
        }
    }
}

pub struct PositionManagerActor {
    cmd_rx: mpsc::Receiver<PositionCommand>,
    pos_rx: mpsc::Receiver<WsPositionPatch>,
    event_tx: broadcast::Sender<Position>,
    store: PositionStore,
}

impl PositionManagerActor {
    pub fn spawn<P>(provider: Arc<P>, config: PositionManagerConfig) -> PositionManagerHandle
    where
        P: PositionProvider + 'static,
    {
        let (cmd_tx, cmd_rx) = mpsc::channel(config.mailbox_capacity);
        let (event_tx, _) = broadcast::channel(config.event_broadcast_capacity);
        let (pos_tx, pos_rx) = mpsc::channel::<WsPositionPatch>(config.mailbox_capacity);

        let mut actor = Self {
            cmd_rx,
            pos_rx,
            event_tx: event_tx.clone(),
            store: PositionStore::new(),
        };

        // WebSocket position stream task — reconnects automatically on error.
        let provider_ws = Arc::clone(&provider);
        let pos_tx_ws = pos_tx.clone();
        tokio::spawn(async move {
            loop {
                match provider_ws.stream_position_updates(pos_tx_ws.clone()).await {
                    Ok(()) => {
                        debug!("Position WS stream closed cleanly, reconnecting in 5s");
                    }
                    Err(e) => {
                        warn!(error = %e, "Position WS stream error, reconnecting in 5s");
                    }
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });

        // REST reconcile task — initial fetch + periodic refresh.
        let cmd_tx_rest = cmd_tx.clone();
        let reconcile_interval = config.reconcile_interval;
        let provider_rest = Arc::clone(&provider);
        tokio::spawn(async move {
            match provider_rest.fetch_positions().await {
                Ok(positions) => {
                    let _ = cmd_tx_rest
                        .send(PositionCommand::SyncPositions(positions))
                        .await;
                }
                Err(e) => {
                    warn!(error = %e, "Failed to fetch initial positions");
                }
            }

            loop {
                tokio::time::sleep(reconcile_interval).await;
                match provider_rest.fetch_positions().await {
                    Ok(positions) => {
                        if cmd_tx_rest
                            .send(PositionCommand::SyncPositions(positions))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to reconcile positions from REST");
                    }
                }
            }
        });

        tokio::spawn(async move {
            actor.run().await;
        });

        PositionManagerHandle {
            tx: cmd_tx,
            event_rx: event_tx,
        }
    }

    async fn run(&mut self) {
        info!("PositionManagerActor started");

        loop {
            tokio::select! {
                Some(cmd) = self.cmd_rx.recv() => {
                    self.handle_command(cmd);
                }
                Some(patch) = self.pos_rx.recv() => {
                    self.apply_ws_patch(patch);
                }
                else => {
                    info!("PositionManagerActor shutting down");
                    break;
                }
            }
        }
    }

    fn handle_command(&mut self, cmd: PositionCommand) {
        match cmd {
            PositionCommand::GetPosition { key, reply_to } => {
                let _ = reply_to.send(self.store.get(&key).cloned());
            }
            PositionCommand::GetAllPositions { reply_to } => {
                let _ = reply_to.send(self.store.all());
            }
            PositionCommand::SyncPositions(rest_positions) => {
                // Build a key set for stale-entry eviction.
                let rest_keys: HashSet<InstrumentKey> =
                    rest_positions.iter().map(|p| p.key).collect();

                // Remove positions present in the store but absent from REST
                // (closed since the last WS event).
                self.store.retain_keys(&rest_keys);

                // Merge REST data:
                //   REST owns  → liquidation_price, leverage
                //   WS  owns  → size, entry_price, unrealized_pnl
                for rest_pos in rest_positions {
                    if let Some(existing) = self.store.get_mut(&rest_pos.key) {
                        // Already tracked via WS — update REST-owned fields only.
                        existing.liquidation_price = rest_pos.liquidation_price;
                        existing.leverage = rest_pos.leverage;
                    } else {
                        // Not yet seen on WS — bootstrap from REST.
                        self.store.update(rest_pos);
                    }
                }

                debug!(count = self.store.0.len(), "Positions reconciled from REST");
            }
        }
    }

    /// Apply a WebSocket `ACCOUNT_UPDATE` patch.
    ///
    /// - `size == 0` → remove from store, broadcast zero-size position so
    ///   strategies can react to the close event.
    /// - `size != 0` → upsert WS-owned fields, preserve REST-owned fields
    ///   (`liquidation_price`, `leverage`) from the existing entry, then broadcast.
    fn apply_ws_patch(&mut self, patch: WsPositionPatch) {
        if patch.size.is_zero() {
            // Grab REST-owned fields before eviction so the broadcast is complete.
            let (leverage, liquidation_price) = self
                .store
                .get(&patch.key)
                .map(|p| (p.leverage, p.liquidation_price))
                .unwrap_or((rust_decimal::Decimal::ONE, None));

            self.store.update(Position {
                key: patch.key,
                size: patch.size,
                ..Default::default()
            });

            debug!(key = ?patch.key, "WS position closed");

            let _ = self.event_tx.send(Position {
                key: patch.key,
                size: patch.size,
                entry_price: patch.entry_price,
                unrealized_pnl: patch.unrealized_pnl,
                leverage,
                liquidation_price,
            });
            return;
        }

        // Preserve REST-owned fields from the existing entry.
        let (liquidation_price, leverage) = self
            .store
            .get(&patch.key)
            .map(|p| (p.liquidation_price, p.leverage))
            .unwrap_or((None, rust_decimal::Decimal::ONE));

        let pos = Position {
            key: patch.key,
            size: patch.size,
            entry_price: patch.entry_price,
            unrealized_pnl: patch.unrealized_pnl,
            leverage,
            liquidation_price,
        };

        debug!(
            key = ?pos.key,
            size = %pos.size,
            entry_price = %pos.entry_price,
            unrealized_pnl = %pos.unrealized_pnl,
            "WS position update"
        );

        self.store.update(pos.clone());
        let _ = self.event_tx.send(pos);
    }
}
