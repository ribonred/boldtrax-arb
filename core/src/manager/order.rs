use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use rust_decimal::Decimal;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};

use crate::registry::InstrumentRegistry;
use crate::traits::{OrderExecutionProvider, TradingError};
use crate::types::{Exchange, Order, OrderEvent, OrderRequest, OrderStatus};

#[derive(Debug, Clone)]
pub struct OrderManagerConfig {
    pub mailbox_capacity: usize,
    pub event_broadcast_capacity: usize,
    pub ws_channel_capacity: usize,
    pub reconcile_interval: Duration,
}

impl Default for OrderManagerConfig {
    fn default() -> Self {
        Self {
            mailbox_capacity: 128,
            event_broadcast_capacity: 1024,
            ws_channel_capacity: 1024,
            reconcile_interval: Duration::from_secs(30),
        }
    }
}

#[derive(Debug)]
pub enum OrderCommand {
    SubmitOrder {
        request: OrderRequest,
        reply_to: tokio::sync::oneshot::Sender<Result<Order, TradingError>>,
    },
    CancelOrder {
        internal_id: String,
        reply_to: tokio::sync::oneshot::Sender<Result<Order, TradingError>>,
    },
    Reconcile,
    HandleSubmitResult {
        internal_id: String,
        result: Result<Order, TradingError>,
    },
    HandleCancelResult {
        internal_id: String,
        result: Result<Order, TradingError>,
    },
}

#[derive(Clone)]
pub struct OrderManagerHandle {
    tx: mpsc::Sender<OrderCommand>,
    event_rx: broadcast::Sender<OrderEvent>,
}

impl OrderManagerHandle {
    pub async fn submit_order(&self, request: OrderRequest) -> Result<Order, TradingError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(OrderCommand::SubmitOrder {
                request,
                reply_to: reply_tx,
            })
            .await
            .map_err(|_| TradingError::Other("OrderManagerActor dropped".to_string()))?;

        reply_rx.await.map_err(|_| {
            TradingError::Other("OrderManagerActor dropped reply channel".to_string())
        })?
    }

    pub async fn cancel_order(&self, internal_id: String) -> Result<Order, TradingError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(OrderCommand::CancelOrder {
                internal_id,
                reply_to: reply_tx,
            })
            .await
            .map_err(|_| TradingError::Other("OrderManagerActor dropped".to_string()))?;

        reply_rx.await.map_err(|_| {
            TradingError::Other("OrderManagerActor dropped reply channel".to_string())
        })?
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<OrderEvent> {
        self.event_rx.subscribe()
    }
}

pub struct OrderManagerActor<P> {
    exchange: Exchange,
    provider: Arc<P>,
    registry: InstrumentRegistry,
    cmd_tx: mpsc::Sender<OrderCommand>,
    cmd_rx: mpsc::Receiver<OrderCommand>,
    ws_rx: mpsc::Receiver<OrderEvent>,
    event_tx: broadcast::Sender<OrderEvent>,
    active_orders: HashMap<String, Order>,
    client_id_map: HashMap<String, String>, // client_order_id -> internal_id
}

impl<P> OrderManagerActor<P>
where
    P: OrderExecutionProvider + 'static,
{
    pub fn spawn(
        exchange: Exchange,
        provider: Arc<P>,
        registry: InstrumentRegistry,
        config: OrderManagerConfig,
    ) -> OrderManagerHandle {
        let (cmd_tx, cmd_rx) = mpsc::channel(config.mailbox_capacity);
        let (ws_tx, ws_rx) = mpsc::channel(config.ws_channel_capacity);
        let (event_tx, _) = broadcast::channel(config.event_broadcast_capacity);

        let mut actor = Self {
            exchange,
            provider: Arc::clone(&provider),
            registry,
            cmd_tx: cmd_tx.clone(),
            cmd_rx,
            ws_rx,
            event_tx: event_tx.clone(),
            active_orders: HashMap::new(),
            client_id_map: HashMap::new(),
        };

        // Spawn WS feed task
        let provider_clone = Arc::clone(&provider);
        let ws_tx_clone = ws_tx.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = provider_clone.stream_executions(ws_tx_clone.clone()).await {
                    error!(error = %e, "Order execution stream failed, reconnecting in 5s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                } else {
                    info!("Order execution stream closed cleanly, reconnecting in 5s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        });

        // Spawn reconciliation loop
        let cmd_tx_clone = cmd_tx.clone();
        let reconcile_interval = config.reconcile_interval;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(reconcile_interval).await;
                if cmd_tx_clone.send(OrderCommand::Reconcile).await.is_err() {
                    break; // Actor dropped
                }
            }
        });

        tokio::spawn(async move {
            actor.run().await;
        });

        OrderManagerHandle {
            tx: cmd_tx,
            event_rx: event_tx,
        }
    }

    async fn run(&mut self) {
        info!(exchange = ?self.exchange, "OrderManagerActor started");

        loop {
            tokio::select! {
                Some(cmd) = self.cmd_rx.recv() => {
                    self.handle_command(cmd).await;
                }
                Some(event) = self.ws_rx.recv() => {
                    self.handle_ws_event(event).await;
                }
                else => {
                    info!(exchange = ?self.exchange, "OrderManagerActor shutting down");
                    break;
                }
            }
        }
    }

    async fn handle_command(&mut self, cmd: OrderCommand) {
        match cmd {
            OrderCommand::SubmitOrder { request, reply_to } => {
                let result = self.process_submit_order(request).await;
                let _ = reply_to.send(result);
            }
            OrderCommand::CancelOrder {
                internal_id,
                reply_to,
            } => {
                let result = self.process_cancel_order(internal_id).await;
                let _ = reply_to.send(result);
            }
            OrderCommand::Reconcile => {
                self.process_reconcile().await;
            }
            OrderCommand::HandleSubmitResult {
                internal_id,
                result,
            } => {
                self.handle_submit_result(internal_id, result).await;
            }
            OrderCommand::HandleCancelResult {
                internal_id,
                result,
            } => {
                self.handle_cancel_result(internal_id, result).await;
            }
        }
    }

    async fn process_submit_order(&mut self, request: OrderRequest) -> Result<Order, TradingError> {
        // 1. Fetch instrument for normalization
        let instrument = self
            .registry
            .get(&request.key)
            .ok_or(TradingError::UnsupportedInstrument)?;

        let mut normalized_req = request.clone();

        normalized_req.size = instrument.normalize_quantity(normalized_req.size);

        if let Some(price) = normalized_req.price {
            normalized_req.price = Some(instrument.normalize_price(price));
        }

        let notional = normalized_req.price.unwrap_or(Decimal::ZERO) * normalized_req.size;
        if !instrument.is_notional_valid(
            normalized_req.price.unwrap_or(Decimal::ZERO),
            normalized_req.size,
        ) {
            return Err(TradingError::MinNotionalViolated {
                requested: notional,
                min: instrument.min_notional.unwrap_or(Decimal::ZERO),
            });
        }

        let timestamp_ms = Utc::now().timestamp_millis();
        let mut internal_id = String::with_capacity(64);
        write!(
            internal_id,
            "{}-{}-{}",
            normalized_req.strategy_id,
            timestamp_ms,
            self.exchange.short_code()
        )
        .unwrap();
        let client_order_id = self.provider.format_client_id(&internal_id);

        let now = Utc::now();
        let pending_order = Order {
            internal_id: internal_id.clone(),
            strategy_id: normalized_req.strategy_id.clone(),
            client_order_id: client_order_id.clone(),
            request: normalized_req.clone(),
            status: OrderStatus::PendingSubmit,
            created_at: now,
            updated_at: now,
            ..Default::default()
        };

        // Store in active tracking immediately so WS can find it if it arrives first
        self.active_orders
            .insert(internal_id.clone(), pending_order.clone());
        self.client_id_map
            .insert(client_order_id.clone(), internal_id.clone());

        debug!(
            internal_id = %internal_id,
            client_order_id = %client_order_id,
            "Submitting normalized order in background"
        );

        let provider = Arc::clone(&self.provider);
        let req_clone = normalized_req;
        let cid_clone = client_order_id;
        let id_clone = internal_id.clone();

        crate::utils::spawn_and_send(
            self.cmd_tx.clone(),
            async move { provider.place_order(req_clone, cid_clone).await },
            move |result| OrderCommand::HandleSubmitResult {
                internal_id: id_clone,
                result,
            },
        );

        Ok(pending_order)
    }

    async fn process_cancel_order(&mut self, internal_id: String) -> Result<Order, TradingError> {
        let order =
            self.active_orders
                .get(&internal_id)
                .ok_or_else(|| TradingError::OrderNotFound {
                    exchange: format!("{:?}", self.exchange),
                    order_id: internal_id.clone(),
                })?;

        let client_order_id = order.client_order_id.clone();
        let key = order.request.key;

        debug!(internal_id = %internal_id, "Canceling order in background");

        let provider = Arc::clone(&self.provider);
        let cid_clone = client_order_id.clone();
        let id_clone = internal_id.clone();

        crate::utils::spawn_and_send(
            self.cmd_tx.clone(),
            async move { provider.cancel_order(key, &cid_clone).await },
            move |result| OrderCommand::HandleCancelResult {
                internal_id: id_clone,
                result,
            },
        );

        // Return the current state of the order optimistically
        Ok(order.clone())
    }

    async fn handle_submit_result(
        &mut self,
        internal_id: String,
        result: Result<Order, TradingError>,
    ) {
        match result {
            Ok(rest_order) => {
                // The REST call succeeded.
                // Check if the order is still in our active map (WS might have already filled/canceled it).
                if let Some(order) = self.active_orders.get_mut(&internal_id) {
                    // Update exchange_order_id if we didn't have it yet
                    if order.exchange_order_id.is_none() {
                        order.exchange_order_id = rest_order.exchange_order_id;
                        order.updated_at = Utc::now();
                    }

                    // Only transition to New if we are still in PendingSubmit.
                    // If WS already moved us to PartiallyFilled or Filled, don't downgrade the status.
                    if order.is_pending_submit() {
                        order.mark_new();
                        let _ = self.event_tx.send(order.clone().into());
                    }

                    debug!(internal_id = %internal_id, "REST submit confirmed");
                } else {
                    // Order was already fully processed by WS and removed from active_orders
                    debug!(internal_id = %internal_id, "REST submit confirmed, but order already completed via WS");
                }
            }
            Err(e) => {
                // The REST call failed.
                error!(internal_id = %internal_id, error = %e, "REST submit failed");
                if let Some(mut order) = self.active_orders.remove(&internal_id) {
                    // Only reject if it hasn't been partially filled by WS already
                    if order.is_pending_or_new() {
                        order.mark_rejected();
                        self.client_id_map.remove(&order.client_order_id);
                        let _ = self.event_tx.send(order.clone().into());
                    } else {
                        // It was partially filled by WS before REST returned an error?
                        // This is a weird edge case (e.g. timeout on REST but exchange accepted it).
                        warn!(internal_id = %internal_id, "REST failed but order has WS updates, keeping active");
                        self.active_orders.insert(internal_id, order);
                    }
                }
            }
        }
    }

    async fn handle_cancel_result(
        &mut self,
        internal_id: String,
        result: Result<Order, TradingError>,
    ) {
        match result {
            Ok(mut canceled_order) => {
                debug!(internal_id = %internal_id, "REST cancel confirmed");
                canceled_order.internal_id = internal_id.clone();

                // Update state
                self.active_orders.remove(&internal_id);
                self.client_id_map.remove(&canceled_order.client_order_id);

                // Broadcast Canceled event
                let _ = self.event_tx.send(canceled_order.into());
            }
            Err(e) => {
                error!(internal_id = %internal_id, error = %e, "REST cancel failed");
            }
        }
    }

    async fn handle_ws_event(&mut self, event: OrderEvent) {
        let exchange_order = event.inner();

        // Try to resolve internal_id from client_order_id if not present
        let internal_id = if exchange_order.internal_id.is_empty() {
            if let Some(id) = self.client_id_map.get(&exchange_order.client_order_id) {
                id.clone()
            } else {
                panic!(
                    "FATAL: Received WS event for unknown order (client_order_id: {}). Executing orders outside the strategy is fatal.",
                    exchange_order.client_order_id
                );
            }
        } else {
            exchange_order.internal_id.clone()
        };

        // Get the internal order
        let mut internal_order = if let Some(order) = self.active_orders.get(&internal_id) {
            order.clone()
        } else {
            panic!(
                "FATAL: Received WS event for untracked order (internal_id: {}, client_order_id: {}). Order state lost or executed outside strategy.",
                internal_id, exchange_order.client_order_id
            );
        };

        // Mutate internal order with exchange order data
        internal_order.apply_exchange_update(exchange_order);

        debug!(
            internal_id = %internal_id,
            status = ?internal_order.status,
            "Received WS order event"
        );

        // Update state
        if internal_order.is_final_state() {
            self.active_orders.remove(&internal_id);
            self.client_id_map.remove(&internal_order.client_order_id);
        } else {
            self.active_orders
                .insert(internal_id.clone(), internal_order.clone());
        }

        // Broadcast
        let _ = self.event_tx.send(internal_order.into());
    }

    async fn process_reconcile(&mut self) {
        debug!("Running order reconciliation");

        // Group active orders by InstrumentKey to minimize REST calls
        let mut keys_to_check = HashSet::new();
        for order in self.active_orders.values() {
            keys_to_check.insert(order.request.key);
        }

        for key in keys_to_check {
            match self.provider.get_open_orders(key).await {
                Ok(exchange_orders) => {
                    let exchange_client_ids: HashSet<String> = exchange_orders
                        .into_iter()
                        .map(|o| o.client_order_id)
                        .collect();

                    let missing_internal_ids: Vec<(String, String)> = self
                        .active_orders
                        .iter()
                        .filter(|(_, order)| {
                            order.request.key == key
                                && !exchange_client_ids.contains(&order.client_order_id)
                        })
                        .map(|(internal_id, order)| {
                            (internal_id.clone(), order.client_order_id.clone())
                        })
                        .collect();

                    for (internal_id, client_order_id) in missing_internal_ids {
                        warn!(
                            internal_id = %internal_id,
                            client_order_id = %client_order_id,
                            "Order missing from exchange open orders, fetching status"
                        );

                        // Fetch final status to see if it filled or canceled
                        match self.provider.get_order_status(key, &client_order_id).await {
                            Ok(mut final_order) => {
                                // Inject internal_id
                                final_order.internal_id = internal_id.clone();

                                if !final_order.is_final_state() {
                                    continue; // Still open somehow?
                                }

                                // Update state and broadcast
                                self.active_orders.remove(&internal_id);
                                self.client_id_map.remove(&client_order_id);
                                let _ = self.event_tx.send(final_order.into());
                            }
                            Err(e) => {
                                error!(
                                    internal_id = %internal_id,
                                    error = %e,
                                    "Failed to fetch final status for missing order"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    error!(key = ?key, error = %e, "Failed to fetch open orders for reconciliation");
                }
            }
        }
    }
}
