use async_trait::async_trait;
use std::marker::PhantomData;

use crate::arbitrage::policy::{ExecutionError, ExecutionPolicy};
use crate::arbitrage::types::{DeciderAction, PairState, PairStatus};

use boldtrax_core::types::Exchange;

/// Paper-trading decorator for any [`ExecutionPolicy`].
///
/// Wraps a real execution engine and intercepts all orders, logging simulated
/// fills without forwarding anything to the exchange. Swap in `PaperExecution<P, E>`
/// at composition time to run any strategy in simulation without code changes.
///
/// Generic over the pair type `P` so the same decorator works for spot-perp,
/// perp-perp, or any future pair strategy.
pub struct PaperExecution<P, E> {
    inner: E,
    _pair: PhantomData<P>,
}

impl<P, E> PaperExecution<P, E> {
    pub fn new(inner: E) -> Self {
        Self {
            inner,
            _pair: PhantomData,
        }
    }

    /// Access to the underlying execution engine (e.g. for inspection in tests).
    pub fn inner(&self) -> &E {
        &self.inner
    }
}

#[async_trait]
impl<P, E> ExecutionPolicy<P> for PaperExecution<P, E>
where
    P: PairState,
    E: ExecutionPolicy<P> + Send,
{
    fn name(&self) -> &'static str {
        "paper_execution"
    }

    async fn cancel_order(
        &mut self,
        exchange: Exchange,
        order_id: String,
    ) -> Result<(), ExecutionError> {
        tracing::info!(
            mode = "paper",
            %exchange,
            %order_id,
            "PAPER: simulated cancel"
        );
        Ok(())
    }

    async fn execute_inner(
        &mut self,
        action: &DeciderAction,
        pair: &mut P,
    ) -> Result<(), ExecutionError> {
        let keys = pair.leg_keys();
        match action {
            DeciderAction::Enter {
                size_long,
                size_short,
            } => {
                tracing::info!(
                    mode = "paper",
                    legs = ?keys,
                    size_long = %size_long,
                    size_short = %size_short,
                    monotonic_counter.paper_orders_simulated = 2,
                    "PAPER: simulated Enter fill"
                );
                pair.set_status(PairStatus::Active);
            }
            DeciderAction::Rebalance {
                size_long,
                size_short,
            } => {
                tracing::info!(
                    mode = "paper",
                    legs = ?keys,
                    size_long = %size_long,
                    size_short = %size_short,
                    monotonic_counter.paper_orders_simulated = 2,
                    "PAPER: simulated Rebalance fill"
                );
                pair.set_status(PairStatus::Active);
            }
            DeciderAction::Exit => {
                tracing::info!(
                    mode = "paper",
                    legs = ?keys,
                    monotonic_counter.paper_orders_simulated = 2,
                    "PAPER: simulated Exit fill"
                );
                pair.set_status(PairStatus::Inactive);
            }
            DeciderAction::Recover {
                size_long,
                size_short,
            } => {
                tracing::info!(
                    mode = "paper",
                    legs = ?keys,
                    size_long = %size_long,
                    size_short = %size_short,
                    monotonic_counter.paper_orders_simulated = 1,
                    "PAPER: simulated Recover fill"
                );
                pair.set_status(PairStatus::Active);
            }
            DeciderAction::Unwind {
                size_long,
                size_short,
            } => {
                tracing::info!(
                    mode = "paper",
                    legs = ?keys,
                    size_long = %size_long,
                    size_short = %size_short,
                    monotonic_counter.paper_orders_simulated = 2,
                    "PAPER: simulated Unwind chunk"
                );
                // In paper mode, assume the chunk fully closes the position.
                pair.set_status(PairStatus::Inactive);
            }
            DeciderAction::DoNothing => {}
        }
        Ok(())
    }
}
