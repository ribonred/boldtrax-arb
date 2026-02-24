use async_trait::async_trait;

use crate::arbitrage::perp_perp::types::PerpPerpPair;
use crate::arbitrage::policy::{ExecutionError, ExecutionPolicy};
use crate::arbitrage::types::{DeciderAction, PairStatus};
use boldtrax_core::CoreApi;
use boldtrax_core::types::{InstrumentKey, OrderRequest, OrderSide, OrderType};
use boldtrax_core::zmq::router::ZmqRouter;
use rust_decimal::Decimal;
use tracing::{debug, error, info};

pub struct PerpPerpExecutionEngine {
    /// Smart router — auto-dispatches orders to the correct exchange
    /// based on `InstrumentKey.exchange`.
    pub router: ZmqRouter,
}

impl PerpPerpExecutionEngine {
    pub fn new(router: ZmqRouter) -> Self {
        Self { router }
    }

    async fn place_perp_order(
        router: &ZmqRouter,
        key: &InstrumentKey,
        size: Decimal,
        price: Option<Decimal>,
        post_only: bool,
        reduce_only: bool,
    ) -> Result<(), ExecutionError> {
        if size.is_zero() {
            return Ok(());
        }

        let side = if size.is_sign_positive() {
            OrderSide::Buy
        } else {
            OrderSide::Sell
        };

        let (order_type, order_price) = if let Some(p) = price {
            (OrderType::Limit, Some(p))
        } else {
            (OrderType::Market, None)
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
                info!("Order submitted: {:?}", order);
                Ok(())
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
}

#[async_trait]
impl ExecutionPolicy<PerpPerpPair> for PerpPerpExecutionEngine {
    fn name(&self) -> &'static str {
        "perp_perp_execution_engine"
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
                let long_price = pair.long_leg.current_price;
                let short_price = pair.short_leg.current_price;
                info!(
                    long = %size_long,
                    short = %size_short,
                    long_price = %long_price,
                    short_price = %short_price,
                    "Executing Enter: limit post_only on both legs"
                );
                Self::place_perp_order(
                    &self.router,
                    &pair.long_leg.key,
                    *size_long,
                    Some(long_price),
                    true,
                    false,
                )
                .await?;
                Self::place_perp_order(
                    &self.router,
                    &pair.short_leg.key,
                    *size_short,
                    Some(short_price),
                    true,
                    false,
                )
                .await?;
            }
            DeciderAction::Rebalance {
                size_long,
                size_short,
            } => {
                let long_price = pair.long_leg.current_price;
                let short_price = pair.short_leg.current_price;
                info!(
                    long = %size_long,
                    short = %size_short,
                    "Executing Rebalance: limit post_only"
                );
                Self::place_perp_order(
                    &self.router,
                    &pair.long_leg.key,
                    *size_long,
                    Some(long_price),
                    true,
                    false,
                )
                .await?;
                Self::place_perp_order(
                    &self.router,
                    &pair.short_leg.key,
                    *size_short,
                    Some(short_price),
                    true,
                    false,
                )
                .await?;
            }
            DeciderAction::Exit => {
                info!("Executing Exit: market + reduce_only on both legs");
                let long_close = -pair.long_leg.position_size;
                let short_close = -pair.short_leg.position_size;
                Self::place_perp_order(
                    &self.router,
                    &pair.long_leg.key,
                    long_close,
                    None,
                    false,
                    true,
                )
                .await?;
                Self::place_perp_order(
                    &self.router,
                    &pair.short_leg.key,
                    short_close,
                    None,
                    false,
                    true,
                )
                .await?;
                pair.status = PairStatus::Inactive;
            }
            DeciderAction::DoNothing => {
                debug!("No action required");
            }
        }
        Ok(())
    }
}
