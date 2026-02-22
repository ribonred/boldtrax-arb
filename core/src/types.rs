use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal::prelude::Signed;
use strum::{Display, EnumProperty, EnumString};

// currency we care about
#[derive(
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    EnumString,
    Display,
)]
#[archive(check_bytes)]
#[repr(u8)]
pub enum Currency {
    BTC,
    ETH,
    XRP,
    USDT,
    USDC,
    SOL,
}
// instrument types we care about
#[derive(
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    EnumString,
    Display,
)]
#[archive(check_bytes)]
#[repr(u8)]
#[strum(serialize_all = "lowercase")]
pub enum InstrumentType {
    Spot,
    Swap,
}
// exchange we care about
#[derive(
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    EnumString,
    Display,
    EnumProperty,
)]
#[archive(check_bytes)]
#[repr(u8)]
pub enum Exchange {
    #[strum(to_string = "binance", serialize = "BN", props(short_code = "BN"))]
    Binance,
    #[strum(to_string = "bybit", serialize = "BY", props(short_code = "BY"))]
    Bybit,
    #[strum(to_string = "gateio", serialize = "GT", props(short_code = "GT"))]
    Gateio,
    #[strum(to_string = "aster", serialize = "AS", props(short_code = "AS"))]
    Aster,
    #[strum(to_string = "okx", serialize = "OK", props(short_code = "OK"))]
    Okx,
}

impl Exchange {
    pub fn short_code(&self) -> &'static str {
        self.get_str("short_code").unwrap_or("UNKNOWN")
    }
}

// list of pairs we care about
#[derive(
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    EnumString,
    Display,
)]
#[archive(check_bytes)]
#[repr(u8)]
#[strum(serialize_all = "UPPERCASE")]
pub enum Pairs {
    BTCUSDT,
    ETHUSDT,
    XRPUSDT,
    USDCUSDT,
    SOLUSDT,
}

impl Pairs {
    pub fn base(&self) -> Currency {
        match self {
            Pairs::BTCUSDT => Currency::BTC,
            Pairs::ETHUSDT => Currency::ETH,
            Pairs::XRPUSDT => Currency::XRP,
            Pairs::USDCUSDT => Currency::USDC,
            Pairs::SOLUSDT => Currency::SOL,
        }
    }

    pub fn quote(&self) -> Currency {
        match self {
            Pairs::BTCUSDT => Currency::USDT,
            Pairs::ETHUSDT => Currency::USDT,
            Pairs::XRPUSDT => Currency::USDT,
            Pairs::USDCUSDT => Currency::USDT,
            Pairs::SOLUSDT => Currency::USDT,
        }
    }

    pub fn from_currencies(base: Currency, quote: Currency) -> Option<Self> {
        match (base, quote) {
            (Currency::BTC, Currency::USDT) => Some(Pairs::BTCUSDT),
            (Currency::ETH, Currency::USDT) => Some(Pairs::ETHUSDT),
            (Currency::XRP, Currency::USDT) => Some(Pairs::XRPUSDT),
            (Currency::USDC, Currency::USDT) => Some(Pairs::USDCUSDT),
            (Currency::SOL, Currency::USDT) => Some(Pairs::SOLUSDT),
            _ => None,
        }
    }
}

#[derive(
    rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash,
)]
#[archive(check_bytes)]
#[repr(u8)]
pub enum FundingInterval {
    EveryHour,
    Every4Hours,
    Every8Hours,
    CustomSeconds(u32),
}

#[derive(
    rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash,
)]
#[archive(check_bytes)]
pub struct InstrumentKey {
    pub exchange: Exchange,
    pub pair: Pairs,
    pub instrument_type: InstrumentType,
}

impl Default for InstrumentKey {
    fn default() -> Self {
        Self {
            exchange: Exchange::Binance,
            pair: Pairs::BTCUSDT,
            instrument_type: InstrumentType::Spot,
        }
    }
}

impl std::fmt::Display for InstrumentKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}-{}-{}",
            self.pair,
            self.exchange.short_code(),
            self.instrument_type.to_string().to_uppercase()
        )
    }
}

impl std::str::FromStr for InstrumentKey {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() != 3 {
            return Err(format!("Invalid InstrumentKey format: {}", s));
        }

        let pair = parts[0]
            .parse::<Pairs>()
            .map_err(|_| format!("Invalid pair: {}", parts[0]))?;

        let exchange = parts[1]
            .parse::<Exchange>()
            .map_err(|_| format!("Invalid exchange short code: {}", parts[1]))?;

        let instrument_type = parts[2]
            .to_lowercase()
            .parse::<InstrumentType>()
            .map_err(|_| format!("Invalid instrument type: {}", parts[2]))?;

        Ok(Self {
            exchange,
            pair,
            instrument_type,
        })
    }
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct Instrument {
    pub key: InstrumentKey,
    pub exchange_symbol: String,
    pub exchange_id: String,
    pub tick_size: Decimal,
    pub lot_size: Decimal,
    pub min_notional: Option<Decimal>,
    pub contract_size: Option<Decimal>,
    pub multiplier: Decimal,
    pub funding_interval: Option<FundingInterval>,
}

impl Instrument {
    pub fn normalize_price(&self, price: Decimal) -> Decimal {
        let price_steps = (price / self.tick_size).round();
        (price_steps * self.tick_size).normalize()
    }

    pub fn normalize_quantity(&self, quantity: Decimal) -> Decimal {
        let qty_steps = (quantity / self.lot_size).floor();
        (qty_steps * self.lot_size).normalize()
    }

    pub fn is_notional_valid(&self, price: Decimal, quantity: Decimal) -> bool {
        if let Some(min_notional) = self.min_notional {
            let notional = price * quantity;
            notional == Decimal::ZERO || notional >= min_notional
        } else {
            true
        }
    }
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct FundingRatePoint {
    pub funding_rate: Decimal,
    pub event_time_utc: DateTime<Utc>,
    pub event_time_ms: i64,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct FundingRateSnapshot {
    pub key: InstrumentKey,
    pub funding_rate: Decimal,
    pub mark_price: Decimal,
    pub index_price: Decimal,
    pub interest_rate: Option<Decimal>,
    pub next_funding_time_utc: DateTime<Utc>,
    pub next_funding_time_ms: i64,
    pub event_time_utc: DateTime<Utc>,
    pub event_time_ms: i64,
    pub interval: FundingInterval,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct FundingRateSeries {
    pub key: InstrumentKey,
    pub interval: FundingInterval,
    pub points: Vec<FundingRatePoint>,
}

/// A single price level in an order book (price + available quantity).
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct PriceLevel {
    pub price: Decimal,
    pub quantity: Decimal,
}

/// A 5-level order book snapshot (best 5 bids and asks) with pre-computed
/// convenience fields so callers don't have to recompute them every time.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct Ticker {
    pub key: InstrumentKey,
    pub price: Decimal,
    pub volume_24h: Option<Decimal>,
    pub timestamp: DateTime<Utc>,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct Trade {
    pub key: InstrumentKey,
    pub trade_id: String,
    pub price: Decimal,
    pub size: Decimal,
    pub side: OrderSide,
    pub timestamp: DateTime<Utc>,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct OrderBookSnapshot {
    pub key: InstrumentKey,
    /// Up to 5 bid levels, sorted descending by price.
    pub bids: Vec<PriceLevel>,
    /// Up to 5 ask levels, sorted ascending by price.
    pub asks: Vec<PriceLevel>,
    /// Best (highest) bid price, or `None` when the book is empty.
    pub best_bid: Option<Decimal>,
    /// Best (lowest) ask price, or `None` when the book is empty.
    pub best_ask: Option<Decimal>,
    /// Mid-price ((best_bid + best_ask) / 2), `None` if either side is missing.
    pub mid: Option<Decimal>,
    /// Spread (best_ask - best_bid), `None` if either side is missing.
    pub spread: Option<Decimal>,
    pub timestamp_utc: DateTime<Utc>,
    pub timestamp_ms: i64,
}

impl OrderBookSnapshot {
    /// Build a snapshot from raw level lists and fill in the derived fields.
    pub fn new(
        key: InstrumentKey,
        bids: Vec<PriceLevel>,
        asks: Vec<PriceLevel>,
        timestamp_utc: DateTime<Utc>,
        timestamp_ms: i64,
    ) -> Self {
        let best_bid = bids.first().map(|l| l.price);
        let best_ask = asks.first().map(|l| l.price);
        let mid = best_bid.zip(best_ask).map(|(b, a)| (b + a) / Decimal::TWO);
        let spread = best_bid.zip(best_ask).map(|(b, a)| a - b);
        Self {
            key,
            bids,
            asks,
            best_bid,
            best_ask,
            mid,
            spread,
            timestamp_utc,
            timestamp_ms,
        }
    }
}

/// Wraps an `OrderBookSnapshot` for delivery over a channel from a WebSocket
/// feed task to the `PriceManagerActor`.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct OrderBookUpdate {
    pub snapshot: OrderBookSnapshot,
}

#[derive(
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
    serde::Serialize,
    serde::Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Default,
)]
#[serde(rename_all = "lowercase")]
#[archive(check_bytes)]
#[repr(u8)]
pub enum ExecutionMode {
    Live,
    #[default]
    Paper,
}

#[derive(
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    EnumString,
    Display,
)]
#[archive(check_bytes)]
#[repr(u8)]
#[strum(serialize_all = "lowercase")]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    EnumString,
    Display,
)]
#[archive(check_bytes)]
#[repr(u8)]
#[strum(serialize_all = "lowercase")]
pub enum OrderType {
    Market,
    Limit,
    PostOnly,
}

#[derive(
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::Display,
)]
#[archive(check_bytes)]
#[repr(u8)]
pub enum OrderStatus {
    PendingSubmit,
    New,
    PartiallyFilled,
    Filled,
    Canceled,
    Rejected,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct OrderRequest {
    pub key: InstrumentKey,
    pub strategy_id: String,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub price: Option<Decimal>,
    pub size: Decimal,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct Order {
    pub internal_id: String,
    pub strategy_id: String,
    pub client_order_id: String,
    pub exchange_order_id: Option<String>,
    pub request: OrderRequest,
    pub status: OrderStatus,
    pub filled_size: Decimal,
    pub avg_fill_price: Option<Decimal>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Order {
    pub fn is_pending_submit(&self) -> bool {
        self.status == OrderStatus::PendingSubmit
    }

    pub fn is_pending_or_new(&self) -> bool {
        self.status == OrderStatus::PendingSubmit || self.status == OrderStatus::New
    }

    pub fn is_final_state(&self) -> bool {
        matches!(
            self.status,
            OrderStatus::Filled | OrderStatus::Canceled | OrderStatus::Rejected
        )
    }

    pub fn mark_new(&mut self) {
        self.status = OrderStatus::New;
        self.updated_at = Utc::now();
    }

    pub fn mark_rejected(&mut self) {
        self.status = OrderStatus::Rejected;
        self.updated_at = Utc::now();
    }

    pub fn mark_canceled(&mut self) {
        self.status = OrderStatus::Canceled;
        self.updated_at = Utc::now();
    }

    /// Applies updates from an exchange order to this internal order.
    /// This updates the status, filled size, average fill price, and timestamps.
    pub fn apply_exchange_update(&mut self, exchange_order: &Order) {
        self.status = exchange_order.status;
        self.filled_size = exchange_order.filled_size;
        self.avg_fill_price = exchange_order.avg_fill_price;
        self.updated_at = exchange_order.updated_at;

        if self.exchange_order_id.is_none() {
            self.exchange_order_id = exchange_order.exchange_order_id.clone();
        }
    }
}

impl Default for Order {
    fn default() -> Self {
        let now = Utc::now();
        Self {
            internal_id: String::new(),
            strategy_id: String::new(),
            client_order_id: String::new(),
            exchange_order_id: None,
            request: OrderRequest {
                key: InstrumentKey::default(),
                strategy_id: String::new(),
                side: OrderSide::Buy,
                order_type: OrderType::Market,
                price: None,
                size: Decimal::ZERO,
            },
            status: OrderStatus::PendingSubmit,
            filled_size: Decimal::ZERO,
            avg_fill_price: None,
            created_at: now,
            updated_at: now,
        }
    }
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
#[repr(u8)]
pub enum OrderEvent {
    New(Order),
    PartiallyFilled(Order),
    Filled(Order),
    Canceled(Order),
    Rejected(Order),
}

impl OrderEvent {
    pub fn inner(&self) -> &Order {
        match self {
            OrderEvent::New(o) => o,
            OrderEvent::PartiallyFilled(o) => o,
            OrderEvent::Filled(o) => o,
            OrderEvent::Canceled(o) => o,
            OrderEvent::Rejected(o) => o,
        }
    }

    pub fn into_inner(self) -> Order {
        match self {
            OrderEvent::New(o) => o,
            OrderEvent::PartiallyFilled(o) => o,
            OrderEvent::Filled(o) => o,
            OrderEvent::Canceled(o) => o,
            OrderEvent::Rejected(o) => o,
        }
    }
}

impl From<Order> for OrderEvent {
    fn from(order: Order) -> Self {
        match order.status {
            OrderStatus::PartiallyFilled => OrderEvent::PartiallyFilled(order),
            OrderStatus::Filled => OrderEvent::Filled(order),
            OrderStatus::Canceled => OrderEvent::Canceled(order),
            OrderStatus::Rejected => OrderEvent::Rejected(order),
            _ => OrderEvent::New(order),
        }
    }
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct Position {
    pub key: InstrumentKey,
    pub size: Decimal,
    pub entry_price: Decimal,
    pub unrealized_pnl: Decimal,
    pub leverage: Decimal,
    pub liquidation_price: Option<Decimal>,
}

impl Default for Position {
    fn default() -> Self {
        Self {
            key: InstrumentKey::default(),
            size: Decimal::ZERO,
            entry_price: Decimal::ZERO,
            unrealized_pnl: Decimal::ZERO,
            leverage: Decimal::ONE,
            liquidation_price: None,
        }
    }
}

impl Position {
    pub fn apply_fill(&mut self, side: OrderSide, fill_size: Decimal, fill_price: Decimal) {
        let old_size = self.size;
        let is_buy = side == OrderSide::Buy;

        let signed_fill_size = if is_buy { fill_size } else { -fill_size };
        let new_size = old_size + signed_fill_size;

        if new_size.is_zero() {
            self.entry_price = Decimal::ZERO;
        } else if old_size.is_zero() || old_size.signum() == signed_fill_size.signum() {
            let total_value = (old_size.abs() * self.entry_price) + (fill_size * fill_price);
            self.entry_price = total_value / new_size.abs();
        }

        self.size = new_size;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_instrument_key_display_and_parse() {
        let key = InstrumentKey {
            exchange: Exchange::Binance,
            pair: Pairs::BTCUSDT,
            instrument_type: InstrumentType::Swap,
        };

        let key_str = key.to_string();
        assert_eq!(key_str, "BTCUSDT-BN-SWAP");

        let parsed_key = InstrumentKey::from_str(&key_str).unwrap();
        assert_eq!(parsed_key, key);
    }

    #[test]
    fn test_instrument_key_parse_invalid() {
        assert!(InstrumentKey::from_str("BTCUSDT-BN").is_err());
        assert!(InstrumentKey::from_str("INVALID-BN-SWAP").is_err());
        assert!(InstrumentKey::from_str("BTCUSDT-INVALID-SWAP").is_err());
        assert!(InstrumentKey::from_str("BTCUSDT-BN-INVALID").is_err());
    }

    #[test]
    fn test_exchange_strum_properties() {
        assert_eq!(Exchange::Binance.short_code(), "BN");
        assert_eq!(Exchange::Binance.to_string(), "binance");
        assert_eq!(Exchange::from_str("BN").unwrap(), Exchange::Binance);
        assert_eq!(Exchange::from_str("binance").unwrap(), Exchange::Binance);
    }
}
