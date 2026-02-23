use async_trait::async_trait;

use crate::arbitrage::decider::DeciderAction;
use crate::arbitrage::policy::{ExecutionError, ExecutionPolicy};
use crate::arbitrage::types::SpotPerpPair;

/// Paper-trading decorator for any [`ExecutionPolicy`].
///
/// Wraps a real execution engine and intercepts all orders, logging simulated
/// fills without forwarding anything to the exchange. Swap in `PaperExecution<E>`
/// at composition time to run any strategy in simulation without code changes.
pub struct PaperExecution<E> {
    inner: E,
}

impl<E> PaperExecution<E> {
    pub fn new(inner: E) -> Self {
        Self { inner }
    }

    /// Access to the underlying execution engine (e.g. for inspection in tests).
    pub fn inner(&self) -> &E {
        &self.inner
    }
}

#[async_trait]
impl<E: ExecutionPolicy<SpotPerpPair> + Send> ExecutionPolicy<SpotPerpPair> for PaperExecution<E> {
    fn name(&self) -> &'static str {
        "paper_execution"
    }

    async fn execute_inner(
        &mut self,
        action: &DeciderAction,
        pair: &mut SpotPerpPair,
    ) -> Result<(), ExecutionError> {
        match action {
            DeciderAction::Enter {
                size_long,
                size_short,
            } => {
                tracing::info!(
                    mode = "paper",
                    spot = ?pair.spot.key,
                    perp = ?pair.perp.key,
                    size_long = %size_long,
                    size_short = %size_short,
                    monotonic_counter.paper_orders_simulated = 2,
                    "PAPER: simulated Enter fill"
                );
                pair.status = crate::arbitrage::types::PairStatus::Active;
            }
            DeciderAction::Rebalance {
                size_long,
                size_short,
            } => {
                tracing::info!(
                    mode = "paper",
                    spot = ?pair.spot.key,
                    perp = ?pair.perp.key,
                    size_long = %size_long,
                    size_short = %size_short,
                    monotonic_counter.paper_orders_simulated = 2,
                    "PAPER: simulated Rebalance fill"
                );
                pair.status = crate::arbitrage::types::PairStatus::Active;
            }
            DeciderAction::Exit => {
                tracing::info!(
                    mode = "paper",
                    spot = ?pair.spot.key,
                    perp = ?pair.perp.key,
                    monotonic_counter.paper_orders_simulated = 2,
                    "PAPER: simulated Exit fill"
                );
                pair.status = crate::arbitrage::types::PairStatus::Inactive;
            }
            DeciderAction::DoNothing => {}
        }
        Ok(())
    }
}
