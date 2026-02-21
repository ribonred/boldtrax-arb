use crate::manager::types::AccountSnapshot;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::registry::InstrumentRegistry;
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

#[derive(Debug, Error)]
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
    async fn load_instruments(&self, registry: &InstrumentRegistry) -> Result<(), MarketDataError>;
}

#[async_trait]
pub trait FundingRateMarketData: MarketDataProvider {
    async fn funding_rate_snapshot(
        &self,
        key: InstrumentKey,
        registry: &InstrumentRegistry,
    ) -> Result<FundingRateSnapshot, MarketDataError>;

    async fn funding_rate_history(
        &self,
        key: InstrumentKey,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        limit: usize,
        registry: &InstrumentRegistry,
    ) -> Result<FundingRateSeries, MarketDataError>;
}

// ──────────────────────────────────────────────────────────────────
// Trading / Execution errors and traits
// ──────────────────────────────────────────────────────────────────

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

#[async_trait]
pub trait OrderExecutionProvider: Send + Sync {
    /// Formats the internal ID into an exchange-compliant client_order_id
    fn format_client_id(&self, internal_id: &str) -> String;

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
    ) -> anyhow::Result<()>;
}

#[async_trait]
impl<T: OrderExecutionProvider + Send + Sync> OrderExecutionProvider for std::sync::Arc<T> {
    fn format_client_id(&self, internal_id: &str) -> String {
        self.as_ref().format_client_id(internal_id)
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
    ) -> anyhow::Result<()> {
        self.as_ref().stream_executions(tx).await
    }
}
