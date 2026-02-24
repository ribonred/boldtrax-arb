#[cfg(test)]
mod tests {
    use crate::arbitrage::perp_perp::decider::PerpPerpDecider;
    use crate::arbitrage::perp_perp::types::{CarryDirection, PerpPerpPair};
    use crate::arbitrage::types::{DeciderAction, PairStatus};
    use rust_decimal::Decimal;
    use std::str::FromStr;
    use std::time::Duration;

    use boldtrax_core::types::{Exchange, InstrumentKey, InstrumentType, Pairs};

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn long_key() -> InstrumentKey {
        InstrumentKey {
            exchange: Exchange::Binance,
            pair: Pairs::XRPUSDT,
            instrument_type: InstrumentType::Swap,
        }
    }

    fn short_key() -> InstrumentKey {
        InstrumentKey {
            exchange: Exchange::Bybit,
            pair: Pairs::XRPUSDT,
            instrument_type: InstrumentType::Swap,
        }
    }

    fn make_pair(long_rate: &str, short_rate: &str) -> PerpPerpPair {
        let mut pair = PerpPerpPair::new(
            long_key(),
            short_key(),
            CarryDirection::Positive,
            Duration::from_secs(60),
        );
        pair.long_leg.funding_rate = dec(long_rate);
        pair.short_leg.funding_rate = dec(short_rate);
        pair.long_leg.current_price = dec("2.5");
        pair.short_leg.current_price = dec("2.5");
        pair.target_notional = dec("100");
        pair
    }

    fn make_decider() -> PerpPerpDecider {
        PerpPerpDecider::new(
            dec("0.0002"), // min_spread_threshold
            None,          // exit_threshold: None → exit on sign flip
            dec("1000"),   // max_position_size
            dec("100"),    // target_notional
            dec("10"),     // rebalance_drift_pct
        )
    }

    // -----------------------------------------------------------------------
    // Entry: spread above threshold → Enter
    // -----------------------------------------------------------------------
    #[test]
    fn test_enter_on_spread_above_threshold() {
        // long_rate=0.0005, short_rate=0.0001 → spread=0.0004 ≥ 0.0002
        let pair = make_pair("0.0005", "0.0001");
        let decider = make_decider();
        let action = decider.evaluate_inner_impl(&pair);

        match action {
            DeciderAction::Enter {
                size_long,
                size_short,
            } => {
                // target_notional=100, price=2.5 → qty=40
                assert_eq!(size_long, dec("40"));
                assert_eq!(size_short, dec("-40"));
            }
            other => panic!("Expected Enter, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // No entry: spread below threshold → DoNothing
    // -----------------------------------------------------------------------
    #[test]
    fn test_no_entry_below_threshold() {
        // long_rate=0.0002, short_rate=0.0001 → spread=0.0001 < 0.0002
        let pair = make_pair("0.0002", "0.0001");
        let decider = make_decider();
        let action = decider.evaluate_inner_impl(&pair);

        assert!(matches!(action, DeciderAction::DoNothing));
    }

    // -----------------------------------------------------------------------
    // Exit: spread flips sign → Exit (no exit_threshold set)
    // -----------------------------------------------------------------------
    #[test]
    fn test_exit_on_spread_flip() {
        // Active pair, spread flips: long_rate=0.0001, short_rate=0.0003 → spread=-0.0002 < 0
        let mut pair = make_pair("0.0001", "0.0003");
        pair.status = PairStatus::Active;
        pair.long_leg.position_size = dec("40");
        pair.short_leg.position_size = dec("-40");

        let decider = make_decider();
        let action = decider.evaluate_inner_impl(&pair);

        assert!(matches!(action, DeciderAction::Exit));
    }

    // -----------------------------------------------------------------------
    // Exit with threshold: spread drops below exit_threshold
    // -----------------------------------------------------------------------
    #[test]
    fn test_exit_with_threshold() {
        let mut pair = make_pair("0.0002", "0.0001");
        pair.status = PairStatus::Active;
        pair.long_leg.position_size = dec("40");
        pair.short_leg.position_size = dec("-40");

        // exit_threshold = 0.00005 → spread=0.0001 ≥ 0.00005 → hold
        let decider = PerpPerpDecider::new(
            dec("0.0002"),
            Some(dec("0.00005")),
            dec("1000"),
            dec("100"),
            dec("10"),
        );
        let action = decider.evaluate_inner_impl(&pair);
        assert!(matches!(action, DeciderAction::DoNothing));

        // Now spread drops below exit_threshold
        let mut pair2 = make_pair("0.00006", "0.00005");
        pair2.status = PairStatus::Active;
        pair2.long_leg.position_size = dec("40");
        pair2.short_leg.position_size = dec("-40");

        let action2 = decider.evaluate_inner_impl(&pair2);
        assert!(matches!(action2, DeciderAction::Exit));
    }

    // -----------------------------------------------------------------------
    // Hold: active with good spread, no drift → DoNothing
    // -----------------------------------------------------------------------
    #[test]
    fn test_hold_active_good_spread() {
        let mut pair = make_pair("0.0005", "0.0001");
        pair.status = PairStatus::Active;
        pair.long_leg.position_size = dec("40");
        pair.short_leg.position_size = dec("-40");

        let decider = make_decider();
        let action = decider.evaluate_inner_impl(&pair);

        assert!(matches!(action, DeciderAction::DoNothing));
    }

    // -----------------------------------------------------------------------
    // Rebalance: delta drift exceeds threshold
    // -----------------------------------------------------------------------
    #[test]
    fn test_rebalance_on_drift() {
        let mut pair = make_pair("0.0005", "0.0001");
        pair.status = PairStatus::Active;
        // Simulate drift: long leg gained, short leg stable
        pair.long_leg.position_size = dec("50"); // 50 * 2.5 = 125
        pair.short_leg.position_size = dec("-35"); // -35 * 2.5 = -87.5
        // total_delta = 125 - 87.5 = 37.5, drift_limit = 100 * 10% = 10 → exceeds

        let decider = make_decider();
        let action = decider.evaluate_inner_impl(&pair);

        match action {
            DeciderAction::Rebalance {
                size_long,
                size_short,
            } => {
                // half_delta = 37.5 / 2 = 18.75
                // long_correction = -18.75 / 2.5 = -7.5
                // short_correction = -18.75 / 2.5 = -7.5
                assert_eq!(size_long, dec("-7.5"));
                assert_eq!(size_short, dec("-7.5"));
            }
            other => panic!("Expected Rebalance, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Orphan guard: Inactive but positions exist → DoNothing
    // -----------------------------------------------------------------------
    #[test]
    fn test_orphan_guard() {
        let mut pair = make_pair("0.0005", "0.0001");
        pair.long_leg.position_size = dec("10"); // orphaned
        // status is Inactive

        let decider = make_decider();
        let action = decider.evaluate_inner_impl(&pair);

        assert!(matches!(action, DeciderAction::DoNothing));
    }

    // -----------------------------------------------------------------------
    // Negative carry direction: reversed spread calculation
    // -----------------------------------------------------------------------
    #[test]
    fn test_negative_carry_direction() {
        let mut pair = PerpPerpPair::new(
            long_key(),
            short_key(),
            CarryDirection::Negative,
            Duration::from_secs(60),
        );
        pair.long_leg.funding_rate = dec("0.0001");
        pair.short_leg.funding_rate = dec("0.0005");
        pair.long_leg.current_price = dec("2.5");
        pair.short_leg.current_price = dec("2.5");
        pair.target_notional = dec("100");

        // Negative carry: spread = short - long = 0.0004 ≥ 0.0002 → Enter
        let decider = make_decider();
        let action = decider.evaluate_inner_impl(&pair);

        assert!(matches!(action, DeciderAction::Enter { .. }));
    }
}
