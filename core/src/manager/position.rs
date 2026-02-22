use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::traits::PositionProvider;
use crate::types::{InstrumentKey, OrderEvent, OrderStatus, Position};

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
    event_rx: broadcast::Receiver<OrderEvent>,
    event_tx: broadcast::Sender<Position>,
    positions: HashMap<InstrumentKey, Position>,
    // Track the last known filled size for each order to calculate incremental fills
    order_fills: HashMap<String, Decimal>,
}

impl PositionManagerActor {
    pub fn spawn<P>(
        provider: Arc<P>,
        event_rx: broadcast::Receiver<OrderEvent>,
        config: PositionManagerConfig,
    ) -> PositionManagerHandle
    where
        P: PositionProvider + 'static,
    {
        let (cmd_tx, cmd_rx) = mpsc::channel(config.mailbox_capacity);
        let (event_tx, _) = broadcast::channel(config.event_broadcast_capacity);

        let mut actor = Self {
            cmd_rx,
            event_rx,
            event_tx: event_tx.clone(),
            positions: HashMap::new(),
            order_fills: HashMap::new(),
        };

        let cmd_tx_clone = cmd_tx.clone();
        let reconcile_interval = config.reconcile_interval;

        tokio::spawn(async move {
            // Initial fetch
            match provider.fetch_positions().await {
                Ok(positions) => {
                    let _ = cmd_tx_clone
                        .send(PositionCommand::SyncPositions(positions))
                        .await;
                }
                Err(e) => {
                    if e.to_string().contains("not implemented") {
                        debug!("PositionProvider not implemented for this exchange");
                    } else {
                        warn!(error = %e, "Failed to fetch initial positions");
                    }
                }
            }

            // Background reconcile loop
            loop {
                tokio::time::sleep(reconcile_interval).await;
                match provider.fetch_positions().await {
                    Ok(positions) => {
                        if cmd_tx_clone
                            .send(PositionCommand::SyncPositions(positions))
                            .await
                            .is_err()
                        {
                            break; // Actor dropped
                        }
                    }
                    Err(e) => {
                        if e.to_string().contains("not implemented") {
                            break; // No need to keep polling if not implemented
                        } else {
                            warn!(error = %e, "Failed to fetch positions for reconciliation");
                        }
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
                    self.handle_command(cmd).await;
                }
                Ok(event) = self.event_rx.recv() => {
                    self.handle_order_event(event).await;
                }
                else => {
                    info!("PositionManagerActor shutting down");
                    break;
                }
            }
        }
    }

    async fn handle_command(&mut self, cmd: PositionCommand) {
        match cmd {
            PositionCommand::GetPosition { key, reply_to } => {
                let _ = reply_to.send(self.positions.get(&key).cloned());
            }
            PositionCommand::GetAllPositions { reply_to } => {
                let _ = reply_to.send(self.positions.values().cloned().collect());
            }
            PositionCommand::SyncPositions(positions) => {
                self.positions.clear();
                for pos in positions {
                    self.positions.insert(pos.key, pos);
                }
                debug!(
                    count = self.positions.len(),
                    "Positions synced from exchange"
                );
            }
        }
    }

    async fn handle_order_event(&mut self, event: OrderEvent) {
        let order = match event {
            OrderEvent::Filled(o) | OrderEvent::PartiallyFilled(o) => o,
            _ => return, // We only care about fills for position tracking
        };

        if order.status != OrderStatus::Filled && order.status != OrderStatus::PartiallyFilled {
            return;
        }

        // Ignore Spot orders - they are tracked as balances in the AccountManager.
        // Strategies still receive the OrderEvent directly from the OrderManager.
        if order.request.key.instrument_type == crate::types::InstrumentType::Spot {
            return;
        }

        let key = order.request.key;

        // Calculate the incremental fill size
        let previous_fill = self
            .order_fills
            .get(&order.internal_id)
            .copied()
            .unwrap_or_default();
        let incremental_fill = order.filled_size - previous_fill;

        if incremental_fill.is_zero() {
            return; // No new fill to process
        }

        // Update the tracked fill size
        self.order_fills
            .insert(order.internal_id.clone(), order.filled_size);

        // Clean up tracking if the order is fully filled
        if order.status == OrderStatus::Filled {
            self.order_fills.remove(&order.internal_id);
        }

        let fill_price = order.avg_fill_price.unwrap_or_default();

        let position = self.positions.entry(key).or_insert_with(|| Position {
            key,
            ..Default::default()
        });

        let old_size = position.size;
        position.apply_fill(order.request.side, incremental_fill, fill_price);
        let new_size = position.size;

        let _ = self.event_tx.send(position.clone());

        debug!(
            key = ?key,
            old_size = %old_size,
            new_size = %new_size,
            entry_price = %position.entry_price,
            "Position updated from order fill"
        );
    }
}
