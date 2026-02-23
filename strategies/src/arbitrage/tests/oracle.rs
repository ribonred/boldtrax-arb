#[cfg(test)]
mod tests {
    use crate::arbitrage::oracle::PriceOracle;
    use crate::arbitrage::policy::PriceSource;
    use boldtrax_core::types::{
        Exchange, InstrumentKey, InstrumentType, OrderBookSnapshot, Pairs, PriceLevel, Ticker,
    };
    use chrono::Utc;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn create_test_key() -> InstrumentKey {
        InstrumentKey {
            exchange: Exchange::Binance,
            pair: Pairs::BTCUSDT,
            instrument_type: InstrumentType::Spot,
        }
    }

    #[test]
    fn test_oracle_update_snapshot() {
        let mut oracle = PriceOracle::new();
        let key = create_test_key();

        let snapshot = OrderBookSnapshot {
            key,
            bids: vec![PriceLevel {
                price: dec("50000.0"),
                quantity: dec("1.0"),
            }],
            asks: vec![PriceLevel {
                price: dec("50010.0"),
                quantity: dec("1.0"),
            }],
            best_bid: Some(dec("50000.0")),
            best_ask: Some(dec("50010.0")),
            mid: Some(dec("50005.0")),
            spread: Some(dec("10.0")),
            timestamp_utc: Utc::now(),
            timestamp_ms: 0,
        };

        oracle.update_snapshot(snapshot);

        assert_eq!(oracle.get_best_bid(&key), Some(dec("50000.0")));
        assert_eq!(oracle.get_best_ask(&key), Some(dec("50010.0")));
        assert_eq!(oracle.get_mid_price(&key), Some(dec("50005.0")));
    }

    #[test]
    fn test_oracle_update_ticker() {
        let mut oracle = PriceOracle::new();
        let key = create_test_key();

        let ticker = Ticker {
            key,
            price: dec("50005.0"),
            volume_24h: Some(dec("100.0")),
            timestamp: Utc::now(),
        };

        oracle.update_ticker(ticker);

        // Ticker doesn't update best_bid/best_ask/mid in the current implementation
        assert_eq!(oracle.get_best_bid(&key), None);
        assert_eq!(oracle.get_best_ask(&key), None);
        assert_eq!(oracle.get_mid_price(&key), None);
    }

    #[test]
    fn test_oracle_missing_data() {
        let oracle = PriceOracle::new();
        let key = create_test_key();

        assert_eq!(oracle.get_best_bid(&key), None);
        assert_eq!(oracle.get_best_ask(&key), None);
        assert_eq!(oracle.get_mid_price(&key), None);
    }
}
