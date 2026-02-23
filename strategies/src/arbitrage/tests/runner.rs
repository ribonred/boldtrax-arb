#[cfg(test)]
mod tests {
    use crate::arbitrage::decider::SpotRebalanceDecider;
    use crate::arbitrage::engine::ArbitrageEngine;
    use crate::arbitrage::margin::MarginManager;
    use crate::arbitrage::oracle::PriceOracle;
    use crate::arbitrage::stubs::StubExecution;
    use crate::arbitrage::types::{PairStatus, SpotPerpPair};
    use boldtrax_core::types::{
        Exchange, FundingInterval, FundingRateSnapshot, InstrumentKey, InstrumentType, Pairs,
    };
    use chrono::Utc;
    use rust_decimal::Decimal;
    use std::str::FromStr;
    use tokio::sync::mpsc;

    type TestEngine = ArbitrageEngine<
        SpotPerpPair,
        SpotRebalanceDecider,
        StubExecution,
        MarginManager,
        PriceOracle,
    >;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    /// Spot leg: BTC spot on Binance — no funding rate, no position.
    fn spot_key() -> InstrumentKey {
        InstrumentKey {
            exchange: Exchange::Binance,
            pair: Pairs::BTCUSDT,
            instrument_type: InstrumentType::Spot,
        }
    }

    /// Perp leg: BTC perpetual on Binance — has funding rate and position.
    fn perp_key() -> InstrumentKey {
        InstrumentKey {
            exchange: Exchange::Binance,
            pair: Pairs::BTCUSDT,
            instrument_type: InstrumentType::Swap,
        }
    }

    /// Create a FundingRate event — only valid for the perp leg.
    fn funding_event(key: InstrumentKey, rate: &str) -> boldtrax_core::zmq::protocol::ZmqEvent {
        boldtrax_core::zmq::protocol::ZmqEvent::FundingRate(FundingRateSnapshot {
            key,
            funding_rate: dec(rate),
            mark_price: dec("50000"),
            index_price: dec("50000"),
            interest_rate: None,
            next_funding_time_utc: Utc::now(),
            next_funding_time_ms: 0,
            event_time_utc: Utc::now(),
            event_time_ms: 0,
            interval: FundingInterval::Every8Hours,
        })
    }

    fn build_engine() -> TestEngine {
        let pair = SpotPerpPair::new(spot_key(), perp_key());
        let oracle = PriceOracle::new();
        // min funding threshold 0.001, max position 100, target notional 1.0, drift 10%
        let decider = SpotRebalanceDecider::new(dec("0.001"), dec("100"), dec("1.0"), dec("10"));
        let margin = MarginManager::new(dec("0.9"), dec("5.0"));
        let exec = StubExecution::new();
        ArbitrageEngine::new(pair, oracle, decider, margin, exec)
    }

    // -----------------------------------------------------------------------
    // Scenario 1: Perp funding rate >= threshold → Enter → pair becomes Active
    // Only the perp leg receives FundingRate events.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_enter_on_positive_funding() {
        let engine = build_engine();
        let (tx, rx) = mpsc::channel(16);

        // Only send funding for the perp key — spot has NO funding.
        tx.send(funding_event(perp_key(), "0.002")).await.unwrap();
        drop(tx);

        let (pair, stub) = engine.run_with_stream(rx).await;

        assert_eq!(pair.status, PairStatus::Active);
        assert!(
            stub.actions
                .first()
                .map(|a| a.starts_with("enter"))
                .unwrap_or(false),
            "expected enter as first action, got: {:?}",
            stub.actions
        );
    }

    // -----------------------------------------------------------------------
    // Scenario 2: Perp funding below threshold → DoNothing → stays Inactive
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_noop_below_threshold() {
        let engine = build_engine();
        let (tx, rx) = mpsc::channel(16);

        tx.send(funding_event(perp_key(), "0.0005")).await.unwrap();
        drop(tx);

        let (pair, stub) = engine.run_with_stream(rx).await;

        assert_eq!(pair.status, PairStatus::Inactive);
        assert!(
            stub.actions.iter().all(|a| a == "noop"),
            "unexpected actions: {:?}",
            stub.actions
        );
    }

    // -----------------------------------------------------------------------
    // Scenario 3: Enter → funding flips negative → Exit → pair Inactive
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_full_cycle_enter_then_exit() {
        let engine = build_engine();
        let (tx, rx) = mpsc::channel(16);

        // Step 1 — Enter: perp funding 0.002 >= threshold 0.001
        tx.send(funding_event(perp_key(), "0.002")).await.unwrap();
        // Step 2 — Exit: perp funding -0.001 < 0
        tx.send(funding_event(perp_key(), "-0.001")).await.unwrap();
        drop(tx);

        let (pair, stub) = engine.run_with_stream(rx).await;

        assert_eq!(
            pair.status,
            PairStatus::Inactive,
            "expected Inactive after full cycle\nactions: {:?}",
            stub.actions
        );
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
    // Scenario 4: FundingRate event for the spot key is ignored.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_funding_for_spot_key_ignored() {
        let engine = build_engine();
        let (tx, rx) = mpsc::channel(16);

        // Send funding for the SPOT key — engine should ignore it entirely.
        tx.send(funding_event(spot_key(), "0.005")).await.unwrap();
        drop(tx);

        let (pair, stub) = engine.run_with_stream(rx).await;

        assert_eq!(pair.status, PairStatus::Inactive);
        // No evaluation should have happened at all.
        assert!(
            stub.actions.is_empty(),
            "expected no actions for spot funding event, got: {:?}",
            stub.actions
        );
    }

    // -----------------------------------------------------------------------
    // Scenario 5: PaperExecution wrapping StubExecution — verifies the
    // decorator composes correctly through run_with_stream without ZMQ.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_paper_engine_enter_cycle() {
        use crate::arbitrage::paper::PaperExecution;

        let pair = SpotPerpPair::new(spot_key(), perp_key());
        let decider = SpotRebalanceDecider::new(dec("0.001"), dec("100"), dec("1.0"), dec("10"));
        let margin = MarginManager::new(dec("0.9"), dec("5.0"));
        let paper = PaperExecution::new(StubExecution::new());
        let engine: ArbitrageEngine<
            SpotPerpPair,
            SpotRebalanceDecider,
            PaperExecution<StubExecution>,
            MarginManager,
            PriceOracle,
        > = ArbitrageEngine::new(pair, PriceOracle::new(), decider, margin, paper);

        let (tx, rx) = mpsc::channel(16);
        // Only perp receives funding.
        tx.send(funding_event(perp_key(), "0.002")).await.unwrap();
        drop(tx);

        let (pair, _paper_exec) = engine.run_with_stream(rx).await;

        assert_eq!(pair.status, PairStatus::Active);
    }
}
