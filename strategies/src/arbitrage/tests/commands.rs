#[cfg(test)]
mod tests {
    use crate::arbitrage::engine::ArbitrageEngine;
    use crate::arbitrage::margin::MarginManager;
    use crate::arbitrage::oracle::PriceOracle;
    use crate::arbitrage::perp_perp::decider::PerpPerpDecider;
    use crate::arbitrage::perp_perp::stubs::StubExecution;
    use crate::arbitrage::perp_perp::types::{CarryDirection, PerpPerpPair};
    use crate::arbitrage::types::{PairStatus, StrategyCommand};
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
        let decider = PerpPerpDecider::new(dec("0.0002"), None, dec("100"), dec("10"));
        let margin = MarginManager::new(dec("5.0"), dec("15.0"));
        let exec = StubExecution::new();
        ArbitrageEngine::new(pair, oracle, decider, margin, exec)
    }

    /// Send funding events that produce a good spread and enter the pair.
    async fn enter(tx: &mpsc::Sender<boldtrax_core::zmq::protocol::ZmqEvent>) {
        tx.send(funding_event(long_key(), "0.0005")).await.unwrap();
        tx.send(funding_event(short_key(), "0.0001")).await.unwrap();
    }

    /// Small delay to let the engine's select loop process buffered messages.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // ===================================================================
    // Pause -> should suppress evaluation
    // ===================================================================

    #[tokio::test]
    async fn test_pause_suppresses_evaluation() {
        let engine = build_engine();
        let (tx, rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let handle = tokio::spawn(async move { engine.run_with_commands(rx, cmd_rx).await });

        // Enter -> Active
        enter(&tx).await;
        settle().await;

        // Pause
        cmd_tx.send(StrategyCommand::Pause).await.unwrap();
        settle().await;

        // Send events that would normally trigger exit, but paused
        tx.send(funding_event(long_key(), "0.0001")).await.unwrap();
        tx.send(funding_event(short_key(), "0.0005")).await.unwrap();
        settle().await;

        drop(tx);
        drop(cmd_tx);

        let (pair, stub) = handle.await.unwrap();

        assert!(
            stub.actions.iter().any(|a| a.starts_with("enter")),
            "expected enter, got: {:?}",
            stub.actions
        );
        assert!(
            !stub.actions.iter().any(|a| a == "exit"),
            "expected no exit while paused, got: {:?}",
            stub.actions
        );
        assert_eq!(pair.status, PairStatus::Active);
    }

    // ===================================================================
    // Pause -> Resume -> should re-enable evaluation
    // ===================================================================

    #[tokio::test]
    async fn test_resume_re_enables_evaluation() {
        let engine = build_engine();
        let (tx, rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let handle = tokio::spawn(async move { engine.run_with_commands(rx, cmd_rx).await });

        // Enter -> Active
        enter(&tx).await;
        settle().await;

        // Pause
        cmd_tx.send(StrategyCommand::Pause).await.unwrap();
        settle().await;

        // Flip funding (would exit if not paused)
        tx.send(funding_event(long_key(), "0.0001")).await.unwrap();
        tx.send(funding_event(short_key(), "0.0005")).await.unwrap();
        settle().await;

        // Resume -- triggers immediate evaluate_and_execute
        cmd_tx.send(StrategyCommand::Resume).await.unwrap();
        settle().await;

        drop(tx);
        drop(cmd_tx);

        let (pair, stub) = handle.await.unwrap();

        assert!(
            stub.actions.iter().any(|a| a == "exit"),
            "expected exit after resume, got: {:?}",
            stub.actions
        );
        assert_eq!(pair.status, PairStatus::Inactive);
    }

    // ===================================================================
    // ForceExit -- market-close both legs
    // ===================================================================

    #[tokio::test]
    async fn test_force_exit_from_active() {
        let engine = build_engine();
        let (tx, rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let handle = tokio::spawn(async move { engine.run_with_commands(rx, cmd_rx).await });

        // Enter -> Active
        enter(&tx).await;
        settle().await;

        // Force exit
        cmd_tx.send(StrategyCommand::ForceExit).await.unwrap();
        settle().await;

        drop(tx);
        drop(cmd_tx);

        let (pair, stub) = handle.await.unwrap();

        assert!(
            stub.actions.iter().any(|a| a == "exit"),
            "expected exit from ForceExit, got: {:?}",
            stub.actions
        );
        assert_eq!(pair.status, PairStatus::Inactive);
    }

    // ===================================================================
    // GracefulExit -- should trigger unwind
    // ===================================================================

    #[tokio::test]
    async fn test_graceful_exit_from_active() {
        let engine = build_engine();
        let (tx, rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let handle = tokio::spawn(async move { engine.run_with_commands(rx, cmd_rx).await });

        // Enter -> Active
        enter(&tx).await;
        settle().await;

        // GracefulExit -- should start unwind
        cmd_tx.send(StrategyCommand::GracefulExit).await.unwrap();
        settle().await;

        drop(tx);
        drop(cmd_tx);

        let (_pair, stub) = handle.await.unwrap();

        assert!(
            stub.actions.iter().any(|a| a.starts_with("unwind")),
            "expected unwind from GracefulExit, got: {:?}",
            stub.actions
        );
    }

    // ===================================================================
    // GracefulExit ignored when Inactive
    // ===================================================================

    #[tokio::test]
    async fn test_graceful_exit_ignored_when_inactive() {
        let engine = build_engine();
        let (tx, rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let handle = tokio::spawn(async move { engine.run_with_commands(rx, cmd_rx).await });

        // Stay Inactive -- send command directly
        cmd_tx.send(StrategyCommand::GracefulExit).await.unwrap();
        settle().await;

        drop(tx);
        drop(cmd_tx);

        let (pair, stub) = handle.await.unwrap();

        assert!(
            !stub.actions.iter().any(|a| a.starts_with("unwind")),
            "GracefulExit should be ignored when Inactive, got: {:?}",
            stub.actions
        );
        assert_eq!(pair.status, PairStatus::Inactive);
    }

    // ===================================================================
    // GetStatus -- watch channel publishes correct status
    // ===================================================================

    #[tokio::test]
    async fn test_status_watch_reflects_pair_state() {
        let engine = build_engine().with_strategy_id("test-strat");
        let status_rx = engine.status_watch();

        let (tx, rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let handle = tokio::spawn(async move { engine.run_with_commands(rx, cmd_rx).await });

        // Enter -> Active
        enter(&tx).await;
        settle().await;

        // Request status refresh
        cmd_tx.send(StrategyCommand::GetStatus).await.unwrap();
        settle().await;

        drop(tx);
        drop(cmd_tx);

        let _ = handle.await.unwrap();

        let latest = status_rx.borrow().clone();
        match latest {
            crate::arbitrage::types::StrategyResponse::Status(s) => {
                assert_eq!(s.strategy_id, "test-strat");
                assert!(!s.paused);
            }
            other => panic!("Expected Status, got {:?}", other),
        }
    }

    // ===================================================================
    // ForceExit ignored when fully exited
    // ===================================================================

    #[tokio::test]
    async fn test_force_exit_ignored_when_already_exited() {
        let engine = build_engine();
        let (tx, rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let handle = tokio::spawn(async move { engine.run_with_commands(rx, cmd_rx).await });

        // Already inactive -- ForceExit should be skipped
        cmd_tx.send(StrategyCommand::ForceExit).await.unwrap();
        settle().await;

        drop(tx);
        drop(cmd_tx);

        let (pair, stub) = handle.await.unwrap();

        assert!(
            !stub.actions.iter().any(|a| a == "exit"),
            "ForceExit should be skipped when already exited, got: {:?}",
            stub.actions
        );
        assert_eq!(pair.status, PairStatus::Inactive);
    }
}
