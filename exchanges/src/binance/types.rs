//! Binance API response types (internal, only fields we care about)

use boldtrax_core::traits::{BaseInstrumentMeta, SpotInstrumentMeta, SwapInstrumentMeta};
use boldtrax_core::types::{
    Currency, Exchange, FundingInterval, Instrument, InstrumentKey, InstrumentType, Pairs,
};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::str::FromStr;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceServerTime {
    pub server_time: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceFuturesExchangeInfo {
    pub symbols: Vec<BinanceFuturesSymbol>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceFuturesSymbol {
    pub symbol: String,
    pub status: String,
    pub contract_type: String,
    pub base_asset: String,
    pub quote_asset: String,
    pub filters: Vec<BinanceFilter>,
    #[serde(default)]
    pub contract_size: Option<Decimal>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceSpotExchangeInfo {
    pub symbols: Vec<BinanceSpotSymbol>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceSpotSymbol {
    pub symbol: String,
    pub status: String,
    pub base_asset: String,
    pub quote_asset: String,
    pub filters: Vec<BinanceFilter>,
    #[serde(default)]
    pub is_spot_trading_allowed: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "filterType")]
pub enum BinanceFilter {
    #[serde(rename = "PRICE_FILTER")]
    PriceFilter {
        #[serde(rename = "tickSize")]
        tick_size: String,
    },
    #[serde(rename = "LOT_SIZE")]
    LotSize {
        #[serde(rename = "stepSize")]
        step_size: String,
    },
    #[serde(rename = "MIN_NOTIONAL")]
    MinNotional {
        #[serde(default)]
        notional: Option<String>,
        #[serde(rename = "minNotional", default)]
        min_notional: Option<String>,
    },
    #[serde(rename = "NOTIONAL")]
    Notional {
        #[serde(rename = "minNotional", default)]
        min_notional: Option<String>,
    },
    #[serde(other)]
    Other,
}

fn extract_tick_size(filters: &[BinanceFilter]) -> Decimal {
    for filter in filters {
        if let BinanceFilter::PriceFilter { tick_size } = filter {
            return Decimal::from_str(tick_size).unwrap_or(Decimal::ONE);
        }
    }
    Decimal::ONE
}

fn extract_lot_size(filters: &[BinanceFilter]) -> Decimal {
    for filter in filters {
        if let BinanceFilter::LotSize { step_size } = filter {
            return Decimal::from_str(step_size).unwrap_or(Decimal::ONE);
        }
    }
    Decimal::ONE
}

fn extract_min_notional(filters: &[BinanceFilter]) -> Option<Decimal> {
    for filter in filters {
        match filter {
            BinanceFilter::MinNotional {
                notional,
                min_notional,
            } => {
                let val = notional.as_ref().or(min_notional.as_ref())?;
                return Decimal::from_str(val).ok();
            }
            BinanceFilter::Notional {
                min_notional: Some(val),
            } => {
                return Decimal::from_str(val).ok();
            }
            BinanceFilter::Notional { min_notional: None } => {}
            _ => {}
        }
    }
    None
}

fn hours_to_funding_interval(hours: u32) -> FundingInterval {
    match hours {
        1 => FundingInterval::EveryHour,
        4 => FundingInterval::Every4Hours,
        8 => FundingInterval::Every8Hours,
        h => FundingInterval::CustomSeconds(h * 3600),
    }
}

pub struct BinanceSwapMeta<'a> {
    context: &'a BinanceFuturesSymbol,
    funding_interval_hours: Option<u32>,
    base: Currency,
    quote: Currency,
    pair: Pairs,
}

impl<'a> BinanceSwapMeta<'a> {
    pub fn try_new(
        context: &'a BinanceFuturesSymbol,
        funding_interval_hours: Option<u32>,
    ) -> Option<Self> {
        let base = Currency::from_str(&context.base_asset).ok()?;
        let quote = Currency::from_str(&context.quote_asset).ok()?;
        let pair = Pairs::from_currencies(base, quote)?;
        Some(Self {
            context,
            funding_interval_hours,
            base,
            quote,
            pair,
        })
    }
}

impl BaseInstrumentMeta for BinanceSwapMeta<'_> {
    fn symbol(&self) -> &str {
        &self.context.symbol
    }

    fn base_currency(&self) -> Currency {
        self.base
    }

    fn quote_currency(&self) -> Currency {
        self.quote
    }

    fn pairs(&self) -> Pairs {
        self.pair
    }

    fn is_derivative(&self) -> bool {
        self.context.contract_type == "PERPETUAL"
    }

    fn is_trading_allowed(&self) -> bool {
        self.context.status == "TRADING"
    }

    fn tick_size(&self) -> Decimal {
        extract_tick_size(&self.context.filters)
    }

    fn lot_size(&self) -> Decimal {
        extract_lot_size(&self.context.filters)
    }

    fn min_notional(&self) -> Option<Decimal> {
        extract_min_notional(&self.context.filters)
    }
}

impl SwapInstrumentMeta for BinanceSwapMeta<'_> {
    fn contract_size(&self) -> Option<Decimal> {
        self.context.contract_size
    }

    fn funding_interval(&self) -> Option<FundingInterval> {
        self.funding_interval_hours.map(hours_to_funding_interval)
    }
}

impl From<BinanceSwapMeta<'_>> for Instrument {
    fn from(meta: BinanceSwapMeta<'_>) -> Self {
        Instrument {
            key: InstrumentKey {
                exchange: Exchange::Binance,
                pair: meta.pair,
                instrument_type: InstrumentType::Swap,
            },
            exchange_symbol: meta.context.symbol.clone(),
            tick_size: extract_tick_size(&meta.context.filters),
            lot_size: extract_lot_size(&meta.context.filters),
            min_notional: extract_min_notional(&meta.context.filters),
            contract_size: meta.context.contract_size,
            multiplier: Decimal::ONE,
            funding_interval: meta.funding_interval_hours.map(hours_to_funding_interval),
            exchange_id: meta.context.symbol.clone(),
        }
    }
}

pub struct BinanceSpotMeta<'a> {
    context: &'a BinanceSpotSymbol,
    base: Currency,
    quote: Currency,
    pair: Pairs,
}

impl<'a> BinanceSpotMeta<'a> {
    pub fn try_new(context: &'a BinanceSpotSymbol) -> Option<Self> {
        let base = Currency::from_str(&context.base_asset).ok()?;
        let quote = Currency::from_str(&context.quote_asset).ok()?;
        let pair = Pairs::from_currencies(base, quote)?;
        Some(Self {
            context,
            base,
            quote,
            pair,
        })
    }
}

impl BaseInstrumentMeta for BinanceSpotMeta<'_> {
    fn symbol(&self) -> &str {
        &self.context.symbol
    }

    fn base_currency(&self) -> Currency {
        self.base
    }

    fn quote_currency(&self) -> Currency {
        self.quote
    }

    fn pairs(&self) -> Pairs {
        self.pair
    }

    fn is_derivative(&self) -> bool {
        false
    }

    fn is_trading_allowed(&self) -> bool {
        self.context.status == "TRADING" && self.context.is_spot_trading_allowed
    }

    fn tick_size(&self) -> Decimal {
        extract_tick_size(&self.context.filters)
    }

    fn lot_size(&self) -> Decimal {
        extract_lot_size(&self.context.filters)
    }

    fn min_notional(&self) -> Option<Decimal> {
        extract_min_notional(&self.context.filters)
    }
}

impl SpotInstrumentMeta for BinanceSpotMeta<'_> {}

impl From<BinanceSpotMeta<'_>> for Instrument {
    fn from(meta: BinanceSpotMeta<'_>) -> Self {
        Instrument {
            key: InstrumentKey {
                exchange: Exchange::Binance,
                pair: meta.pair,
                instrument_type: InstrumentType::Spot,
            },
            exchange_symbol: meta.context.symbol.clone(),
            tick_size: extract_tick_size(&meta.context.filters),
            lot_size: extract_lot_size(&meta.context.filters),
            min_notional: extract_min_notional(&meta.context.filters),
            contract_size: None,
            multiplier: Decimal::ONE,
            funding_interval: None,
            exchange_id: meta.context.symbol.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinancePremiumIndex {
    pub symbol: String,
    pub mark_price: String,
    pub index_price: String,
    pub last_funding_rate: String,
    pub next_funding_time: i64,
    #[serde(default)]
    pub interest_rate: Option<String>,
    pub time: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceFundingRateHistory {
    pub funding_rate: String,
    pub funding_time: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceFundingInfo {
    pub symbol: String,
    pub funding_interval_hours: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceSpotAccount {
    pub balances: Vec<BinanceSpotBalance>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceSpotBalance {
    pub asset: String,
    pub free: String,
    pub locked: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceFuturesBalance {
    pub account_alias: Option<String>,
    pub asset: String,
    pub balance: String,
    pub cross_un_pnl: String,
    #[serde(alias = "crossWallet")]
    pub cross_wallet_balance: Option<String>,
    pub available_balance: String,
    pub update_time: i64,
}

// ──────────────────────────────────────────────────────────────────
// Order-book types
// ──────────────────────────────────────────────────────────────────

/// REST response for `GET /fapi/v1/depth` and `GET /api/v3/depth`.
/// Each level is `[price_str, qty_str]`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinancePartialDepth {
    /// Exchange-assigned last update ID (used for sequencing).
    pub last_update_id: u64,
    /// Up to 5 bid levels: [[price, qty], ...]
    pub bids: Vec<[String; 2]>,
    /// Up to 5 ask levels: [[price, qty], ...]
    pub asks: Vec<[String; 2]>,
}

/// WebSocket stream event from `<symbol>@depth5@100ms`.
/// Handles both Binance futures (fields `b`/`a`/`E`/`T`) and spot
/// (fields `bids`/`asks`, no timestamp fields).
///
/// Futures combined-stream `data` carries **two** timestamp fields:
/// - `E` — event time (when the event was generated by the engine)
/// - `T` — transaction time (when the matching engine processed it)
///
/// Aliasing both to one serde field causes a "duplicate field" error;
/// they are kept as separate struct fields.
#[derive(Debug, Deserialize)]
pub struct BinanceWsDepthEvent {
    /// Event publish time (ms). Present in futures depthUpdate events as `E`.
    /// Absent in spot partial-depth events (defaults to 0).
    #[serde(rename = "E", default)]
    pub event_time_ms: i64,
    /// Transaction / matching-engine time (ms). Present in futures
    /// depthUpdate events as `T`; absent in spot (defaults to 0).
    /// More precise than `event_time_ms` when available.
    #[serde(rename = "T", default)]
    pub transaction_time_ms: i64,
    /// Bid levels: futures field `b`, spot field `bids`.
    #[serde(rename = "b", alias = "bids")]
    pub bids: Vec<[String; 2]>,
    /// Ask levels: futures field `a`, spot field `asks`.
    #[serde(rename = "a", alias = "asks")]
    pub asks: Vec<[String; 2]>,
}

/// Wrapper produced by Binance combined-stream endpoints.
/// Example: `{"stream":"btcusdt@depth5@100ms","data":{...}}`
#[derive(Debug, Deserialize)]
pub struct BinanceCombinedStreamMsg {
    /// Stream name, e.g. `btcusdt@depth5@100ms`.
    pub stream: String,
    /// Raw event data — deserialised separately based on stream type.
    pub data: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct BinanceListenKeyResponse {
    #[serde(rename = "listenKey")]
    pub listen_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "e")]
pub enum BinanceSpotUserDataEvent {
    #[serde(rename = "executionReport")]
    ExecutionReport(Box<BinanceSpotExecutionReport>),
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct BinanceSpotExecutionReport {
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "c")]
    pub client_order_id: String,
    #[serde(rename = "S")]
    pub side: String,
    #[serde(rename = "o")]
    pub order_type: String,
    #[serde(rename = "X")]
    pub order_status: String,
    #[serde(rename = "q")]
    pub order_quantity: String,
    #[serde(rename = "p")]
    pub order_price: String,
    #[serde(rename = "z")]
    pub cumulative_filled_quantity: String,
    #[serde(rename = "L")]
    pub last_executed_price: String,
    #[serde(rename = "O")]
    pub order_creation_time: i64,
    #[serde(rename = "T")]
    pub transaction_time: i64,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "e")]
pub enum BinanceFuturesUserDataEvent {
    #[serde(rename = "ORDER_TRADE_UPDATE")]
    OrderTradeUpdate {
        #[serde(rename = "o")]
        order: Box<BinanceFuturesOrderTradeUpdate>,
    },
    #[serde(rename = "ACCOUNT_UPDATE")]
    AccountUpdate {
        #[serde(rename = "a")]
        update: BinanceFuturesAccountUpdate,
    },
    #[serde(other)]
    Other,
}

/// Payload inside an `ACCOUNT_UPDATE` user-data event.
#[derive(Debug, Deserialize)]
pub struct BinanceFuturesAccountUpdate {
    /// Changed positions (only positions that changed are included).
    #[serde(rename = "P")]
    pub positions: Vec<BinanceFuturesWsPosition>,
}

/// A single position entry inside an `ACCOUNT_UPDATE` event.
#[derive(Debug, Deserialize)]
pub struct BinanceFuturesWsPosition {
    /// Symbol, e.g. `"BTCUSDT"`.
    #[serde(rename = "s")]
    pub symbol: String,
    /// Signed position amount (positive = long, negative = short, zero = closed).
    #[serde(rename = "pa")]
    pub position_amt: String,
    /// Entry price.
    #[serde(rename = "ep")]
    pub entry_price: String,
    /// Unrealized PnL.
    #[serde(rename = "up")]
    pub unrealized_profit: String,
    /// `"BOTH"` in one-way mode; `"LONG"` / `"SHORT"` in hedge mode.
    #[serde(rename = "ps")]
    pub position_side: String,
}

#[derive(Debug, Deserialize)]
pub struct BinanceFuturesOrderTradeUpdate {
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "c")]
    pub client_order_id: String,
    #[serde(rename = "S")]
    pub side: String,
    #[serde(rename = "o")]
    pub order_type: String,
    #[serde(rename = "X")]
    pub order_status: String,
    #[serde(rename = "q")]
    pub original_quantity: String,
    #[serde(rename = "p")]
    pub original_price: String,
    #[serde(rename = "z")]
    pub order_filled_accumulated_quantity: String,
    #[serde(rename = "ap")]
    pub average_price: String,
    #[serde(rename = "T")]
    pub order_trade_time: i64,
}

// ─── Binance WebSocket API (wss://ws-api.binance.com) ────────────────────────

/// Response ack sent by Binance after `userDataStream.subscribe.signature`.
/// ```json
/// {"id":"<uuid>","status":200,"result":{"subscriptionId":0},"rateLimits":[...]}
/// ```
#[derive(Debug, Deserialize)]
pub struct BinanceWsApiResponse {
    pub status: u16,
    pub result: Option<BinanceWsApiSubscribeResult>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceWsApiSubscribeResult {
    pub subscription_id: u64,
}

/// Wrapper for event frames delivered over the WebSocket API after subscribing.
/// ```json
/// {"subscriptionId":0,"event":{...}}
/// ```
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceWsApiEnvelope {
    pub subscription_id: u64,
    pub event: serde_json::Value,
}

// ──────────────────────────────────────────────────────────────────
// Order REST types
// ──────────────────────────────────────────────────────────────────

/// Unified REST response for order placement, queries, and cancellations.
///
/// Works for both Spot (`/api/v3/order`) and Futures (`/fapi/v1/order`).
/// Fields that are only present on one side are made optional.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceOrderResponse {
    pub symbol: String,
    pub order_id: i64,
    pub client_order_id: String,
    /// Price string; `"0"` for market orders.
    pub price: String,
    /// Original order quantity.
    pub orig_qty: String,
    /// Cumulative filled quantity.
    pub executed_qty: String,
    pub status: String,
    #[serde(rename = "type")]
    pub order_type: String,
    pub side: String,
    /// Order creation time (ms). Present on query/cancel responses.
    #[serde(default)]
    pub time: Option<i64>,
    pub update_time: i64,
    /// Average fill price – Futures field (`avgPrice`).
    #[serde(default)]
    pub avg_price: Option<String>,
    /// Cumulative quote asset traded – Spot field (`cummulativeQuoteQty`).
    /// Used to compute the average fill price for spot orders.
    #[serde(default)]
    pub cummulative_quote_qty: Option<String>,
}

// ──────────────────────────────────────────────────────────────────
// Position REST types
// ──────────────────────────────────────────────────────────────────

/// Single entry from `GET /fapi/v3/positionRisk`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceFuturesPositionRisk {
    pub symbol: String,
    /// `"BOTH"` in one-way mode; `"LONG"` / `"SHORT"` in hedge mode.
    #[serde(default)]
    pub position_side: String,
    /// Signed position size (negative = short).
    pub position_amt: String,
    pub entry_price: String,
    /// `unRealizedProfit` in the v3 response (capital R).
    /// With `camelCase`, the field name `un_realized_profit` serialises to `unRealizedProfit`.
    pub un_realized_profit: String,
    /// `"0"` when there is no open position.
    pub liquidation_price: String,
    pub mark_price: String,
    pub update_time: i64,
}
