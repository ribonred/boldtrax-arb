//! Test fixtures for exchange implementations
//!
//! This module provides reusable test data and helper functions
//! for testing exchange connectors.

use boldtrax_core::registry::InstrumentRegistry;
use boldtrax_core::types::{
    Exchange, FundingInterval, Instrument, InstrumentKey, InstrumentType, Pairs,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Create a test registry pre-populated with common instruments
pub fn test_registry() -> InstrumentRegistry {
    let registry = InstrumentRegistry::new();

    registry.insert(btcusdt_swap());
    registry.insert(ethusdt_swap());
    registry.insert(btcusdt_spot());

    registry
}

pub fn empty_registry() -> InstrumentRegistry {
    InstrumentRegistry::new()
}

pub mod binance {
    pub fn futures_exchange_info_json() -> &'static str {
        r#"{
            "symbols": [
                {
                    "symbol": "BTCUSDT",
                    "status": "TRADING",
                    "contractType": "PERPETUAL",
                    "baseAsset": "BTC",
                    "quoteAsset": "USDT",
                    "contractSize": "1",
                    "filters": [
                        {"filterType": "PRICE_FILTER", "tickSize": "0.10"},
                        {"filterType": "LOT_SIZE", "stepSize": "0.001"},
                        {"filterType": "MIN_NOTIONAL", "notional": "5"}
                    ]
                },
                {
                    "symbol": "ETHUSDT",
                    "status": "TRADING",
                    "contractType": "PERPETUAL",
                    "baseAsset": "ETH",
                    "quoteAsset": "USDT",
                    "contractSize": "1",
                    "filters": [
                        {"filterType": "PRICE_FILTER", "tickSize": "0.01"},
                        {"filterType": "LOT_SIZE", "stepSize": "0.001"},
                        {"filterType": "MIN_NOTIONAL", "notional": "5"}
                    ]
                },
                {
                    "symbol": "SOLUSDT",
                    "status": "BREAK",
                    "contractType": "PERPETUAL",
                    "baseAsset": "SOL",
                    "quoteAsset": "USDT",
                    "contractSize": "1",
                    "filters": []
                },
                {
                    "symbol": "BTCUSD_PERP",
                    "status": "TRADING",
                    "contractType": "PERPETUAL",
                    "baseAsset": "BTC",
                    "quoteAsset": "USD",
                    "contractSize": "100",
                    "filters": []
                }
            ]
        }"#
    }

    /// Sample Binance funding info JSON response
    pub fn funding_info_json() -> &'static str {
        r#"[
            {"symbol": "BTCUSDT", "fundingIntervalHours": 8},
            {"symbol": "ETHUSDT", "fundingIntervalHours": 8},
            {"symbol": "SOLUSDT", "fundingIntervalHours": 4}
        ]"#
    }

    /// Sample Binance Spot exchange info JSON response
    pub fn spot_exchange_info_json() -> &'static str {
        r#"{
            "symbols": [
                {
                    "symbol": "BTCUSDT",
                    "status": "TRADING",
                    "baseAsset": "BTC",
                    "quoteAsset": "USDT",
                    "isSpotTradingAllowed": true,
                    "filters": [
                        {"filterType": "PRICE_FILTER", "tickSize": "0.01"},
                        {"filterType": "LOT_SIZE", "stepSize": "0.00001"},
                        {"filterType": "NOTIONAL", "minNotional": "10"}
                    ]
                },
                {
                    "symbol": "ETHUSDT",
                    "status": "TRADING",
                    "baseAsset": "ETH",
                    "quoteAsset": "USDT",
                    "isSpotTradingAllowed": true,
                    "filters": [
                        {"filterType": "PRICE_FILTER", "tickSize": "0.01"},
                        {"filterType": "LOT_SIZE", "stepSize": "0.0001"}
                    ]
                },
                {
                    "symbol": "RANDOMCOIN",
                    "status": "TRADING",
                    "baseAsset": "RANDOM",
                    "quoteAsset": "USDT",
                    "isSpotTradingAllowed": true,
                    "filters": []
                }
            ]
        }"#
    }

    /// Sample Binance premium index JSON response
    pub fn premium_index_json() -> &'static str {
        r#"{
            "symbol": "BTCUSDT",
            "markPrice": "100000.50",
            "indexPrice": "100000.00",
            "lastFundingRate": "0.0001",
            "nextFundingTime": 1737417600000,
            "time": 1737410400000
        }"#
    }

    /// Sample Binance funding rate history JSON response
    pub fn funding_rate_history_json() -> &'static str {
        r#"[
            {"symbol": "BTCUSDT", "fundingRate": "0.0001", "fundingTime": 1737388800000},
            {"symbol": "BTCUSDT", "fundingRate": "0.00015", "fundingTime": 1737360000000},
            {"symbol": "BTCUSDT", "fundingRate": "0.00008", "fundingTime": 1737331200000}
        ]"#
    }

    /// Sample server time JSON response
    pub fn server_time_json() -> &'static str {
        r#"{"serverTime": 1737410400000}"#
    }

    pub fn spot_account_json() -> &'static str {
        r#"{
            "makerCommission": 10,
            "takerCommission": 10,
            "buyerCommission": 0,
            "sellerCommission": 0,
            "canTrade": true,
            "canWithdraw": true,
            "canDeposit": true,
            "updateTime": 123456789,
            "accountType": "SPOT",
            "balances": [
                {"asset": "USDT", "free": "100.50000000", "locked": "10.00000000"},
                {"asset": "BTC", "free": "0.10000000", "locked": "0.00000000"},
                {"asset": "UNKNOWN", "free": "5.00000000", "locked": "0.00000000"}
            ]
        }"#
    }

    pub fn futures_balance_json() -> &'static str {
        r#"[
            {
                "accountAlias": "SgsR",
                "asset": "USDT",
                "balance": "250.00000000",
                "crossWalletBalance": "240.00000000",
                "crossUnPnl": "5.25000000",
                "availableBalance": "200.00000000",
                "updateTime": 1576756674610
            },
            {
                "accountAlias": "SgsR",
                "asset": "ETH",
                "balance": "1.50000000",
                "crossWalletBalance": "1.20000000",
                "crossUnPnl": "-0.10000000",
                "availableBalance": "1.00000000",
                "updateTime": 1576756674610
            },
            {
                "accountAlias": "SgsR",
                "asset": "UNSUPPORTED",
                "balance": "9.0",
                "crossWalletBalance": "9.0",
                "crossUnPnl": "0.0",
                "availableBalance": "9.0",
                "updateTime": 1576756674610
            }
        ]"#
    }
}

// ============================================================================
// Instrument Fixtures
// ============================================================================

/// BTCUSDT perpetual swap instrument
pub fn btcusdt_swap() -> Instrument {
    Instrument {
        key: InstrumentKey {
            exchange: Exchange::Binance,
            instrument_type: InstrumentType::Swap,
            pair: Pairs::BTCUSDT,
        },
        exchange_symbol: "BTCUSDT".to_string(),
        tick_size: dec!(0.10),
        lot_size: dec!(0.001),
        min_notional: Some(dec!(5)),
        contract_size: Some(Decimal::ONE),
        multiplier: Decimal::ONE,
        funding_interval: Some(FundingInterval::Every8Hours),
        exchange_id: "BTCUSDT".to_string(),
    }
}

/// ETHUSDT perpetual swap instrument
pub fn ethusdt_swap() -> Instrument {
    Instrument {
        key: InstrumentKey {
            exchange: Exchange::Binance,
            instrument_type: InstrumentType::Swap,
            pair: Pairs::ETHUSDT,
        },
        exchange_symbol: "ETHUSDT".to_string(),
        tick_size: dec!(0.01),
        lot_size: dec!(0.001),
        min_notional: Some(dec!(5)),
        contract_size: Some(Decimal::ONE),
        multiplier: Decimal::ONE,
        funding_interval: Some(FundingInterval::Every8Hours),
        exchange_id: "ETHUSDT".to_string(),
    }
}

/// BTCUSDT spot instrument
pub fn btcusdt_spot() -> Instrument {
    Instrument {
        key: InstrumentKey {
            exchange: Exchange::Binance,
            instrument_type: InstrumentType::Spot,
            pair: Pairs::BTCUSDT,
        },
        exchange_symbol: "BTCUSDT".to_string(),
        tick_size: dec!(0.01),
        lot_size: dec!(0.00001),
        min_notional: Some(dec!(10)),
        contract_size: None,
        multiplier: Decimal::ONE,
        funding_interval: None,
        exchange_id: "BTCUSDT".to_string(),
    }
}
