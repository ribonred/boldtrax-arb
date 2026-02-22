//! Mappers to convert Binance types to core types

use boldtrax_core::manager::types::{
    AccountModel, AccountPartitionRef, AccountSnapshot, BalanceView, CollateralScope,
    PartitionKind, WsPositionPatch,
};
use boldtrax_core::types::{
    Exchange, FundingInterval, FundingRatePoint, FundingRateSeries, FundingRateSnapshot,
    InstrumentKey, InstrumentType, OrderBookSnapshot, Pairs, PriceLevel,
};
use chrono::{DateTime, TimeZone, Utc};
use rust_decimal::Decimal;
use std::str::FromStr;

use crate::binance::types::{
    BinanceFundingRateHistory, BinanceFuturesBalance, BinancePartialDepth, BinancePremiumIndex,
    BinanceSpotBalance, BinanceWsDepthEvent,
};
use crate::binance::types::{BinanceFuturesOrderTradeUpdate, BinanceSpotExecutionReport};
use crate::binance::types::{
    BinanceFuturesPositionRisk, BinanceFuturesWsPosition, BinanceOrderResponse,
};
use boldtrax_core::types::Position;
use boldtrax_core::types::{Order, OrderEvent, OrderRequest, OrderSide, OrderStatus, OrderType};

pub fn pairs_to_symbol(pair: Pairs) -> String {
    format!("{}{}", pair.base(), pair.quote())
}

pub fn ms_to_datetime(ms: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms).single()
}

pub fn premium_index_to_snapshot(
    index: &BinancePremiumIndex,
    pair: Pairs,
    interval: FundingInterval,
) -> Option<FundingRateSnapshot> {
    let key = InstrumentKey {
        exchange: Exchange::Binance,
        pair,
        instrument_type: InstrumentType::Swap,
    };

    Some(FundingRateSnapshot {
        key,
        funding_rate: Decimal::from_str(&index.last_funding_rate).ok()?,
        mark_price: Decimal::from_str(&index.mark_price).ok()?,
        index_price: Decimal::from_str(&index.index_price).ok()?,
        interest_rate: index
            .interest_rate
            .as_ref()
            .and_then(|r| Decimal::from_str(r).ok()),
        next_funding_time_utc: ms_to_datetime(index.next_funding_time)?,
        next_funding_time_ms: index.next_funding_time,
        event_time_utc: ms_to_datetime(index.time)?,
        event_time_ms: index.time,
        interval,
    })
}

pub fn funding_history_to_series(
    history: &[BinanceFundingRateHistory],
    pair: Pairs,
    interval: FundingInterval,
) -> FundingRateSeries {
    let key = InstrumentKey {
        exchange: Exchange::Binance,
        pair,
        instrument_type: InstrumentType::Swap,
    };

    let points: Vec<FundingRatePoint> = history
        .iter()
        .filter_map(|h| {
            Some(FundingRatePoint {
                funding_rate: Decimal::from_str(&h.funding_rate).ok()?,
                event_time_utc: ms_to_datetime(h.funding_time)?,
                event_time_ms: h.funding_time,
            })
        })
        .collect();

    FundingRateSeries {
        key,
        interval,
        points,
    }
}

pub fn binance_spot_partition() -> AccountPartitionRef {
    AccountPartitionRef {
        exchange: Exchange::Binance,
        kind: PartitionKind::Spot,
        account_id: Some("spot".to_string()),
    }
}

pub fn binance_futures_partition() -> AccountPartitionRef {
    AccountPartitionRef {
        exchange: Exchange::Binance,
        kind: PartitionKind::Swap,
        account_id: Some("futures".to_string()),
    }
}

pub fn spot_balance_to_view(
    balance: &BinanceSpotBalance,
    as_of_utc: DateTime<Utc>,
) -> Option<BalanceView> {
    let asset = balance.asset.parse().ok()?;
    let free = Decimal::from_str(&balance.free).ok()?;
    let locked = Decimal::from_str(&balance.locked).ok()?;
    let total = free + locked;

    Some(BalanceView {
        exchange: Exchange::Binance,
        partition: Some(binance_spot_partition()),
        asset,
        total,
        free,
        locked,
        borrowed: Decimal::ZERO,
        unrealized_pnl: Decimal::ZERO,
        collateral_scope: CollateralScope::PartitionOnly,
        as_of_utc,
    })
}

pub fn futures_balance_to_view(
    balance: &BinanceFuturesBalance,
    as_of_utc: DateTime<Utc>,
) -> Option<BalanceView> {
    let asset = balance.asset.parse().ok()?;
    let total = Decimal::from_str(&balance.balance).ok()?;
    let free = Decimal::from_str(&balance.available_balance).ok()?;
    let locked = (total - free).max(Decimal::ZERO);
    let unrealized_pnl = Decimal::from_str(&balance.cross_un_pnl).ok()?;

    Some(BalanceView {
        exchange: Exchange::Binance,
        partition: Some(binance_futures_partition()),
        asset,
        total,
        free,
        locked,
        borrowed: Decimal::ZERO,
        unrealized_pnl,
        collateral_scope: CollateralScope::PartitionOnly,
        as_of_utc,
    })
}

pub fn build_segmented_account_snapshot(
    include_spot: bool,
    include_futures: bool,
    spot_balances: &[BinanceSpotBalance],
    futures_balances: &[BinanceFuturesBalance],
    as_of_utc: DateTime<Utc>,
) -> AccountSnapshot {
    let mut partitions = Vec::new();
    if include_spot {
        partitions.push(binance_spot_partition());
    }
    if include_futures {
        partitions.push(binance_futures_partition());
    }

    let mut balances: Vec<BalanceView> = spot_balances
        .iter()
        .filter_map(|b| spot_balance_to_view(b, as_of_utc))
        .collect();

    balances.extend(
        futures_balances
            .iter()
            .filter_map(|b| futures_balance_to_view(b, as_of_utc)),
    );

    AccountSnapshot {
        exchange: Exchange::Binance,
        model: AccountModel::Segmented,
        partitions,
        balances,
        as_of_utc,
    }
}

// ──────────────────────────────────────────────────────────────────
// Order-book mappers
// ──────────────────────────────────────────────────────────────────

/// Parse a raw `[price_str, qty_str]` pair from the Binance API into a
/// `PriceLevel`. Returns `None` if either field fails to parse.
pub fn raw_level_to_price_level(raw: &[String; 2]) -> Option<PriceLevel> {
    let price = Decimal::from_str(&raw[0]).ok()?;
    let quantity = Decimal::from_str(&raw[1]).ok()?;
    Some(PriceLevel { price, quantity })
}

/// Convert a REST partial-depth response into an `OrderBookSnapshot`.
///
/// Returns `None` if any level fails to parse or the timestamp is invalid.
pub fn partial_depth_to_order_book(
    depth: &BinancePartialDepth,
    key: InstrumentKey,
    timestamp_ms: i64,
) -> Option<OrderBookSnapshot> {
    let bids: Vec<PriceLevel> = depth
        .bids
        .iter()
        .filter_map(raw_level_to_price_level)
        .collect();
    let asks: Vec<PriceLevel> = depth
        .asks
        .iter()
        .filter_map(raw_level_to_price_level)
        .collect();
    let timestamp_utc = ms_to_datetime(timestamp_ms)?;
    Some(OrderBookSnapshot::new(
        key,
        bids,
        asks,
        timestamp_utc,
        timestamp_ms,
    ))
}

/// Convert a WebSocket depth-stream event into an `OrderBookSnapshot`.
///
/// Timestamp priority: `T` (transaction time, most precise) → `E` (event time)
/// → wall-clock (spot partial-depth events carry no timestamp fields).
/// Returns `None` if any level fails to parse.
pub fn ws_depth_event_to_order_book(
    event: &BinanceWsDepthEvent,
    key: InstrumentKey,
) -> Option<OrderBookSnapshot> {
    let bids: Vec<PriceLevel> = event
        .bids
        .iter()
        .filter_map(raw_level_to_price_level)
        .collect();
    let asks: Vec<PriceLevel> = event
        .asks
        .iter()
        .filter_map(raw_level_to_price_level)
        .collect();
    // Prefer transaction time (T) → event time (E) → wall-clock.
    let (timestamp_utc, timestamp_ms) = if event.transaction_time_ms != 0 {
        let ts = ms_to_datetime(event.transaction_time_ms)?;
        (ts, event.transaction_time_ms)
    } else if event.event_time_ms != 0 {
        let ts = ms_to_datetime(event.event_time_ms)?;
        (ts, event.event_time_ms)
    } else {
        let now = Utc::now();
        let ms = now.timestamp_millis();
        (now, ms)
    };
    Some(OrderBookSnapshot::new(
        key,
        bids,
        asks,
        timestamp_utc,
        timestamp_ms,
    ))
}

pub fn spot_execution_to_order_event(
    report: &BinanceSpotExecutionReport,
    key: InstrumentKey,
) -> Option<OrderEvent> {
    let side = match report.side.as_str() {
        "BUY" => OrderSide::Buy,
        "SELL" => OrderSide::Sell,
        _ => return None,
    };

    let order_type = match report.order_type.as_str() {
        "MARKET" => OrderType::Market,
        "LIMIT" => OrderType::Limit,
        _ => return None,
    };

    let status = match report.order_status.as_str() {
        "NEW" => OrderStatus::New,
        "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled,
        "FILLED" => OrderStatus::Filled,
        "CANCELED" => OrderStatus::Canceled,
        "REJECTED" => OrderStatus::Rejected,
        "EXPIRED" => OrderStatus::Canceled,
        _ => return None,
    };

    let size = Decimal::from_str(&report.order_quantity).ok()?;
    let price = Decimal::from_str(&report.order_price)
        .ok()
        .filter(|p| !p.is_zero());
    let filled_size = Decimal::from_str(&report.cumulative_filled_quantity).ok()?;
    let avg_fill_price = Decimal::from_str(&report.last_executed_price)
        .ok()
        .filter(|p| !p.is_zero());

    let request = OrderRequest {
        key,
        side,
        order_type,
        price,
        size,
        strategy_id: "".to_string(), // Binance doesn't return our internal strategy ID
    };

    let order = Order {
        internal_id: report.client_order_id.clone(),
        strategy_id: "".to_string(), // OrderManager will fill this in based on internal_id
        client_order_id: report.client_order_id.clone(),
        exchange_order_id: Some(report.client_order_id.clone()),
        request,
        status,
        filled_size,
        avg_fill_price,
        created_at: ms_to_datetime(report.order_creation_time)?,
        updated_at: ms_to_datetime(report.transaction_time)?,
    };

    Some(order.into())
}

// ──────────────────────────────────────────────────────────────────
// Order REST mappers
// ──────────────────────────────────────────────────────────────────

/// Map a Binance REST order response (new order, query, or cancel) to the
/// internal [`Order`] type.
///
/// `internal_id` is the caller-maintained identifier (usually the
/// `client_order_id` that was submitted to the exchange).  `strategy_id` can
/// be passed as an empty string when it is not known at the call site (e.g.
/// when querying open orders); the `OrderManager` can patch it later.
pub fn binance_order_response_to_order(
    response: &BinanceOrderResponse,
    key: InstrumentKey,
    strategy_id: &str,
    internal_id: &str,
) -> Option<Order> {
    let update_time = ms_to_datetime(response.update_time)?;
    let creation_time = response
        .time
        .and_then(ms_to_datetime)
        .unwrap_or(update_time);

    let side = match response.side.as_str() {
        "BUY" => OrderSide::Buy,
        "SELL" => OrderSide::Sell,
        _ => return None,
    };

    let order_type = match response.order_type.as_str() {
        "MARKET" => OrderType::Market,
        "LIMIT" => OrderType::Limit,
        "LIMIT_MAKER" | "POST_ONLY" => OrderType::PostOnly,
        _ => OrderType::Market,
    };

    let status = match response.status.as_str() {
        "NEW" | "PENDING_NEW" => OrderStatus::New,
        "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled,
        "FILLED" => OrderStatus::Filled,
        "CANCELED" | "EXPIRED" | "EXPIRED_IN_MATCH" => OrderStatus::Canceled,
        "REJECTED" => OrderStatus::Rejected,
        _ => return None,
    };

    let size = Decimal::from_str(&response.orig_qty).ok()?;
    let filled_size = Decimal::from_str(&response.executed_qty).ok()?;
    let price = Decimal::from_str(&response.price)
        .ok()
        .filter(|p| !p.is_zero());

    // Prefer the explicit `avgPrice` futures field; fall back to computing
    // from `cummulativeQuoteQty / executedQty` for spot filled orders.
    let avg_fill_price = response
        .avg_price
        .as_ref()
        .and_then(|s| Decimal::from_str(s).ok())
        .filter(|p| !p.is_zero())
        .or_else(|| {
            if filled_size.is_zero() {
                return None;
            }
            response
                .cummulative_quote_qty
                .as_ref()
                .and_then(|q| Decimal::from_str(q).ok())
                .filter(|q| !q.is_zero())
                .map(|q| q / filled_size)
        });

    let request = OrderRequest {
        key,
        side,
        order_type,
        price,
        size,
        strategy_id: strategy_id.to_string(),
    };

    Some(Order {
        internal_id: internal_id.to_string(),
        strategy_id: strategy_id.to_string(),
        client_order_id: response.client_order_id.clone(),
        exchange_order_id: Some(response.order_id.to_string()),
        request,
        status,
        filled_size,
        avg_fill_price,
        created_at: creation_time,
        updated_at: update_time,
    })
}

// ──────────────────────────────────────────────────────────────────
// Position REST mappers
// ──────────────────────────────────────────────────────────────────

/// Convert a single Binance futures position risk entry to a [`Position`].
///
/// Returns `None` when `positionAmt` is zero (i.e. no open position).
pub fn binance_position_risk_to_position(
    risk: &BinanceFuturesPositionRisk,
    key: InstrumentKey,
) -> Option<Position> {
    let size = Decimal::from_str(&risk.position_amt).ok()?;
    if size.is_zero() {
        return None;
    }

    let entry_price = Decimal::from_str(&risk.entry_price).ok()?;
    let unrealized_pnl = Decimal::from_str(&risk.un_realized_profit).ok()?;

    let liquidation_price = Decimal::from_str(&risk.liquidation_price)
        .ok()
        .filter(|p| !p.is_zero());

    Some(Position {
        key,
        size,
        entry_price,
        unrealized_pnl,
        leverage: Decimal::ONE,
        liquidation_price,
    })
}

/// Convert a WebSocket `ACCOUNT_UPDATE` position entry to a [`WsPositionPatch`].
///
/// Unlike the old mapper this never returns `None`: a zero `size` is a valid
/// "position closed" signal that the `PositionManagerActor` needs to act on.
/// Returns `None` only when the numeric fields cannot be parsed.
pub fn ws_position_to_patch(
    ws_pos: &BinanceFuturesWsPosition,
    key: InstrumentKey,
) -> Option<WsPositionPatch> {
    let size = Decimal::from_str(&ws_pos.position_amt).ok()?;
    let entry_price = Decimal::from_str(&ws_pos.entry_price).ok()?;
    let unrealized_pnl = Decimal::from_str(&ws_pos.unrealized_profit).ok()?;
    Some(WsPositionPatch {
        key,
        size,
        entry_price,
        unrealized_pnl,
    })
}

pub fn futures_execution_to_order_event(
    report: &BinanceFuturesOrderTradeUpdate,
    key: InstrumentKey,
) -> Option<OrderEvent> {
    let side = match report.side.as_str() {
        "BUY" => OrderSide::Buy,
        "SELL" => OrderSide::Sell,
        _ => return None,
    };

    let order_type = match report.order_type.as_str() {
        "MARKET" => OrderType::Market,
        "LIMIT" => OrderType::Limit,
        _ => return None,
    };

    let status = match report.order_status.as_str() {
        "NEW" => OrderStatus::New,
        "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled,
        "FILLED" => OrderStatus::Filled,
        "CANCELED" => OrderStatus::Canceled,
        "REJECTED" => OrderStatus::Rejected,
        "EXPIRED" => OrderStatus::Canceled,
        _ => return None,
    };

    let size = Decimal::from_str(&report.original_quantity).ok()?;
    let price = Decimal::from_str(&report.original_price)
        .ok()
        .filter(|p| !p.is_zero());
    let filled_size = Decimal::from_str(&report.order_filled_accumulated_quantity).ok()?;
    let avg_fill_price = Decimal::from_str(&report.average_price)
        .ok()
        .filter(|p| !p.is_zero());

    let request = OrderRequest {
        key,
        side,
        order_type,
        price,
        size,
        strategy_id: "".to_string(),
    };

    let order = Order {
        internal_id: report.client_order_id.clone(),
        strategy_id: "".to_string(), // OrderManager will fill this in based on internal_id
        client_order_id: report.client_order_id.clone(),
        exchange_order_id: Some(report.client_order_id.clone()),
        request,
        status,
        filled_size,
        avg_fill_price,
        created_at: ms_to_datetime(report.order_trade_time)?,
        updated_at: ms_to_datetime(report.order_trade_time)?,
    };

    Some(order.into())
}
