#[cfg(test)]
mod tests {
    use crate::arbitrage::policy::DecisionPolicy;
    use crate::arbitrage::spot_perp::decider::SpotRebalanceDecider;
    use crate::arbitrage::spot_perp::types::SpotPerpPair;
    use crate::arbitrage::types::{DeciderAction, PairStatus};
    use boldtrax_core::types::{Exchange, InstrumentKey, InstrumentType, Pairs};
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn create_test_pair() -> SpotPerpPair {
        let spot_key = InstrumentKey {
            exchange: Exchange::Binance,
            pair: Pairs::BTCUSDT,
            instrument_type: InstrumentType::Spot,
        };
        let perp_key = InstrumentKey {
            exchange: Exchange::Binance,
            pair: Pairs::BTCUSDT,
            instrument_type: InstrumentType::Swap,
        };
        SpotPerpPair::new(spot_key, perp_key)
    }

    #[test]
    fn test_decider_enter_position() {
        let decider = SpotRebalanceDecider::new(dec("0.001"), dec("100.0"), dec("10"));
        let mut pair = create_test_pair();

        // Perp funding rate exceeds threshold — should enter.
        pair.perp.funding_rate = dec("0.002");
        // Prices must be set for notional→quantity conversion.
        pair.spot.current_price = dec("50000.0");
        pair.perp.current_price = dec("50000.0");

        let action = decider.evaluate(&pair);
        assert!(matches!(action, DeciderAction::Enter { .. }));
    }

    #[test]
    fn test_decider_exit_position() {
        let decider = SpotRebalanceDecider::new(dec("0.001"), dec("100.0"), dec("10"));
        let mut pair = create_test_pair();

        // Perp funding turned negative — should exit.
        pair.perp.funding_rate = dec("-0.001");

        // Simulate an open position.
        pair.status = PairStatus::Active;
        pair.spot.quantity = dec("1.0");
        pair.perp.position_size = dec("-1.0");

        let action = decider.evaluate(&pair);
        assert!(matches!(action, DeciderAction::Exit));
    }

    #[test]
    fn test_decider_rebalance_delta() {
        let decider = SpotRebalanceDecider::new(dec("0.001"), dec("100.0"), dec("10"));
        let mut pair = create_test_pair();

        // Perp funding still positive.
        pair.perp.funding_rate = dec("0.002");

        // Simulate an open position with a delta imbalance.
        pair.status = PairStatus::Active;
        pair.spot.quantity = dec("1.0");
        pair.spot.current_price = dec("50000.0");
        pair.perp.position_size = dec("-0.9"); // Imbalance: spot notional 50000, perp notional -45000 → delta 5000
        pair.perp.current_price = dec("50000.0");

        let action = decider.evaluate(&pair);
        assert!(matches!(action, DeciderAction::Rebalance { .. }));
    }

    #[test]
    fn test_decider_hold() {
        let decider = SpotRebalanceDecider::new(dec("0.001"), dec("100.0"), dec("10"));
        let mut pair = create_test_pair();

        // Perp funding below threshold — should do nothing.
        pair.perp.funding_rate = dec("0.0005");

        let action = decider.evaluate(&pair);
        assert!(matches!(action, DeciderAction::DoNothing));
    }
}
