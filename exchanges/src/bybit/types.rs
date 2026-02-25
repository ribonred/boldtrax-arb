//! Bybit V5 API response types.
//!
//! All Bybit V5 responses are wrapped in:
//! ```json
//! {"retCode": 0, "retMsg": "OK", "result": {...}, "retExtInfo": {}, "time": 1234}
//! ```
//! Numbers are returned as JSON strings — `rust_decimal::Decimal` with string
//! deserialization handles this correctly.

use boldtrax_core::traits::SwapInstrumentMeta;
use boldtrax_core::types::{Currency, FundingInterval, Instrument, Pairs};
use boldtrax_core::{Exchange, InstrumentKey, InstrumentType};
use rust_decimal::Decimal;
use serde::Deserialize;

/// All Bybit V5 responses share this wrapper.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitApiResponse<T> {
    pub ret_code: i32,
    pub ret_msg: String,
    pub result: T,
    #[serde(default)]
    pub time: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitServerTime {
    pub time_second: String,
    pub time_nano: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitInstrumentsResult {
    pub category: String,
    pub list: Vec<BybitInstrumentInfo>,
    #[serde(default)]
    pub next_page_cursor: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitInstrumentInfo {
    pub symbol: String,
    pub contract_type: String,
    pub status: String,
    pub base_coin: String,
    pub quote_coin: String,
    pub settle_coin: String,
    pub launch_time: String,
    pub price_scale: String,
    pub leverage_filter: BybitLeverageFilter,
    pub price_filter: BybitPriceFilter,
    pub lot_size_filter: BybitLotSizeFilter,
    #[serde(default)]
    pub funding_interval: u32,
    #[serde(default)]
    pub unified_margin_trade: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitLeverageFilter {
    pub min_leverage: String,
    pub max_leverage: String,
    pub leverage_step: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitPriceFilter {
    pub min_price: String,
    pub max_price: String,
    pub tick_size: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitLotSizeFilter {
    pub max_order_qty: String,
    pub min_order_qty: String,
    pub qty_step: String,
    #[serde(default)]
    pub min_notional_value: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitTickerResult {
    pub category: String,
    pub list: Vec<BybitLinearTicker>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitLinearTicker {
    pub symbol: String,
    pub last_price: String,
    pub index_price: String,
    pub mark_price: String,
    pub funding_rate: String,
    pub next_funding_time: String,
    #[serde(default)]
    pub open_interest: String,
    #[serde(default)]
    pub open_interest_value: String,
    #[serde(default)]
    pub volume24h: String,
    #[serde(default)]
    pub turnover24h: String,
    #[serde(default)]
    pub bid1_price: String,
    #[serde(default)]
    pub ask1_price: String,
    #[serde(default)]
    pub bid1_size: String,
    #[serde(default)]
    pub ask1_size: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitFundingHistoryResult {
    pub category: String,
    pub list: Vec<BybitFundingRateEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitFundingRateEntry {
    pub symbol: String,
    pub funding_rate: String,
    pub funding_rate_timestamp: String,
}

#[derive(Debug, Deserialize)]
pub struct BybitOrderBookResult {
    pub s: String,
    pub b: Vec<[String; 2]>,
    pub a: Vec<[String; 2]>,
    /// Present in REST responses; absent in WS `data` payloads.
    #[serde(default)]
    pub ts: Option<u64>,
    pub u: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitPositionResult {
    pub category: String,
    pub list: Vec<BybitPositionEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitPositionEntry {
    pub symbol: String,
    pub side: String,
    pub size: String,
    pub avg_price: String,
    pub leverage: String,
    #[serde(default)]
    pub liq_price: String,
    pub unrealised_pnl: String,
    #[serde(default)]
    pub position_value: String,
    #[serde(default)]
    pub updated_time: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitWalletBalanceResult {
    pub list: Vec<BybitAccountEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitAccountEntry {
    pub account_type: String,
    #[serde(default)]
    pub total_equity: String,
    #[serde(default)]
    pub total_wallet_balance: String,
    #[serde(default)]
    pub total_available_balance: String,
    #[serde(default)]
    pub total_perp_upl: String,
    pub coin: Vec<BybitCoinBalance>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitCoinBalance {
    pub coin: String,
    pub wallet_balance: String,
    #[serde(default)]
    pub locked: String,
    #[serde(default)]
    pub equity: String,
    #[serde(default)]
    pub unrealised_pnl: String,
}

/// Response from POST /v5/position/set-leverage — empty result on success.
/// Bybit returns retCode=0 with an empty result object.
#[derive(Debug, Deserialize)]
pub struct BybitEmptyResult {}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitOrderResult {
    pub order_id: String,
    pub order_link_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitOrderListResult {
    pub category: String,
    pub list: Vec<BybitOrderEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitOrderEntry {
    pub symbol: String,
    pub order_id: String,
    pub order_link_id: String,
    pub side: String,
    pub order_type: String,
    pub price: String,
    pub qty: String,
    pub cum_exec_qty: String,
    pub avg_price: String,
    pub order_status: String,
    #[serde(default)]
    pub created_time: String,
    #[serde(default)]
    pub updated_time: String,
    #[serde(default)]
    pub reduce_only: bool,
}

/// Envelope for Bybit V5 public WS messages.
#[derive(Debug, Deserialize)]
pub struct BybitWsMessage {
    pub topic: String,
    #[serde(rename = "type")]
    pub msg_type: String,
    pub ts: u64,
    pub data: BybitWsOrderBookData,
}

#[derive(Debug, Deserialize)]
pub struct BybitWsOrderBookData {
    pub s: String,
    pub b: Vec<[String; 2]>,
    pub a: Vec<[String; 2]>,
    pub u: u64,
}

/// Subscribe/ping control message for Bybit WS.
#[derive(Debug, serde::Serialize)]
pub struct BybitWsControl {
    pub op: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
}

/// Top-level envelope for Bybit private WS messages.
/// Both control (auth/subscribe ack) and data messages share this shape.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitWsPrivateMessage {
    /// Topic name, e.g. "order", "position", "execution".
    /// Absent on control messages (auth, subscribe ack).
    #[serde(default)]
    pub topic: Option<String>,
    /// Present on control messages.
    #[serde(default)]
    pub op: Option<String>,
    /// Present on control messages.
    #[serde(default)]
    pub success: Option<bool>,
    /// Present on control messages.
    #[serde(default)]
    pub ret_msg: Option<String>,
    /// Data created timestamp (ms). Present on data messages.
    #[serde(default)]
    pub creation_time: Option<u64>,
    /// Raw data array. We parse into specific types depending on `topic`.
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

/// Bybit private WS order update fields (topic: "order").
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitWsOrderUpdate {
    pub symbol: String,
    pub order_id: String,
    pub order_link_id: String,
    pub side: String,
    pub order_type: String,
    pub price: String,
    pub qty: String,
    pub order_status: String,
    #[serde(default)]
    pub avg_price: String,
    #[serde(default)]
    pub cum_exec_qty: String,
    #[serde(default)]
    pub leaves_qty: String,
    #[serde(default)]
    pub reduce_only: bool,
    #[serde(default)]
    pub created_time: String,
    #[serde(default)]
    pub updated_time: String,
    #[serde(default)]
    pub category: String,
}

/// Bybit private WS position update fields (topic: "position").
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BybitWsPositionUpdate {
    pub symbol: String,
    pub side: String,
    pub size: String,
    #[serde(default)]
    pub entry_price: String,
    #[serde(default)]
    pub leverage: String,
    #[serde(default)]
    pub liq_price: String,
    #[serde(default)]
    pub unrealised_pnl: String,
    #[serde(default)]
    pub category: String,
}

pub struct BybitSwapMeta {
    pub symbol: String,
    pub base_currency: Currency,
    pub quote_currency: Currency,
    pub pairs: Pairs,
    pub tick_size: Decimal,
    pub lot_size: Decimal,
    pub min_notional: Option<Decimal>,
    pub is_trading_allowed: bool,
    pub funding_interval: Option<FundingInterval>,
}

impl BybitSwapMeta {
    pub fn try_new(info: &BybitInstrumentInfo) -> Option<Self> {
        // Only process LinearPerpetual contracts
        if info.contract_type != "LinearPerpetual" {
            return None;
        }

        let pairs = format!("{}{}", info.base_coin, info.quote_coin)
            .parse::<Pairs>()
            .ok()?;
        let base_currency = info.base_coin.parse::<Currency>().ok()?;
        let quote_currency = info.quote_coin.parse::<Currency>().ok()?;

        let tick_size = info.price_filter.tick_size.parse::<Decimal>().ok()?;
        let lot_size = info.lot_size_filter.qty_step.parse::<Decimal>().ok()?;
        let min_notional = info
            .lot_size_filter
            .min_notional_value
            .parse::<Decimal>()
            .ok()
            .filter(|v| !v.is_zero());

        if tick_size.is_zero() || lot_size.is_zero() {
            return None;
        }

        // funding_interval is in minutes from Bybit
        let funding_interval = match info.funding_interval {
            60 => Some(FundingInterval::EveryHour),
            240 => Some(FundingInterval::Every4Hours),
            480 => Some(FundingInterval::Every8Hours),
            _ if info.funding_interval > 0 => Some(FundingInterval::Every8Hours), // fallback
            _ => None,
        };

        Some(Self {
            symbol: info.symbol.clone(),
            base_currency,
            quote_currency,
            pairs,
            tick_size,
            lot_size,
            min_notional,
            is_trading_allowed: info.status == "Trading",
            funding_interval,
        })
    }
}

impl boldtrax_core::traits::BaseInstrumentMeta for BybitSwapMeta {
    fn symbol(&self) -> &str {
        &self.symbol
    }

    fn base_currency(&self) -> Currency {
        self.base_currency
    }

    fn quote_currency(&self) -> Currency {
        self.quote_currency
    }

    fn pairs(&self) -> Pairs {
        self.pairs
    }

    fn is_derivative(&self) -> bool {
        true
    }

    fn is_trading_allowed(&self) -> bool {
        self.is_trading_allowed
    }

    fn tick_size(&self) -> Decimal {
        self.tick_size
    }

    fn lot_size(&self) -> Decimal {
        self.lot_size
    }

    fn min_notional(&self) -> Option<Decimal> {
        self.min_notional
    }
}

impl SwapInstrumentMeta for BybitSwapMeta {
    fn contract_size(&self) -> Option<Decimal> {
        Some(Decimal::ONE)
    }

    fn funding_interval(&self) -> Option<FundingInterval> {
        self.funding_interval
    }
}

impl From<BybitSwapMeta> for Instrument {
    fn from(meta: BybitSwapMeta) -> Self {
        Instrument {
            key: InstrumentKey {
                exchange: Exchange::Bybit,
                pair: meta.pairs,
                instrument_type: InstrumentType::Swap,
            },
            exchange_symbol: meta.symbol.clone(),
            exchange_id: meta.symbol,
            tick_size: meta.tick_size,
            lot_size: meta.lot_size,
            min_notional: meta.min_notional,
            contract_size: Some(Decimal::ONE),
            multiplier: Decimal::ONE,
            funding_interval: meta.funding_interval,
        }
    }
}
