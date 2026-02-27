#[cfg(test)]
mod tests {
    use crate::arbitrage::engine::ArbitrageEngine;
    use crate::arbitrage::margin::MarginManager;
    use crate::arbitrage::oracle::PriceOracle;
    use crate::arbitrage::paper::PaperExecution;
    use crate::arbitrage::perp_perp::decider::PerpPerpDecider;
    use crate::arbitrage::perp_perp::stubs::StubExecution;
    use crate::arbitrage::perp_perp::types::{CarryDirection, PerpPerpPair};
    use crate::arbitrage::types::PairStatus;
    use boldtrax_core::types::{
        Exchange, FundingInterval, FundingRateSnapshot, InstrumentKey, InstrumentType, Pairs,
    };
    use chrono::Utc;
    use rust_decimal::Decimal;
    use std::str::FromStr;
    use std::time::Duration;
    use tokio::sync::mpsc;

    type TestEngine =
        ArbitrageEngine<PerpPerpPair, PerpPerpDecider, StubExecution, MarginManager, PriceOracle>;

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

    fn funding_event(key: InstrumentKey, rate: &str) -> boldtrax_core::zmq::protocol::ZmqEvent {
        boldtrax_core::zmq::protocol::ZmqEvent::FundingRate(FundingRateSnapshot {
            key,
            funding_rate: dec(rate),
            mark_price: dec("2.5"),
            index_price: dec("2.5"),
            interest_rate: None,
            next_funding_time_utc: Utc::now(),
            next_funding_time_ms: 0,
            event_time_utc: Utc::now(),
            event_time_ms: 0,
            interval: FundingInterval::Every8Hours,
        })
    }

    fn build_engine() -> TestEngine {
        let pair = PerpPerpPair::new(
            long_key(),
            short_key(),
            CarryDirection::Positive,
            Duration::from_secs(60),
        );
        let oracle = PriceOracle::new();
        // min_spread=0.0002, exit_threshold=None, target=100, drift=10%
        let decider = PerpPerpDecider::new(dec("0.0002"), None, dec("100"), dec("10"));
        let margin = MarginManager::new(dec("5.0"), dec("15.0"));
        let exec = StubExecution::new();
        ArbitrageEngine::new(pair, oracle, decider, margin, exec)
    }

    // -----------------------------------------------------------------------
    // Scenario 1: Both legs receive funding rates with good spread → Enter
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_enter_on_good_spread() {
        let engine = build_engine();
        let (tx, rx) = mpsc::channel(16);

        // Long leg: 0.0005 funding rate
        tx.send(funding_event(long_key(), "0.0005")).await.unwrap();
        // Short leg: 0.0001 funding rate → spread = 0.0004 ≥ 0.0002
        tx.send(funding_event(short_key(), "0.0001")).await.unwrap();
        drop(tx);

        let (pair, stub) = engine.run_with_stream(rx).await;

        assert_eq!(pair.status, PairStatus::Active);
        assert!(
            stub.actions.iter().any(|a| a.starts_with("enter")),
            "expected enter action, got: {:?}",
            stub.actions
        );
    }

    // -----------------------------------------------------------------------
    // Scenario 2: Only one leg receives funding → Enter on second event
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_no_entry_with_single_leg_funding() {
        let engine = build_engine();
        let (tx, rx) = mpsc::channel(16);

        // Only long leg receives funding — spread is 0.0005 - 0 = 0.0005 ≥ 0.0002
        // But short leg price is 0 → cannot enter (prices guard).
        // Actually short_leg.current_price will be ZERO since it has no funding snapshot yet
        // to seed it. But long price will be seeded from mark_price.
        //
        // Actually let's check: mark_price is 2.5 in funding_event, so long.current_price
        // gets seeded. But short.current_price stays 0. The decider should bail on zero price.
        tx.send(funding_event(long_key(), "0.0005")).await.unwrap();
        drop(tx);

        let (pair, stub) = engine.run_with_stream(rx).await;

        assert_eq!(pair.status, PairStatus::Inactive);
        // Should have evaluated but got DoNothing due to zero price on short leg
        assert!(
            stub.actions.iter().all(|a| a == "noop"),
            "expected only noop actions, got: {:?}",
            stub.actions
        );
    }

    // -----------------------------------------------------------------------
    // Scenario 3: Spread below threshold → DoNothing
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_noop_below_threshold() {
        let engine = build_engine();
        let (tx, rx) = mpsc::channel(16);

        tx.send(funding_event(long_key(), "0.0002")).await.unwrap();
        tx.send(funding_event(short_key(), "0.0001")).await.unwrap();
        // spread = 0.0001 < 0.0002 threshold
        drop(tx);

        let (pair, stub) = engine.run_with_stream(rx).await;

        assert_eq!(pair.status, PairStatus::Inactive);
        assert!(
            stub.actions.iter().all(|a| a == "noop"),
            "expected noop, got: {:?}",
            stub.actions
        );
    }

    // -----------------------------------------------------------------------
    // Scenario 4: Enter → spread flips → Exit
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_full_cycle_enter_then_exit() {
        let engine = build_engine();
        let (tx, rx) = mpsc::channel(16);

        // Step 1: Enter with good spread
        tx.send(funding_event(long_key(), "0.0005")).await.unwrap();
        tx.send(funding_event(short_key(), "0.0001")).await.unwrap();
        // Step 2: Spread flips → exit
        tx.send(funding_event(long_key(), "0.0001")).await.unwrap();
        tx.send(funding_event(short_key(), "0.0005")).await.unwrap();
        drop(tx);

        let (pair, stub) = engine.run_with_stream(rx).await;

        assert_eq!(pair.status, PairStatus::Inactive);
        assert!(
            stub.actions.iter().any(|a| a.starts_with("enter")),
            "enter missing: {:?}",
            stub.actions
        );
        assert!(
            stub.actions.iter().any(|a| a == "exit"),
            "exit missing: {:?}",
            stub.actions
        );
    }

    // -----------------------------------------------------------------------
    // Scenario 5: Funding event for unrelated key is ignored
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_unrelated_funding_ignored() {
        let engine = build_engine();
        let (tx, rx) = mpsc::channel(16);

        let unrelated_key = InstrumentKey {
            exchange: Exchange::Binance,
            pair: Pairs::BTCUSDT,
            instrument_type: InstrumentType::Swap,
        };
        tx.send(funding_event(unrelated_key, "0.005"))
            .await
            .unwrap();
        drop(tx);

        let (pair, stub) = engine.run_with_stream(rx).await;

        assert_eq!(pair.status, PairStatus::Inactive);
        assert!(
            stub.actions.is_empty(),
            "expected no actions for unrelated key, got: {:?}",
            stub.actions
        );
    }

    // -----------------------------------------------------------------------
    // Scenario 6: PaperExecution wrapping StubExecution composes correctly
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_paper_engine_enter_cycle() {
        let pair = PerpPerpPair::new(
            long_key(),
            short_key(),
            CarryDirection::Positive,
            Duration::from_secs(60),
        );
        let decider = PerpPerpDecider::new(dec("0.0002"), None, dec("100"), dec("10"));
        let margin = MarginManager::new(dec("5.0"), dec("15.0"));
        let paper = PaperExecution::new(StubExecution::new());
        let engine: ArbitrageEngine<
            PerpPerpPair,
            PerpPerpDecider,
            PaperExecution<PerpPerpPair, StubExecution>,
            MarginManager,
            PriceOracle,
        > = ArbitrageEngine::new(pair, PriceOracle::new(), decider, margin, paper);

        let (tx, rx) = mpsc::channel(16);
        tx.send(funding_event(long_key(), "0.0005")).await.unwrap();
        tx.send(funding_event(short_key(), "0.0001")).await.unwrap();
        drop(tx);

        let (pair, _paper_exec) = engine.run_with_stream(rx).await;
        assert_eq!(pair.status, PairStatus::Active);
    }
}
