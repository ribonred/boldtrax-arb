use crate::arbitrage::decider::DeciderAction;
use crate::arbitrage::policy::{ExecutionError, ExecutionPolicy};
use crate::arbitrage::types::SpotPerpPair;
use async_trait::async_trait;
use boldtrax_core::types::{InstrumentKey, OrderRequest, OrderSide, OrderType};
use boldtrax_core::zmq::client::ZmqCommandClient;
use rust_decimal::Decimal;
use tracing::{debug, error, info, warn};

pub struct ExecutionEngine {
    pub client: ZmqCommandClient,
}

impl ExecutionEngine {
    pub fn new(client: ZmqCommandClient) -> Self {
        Self { client }
    }

    pub async fn execute(&mut self, action: DeciderAction, pair: &mut SpotPerpPair) {
        let _ = ExecutionPolicy::<SpotPerpPair>::execute(self, &action, pair).await;
    }

    /// Place a spot order using LIMIT_MAKER (post-only) at the best orderbook
    /// level to earn maker fees. Falls back to Market if no orderbook data.
    async fn place_spot_order(
        &mut self,
        key: &InstrumentKey,
        size: Decimal,
        best_bid: Option<Decimal>,
        best_ask: Option<Decimal>,
        ask_depth: Option<Decimal>,
        bid_depth: Option<Decimal>,
    ) -> Result<(), ExecutionError> {
        if size.is_zero() {
            return Ok(());
        }

        let side = if size.is_sign_positive() {
            OrderSide::Buy
        } else {
            OrderSide::Sell
        };

        let abs_size = size.abs();

        // Depth check: warn if our order size exceeds the top 3 levels of available liquidity.
        match side {
            OrderSide::Buy => {
                if let Some(depth) = ask_depth
                    && abs_size > depth
                {
                    warn!(
                        key = %key, size = %abs_size, depth = %depth,
                        "Order size exceeds top-3 ask depth"
                    );
                }
            }
            OrderSide::Sell => {
                if let Some(depth) = bid_depth
                    && abs_size > depth
                {
                    warn!(
                        key = %key, size = %abs_size, depth = %depth,
                        "Order size exceeds top-3 bid depth"
                    );
                }
            }
        }

        // For LIMIT_MAKER (post-only):
        //   BUY at best_bid  → sits on the bid side, does NOT cross the spread.
        //   SELL at best_ask → sits on the ask side, does NOT cross the spread.
        let (order_type, price) = match side {
            OrderSide::Buy => match best_bid {
                Some(bid) => {
                    info!(
                        key = %key, side = %side, price = %bid,
                        "Spot LIMIT_MAKER at best bid"
                    );
                    (OrderType::Limit, Some(bid))
                }
                None => {
                    warn!(key = %key, "No orderbook data for spot, falling back to Market");
                    (OrderType::Market, None)
                }
            },
            OrderSide::Sell => match best_ask {
                Some(ask) => {
                    info!(
                        key = %key, side = %side, price = %ask,
                        "Spot LIMIT_MAKER at best ask"
                    );
                    (OrderType::Limit, Some(ask))
                }
                None => {
                    warn!(key = %key, "No orderbook data for spot, falling back to Market");
                    (OrderType::Market, None)
                }
            },
        };

        let post_only = order_type == OrderType::Limit;

        let req = OrderRequest {
            key: *key,
            side,
            order_type,
            price,
            size: size.abs(),
            post_only,
            reduce_only: false, // Spot never has reduce_only
            strategy_id: "arbitrage".to_string(),
        };

        self.submit_order(req).await
    }

    /// Place a perp order using Market for immediate execution.
    async fn place_perp_order(
        &mut self,
        key: &InstrumentKey,
        size: Decimal,
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

        let req = OrderRequest {
            key: *key,
            side,
            order_type: OrderType::Market,
            price: None,
            size: size.abs(),
            post_only: false,
            reduce_only,
            strategy_id: "arbitrage".to_string(),
        };

        self.submit_order(req).await
    }

    async fn submit_order(&mut self, req: OrderRequest) -> Result<(), ExecutionError> {
        info!(
            key = %req.key,
            side = %req.side,
            order_type = %req.order_type,
            size = %req.size,
            price = ?req.price,
            post_only = req.post_only,
            reduce_only = req.reduce_only,
            "Submitting order"
        );
        let key = req.key;
        match self.client.submit_order(req).await {
            Ok(order) => {
                info!("Order submitted: {:?}", order);
                Ok(())
            }
            Err(e) => {
                error!("Failed to submit order: {}", e);
                Err(ExecutionError::OrderFailed {
                    key: format!("{:?}", key),
                    reason: e.to_string(),
                })
            }
        }
    }
}

#[async_trait]
impl ExecutionPolicy<SpotPerpPair> for ExecutionEngine {
    fn name(&self) -> &'static str {
        "zmq_execution_engine"
    }

    async fn execute_inner(
        &mut self,
        action: &DeciderAction,
        pair: &mut SpotPerpPair,
    ) -> Result<(), ExecutionError> {
        match action {
            DeciderAction::Enter {
                size_long,
                size_short,
            } => {
                info!(
                    "Executing Enter action: long={}, short={}",
                    size_long, size_short
                );
                self.place_spot_order(
                    &pair.spot.key.clone(),
                    *size_long,
                    pair.spot.best_bid,
                    pair.spot.best_ask,
                    pair.spot.ask_depth,
                    pair.spot.bid_depth,
                )
                .await?;
                self.place_perp_order(&pair.perp.key.clone(), *size_short, false)
                    .await?;
                // NOTE: status transitions to Active when both legs fill.
                // The PositionUpdate (perp) and OrderUpdate (spot) events
                // will confirm execution; don't set Active prematurely.
            }
            DeciderAction::Rebalance {
                size_long,
                size_short,
            } => {
                info!(
                    "Executing Rebalance action: long={}, short={}",
                    size_long, size_short
                );
                self.place_spot_order(
                    &pair.spot.key.clone(),
                    *size_long,
                    pair.spot.best_bid,
                    pair.spot.best_ask,
                    pair.spot.ask_depth,
                    pair.spot.bid_depth,
                )
                .await?;
                self.place_perp_order(&pair.perp.key.clone(), *size_short, false)
                    .await?;
            }
            DeciderAction::Exit => {
                info!("Executing Exit action");
                // Close spot: sell all held quantity
                let spot_size = -pair.spot.quantity;
                // Close perp: reverse the position
                let perp_size = -pair.perp.position_size;
                self.place_spot_order(
                    &pair.spot.key.clone(),
                    spot_size,
                    pair.spot.best_bid,
                    pair.spot.best_ask,
                    pair.spot.ask_depth,
                    pair.spot.bid_depth,
                )
                .await?;
                self.place_perp_order(&pair.perp.key.clone(), perp_size, true)
                    .await?;
                pair.status = crate::arbitrage::types::PairStatus::Inactive;
            }
            DeciderAction::DoNothing => {
                debug!("No action required");
            }
        }
        Ok(())
    }
}
