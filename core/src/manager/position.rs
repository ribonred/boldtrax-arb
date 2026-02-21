use std::collections::HashMap;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info};

use crate::types::{InstrumentKey, OrderEvent, OrderStatus, Position};

#[derive(Debug, Clone)]
pub struct PositionManagerConfig {
    pub mailbox_capacity: usize,
    pub event_broadcast_capacity: usize,
}

impl Default for PositionManagerConfig {
    fn default() -> Self {
        Self {
            mailbox_capacity: 1024,
            event_broadcast_capacity: 1024,
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
}

impl PositionManagerActor {
    pub fn spawn(
        event_rx: broadcast::Receiver<OrderEvent>,
        config: PositionManagerConfig,
    ) -> PositionManagerHandle {
        let (cmd_tx, cmd_rx) = mpsc::channel(config.mailbox_capacity);
        let (event_tx, _) = broadcast::channel(config.event_broadcast_capacity);

        let mut actor = Self {
            cmd_rx,
            event_rx,
            event_tx: event_tx.clone(),
            positions: HashMap::new(),
        };

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
        }
    }

    async fn handle_order_event(&mut self, event: OrderEvent) {
        let order = match event {
            OrderEvent::Filled(o) => o,
            _ => return, // We only care about fills for position tracking
        };

        if order.status != OrderStatus::Filled {
            return;
        }

        let key = order.request.key;
        let fill_size = order.filled_size;
        let fill_price = order.avg_fill_price.unwrap_or_default();

        let position = self.positions.entry(key).or_insert_with(|| Position {
            key,
            ..Default::default()
        });

        let old_size = position.size;
        position.apply_fill(order.request.side, fill_size, fill_price);
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
