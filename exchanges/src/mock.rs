use async_trait::async_trait;
use boldtrax_core::manager::account::{AccountManagerError, AccountSnapshotSource};
use boldtrax_core::manager::types::{AccountModel, AccountSnapshot, BalanceView, CollateralScope};
use boldtrax_core::registry::InstrumentRegistry;
use boldtrax_core::traits::{
    FundingRateMarketData, MarketDataError, MarketDataProvider, OrderBookFeeder,
    OrderExecutionProvider, PositionProvider, PriceError, TradingError,
};
use boldtrax_core::types::{
    Currency, Exchange, FundingRateSeries, FundingRateSnapshot, InstrumentKey, Order,
    OrderBookSnapshot, OrderBookUpdate, OrderRequest, OrderSide, OrderStatus, OrderType, Position,
};
use chrono::Utc;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct MockState {
    pub balances: HashMap<Currency, BalanceView>,
    pub positions: HashMap<InstrumentKey, Position>,
    pub open_orders: HashMap<String, Order>,
}

impl Default for MockState {
    fn default() -> Self {
        let mut balances = HashMap::new();
        // Give some initial paper trading balance
        balances.insert(
            Currency::USDT,
            BalanceView {
                exchange: Exchange::Binance, // Default, will be overridden
                partition: None,
                asset: Currency::USDT,
                total: Decimal::from(100_000),
                free: Decimal::from(100_000),
                locked: Decimal::ZERO,
                borrowed: Decimal::ZERO,
                unrealized_pnl: Decimal::ZERO,
                collateral_scope: CollateralScope::SharedAcrossPartitions,
                as_of_utc: Utc::now(),
            },
        );

        Self {
            balances,
            positions: HashMap::new(),
            open_orders: HashMap::new(),
        }
    }
}

/// A wrapper around a real exchange client that intercepts trading and account
/// operations to simulate paper trading, while delegating market data to the real exchange.
pub struct MockExchange<T> {
    inner: T,
    state: Arc<RwLock<MockState>>,
    exchange: Exchange,
    #[allow(dead_code)]
    registry: InstrumentRegistry,
}

impl<T> MockExchange<T> {
    pub fn new(inner: T, exchange: Exchange, registry: InstrumentRegistry) -> Self {
        Self {
            inner,
            state: Arc::new(RwLock::new(MockState::default())),
            exchange,
            registry,
        }
    }

    pub async fn get_state(&self) -> MockState {
        self.state.read().await.clone()
    }
}

#[async_trait]
impl<T: MarketDataProvider + Send + Sync> MarketDataProvider for MockExchange<T> {
    async fn health_check(&self) -> Result<(), MarketDataError> {
        self.inner.health_check().await
    }

    async fn server_time(&self) -> Result<chrono::DateTime<Utc>, MarketDataError> {
        self.inner.server_time().await
    }

    async fn load_instruments(&self) -> Result<(), MarketDataError> {
        self.inner.load_instruments().await
    }
}

#[async_trait]
impl<T: FundingRateMarketData + Send + Sync> FundingRateMarketData for MockExchange<T> {
    async fn funding_rate_snapshot(
        &self,
        key: InstrumentKey,
    ) -> Result<FundingRateSnapshot, MarketDataError> {
        self.inner.funding_rate_snapshot(key).await
    }

    async fn funding_rate_history(
        &self,
        key: InstrumentKey,
        start: chrono::DateTime<Utc>,
        end: chrono::DateTime<Utc>,
        limit: usize,
    ) -> Result<FundingRateSeries, MarketDataError> {
        self.inner
            .funding_rate_history(key, start, end, limit)
            .await
    }
}

#[async_trait]
impl<T: OrderBookFeeder + Send + Sync> OrderBookFeeder for MockExchange<T> {
    async fn fetch_order_book(&self, key: InstrumentKey) -> Result<OrderBookSnapshot, PriceError> {
        self.inner.fetch_order_book(key).await
    }

    async fn stream_order_books(
        &self,
        keys: Vec<InstrumentKey>,
        tx: mpsc::Sender<OrderBookUpdate>,
    ) -> anyhow::Result<()> {
        self.inner.stream_order_books(keys, tx).await
    }
}

#[async_trait]
impl<T: Send + Sync> AccountSnapshotSource for MockExchange<T> {
    async fn fetch_account_snapshot(
        &self,
        exchange: Exchange,
    ) -> Result<AccountSnapshot, AccountManagerError> {
        if exchange != self.exchange {
            return Err(AccountManagerError::Other {
                reason: format!("unsupported exchange for MockExchange: {exchange:?}"),
            });
        }

        let state = self.state.read().await;
        let mut balances: Vec<BalanceView> = state.balances.values().cloned().collect();

        // Update exchange field in balances to match the requested exchange
        for balance in &mut balances {
            balance.exchange = self.exchange;
        }

        Ok(AccountSnapshot {
            exchange: self.exchange,
            model: AccountModel::Unified,
            balances,
            partitions: Vec::new(),
            as_of_utc: Utc::now(),
        })
    }
}

#[async_trait]
impl<T: OrderBookFeeder + Send + Sync> OrderExecutionProvider for MockExchange<T> {
    fn format_client_id(&self, internal_id: &str) -> String {
        format!("mock-{}", internal_id)
    }

    async fn place_order(
        &self,
        request: OrderRequest,
        client_order_id: String,
    ) -> Result<Order, TradingError> {
        // For paper trading, we simulate execution immediately against the current order book
        let book = self
            .inner
            .fetch_order_book(request.key)
            .await
            .map_err(|e| {
                TradingError::Other(format!(
                    "failed to fetch order book for mock execution: {}",
                    e
                ))
            })?;

        let fill_price = match request.side {
            OrderSide::Buy => book
                .best_ask
                .ok_or_else(|| TradingError::Other("no asks in order book".to_string()))?,
            OrderSide::Sell => book
                .best_bid
                .ok_or_else(|| TradingError::Other("no bids in order book".to_string()))?,
        };

        let mut state = self.state.write().await;

        // Very basic mock execution: assume market orders fill completely at best price
        // and limit orders fill completely if price is marketable, otherwise stay open.
        let mut status = OrderStatus::New;
        let mut filled_size = Decimal::ZERO;
        let mut avg_fill_price = None;

        let is_marketable = match request.order_type {
            OrderType::Market => true,
            OrderType::Limit => match request.side {
                OrderSide::Buy => request.price.unwrap_or(Decimal::ZERO) >= fill_price,
                OrderSide::Sell => request.price.unwrap_or(Decimal::MAX) <= fill_price,
            },
            OrderType::PostOnly => false, // Simplified
        };

        if is_marketable {
            status = OrderStatus::Filled;
            filled_size = request.size;
            avg_fill_price = Some(fill_price);

            // Update balances (simplified: only handles quote currency deduction for now)
            let quote_currency = request.key.pair.quote();
            let notional = request.size * fill_price;

            if let Some(balance) = state.balances.get_mut(&quote_currency) {
                match request.side {
                    OrderSide::Buy => {
                        if balance.free < notional {
                            return Err(TradingError::InsufficientBalance {
                                required: notional,
                                available: balance.free,
                            });
                        }
                        balance.free -= notional;
                        balance.total -= notional;
                    }
                    OrderSide::Sell => {
                        balance.free += notional;
                        balance.total += notional;
                    }
                }
            } else if request.side == OrderSide::Buy {
                return Err(TradingError::InsufficientBalance {
                    required: notional,
                    available: Decimal::ZERO,
                });
            }

            // Update positions
            let position = state
                .positions
                .entry(request.key)
                .or_insert_with(|| Position {
                    key: request.key,
                    size: Decimal::ZERO,
                    entry_price: Decimal::ZERO,
                    unrealized_pnl: Decimal::ZERO,
                    leverage: Decimal::ONE,
                    liquidation_price: None,
                });

            match request.side {
                OrderSide::Buy => {
                    let new_size = position.size + request.size;
                    if new_size > Decimal::ZERO {
                        position.entry_price = ((position.size * position.entry_price)
                            + (request.size * fill_price))
                            / new_size;
                    }
                    position.size = new_size;
                }
                OrderSide::Sell => {
                    let new_size = position.size - request.size;
                    if new_size < Decimal::ZERO {
                        position.entry_price = ((position.size.abs() * position.entry_price)
                            + (request.size * fill_price))
                            / new_size.abs();
                    }
                    position.size = new_size;
                }
            }
        }

        let order_id = Uuid::new_v4().to_string();
        let now = Utc::now();

        let order = Order {
            internal_id: String::new(),
            client_order_id: client_order_id.clone(),
            exchange_order_id: Some(order_id.clone()),
            request: request.clone(),
            status,
            filled_size,
            avg_fill_price,
            created_at: now,
            updated_at: now,
            strategy_id: request.strategy_id.clone(),
        };

        if status != OrderStatus::Filled {
            state.open_orders.insert(client_order_id, order.clone());
        }

        Ok(order)
    }

    async fn cancel_order(
        &self,
        _key: InstrumentKey,
        client_order_id: &str,
    ) -> Result<Order, TradingError> {
        let mut state = self.state.write().await;
        if let Some(mut order) = state.open_orders.remove(client_order_id) {
            order.mark_canceled();
            Ok(order)
        } else {
            Err(TradingError::OrderNotFound {
                exchange: format!("{:?}", self.exchange),
                order_id: client_order_id.to_string(),
            })
        }
    }

    async fn get_open_orders(&self, key: InstrumentKey) -> Result<Vec<Order>, TradingError> {
        let state = self.state.read().await;
        Ok(state
            .open_orders
            .values()
            .filter(|o| o.request.key == key)
            .cloned()
            .collect())
    }

    async fn get_order_status(
        &self,
        _key: InstrumentKey,
        client_order_id: &str,
    ) -> Result<Order, TradingError> {
        let state = self.state.read().await;
        state
            .open_orders
            .get(client_order_id)
            .cloned()
            .ok_or_else(|| TradingError::OrderNotFound {
                exchange: format!("{:?}", self.exchange),
                order_id: client_order_id.to_string(),
            })
    }

    async fn stream_executions(
        &self,
        _tx: mpsc::Sender<boldtrax_core::types::OrderEvent>,
    ) -> Result<(), boldtrax_core::AccountError> {
        // In a real mock, we might want to simulate order fills over time here.
        // For now, we just keep the stream open.
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    }
}

#[async_trait]
impl<T: Send + Sync> PositionProvider for MockExchange<T> {
    async fn fetch_positions(&self) -> Result<Vec<Position>, TradingError> {
        let state = self.state.read().await;
        Ok(state.positions.values().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boldtrax_core::types::{Currency, InstrumentType, Pairs, PriceLevel};
    use rust_decimal_macros::dec;

    struct DummyClient;

    #[async_trait]
    impl OrderBookFeeder for DummyClient {
        async fn fetch_order_book(
            &self,
            key: InstrumentKey,
        ) -> Result<OrderBookSnapshot, PriceError> {
            Ok(OrderBookSnapshot::new(
                key,
                vec![PriceLevel {
                    price: dec!(100.0),
                    quantity: dec!(1.0),
                }],
                vec![PriceLevel {
                    price: dec!(101.0),
                    quantity: dec!(1.0),
                }],
                Utc::now(),
                Utc::now().timestamp_millis(),
            ))
        }

        async fn stream_order_books(
            &self,
            _keys: Vec<InstrumentKey>,
            _tx: mpsc::Sender<OrderBookUpdate>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_mock_exchange_market_buy() {
        let client = DummyClient;
        let registry = InstrumentRegistry::new();
        let mock = MockExchange::new(client, Exchange::Binance, registry);

        let key = InstrumentKey {
            exchange: Exchange::Binance,
            pair: Pairs::BTCUSDT,
            instrument_type: InstrumentType::Spot,
        };

        let req = OrderRequest {
            key,
            side: OrderSide::Buy,
            order_type: OrderType::Market,
            price: None,
            size: dec!(1.0),
            strategy_id: "test_strategy".to_string(),
        };

        let order = mock
            .place_order(req, "test_client_id".to_string())
            .await
            .unwrap();
        assert_eq!(order.status, OrderStatus::Filled);
        assert_eq!(order.filled_size, dec!(1.0));
        assert_eq!(order.avg_fill_price, Some(dec!(101.0)));

        let state = mock.get_state().await;
        let usdt_balance = state.balances.get(&Currency::USDT).unwrap();
        assert_eq!(usdt_balance.free, dec!(100_000.0) - dec!(101.0));

        let position = state.positions.get(&key).unwrap();
        assert_eq!(position.size, dec!(1.0));
        assert_eq!(position.entry_price, dec!(101.0));
    }
}
