use boldtrax_core::zmq::client::ZmqEventSubscriber;
use boldtrax_core::zmq::protocol::StrategyCommand;
use strum::{Display, EnumString};
use tokio::sync::mpsc;

use crate::arbitrage::engine::ArbitrageEngine;
use crate::arbitrage::margin::MarginManager;
use crate::arbitrage::oracle::PriceOracle;
use crate::arbitrage::paper::PaperExecution;
use crate::arbitrage::perp_perp::poller::PerpPerpPoller;
use crate::arbitrage::spot_perp::decider::SpotRebalanceDecider;
use crate::arbitrage::spot_perp::execution::SpotPerpExecutionEngine;
use crate::arbitrage::spot_perp::types::SpotPerpPair;

/// Strategy variant names — matches the `strategy` field in `[runner.strategies]`.
/// Used at startup to dispatch into the correct `StrategyRunner` variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, Display)]
pub enum StrategyKind {
    SpotPerp,
    PerpPerp,
}

/// Live funding-spread arbitrage: spot leg vs perp leg, real execution.
pub type SpotPerpEngine = ArbitrageEngine<
    SpotPerpPair,
    SpotRebalanceDecider,
    SpotPerpExecutionEngine,
    MarginManager,
    PriceOracle,
>;

/// Paper funding-spread arbitrage: same logic, simulated execution.
pub type SpotPerpPaperEngine = ArbitrageEngine<
    SpotPerpPair,
    SpotRebalanceDecider,
    PaperExecution<SpotPerpPair, SpotPerpExecutionEngine>,
    MarginManager,
    PriceOracle,
>;
pub type PerpPerpLivePoller = PerpPerpPoller<MarginManager, PriceOracle>;

// Note: PaperExecution wraps the ExecutionPolicy, not the poller itself.
// For paper mode, we swap the execution engine inside the poller.
// This is handled at construction time in strategy.rs.

pub enum StrategyRunner {
    SpotPerp(
        SpotPerpEngine,
        ZmqEventSubscriber,
        mpsc::Receiver<StrategyCommand>,
    ),
    SpotPerpPaper(
        SpotPerpPaperEngine,
        ZmqEventSubscriber,
        mpsc::Receiver<StrategyCommand>,
    ),
    PerpPerp(
        Box<PerpPerpLivePoller>,
        ZmqEventSubscriber,
        ZmqEventSubscriber,
        mpsc::Receiver<StrategyCommand>,
    ),
}

impl StrategyRunner {
    pub async fn run(self) {
        match self {
            StrategyRunner::SpotPerp(engine, sub, cmd_rx) => engine.run(sub, cmd_rx).await,
            StrategyRunner::SpotPerpPaper(engine, sub, cmd_rx) => engine.run(sub, cmd_rx).await,
            StrategyRunner::PerpPerp(poller, long_sub, short_sub, cmd_rx) => {
                poller.run(long_sub, short_sub, cmd_rx).await;
            }
        }
    }
}
