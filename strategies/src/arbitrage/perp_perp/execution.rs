use async_trait::async_trait;

use crate::arbitrage::perp_perp::types::PerpPerpPair;
use crate::arbitrage::policy::{ExecutionError, ExecutionPolicy};
use crate::arbitrage::types::{DeciderAction, PairState, PairStatus};
use boldtrax_core::CoreApi;
use boldtrax_core::types::{Exchange, InstrumentKey, Order, OrderRequest, OrderSide, OrderType};
use boldtrax_core::zmq::router::ZmqRouter;
use rust_decimal::Decimal;
use tracing::{debug, error, info};

/// A single leg's sizing intent: how much to trade and at what price.
/// `price = None` → Market order; `Some(p)` → Limit post-only at `p`.
struct LegOrder {
    size: Decimal,
    price: Option<Decimal>,
}

impl LegOrder {
    fn limit(size: Decimal, price: Decimal) -> Self {
        Self {
            size,
            price: Some(price),
        }
    }

    fn market(size: Decimal) -> Self {
        Self { size, price: None }
    }
}

pub struct PerpPerpExecutionEngine {
    /// Smart router — auto-dispatches orders to the correct exchange
    /// based on `InstrumentKey.exchange`.
    pub router: ZmqRouter,
}

impl PerpPerpExecutionEngine {
    pub fn new(router: ZmqRouter) -> Self {
        Self { router }
    }

    /// Cancel-replace: cancel all pending orders before dispatching fresh
    /// rebalance orders. Prevents order accumulation when prior limit orders
    /// have not yet filled or been cancelled within the poll interval.
    pub(crate) async fn cancel_pending_orders(&mut self, pair: &mut PerpPerpPair) {
        let pending: Vec<_> = pair
            .order_tracker()
            .all_pending()
            .iter()
            .map(|o| (o.key.exchange, o.order_id.clone()))
            .collect();

        if pending.is_empty() {
            return;
        }

        info!(count = pending.len(), "Cancel-replace: cancelling pending orders before rebalance");
        for (exchange, order_id) in pending {
            if let Err(e) = self.router.cancel_order(exchange, order_id.clone()).await {
                // Not fatal — order may have already filled or been cancelled.
                debug!(error = %e, %order_id, "Cancel failed (may already be terminal)");
            }
            pair.order_tracker_mut().force_cancel(&order_id);
        }
    }

    /// Core two-leg dispatch: place long + short perp orders, register them
    /// in the tracker, and advance the pair's lifecycle status.
    async fn dispatch_legs(
        &mut self,
        pair: &mut PerpPerpPair,
        long: LegOrder,
        short: LegOrder,
        reduce_only: bool,
        next_status: Option<PairStatus>,
    ) -> Result<(), ExecutionError> {
        let long_key = pair.long_leg.key;
        let short_key = pair.short_leg.key;

        let long_order =
            Self::place_perp_order(&self.router, &long_key, long.size, long.price, reduce_only)
                .await?;
        let short_order = Self::place_perp_order(
            &self.router,
            &short_key,
            short.size,
            short.price,
            reduce_only,
        )
        .await?;

        let tracker = pair.order_tracker_mut();
        tracker.track_if_some(long_order, long_key, long.size);
        tracker.track_if_some(short_order, short_key, short.size);

        if let Some(status) = next_status {
            pair.status = status;
        }
        Ok(())
    }

    async fn place_perp_order(
        router: &ZmqRouter,
        key: &InstrumentKey,
        size: Decimal,
        price: Option<Decimal>,
        reduce_only: bool,
    ) -> Result<Option<Order>, ExecutionError> {
        if size.is_zero() {
            return Ok(None);
        }

        let side = if size.is_sign_positive() {
            OrderSide::Buy
        } else {
            OrderSide::Sell
        };

        let (order_type, order_price, post_only) = if let Some(p) = price {
            (OrderType::Limit, Some(p), true)
        } else {
            (OrderType::Market, None, false)
        };

        let req = OrderRequest {
            key: *key,
            side,
            order_type,
            price: order_price,
            size: size.abs(),
            post_only,
            reduce_only,
            strategy_id: "perp_perp_arb".to_string(),
        };

        info!(
            key = %req.key,
            side = %req.side,
            size = %req.size,
            order_type = ?req.order_type,
            price = ?req.price,
            post_only = req.post_only,
            reduce_only = req.reduce_only,
            "Submitting perp order"
        );

        let key_str = format!("{:?}", key);
        match router.submit_order(req).await {
            Ok(order) => {
                info!(
                    client_order_id = %order.client_order_id,
                    "Order submitted"
                );
                Ok(Some(order))
            }
            Err(e) => {
                error!("Failed to submit order: {}", e);
                Err(ExecutionError::OrderFailed {
                    key: key_str,
                    reason: e.to_string(),
                })
            }
        }
    }

    /// Market order the missing leg during recovery chase.
    ///
    /// Unlike `Recover` (limit post_only), this uses market orders for
    /// immediate fill when the economic assessment decides chasing is
    /// cheaper than resetting the pair.
    pub async fn chase_missing_leg(
        &mut self,
        pair: &mut PerpPerpPair,
        size_long: Decimal,
        size_short: Decimal,
    ) -> Result<(), ExecutionError> {
        info!(
            long = %size_long, short = %size_short,
            "Chase: market order for missing leg"
        );
        self.dispatch_legs(
            pair,
            LegOrder::market(size_long),
            LegOrder::market(size_short),
            false,
            None, // stay in Recovering — tracker transitions handle it
        )
        .await
    }
}

#[async_trait]
impl ExecutionPolicy<PerpPerpPair> for PerpPerpExecutionEngine {
    fn name(&self) -> &'static str {
        "perp_perp_execution_engine"
    }

    async fn cancel_order(
        &mut self,
        exchange: Exchange,
        order_id: String,
    ) -> Result<(), ExecutionError> {
        self.router
            .cancel_order(exchange, order_id.clone())
            .await
            .map(|_| ())
            .map_err(|e| ExecutionError::OrderFailed {
                key: format!("{exchange:?}:{order_id}"),
                reason: e.to_string(),
            })
    }

    async fn execute_inner(
        &mut self,
        action: &DeciderAction,
        pair: &mut PerpPerpPair,
    ) -> Result<(), ExecutionError> {
        match action {
            DeciderAction::Enter {
                size_long,
                size_short,
            } => {
                let long_price = pair
                    .long_leg
                    .best_bid
                    .unwrap_or(pair.long_leg.current_price);
                let short_price = pair
                    .short_leg
                    .best_ask
                    .unwrap_or(pair.short_leg.current_price);
                info!(
                    long = %size_long, short = %size_short,
                    long_price = %long_price, short_price = %short_price,
                    "Executing Enter: limit post_only on both legs"
                );
                self.dispatch_legs(
                    pair,
                    LegOrder::limit(*size_long, long_price),
                    LegOrder::limit(*size_short, short_price),
                    false,
                    Some(PairStatus::Entering),
                )
                .await
            }
            DeciderAction::Rebalance {
                size_long,
                size_short,
            } => {
                // Cancel-replace: clear stale/pending orders so we never
                // accumulate multiple rebalance generations simultaneously.
                self.cancel_pending_orders(pair).await;

                let long_price = if size_long.is_sign_positive() {
                    pair.long_leg
                        .best_bid
                        .unwrap_or(pair.long_leg.current_price)
                } else {
                    pair.long_leg
                        .best_ask
                        .unwrap_or(pair.long_leg.current_price)
                };
                let short_price = if size_short.is_sign_positive() {
                    pair.short_leg
                        .best_bid
                        .unwrap_or(pair.short_leg.current_price)
                } else {
                    pair.short_leg
                        .best_ask
                        .unwrap_or(pair.short_leg.current_price)
                };
                info!(
                    long = %size_long, short = %size_short,
                    "Executing Rebalance: limit post_only"
                );
                self.dispatch_legs(
                    pair,
                    LegOrder::limit(*size_long, long_price),
                    LegOrder::limit(*size_short, short_price),
                    false,
                    None,
                )
                .await
            }
            DeciderAction::Exit => {
                let long_close = -pair.long_leg.position_size;
                let short_close = -pair.short_leg.position_size;
                info!("Executing Exit: market + reduce_only on both legs");
                self.dispatch_legs(
                    pair,
                    LegOrder::market(long_close),
                    LegOrder::market(short_close),
                    true,
                    Some(PairStatus::Exiting),
                )
                .await
            }
            DeciderAction::Recover {
                size_long,
                size_short,
            } => {
                let long_price = pair
                    .long_leg
                    .best_bid
                    .unwrap_or(pair.long_leg.current_price);
                let short_price = pair
                    .short_leg
                    .best_ask
                    .unwrap_or(pair.short_leg.current_price);
                info!(
                    long = %size_long, short = %size_short,
                    "Executing Recover: limit post_only on missing leg"
                );
                self.dispatch_legs(
                    pair,
                    LegOrder::limit(*size_long, long_price),
                    LegOrder::limit(*size_short, short_price),
                    false,
                    Some(PairStatus::Recovering),
                )
                .await
            }
            DeciderAction::Unwind {
                size_long,
                size_short,
            } => {
                info!(
                    long = %size_long, short = %size_short,
                    "Executing Unwind chunk: market + reduce_only"
                );
                self.dispatch_legs(
                    pair,
                    LegOrder::market(*size_long),
                    LegOrder::market(*size_short),
                    true,
                    Some(PairStatus::Unwinding),
                )
                .await
            }
            DeciderAction::DoNothing => {
                debug!("No action required");
                Ok(())
            }
        }
    }
}
