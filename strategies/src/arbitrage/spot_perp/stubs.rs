use async_trait::async_trait;

use crate::arbitrage::policy::{ExecutionError, ExecutionPolicy};
use crate::arbitrage::spot_perp::types::SpotPerpPair;
use crate::arbitrage::types::{DeciderAction, PairStatus};

/// Zero-network execution stub for integration tests.
///
/// Records every dispatched action as a plain string and immediately
/// transitions `pair.status` to simulate instant order fills:
///
/// | Action      | Status after  |
/// |-------------|---------------|
/// | `Enter`     | `Active`      |
/// | `Rebalance` | `Active`      |
/// | `Exit`      | `Inactive`    |
/// | `DoNothing` | unchanged     |
pub struct StubExecution {
    pub actions: Vec<String>,
}

impl StubExecution {
    pub fn new() -> Self {
        Self {
            actions: Vec::new(),
        }
    }

    pub fn last_action(&self) -> Option<&str> {
        self.actions.last().map(String::as_str)
    }
}

impl Default for StubExecution {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutionPolicy<SpotPerpPair> for StubExecution {
    fn name(&self) -> &'static str {
        "stub_execution"
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
                self.actions
                    .push(format!("enter long={size_long} short={size_short}"));
                pair.status = PairStatus::Active;
            }
            DeciderAction::Rebalance {
                size_long,
                size_short,
            } => {
                self.actions
                    .push(format!("rebalance long={size_long} short={size_short}"));
                pair.status = PairStatus::Active;
            }
            DeciderAction::Exit => {
                self.actions.push("exit".to_string());
                pair.status = PairStatus::Inactive;
            }
            DeciderAction::Recover {
                size_long,
                size_short,
            } => {
                self.actions
                    .push(format!("recover long={size_long} short={size_short}"));
                pair.status = PairStatus::Active;
            }
            DeciderAction::Unwind {
                size_long,
                size_short,
            } => {
                self.actions
                    .push(format!("unwind long={size_long} short={size_short}"));
                pair.status = PairStatus::Inactive;
            }
            DeciderAction::DoNothing => {
                self.actions.push("noop".to_string());
            }
        }
        Ok(())
    }
}
