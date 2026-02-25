use boldtrax_core::traits::SwapInstrumentMeta;
use boldtrax_core::types::{Currency, FundingInterval, Instrument, Pairs};
use boldtrax_core::{Exchange, InstrumentKey, InstrumentType};
use rust_decimal::Decimal;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsterServerTime {
    pub server_time: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsterExchangeInfo {
    pub server_time: u64,
    pub symbols: Vec<AsterSymbolInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsterSymbolInfo {
    pub symbol: String,
    pub pair: String,
    pub contract_type: String,
    pub status: String,
    pub base_asset: String,
    pub quote_asset: String,
    pub margin_asset: String,
    pub filters: Vec<AsterFilter>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "filterType", rename_all = "camelCase")]
pub enum AsterFilter {
    #[serde(rename = "PRICE_FILTER")]
    PriceFilter {
        #[serde(rename = "tickSize")]
        tick_size: Decimal,
    },
    #[serde(rename = "LOT_SIZE")]
    LotSize {
        #[serde(rename = "stepSize")]
        step_size: Decimal,
    },
    #[serde(rename = "MIN_NOTIONAL")]
    MinNotional {
        #[serde(default)]
        notional: Decimal,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsterFundingInfo {
    pub symbol: String,
    pub funding_interval_hours: u8,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsterPremiumIndex {
    pub symbol: String,
    pub mark_price: Decimal,
    pub last_funding_rate: Decimal,
    pub next_funding_time: u64,
    pub time: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsterFundingRateHistory {
    pub symbol: String,
    pub funding_rate: Decimal,
    pub funding_time: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsterPartialDepth {
    #[serde(alias = "e")]
    pub event_type: Option<String>,
    #[serde(alias = "E")]
    pub message_time: Option<u64>,
    #[serde(alias = "T")]
    pub transaction_time: Option<u64>,
    #[serde(alias = "s")]
    pub symbol: Option<String>,
    #[serde(alias = "U")]
    pub first_update_id: Option<u64>,
    #[serde(alias = "u", alias = "lastUpdateId")]
    pub final_update_id: Option<u64>,
    #[serde(alias = "pu")]
    pub previous_final_update_id: Option<u64>,
    #[serde(alias = "b")]
    pub bids: Vec<AsterDepthLevel>,
    #[serde(alias = "a")]
    pub asks: Vec<AsterDepthLevel>,
}

#[derive(Debug, Deserialize)]
pub struct AsterDepthLevel(pub Decimal, pub Decimal);

pub struct AsterSwapMeta {
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

impl AsterSwapMeta {
    pub fn try_new(info: &AsterSymbolInfo, funding_hours: Option<u8>) -> Option<Self> {
        let pairs = info.pair.parse::<Pairs>().ok()?;
        let base_currency = info.base_asset.parse::<Currency>().ok()?;
        let quote_currency = info.quote_asset.parse::<Currency>().ok()?;

        let mut tick_size = Decimal::ZERO;
        let mut lot_size = Decimal::ZERO;
        let mut min_notional = None;

        for filter in &info.filters {
            match filter {
                AsterFilter::PriceFilter { tick_size: ts } => tick_size = *ts,
                AsterFilter::LotSize { step_size: ss } => lot_size = *ss,
                AsterFilter::MinNotional { notional: mn } => min_notional = Some(*mn),
                _ => {}
            }
        }

        if tick_size.is_zero() || lot_size.is_zero() {
            return None;
        }

        let funding_interval = funding_hours.and_then(|h| match h {
            1 => Some(FundingInterval::EveryHour),
            4 => Some(FundingInterval::Every4Hours),
            8 => Some(FundingInterval::Every8Hours),
            _ => None,
        });

        Some(Self {
            symbol: info.symbol.clone(),
            base_currency,
            quote_currency,
            pairs,
            tick_size,
            lot_size,
            min_notional,
            is_trading_allowed: info.status == "TRADING",
            funding_interval,
        })
    }
}

impl boldtrax_core::traits::BaseInstrumentMeta for AsterSwapMeta {
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

impl SwapInstrumentMeta for AsterSwapMeta {
    fn contract_size(&self) -> Option<Decimal> {
        Some(Decimal::ONE)
    }

    fn funding_interval(&self) -> Option<FundingInterval> {
        self.funding_interval
    }
}

impl From<AsterSwapMeta> for Instrument {
    fn from(meta: AsterSwapMeta) -> Self {
        Instrument {
            key: InstrumentKey {
                exchange: Exchange::Aster,
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsterFuturesBalance {
    pub account_alias: Option<String>,
    pub asset: String,
    pub balance: String,
    pub cross_un_pnl: String,
    #[serde(alias = "crossWallet")]
    pub cross_wallet_balance: Option<String>,
    pub available_balance: String,
    pub update_time: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsterLeverageResponse {
    pub symbol: String,
    pub leverage: u32,
    pub max_notional_value: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsterFuturesPositionRisk {
    pub symbol: String,
    #[serde(default)]
    pub position_side: String,
    pub position_amt: String,
    pub entry_price: String,
    pub un_realized_profit: String,
    pub liquidation_price: String,
    pub mark_price: String,
    pub update_time: i64,
}

// ──────────────────────────────────────────────────────────────────
// Order / execution REST types
// ──────────────────────────────────────────────────────────────────

/// Response from `POST /fapi/v1/listenKey`.
#[derive(Debug, Deserialize)]
pub struct AsterListenKeyResponse {
    #[serde(rename = "listenKey")]
    pub listen_key: String,
}

/// Unified REST response for order placement, queries, and cancellations.
///
/// Maps to `POST /fapi/v1/order`, `GET /fapi/v1/order`, and
/// `DELETE /fapi/v1/order`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsterOrderResponse {
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
    /// Last update time (ms). Present on query and cancel responses.
    #[serde(default)]
    pub update_time: Option<i64>,
    /// Average fill price – present on all futures order responses.
    #[serde(default)]
    pub avg_price: Option<String>,
}

// ──────────────────────────────────────────────────────────────────
// User-data WebSocket types
// ──────────────────────────────────────────────────────────────────

/// Envelope for events on the Aster futures user-data stream.
///
/// Aster uses the same format as Binance Futures:
/// `{"e":"ORDER_TRADE_UPDATE","o":{...}}`.
#[derive(Debug, Deserialize)]
#[serde(tag = "e")]
pub enum AsterFuturesUserDataEvent {
    #[serde(rename = "ORDER_TRADE_UPDATE")]
    OrderTradeUpdate {
        #[serde(rename = "o")]
        order: Box<AsterFuturesOrderTradeUpdate>,
    },
    #[serde(rename = "ACCOUNT_UPDATE")]
    AccountUpdate {
        #[serde(rename = "a")]
        update: AsterFuturesAccountUpdate,
    },
    #[serde(other)]
    Other,
}

/// Payload inside an `ACCOUNT_UPDATE` user-data event.
#[derive(Debug, Deserialize)]
pub struct AsterFuturesAccountUpdate {
    /// Changed positions (only positions that changed are included).
    #[serde(rename = "P")]
    pub positions: Vec<AsterFuturesWsPosition>,
}

/// A single position entry inside an `ACCOUNT_UPDATE` event.
#[derive(Debug, Deserialize)]
pub struct AsterFuturesWsPosition {
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
}

/// Order execution report inside `ORDER_TRADE_UPDATE`.
#[derive(Debug, Deserialize)]
pub struct AsterFuturesOrderTradeUpdate {
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "i")]
    pub order_id: u64,
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
