use boldtrax_core::zmq::client::ZmqEventSubscriber;
use strum::{Display, EnumString};

use crate::arbitrage::decider::SpotRebalanceDecider;
use crate::arbitrage::engine::ArbitrageEngine;
use crate::arbitrage::execution::ExecutionEngine;
use crate::arbitrage::margin::MarginManager;
use crate::arbitrage::oracle::PriceOracle;
use crate::arbitrage::paper::PaperExecution;
use crate::arbitrage::types::SpotPerpPair;

/// Strategy variant names — matches the `strategy` field in `[runner.strategies]`.
/// Used at startup to dispatch into the correct `StrategyRunner` variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, Display)]
pub enum StrategyKind {
    SpotPerp,
}

/// Live funding-spread arbitrage: spot leg vs perp leg, real execution.
pub type SpotPerpEngine = ArbitrageEngine<
    SpotPerpPair,
    SpotRebalanceDecider,
    ExecutionEngine,
    MarginManager,
    PriceOracle,
>;

/// Paper funding-spread arbitrage: same logic, simulated execution.
pub type SpotPerpPaperEngine = ArbitrageEngine<
    SpotPerpPair,
    SpotRebalanceDecider,
    PaperExecution<ExecutionEngine>,
    MarginManager,
    PriceOracle,
>;

pub enum StrategyRunner {
    SpotPerp(SpotPerpEngine, ZmqEventSubscriber),
    SpotPerpPaper(SpotPerpPaperEngine, ZmqEventSubscriber),
}

impl StrategyRunner {
    pub async fn run(self) {
        match self {
            StrategyRunner::SpotPerp(engine, sub) => engine.run(sub).await,
            StrategyRunner::SpotPerpPaper(engine, sub) => engine.run(sub).await,
        }
    }
}
