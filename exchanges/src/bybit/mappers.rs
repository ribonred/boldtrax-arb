//! Bybit V5 type mappers — convert exchange-specific types to core types.

use super::types::{
    BybitFundingRateEntry, BybitLinearTicker, BybitOrderBookResult, BybitOrderEntry,
    BybitPositionEntry, BybitWsOrderUpdate, BybitWsPositionUpdate,
};
use boldtrax_core::manager::types::WsPositionPatch;
use boldtrax_core::types::{
    FundingInterval, FundingRatePoint, FundingRateSeries, FundingRateSnapshot, InstrumentKey,
    Order, OrderBookSnapshot, OrderEvent, OrderRequest, OrderSide, OrderStatus, OrderType,
    Position, PriceLevel,
};

use chrono::{DateTime, TimeZone, Utc};
use rust_decimal::Decimal;
use std::str::FromStr;

use crate::bybit::codec::BybitClientOrderIdCodec;

/// Convert a millisecond timestamp to `DateTime<Utc>`.
pub fn ms_to_datetime(ms: u64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms as i64).single()
}

/// Convert a Bybit ticker snapshot to a core `FundingRateSnapshot`.
pub fn ticker_to_funding_snapshot(
    ticker: &BybitLinearTicker,
    key: InstrumentKey,
    interval: FundingInterval,
) -> Option<FundingRateSnapshot> {
    let funding_rate = Decimal::from_str(&ticker.funding_rate).ok()?;
    let mark_price = Decimal::from_str(&ticker.mark_price).ok()?;
    let index_price = Decimal::from_str(&ticker.index_price).ok()?;
    let next_funding_time_ms = ticker.next_funding_time.parse::<u64>().ok()?;
    let next_funding_time_utc = ms_to_datetime(next_funding_time_ms)?;
    let now = Utc::now();

    Some(FundingRateSnapshot {
        key,
        funding_rate,
        mark_price,
        index_price,
        interest_rate: None,
        next_funding_time_utc,
        next_funding_time_ms: next_funding_time_ms as i64,
        event_time_utc: now,
        event_time_ms: now.timestamp_millis(),
        interval,
    })
}

/// Convert Bybit funding rate history entries to a core `FundingRateSeries`.
pub fn funding_history_to_series(
    entries: &[BybitFundingRateEntry],
    key: InstrumentKey,
    interval: FundingInterval,
) -> FundingRateSeries {
    let mut points = Vec::with_capacity(entries.len());

    for entry in entries {
        let funding_rate = match Decimal::from_str(&entry.funding_rate) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let ts_ms = match entry.funding_rate_timestamp.parse::<u64>() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if let Some(event_time_utc) = ms_to_datetime(ts_ms) {
            points.push(FundingRatePoint {
                funding_rate,
                event_time_utc,
                event_time_ms: ts_ms as i64,
            });
        }
    }

    FundingRateSeries {
        key,
        interval,
        points,
    }
}

/// Convert a Bybit REST order book to a core `OrderBookSnapshot`.
pub fn order_book_to_snapshot(
    ob: &BybitOrderBookResult,
    key: InstrumentKey,
) -> Option<OrderBookSnapshot> {
    let bids: Vec<PriceLevel> = ob
        .b
        .iter()
        .filter_map(|level| {
            let price = Decimal::from_str(&level[0]).ok()?;
            let quantity = Decimal::from_str(&level[1]).ok()?;
            Some(PriceLevel { price, quantity })
        })
        .collect();

    let asks: Vec<PriceLevel> = ob
        .a
        .iter()
        .filter_map(|level| {
            let price = Decimal::from_str(&level[0]).ok()?;
            let quantity = Decimal::from_str(&level[1]).ok()?;
            Some(PriceLevel { price, quantity })
        })
        .collect();

    let timestamp_ms = ob.ts.unwrap_or_else(|| chrono::Utc::now().timestamp_millis() as u64);
    let timestamp_utc = ms_to_datetime(timestamp_ms)?;

    Some(OrderBookSnapshot::new(
        key,
        bids,
        asks,
        timestamp_utc,
        timestamp_ms as i64,
    ))
}

/// Convert a Bybit position entry to a core `Position`.
pub fn bybit_position_to_position(
    entry: &BybitPositionEntry,
    key: InstrumentKey,
) -> Option<Position> {
    let raw_size = Decimal::from_str(&entry.size).ok()?;
    if raw_size.is_zero() {
        return None;
    }

    // Bybit uses side="Buy" for long, "Sell" for short.
    // Core Position.size is signed: positive = long, negative = short.
    let size = match entry.side.as_str() {
        "Buy" => raw_size,
        "Sell" => -raw_size,
        _ => return None,
    };

    let entry_price = Decimal::from_str(&entry.avg_price).ok()?;
    let unrealized_pnl = Decimal::from_str(&entry.unrealised_pnl).ok()?;
    let leverage = Decimal::from_str(&entry.leverage).ok().unwrap_or(Decimal::ONE);
    let liquidation_price = Decimal::from_str(&entry.liq_price)
        .ok()
        .filter(|p| !p.is_zero());

    Some(Position {
        key,
        size,
        entry_price,
        unrealized_pnl,
        leverage,
        liquidation_price,
    })
}

/// Convert a Bybit REST order entry to a core `Order`.
pub fn bybit_order_to_order(
    entry: &BybitOrderEntry,
    key: InstrumentKey,
) -> Option<Order> {
    let side = match entry.side.as_str() {
        "Buy" => OrderSide::Buy,
        "Sell" => OrderSide::Sell,
        _ => return None,
    };

    let order_type = match entry.order_type.as_str() {
        "Market" => OrderType::Market,
        "Limit" => OrderType::Limit,
        _ => OrderType::Market,
    };

    let status = match entry.order_status.as_str() {
        "New" | "Untriggered" => OrderStatus::New,
        "PartiallyFilled" => OrderStatus::PartiallyFilled,
        "Filled" => OrderStatus::Filled,
        "Cancelled" | "Deactivated" | "Expired" => OrderStatus::Canceled,
        "Rejected" => OrderStatus::Rejected,
        _ => return None,
    };

    let size = Decimal::from_str(&entry.qty).ok()?;
    let filled_size = Decimal::from_str(&entry.cum_exec_qty).ok()?;
    let price = Decimal::from_str(&entry.price)
        .ok()
        .filter(|p| !p.is_zero());
    let avg_fill_price = Decimal::from_str(&entry.avg_price)
        .ok()
        .filter(|p| !p.is_zero());

    let codec = BybitClientOrderIdCodec;
    let strategy_id = codec
        .decode_strategy_id(&entry.order_link_id)
        .unwrap_or_default();
    let internal_id = codec.decode_internal_id(&entry.order_link_id);

    let created_time = entry
        .created_time
        .parse::<u64>()
        .ok()
        .and_then(ms_to_datetime)
        .unwrap_or_else(Utc::now);
    let updated_time = entry
        .updated_time
        .parse::<u64>()
        .ok()
        .and_then(ms_to_datetime)
        .unwrap_or(created_time);

    let request = OrderRequest {
        key,
        side,
        order_type,
        price,
        size,
        post_only: false,
        reduce_only: entry.reduce_only,
        strategy_id: strategy_id.clone(),
    };

    Some(Order {
        internal_id,
        strategy_id,
        client_order_id: entry.order_link_id.clone(),
        exchange_order_id: Some(entry.order_id.clone()),
        request,
        status,
        filled_size,
        avg_fill_price,
        created_at: created_time,
        updated_at: updated_time,
    })
}

// ──────────────────────────────────────────────────────────────────
// Private WS mappers
// ──────────────────────────────────────────────────────────────────

/// Convert a Bybit private WS order update to a core `OrderEvent`.
pub fn ws_order_to_order_event(
    update: &BybitWsOrderUpdate,
    key: InstrumentKey,
) -> Option<OrderEvent> {
    let side = match update.side.as_str() {
        "Buy" => OrderSide::Buy,
        "Sell" => OrderSide::Sell,
        _ => return None,
    };

    let order_type = match update.order_type.as_str() {
        "Market" => OrderType::Market,
        "Limit" => OrderType::Limit,
        _ => OrderType::Market,
    };

    let status = match update.order_status.as_str() {
        "New" | "Untriggered" => OrderStatus::New,
        "PartiallyFilled" => OrderStatus::PartiallyFilled,
        "Filled" => OrderStatus::Filled,
        "Cancelled" | "Deactivated" | "Expired" => OrderStatus::Canceled,
        "Rejected" => OrderStatus::Rejected,
        _ => return None,
    };

    let size = Decimal::from_str(&update.qty).ok()?;
    let filled_size = Decimal::from_str(&update.cum_exec_qty).ok().unwrap_or(Decimal::ZERO);
    let price = Decimal::from_str(&update.price).ok().filter(|p| !p.is_zero());
    let avg_fill_price = Decimal::from_str(&update.avg_price).ok().filter(|p| !p.is_zero());

    let codec = BybitClientOrderIdCodec;
    let strategy_id = codec
        .decode_strategy_id(&update.order_link_id)
        .unwrap_or_default();
    let internal_id = codec.decode_internal_id(&update.order_link_id);

    let created_time = update
        .created_time
        .parse::<u64>()
        .ok()
        .and_then(ms_to_datetime)
        .unwrap_or_else(Utc::now);
    let updated_time = update
        .updated_time
        .parse::<u64>()
        .ok()
        .and_then(ms_to_datetime)
        .unwrap_or(created_time);

    let request = OrderRequest {
        key,
        side,
        order_type,
        price,
        size,
        post_only: false,
        reduce_only: update.reduce_only,
        strategy_id: strategy_id.clone(),
    };

    let order = Order {
        internal_id,
        strategy_id,
        client_order_id: update.order_link_id.clone(),
        exchange_order_id: Some(update.order_id.clone()),
        request,
        status,
        filled_size,
        avg_fill_price,
        created_at: created_time,
        updated_at: updated_time,
    };

    Some(OrderEvent::from(order))
}

/// Convert a Bybit private WS position update to a `WsPositionPatch`.
pub fn ws_position_to_patch(
    update: &BybitWsPositionUpdate,
    key: InstrumentKey,
) -> Option<WsPositionPatch> {
    let raw_size = Decimal::from_str(&update.size).ok()?;
    // Bybit uses side="Buy" for long, "Sell" for short, "" for zero.
    // WsPositionPatch.size is signed: positive = long, negative = short.
    let size = match update.side.as_str() {
        "Buy" => raw_size,
        "Sell" => -raw_size,
        _ => Decimal::ZERO, // closed or unknown
    };

    let entry_price = Decimal::from_str(&update.entry_price).ok().unwrap_or(Decimal::ZERO);
    let unrealized_pnl = Decimal::from_str(&update.unrealised_pnl).ok().unwrap_or(Decimal::ZERO);

    Some(WsPositionPatch {
        key,
        size,
        entry_price,
        unrealized_pnl,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use boldtrax_core::types::Pairs;
    use boldtrax_core::{Exchange, InstrumentType};

    #[test]
    fn ms_to_datetime_valid() {
        let dt = ms_to_datetime(1672304484978).unwrap();
        assert_eq!(dt.timestamp_millis(), 1672304484978);
    }

    #[test]
    fn order_book_mapping() {
        let ob = BybitOrderBookResult {
            s: "BTCUSDT".to_string(),
            b: vec![["65000.50".to_string(), "1.5".to_string()]],
            a: vec![["65001.00".to_string(), "2.0".to_string()]],
            ts: Some(1672304484978),
            u: 12345,
        };
        let key = InstrumentKey {
            exchange: Exchange::Bybit,
            pair: Pairs::BTCUSDT,
            instrument_type: InstrumentType::Swap,
        };
        let snapshot = order_book_to_snapshot(&ob, key).unwrap();
        assert_eq!(snapshot.bids.len(), 1);
        assert_eq!(snapshot.asks.len(), 1);
    }

    #[test]
    fn position_mapping_long() {
        let entry = BybitPositionEntry {
            symbol: "BTCUSDT".to_string(),
            side: "Buy".to_string(),
            size: "0.5".to_string(),
            avg_price: "65000.00".to_string(),
            leverage: "10".to_string(),
            liq_price: "60000.00".to_string(),
            unrealised_pnl: "100.50".to_string(),
            position_value: "32500.00".to_string(),
            updated_time: "1672304484978".to_string(),
        };
        let key = InstrumentKey {
            exchange: Exchange::Bybit,
            pair: Pairs::BTCUSDT,
            instrument_type: InstrumentType::Swap,
        };
        let pos = bybit_position_to_position(&entry, key).unwrap();
        assert!(pos.size > Decimal::ZERO);
        assert_eq!(pos.size, Decimal::from_str("0.5").unwrap());
    }

    #[test]
    fn position_mapping_short_is_negative() {
        let entry = BybitPositionEntry {
            symbol: "ETHUSDT".to_string(),
            side: "Sell".to_string(),
            size: "2.0".to_string(),
            avg_price: "3500.00".to_string(),
            leverage: "5".to_string(),
            liq_price: "4000.00".to_string(),
            unrealised_pnl: "-50.00".to_string(),
            position_value: "7000.00".to_string(),
            updated_time: "1672304484978".to_string(),
        };
        let key = InstrumentKey {
            exchange: Exchange::Bybit,
            pair: Pairs::ETHUSDT,
            instrument_type: InstrumentType::Swap,
        };
        let pos = bybit_position_to_position(&entry, key).unwrap();
        assert!(pos.size < Decimal::ZERO);
    }

    #[test]
    fn zero_size_position_returns_none() {
        let entry = BybitPositionEntry {
            symbol: "BTCUSDT".to_string(),
            side: "Buy".to_string(),
            size: "0".to_string(),
            avg_price: "0".to_string(),
            leverage: "10".to_string(),
            liq_price: "".to_string(),
            unrealised_pnl: "0".to_string(),
            position_value: "0".to_string(),
            updated_time: "".to_string(),
        };
        let key = InstrumentKey {
            exchange: Exchange::Bybit,
            pair: Pairs::BTCUSDT,
            instrument_type: InstrumentType::Swap,
        };
        assert!(bybit_position_to_position(&entry, key).is_none());
    }
}
