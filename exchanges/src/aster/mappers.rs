use super::types::{
    AsterFundingRateHistory, AsterFuturesOrderTradeUpdate, AsterFuturesPositionRisk,
    AsterFuturesWsPosition, AsterOrderResponse, AsterPartialDepth, AsterPremiumIndex,
};
use boldtrax_core::manager::types::WsPositionPatch;
use boldtrax_core::types::{
    FundingInterval, FundingRateSeries, FundingRateSnapshot, InstrumentKey, Order,
    OrderBookSnapshot, OrderEvent, OrderRequest, OrderSide, OrderStatus, OrderType, Pairs,
    Position, PriceLevel,
};
use boldtrax_core::{Exchange, FundingRatePoint, InstrumentType};
use chrono::{DateTime, TimeZone, Utc};
use rust_decimal::Decimal;
use std::str::FromStr;

use crate::aster::codec::AsterClientOrderIdCodec;

pub fn aster_position_risk_to_position(
    risk: &AsterFuturesPositionRisk,
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

pub fn ms_to_datetime(ms: u64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms as i64).single()
}

pub fn premium_index_to_snapshot(
    index: &AsterPremiumIndex,
    pair: Pairs,
    interval: FundingInterval,
) -> Option<FundingRateSnapshot> {
    let event_time_utc = ms_to_datetime(index.time)?;
    let next_funding_time_utc = ms_to_datetime(index.next_funding_time)?;

    Some(FundingRateSnapshot {
        key: InstrumentKey {
            exchange: Exchange::Aster,
            pair,
            instrument_type: InstrumentType::Swap,
        },
        funding_rate: index.last_funding_rate,
        mark_price: index.mark_price,
        index_price: index.mark_price, // Aster doesn't provide index_price in this endpoint
        interest_rate: None,
        next_funding_time_utc,
        next_funding_time_ms: index.next_funding_time as i64,
        event_time_utc,
        event_time_ms: index.time as i64,
        interval,
    })
}

pub fn funding_history_to_series(
    history: &[AsterFundingRateHistory],
    pair: Pairs,
    interval: FundingInterval,
) -> FundingRateSeries {
    let mut points = Vec::with_capacity(history.len());

    for item in history {
        if let Some(event_time_utc) = ms_to_datetime(item.funding_time) {
            points.push(FundingRatePoint {
                funding_rate: item.funding_rate,
                event_time_utc,
                event_time_ms: item.funding_time as i64,
            });
        }
    }

    FundingRateSeries {
        key: InstrumentKey {
            exchange: Exchange::Aster,
            pair,
            instrument_type: InstrumentType::Swap,
        },
        interval,
        points,
    }
}

pub fn partial_depth_to_order_book(
    depth: &AsterPartialDepth,
    key: InstrumentKey,
    timestamp_ms: i64,
) -> Option<OrderBookSnapshot> {
    let mut bids = Vec::with_capacity(depth.bids.len());
    for bid in &depth.bids {
        bids.push(PriceLevel {
            price: bid.0,
            quantity: bid.1,
        });
    }

    let mut asks = Vec::with_capacity(depth.asks.len());
    for ask in &depth.asks {
        asks.push(PriceLevel {
            price: ask.0,
            quantity: ask.1,
        });
    }

    let timestamp_utc = ms_to_datetime(depth.message_time.unwrap_or(timestamp_ms as u64))?;

    Some(OrderBookSnapshot::new(
        key,
        bids,
        asks,
        timestamp_utc,
        timestamp_ms,
    ))
}
// ─── Order REST mappers ───────────────────────────────────────────────────────

/// Map an Aster REST order response (place, query, cancel) to the internal
/// [`Order`] type.
pub fn aster_order_response_to_order(
    response: &AsterOrderResponse,
    key: InstrumentKey,
    strategy_id: &str,
    internal_id: &str,
) -> Option<Order> {
    let update_time = response
        .update_time
        .or(response.time)
        .and_then(|ms| ms_to_datetime(ms as u64))?;
    let creation_time = response
        .time
        .and_then(|ms| ms_to_datetime(ms as u64))
        .unwrap_or(update_time);

    let side = match response.side.as_str() {
        "BUY" => OrderSide::Buy,
        "SELL" => OrderSide::Sell,
        _ => return None,
    };

    let order_type = match response.order_type.as_str() {
        "MARKET" => OrderType::Market,
        "LIMIT" | "LIMIT_MAKER" | "POST_ONLY" => OrderType::Limit,
        _ => OrderType::Market,
    };
    let post_only = matches!(response.order_type.as_str(), "LIMIT_MAKER" | "POST_ONLY");

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
    let avg_fill_price = response
        .avg_price
        .as_ref()
        .and_then(|s| Decimal::from_str(s).ok())
        .filter(|p| !p.is_zero());

    let request = OrderRequest {
        key,
        side,
        order_type,
        price,
        size,
        post_only,
        reduce_only: false,
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

/// Convert an Aster `ORDER_TRADE_UPDATE` WebSocket report to an [`OrderEvent`].
pub fn aster_execution_to_order_event(
    report: &AsterFuturesOrderTradeUpdate,
    key: InstrumentKey,
) -> Option<OrderEvent> {
    let side = match report.side.as_str() {
        "BUY" => OrderSide::Buy,
        "SELL" => OrderSide::Sell,
        _ => return None,
    };

    let order_type = match report.order_type.as_str() {
        "MARKET" => OrderType::Market,
        "LIMIT" | "LIMIT_MAKER" | "POST_ONLY" => OrderType::Limit,
        _ => return None,
    };
    let post_only = matches!(report.order_type.as_str(), "LIMIT_MAKER" | "POST_ONLY");

    let status = match report.order_status.as_str() {
        "NEW" => OrderStatus::New,
        "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled,
        "FILLED" => OrderStatus::Filled,
        "CANCELED" | "EXPIRED" => OrderStatus::Canceled,
        "REJECTED" => OrderStatus::Rejected,
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

    let codec = AsterClientOrderIdCodec;
    let strategy_id = codec
        .decode_strategy_id(&report.client_order_id)
        .unwrap_or_default();

    let request = OrderRequest {
        key,
        side,
        order_type,
        price,
        size,
        post_only,
        reduce_only: false,
        strategy_id: strategy_id.clone(),
    };

    let order = Order {
        internal_id: codec.decode_internal_id(&report.client_order_id),
        strategy_id,
        client_order_id: report.client_order_id.clone(),
        exchange_order_id: Some(report.order_id.to_string()),
        request,
        status,
        filled_size,
        avg_fill_price,
        created_at: ms_to_datetime(report.order_trade_time as u64)?,
        updated_at: ms_to_datetime(report.order_trade_time as u64)?,
    };

    Some(order.into())
}

/// Convert a WebSocket `ACCOUNT_UPDATE` position entry to a [`WsPositionPatch`].
///
/// A zero `size` is a valid "position closed" signal that the
/// `PositionManagerActor` needs to act on.  Returns `None` only when the
/// numeric fields cannot be parsed.
pub fn ws_position_to_patch(
    ws_pos: &AsterFuturesWsPosition,
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
