use crate::manager::types::AccountSnapshot;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::types::{
    Currency, FundingInterval, FundingRateSeries, FundingRateSnapshot, Instrument, InstrumentKey,
    OrderBookSnapshot, OrderBookUpdate, Pairs,
};

#[derive(Debug, Error)]
pub enum MarketDataError {
    #[error("unsupported instrument")]
    UnsupportedInstrument,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("rate limited")]
    RateLimited,
    #[error("network error: {0}")]
    Network(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("other: {0}")]
    Other(String),
}

#[derive(Debug, Clone, Error)]
pub enum AccountError {
    #[error("unsupported exchange: {0}")]
    UnsupportedExchange(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("rate limited")]
    RateLimited,
    #[error("network error: {0}")]
    Network(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("other: {0}")]
    Other(String),
}

impl From<crate::http::ClientError> for MarketDataError {
    fn from(err: crate::http::ClientError) -> Self {
        use crate::http::ClientError;
        match err {
            ClientError::RateLimited => MarketDataError::RateLimited,
            ClientError::Timeout => MarketDataError::Network("timeout".to_string()),
            ClientError::Request(e) => MarketDataError::Network(e),
            ClientError::InvalidUrl(e) => MarketDataError::Other(e),
            ClientError::Serialization(e) => MarketDataError::Parse(e),
            ClientError::ServerError(code) => {
                MarketDataError::Network(format!("server error: {}", code))
            }
            ClientError::InvalidResponse(e) => MarketDataError::Parse(e),
            ClientError::Unauthorized => MarketDataError::Other("unauthorized".to_string()),
            ClientError::AuthError(e) => MarketDataError::Other(format!("auth error: {}", e)),
        }
    }
}

impl From<crate::http::ClientError> for AccountError {
    fn from(err: crate::http::ClientError) -> Self {
        use crate::http::ClientError;
        match err {
            ClientError::RateLimited => AccountError::RateLimited,
            ClientError::Timeout => AccountError::Network("timeout".to_string()),
            ClientError::Request(e) => AccountError::Network(e),
            ClientError::InvalidUrl(e) => AccountError::Other(e),
            ClientError::Serialization(e) => AccountError::Parse(e),
            ClientError::ServerError(code) => {
                AccountError::Network(format!("server error: {}", code))
            }
            ClientError::InvalidResponse(e) => AccountError::Parse(e),
            ClientError::Unauthorized => AccountError::Unauthorized,
            ClientError::AuthError(e) => AccountError::Other(format!("auth error: {}", e)),
        }
    }
}

pub trait BaseInstrumentMeta {
    fn symbol(&self) -> &str;
    fn base_currency(&self) -> Currency;
    fn quote_currency(&self) -> Currency;
    fn pairs(&self) -> Pairs;
    fn is_derivative(&self) -> bool;
    fn is_trading_allowed(&self) -> bool;
    fn tick_size(&self) -> Decimal;
    fn lot_size(&self) -> Decimal;
    fn min_notional(&self) -> Option<Decimal>;
}

pub trait SwapInstrumentMeta: BaseInstrumentMeta + Into<Instrument> {
    fn contract_size(&self) -> Option<Decimal>;
    fn funding_interval(&self) -> Option<FundingInterval>;
}

pub trait SpotInstrumentMeta: BaseInstrumentMeta + Into<Instrument> {}

// ──────────────────────────────────────────────────────────────────
// Order book / price errors and traits
// ──────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum PriceError {
    #[error("unsupported instrument")]
    UnsupportedInstrument,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("rate limited")]
    RateLimited,
    #[error("network error: {0}")]
    Network(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("other: {0}")]
    Other(String),
}

impl From<crate::http::ClientError> for PriceError {
    fn from(err: crate::http::ClientError) -> Self {
        use crate::http::ClientError;
        match err {
            ClientError::RateLimited => PriceError::RateLimited,
            ClientError::Timeout => PriceError::Network("timeout".to_string()),
            ClientError::Request(e) => PriceError::Network(e),
            ClientError::InvalidUrl(e) => PriceError::Other(e),
            ClientError::Serialization(e) => PriceError::Parse(e),
            ClientError::ServerError(code) => {
                PriceError::Network(format!("server error: {}", code))
            }
            ClientError::InvalidResponse(e) => PriceError::Parse(e),
            ClientError::Unauthorized => PriceError::Other("unauthorized".to_string()),
            ClientError::AuthError(e) => PriceError::Other(format!("auth error: {}", e)),
        }
    }
}

/// Provides order-book data for one or more instruments, either via a REST
/// snapshot or a live WebSocket stream.
#[async_trait]
pub trait OrderBookFeeder: Send + Sync {
    /// Fetch a single REST order-book snapshot (up to 5 levels).
    async fn fetch_order_book(&self, key: InstrumentKey) -> Result<OrderBookSnapshot, PriceError>;

    /// Open a WebSocket stream that pushes `OrderBookUpdate` messages into
    /// `tx`.  This method should block until the stream closure (so callers
    /// should `tokio::spawn` it).
    async fn stream_order_books(
        &self,
        keys: Vec<InstrumentKey>,
        tx: mpsc::Sender<OrderBookUpdate>,
    ) -> anyhow::Result<()>;
}

// Blanket impl: Arc<T> automatically satisfies OrderBookFeeder when T does.
// This lets the runner share a single Arc<Client> across multiple tasks.
#[async_trait]
impl<T: OrderBookFeeder + Send + Sync> OrderBookFeeder for std::sync::Arc<T> {
    async fn fetch_order_book(&self, key: InstrumentKey) -> Result<OrderBookSnapshot, PriceError> {
        self.as_ref().fetch_order_book(key).await
    }

    async fn stream_order_books(
        &self,
        keys: Vec<InstrumentKey>,
        tx: mpsc::Sender<OrderBookUpdate>,
    ) -> anyhow::Result<()> {
        self.as_ref().stream_order_books(keys, tx).await
    }
}

#[async_trait]
pub trait Account {
    async fn account_snapshot(&self) -> Result<AccountSnapshot, AccountError>;
}

#[async_trait]
pub trait MarketDataProvider {
    async fn health_check(&self) -> Result<(), MarketDataError>;
    async fn server_time(&self) -> Result<DateTime<Utc>, MarketDataError>;
    /// Load instruments from the exchange and populate the shared registry.
    /// This should be called once at startup or when instruments need refreshing.
    async fn load_instruments(&self) -> Result<(), MarketDataError>;
}

#[async_trait]
pub trait FundingRateMarketData: MarketDataProvider {
    async fn funding_rate_snapshot(
        &self,
        key: InstrumentKey,
    ) -> Result<FundingRateSnapshot, MarketDataError>;

    async fn funding_rate_history(
        &self,
        key: InstrumentKey,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        limit: usize,
    ) -> Result<FundingRateSeries, MarketDataError>;
}

#[derive(Debug, Error)]
pub enum TradingError {
    #[error("insufficient balance: required {required}, available {available}")]
    InsufficientBalance {
        required: Decimal,
        available: Decimal,
    },
    #[error("position limit exceeded: requested {requested}, limit {limit} for {symbol}")]
    PositionLimitExceeded {
        symbol: String,
        requested: Decimal,
        limit: Decimal,
    },
    #[error("order not found: {order_id} on {exchange}")]
    OrderNotFound { exchange: String, order_id: String },
    #[error("invalid execution mode: cannot execute live trade in paper mode")]
    InvalidExecutionMode,
    #[error("minimum notional violated: requested {requested}, minimum {min}")]
    MinNotionalViolated { requested: Decimal, min: Decimal },
    #[error("order size {requested} exceeds maximum allowed {max}")]
    MaxOrderSizeExceeded { requested: Decimal, max: Decimal },
    #[error("order notional {requested} exceeds maximum allowed {max}")]
    MaxNotionalExceeded { requested: Decimal, max: Decimal },
    #[error("limit price deviates {deviation_pct}% from market price, exceeds max {max_pct}%")]
    PriceBandViolation {
        deviation_pct: Decimal,
        max_pct: Decimal,
    },
    #[error("post_only is only valid for Limit orders")]
    InvalidPostOnly,
    #[error("price data not available for instrument — wait for order book snapshot")]
    PriceNotReady,
    #[error("unsupported instrument")]
    UnsupportedInstrument,
    #[error("rate limited")]
    RateLimited,
    #[error("network error: {0}")]
    Network(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("other: {0}")]
    Other(String),
}

impl From<crate::http::ClientError> for TradingError {
    fn from(err: crate::http::ClientError) -> Self {
        use crate::http::ClientError;
        match err {
            ClientError::RateLimited => TradingError::RateLimited,
            ClientError::Timeout => TradingError::Network("timeout".to_string()),
            ClientError::Request(e) => TradingError::Network(e),
            ClientError::InvalidUrl(e) => TradingError::Other(e),
            ClientError::Serialization(e) => TradingError::Parse(e),
            ClientError::ServerError(code) => {
                TradingError::Network(format!("server error: {}", code))
            }
            ClientError::InvalidResponse(e) => TradingError::Parse(e),
            ClientError::Unauthorized => TradingError::Other("unauthorized".to_string()),
            ClientError::AuthError(e) => TradingError::Other(format!("auth error: {}", e)),
        }
    }
}

impl From<AccountError> for TradingError {
    fn from(err: AccountError) -> Self {
        match err {
            AccountError::Unauthorized => TradingError::Other("unauthorized".to_string()),
            AccountError::RateLimited => TradingError::RateLimited,
            AccountError::Network(e) => TradingError::Network(e),
            AccountError::Parse(e) => TradingError::Parse(e),
            AccountError::UnsupportedExchange(e) => TradingError::Other(e),
            AccountError::Other(e) => TradingError::Other(e),
        }
    }
}

#[async_trait]
pub trait OrderExecutionProvider: Send + Sync {
    /// Encodes the internal order ID and strategy ID into an exchange-compliant
    /// `client_order_id` string, respecting the exchange's length and character constraints.
    fn encode_client_order_id(&self, internal_id: &str, strategy_id: &str) -> String;

    /// Decodes the `strategy_id` from a `client_order_id` received from the exchange.
    /// Returns `None` if the format is unrecognized or the ID was not generated by this bot.
    fn decode_strategy_id(&self, client_order_id: &str) -> Option<String>;

    async fn place_order(
        &self,
        request: crate::types::OrderRequest,
        client_order_id: String,
    ) -> Result<crate::types::Order, TradingError>;

    async fn cancel_order(
        &self,
        key: InstrumentKey,
        client_order_id: &str,
    ) -> Result<crate::types::Order, TradingError>;

    async fn get_open_orders(
        &self,
        key: InstrumentKey,
    ) -> Result<Vec<crate::types::Order>, TradingError>;

    async fn get_order_status(
        &self,
        key: InstrumentKey,
        client_order_id: &str,
    ) -> Result<crate::types::Order, TradingError>;

    /// Open a WebSocket stream that pushes `OrderEvent` messages into `tx`.
    async fn stream_executions(
        &self,
        tx: mpsc::Sender<crate::types::OrderEvent>,
    ) -> Result<(), AccountError>;
}

#[async_trait]
impl<T: OrderExecutionProvider + Send + Sync> OrderExecutionProvider for std::sync::Arc<T> {
    fn encode_client_order_id(&self, internal_id: &str, strategy_id: &str) -> String {
        self.as_ref()
            .encode_client_order_id(internal_id, strategy_id)
    }

    fn decode_strategy_id(&self, client_order_id: &str) -> Option<String> {
        self.as_ref().decode_strategy_id(client_order_id)
    }

    async fn place_order(
        &self,
        request: crate::types::OrderRequest,
        client_order_id: String,
    ) -> Result<crate::types::Order, TradingError> {
        self.as_ref().place_order(request, client_order_id).await
    }

    async fn cancel_order(
        &self,
        key: InstrumentKey,
        client_order_id: &str,
    ) -> Result<crate::types::Order, TradingError> {
        self.as_ref().cancel_order(key, client_order_id).await
    }

    async fn get_open_orders(
        &self,
        key: InstrumentKey,
    ) -> Result<Vec<crate::types::Order>, TradingError> {
        self.as_ref().get_open_orders(key).await
    }

    async fn get_order_status(
        &self,
        key: InstrumentKey,
        client_order_id: &str,
    ) -> Result<crate::types::Order, TradingError> {
        self.as_ref().get_order_status(key, client_order_id).await
    }

    async fn stream_executions(
        &self,
        tx: mpsc::Sender<crate::types::OrderEvent>,
    ) -> Result<(), AccountError> {
        self.as_ref().stream_executions(tx).await
    }
}

#[async_trait]
pub trait LeverageProvider: Send + Sync {
    /// Set the leverage for an instrument on the exchange.
    /// Returns the actual leverage that was set (may differ from requested).
    async fn set_leverage(
        &self,
        key: InstrumentKey,
        leverage: Decimal,
    ) -> Result<Decimal, TradingError>;

    /// Get the current leverage for an instrument.
    /// Returns `None` if the leverage is unknown (e.g. never set via this session).
    async fn get_leverage(&self, key: InstrumentKey) -> Result<Option<Decimal>, TradingError>;
}

#[async_trait]
impl<T: LeverageProvider + Send + Sync> LeverageProvider for std::sync::Arc<T> {
    async fn set_leverage(
        &self,
        key: InstrumentKey,
        leverage: Decimal,
    ) -> Result<Decimal, TradingError> {
        self.as_ref().set_leverage(key, leverage).await
    }

    async fn get_leverage(&self, key: InstrumentKey) -> Result<Option<Decimal>, TradingError> {
        self.as_ref().get_leverage(key).await
    }
}

#[async_trait]
pub trait PositionProvider: Send + Sync {
    /// Fetch current derivative positions via REST (Swap only).
    async fn fetch_positions(&self) -> Result<Vec<crate::types::Position>, TradingError>;

    /// Stream real-time derivative position updates from the exchange WebSocket
    /// user-data stream.  Blocks until the stream closes; reconnect logic lives
    /// in the caller.  The default implementation blocks forever — exchanges
    /// without real-time position streaming are silently idle until REST
    /// reconciliation runs.
    async fn stream_position_updates(
        &self,
        _tx: mpsc::Sender<crate::manager::types::WsPositionPatch>,
    ) -> Result<(), AccountError> {
        std::future::pending().await
    }
}

#[async_trait]
impl<T: PositionProvider + Send + Sync> PositionProvider for std::sync::Arc<T> {
    async fn fetch_positions(&self) -> Result<Vec<crate::types::Position>, TradingError> {
        self.as_ref().fetch_positions().await
    }

    async fn stream_position_updates(
        &self,
        tx: mpsc::Sender<crate::manager::types::WsPositionPatch>,
    ) -> Result<(), AccountError> {
        self.as_ref().stream_position_updates(tx).await
    }
}
use crate::types::{Exchange, Order, OrderRequest, Position};

/// Unified API contract for exchange command routing.
///
/// Every method either auto-routes by [`InstrumentKey::exchange`] or
/// takes an explicit [`Exchange`] argument.  Implementors include
/// [`ZmqRouter`] (production) and potential mock/stub implementations
/// for testing.
#[async_trait]
pub trait CoreApi: Send + Sync {
    async fn submit_order(&self, req: OrderRequest) -> anyhow::Result<Order>;

    async fn get_position(&self, key: InstrumentKey) -> anyhow::Result<Option<Position>>;

    async fn get_instrument(&self, key: InstrumentKey) -> anyhow::Result<Option<Instrument>>;

    async fn get_reference_price(&self, key: InstrumentKey) -> anyhow::Result<Decimal>;

    async fn get_funding_rate(&self, key: InstrumentKey) -> anyhow::Result<FundingRateSnapshot>;

    async fn set_leverage(&self, key: InstrumentKey, leverage: Decimal) -> anyhow::Result<Decimal>;

    async fn cancel_order(&self, exchange: Exchange, id: String) -> anyhow::Result<Order>;

    async fn get_all_positions(&self, exchange: Exchange) -> anyhow::Result<Vec<Position>>;

    async fn get_account_snapshot(&self, exchange: Exchange) -> anyhow::Result<AccountSnapshot>;

    async fn get_all_instruments(&self, exchange: Exchange) -> anyhow::Result<Vec<Instrument>>;
}
