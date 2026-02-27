use crate::arbitrage::policy::{ExecutionError, ExecutionPolicy};
use crate::arbitrage::spot_perp::types::SpotPerpPair;
use crate::arbitrage::types::{DeciderAction, PairState, PairStatus};
use async_trait::async_trait;
use boldtrax_core::CoreApi;
use boldtrax_core::types::{Exchange, InstrumentKey, Order, OrderRequest, OrderSide, OrderType};
use boldtrax_core::zmq::router::ZmqRouter;
use rust_decimal::Decimal;
use tracing::{debug, error, info, warn};

pub struct SpotPerpExecutionEngine {
    pub router: ZmqRouter,
}

impl SpotPerpExecutionEngine {
    pub fn new(router: ZmqRouter) -> Self {
        Self { router }
    }

    /// Cancel-replace: cancel all pending orders before dispatching fresh
    /// rebalance orders. Prevents order accumulation when prior limit orders
    /// have not yet filled or been cancelled within the poll interval.
    async fn cancel_pending_orders(&mut self, pair: &mut SpotPerpPair) {
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
                debug!(error = %e, %order_id, "Cancel failed (may already be terminal)");
            }
            pair.order_tracker_mut().force_cancel(&order_id);
        }
    }

    pub async fn execute(&mut self, action: DeciderAction, pair: &mut SpotPerpPair) {
        let _ = ExecutionPolicy::<SpotPerpPair>::execute(self, &action, pair).await;
    }

    /// Core two-leg dispatch: place spot + perp orders, register them
    /// in the tracker, and advance the pair's lifecycle status.
    ///
    /// `reduce_only` applies to the perp leg only (spot never uses it).
    /// `next_status` is `None` for actions that keep the current status
    /// (e.g. Rebalance stays Active).
    async fn dispatch_legs(
        &mut self,
        pair: &mut SpotPerpPair,
        spot_size: Decimal,
        perp_size: Decimal,
        reduce_only: bool,
        next_status: Option<PairStatus>,
    ) -> Result<(), ExecutionError> {
        let spot_key = pair.spot.key;
        let perp_key = pair.perp.key;

        let spot_order = self
            .place_spot_order(
                &spot_key,
                spot_size,
                pair.spot.best_bid,
                pair.spot.best_ask,
                pair.spot.ask_depth,
                pair.spot.bid_depth,
            )
            .await?;
        let perp_order = self
            .place_perp_order(&perp_key, perp_size, reduce_only)
            .await?;

        let tracker = pair.order_tracker_mut();
        tracker.track_if_some(spot_order, spot_key, spot_size);
        tracker.track_if_some(perp_order, perp_key, perp_size);

        if let Some(status) = next_status {
            pair.status = status;
        }
        Ok(())
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
    ) -> Result<Option<Order>, ExecutionError> {
        if size.is_zero() {
            return Ok(None);
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

        self.submit_order(req).await.map(Some)
    }

    /// Place a perp order using Market for immediate execution.
    async fn place_perp_order(
        &mut self,
        key: &InstrumentKey,
        size: Decimal,
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

        self.submit_order(req).await.map(Some)
    }

    async fn submit_order(&mut self, req: OrderRequest) -> Result<Order, ExecutionError> {
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
        match self.router.submit_order(req).await {
            Ok(order) => {
                info!(
                    client_order_id = %order.client_order_id,
                    "Order submitted"
                );
                Ok(order)
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
impl ExecutionPolicy<SpotPerpPair> for SpotPerpExecutionEngine {
    fn name(&self) -> &'static str {
        "zmq_execution_engine"
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
        pair: &mut SpotPerpPair,
    ) -> Result<(), ExecutionError> {
        match action {
            DeciderAction::Enter {
                size_long,
                size_short,
            } => {
                info!("Executing Enter: spot={}, perp={}", size_long, size_short);
                self.dispatch_legs(
                    pair,
                    *size_long,
                    *size_short,
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

                info!(
                    "Executing Rebalance: spot={}, perp={}",
                    size_long, size_short
                );
                // Rebalance stays Active — tracker just monitors completion.
                self.dispatch_legs(pair, *size_long, *size_short, false, None)
                    .await
            }
            DeciderAction::Exit => {
                let spot_size = -pair.spot.quantity;
                let perp_size = -pair.perp.position_size;
                info!("Executing Exit: spot={}, perp={}", spot_size, perp_size);
                self.dispatch_legs(pair, spot_size, perp_size, true, Some(PairStatus::Exiting))
                    .await
            }
            DeciderAction::Recover {
                size_long,
                size_short,
            } => {
                info!("Executing Recover: spot={}, perp={}", size_long, size_short);
                self.dispatch_legs(
                    pair,
                    *size_long,
                    *size_short,
                    false,
                    Some(PairStatus::Recovering),
                )
                .await
            }
            DeciderAction::Unwind {
                size_long,
                size_short,
            } => {
                info!("Executing Unwind: spot={}, perp={}", size_long, size_short);
                self.dispatch_legs(
                    pair,
                    *size_long,
                    *size_short,
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
