//! Mappers to convert Binance types to core types

use boldtrax_core::manager::types::{
    AccountModel, AccountPartitionRef, AccountSnapshot, BalanceView, CollateralScope, PartitionKind,
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
