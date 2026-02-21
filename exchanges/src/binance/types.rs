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
