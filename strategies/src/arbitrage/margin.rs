use boldtrax_core::AccountSnapshot;
use boldtrax_core::types::Position;
use rust_decimal::Decimal;
use tracing::warn;

use crate::arbitrage::policy::{MarginPolicy, MarginViolation};

pub struct MarginManager {
    pub max_leverage: Decimal,
    pub liquidation_buffer_pct: Decimal,
}

impl MarginManager {
    pub fn new(max_leverage: Decimal, liquidation_buffer_pct: Decimal) -> Self {
        Self {
            max_leverage,
            liquidation_buffer_pct,
        }
    }
}

impl MarginPolicy for MarginManager {
    fn name(&self) -> &'static str {
        "perp_margin_manager"
    }

    fn check_account_inner(&self, snapshot: &AccountSnapshot) -> Result<(), MarginViolation> {
        for balance in &snapshot.balances {
            if balance.total > Decimal::ZERO {
                let leverage = balance.locked / balance.total;
                if leverage > self.max_leverage {
                    warn!(
                        exchange = ?snapshot.exchange,
                        partition = ?balance.partition,
                        leverage = %leverage,
                        max = %self.max_leverage,
                        "Account leverage limit exceeded"
                    );
                    return Err(MarginViolation::LeverageExceeded {
                        leverage,
                        max: self.max_leverage,
                    });
                }
            }
        }
        Ok(())
    }

    fn check_position_inner(
        &self,
        position: &Position,
        spot_price: Decimal,
    ) -> Result<(), MarginViolation> {
        if let Some(liq_price) = position.liquidation_price {
            if spot_price.is_zero() {
                return Ok(());
            }
            let distance_pct = ((spot_price - liq_price).abs() / spot_price) * Decimal::new(100, 0);
            if distance_pct < self.liquidation_buffer_pct {
                warn!(
                    key = ?position.key,
                    distance_pct = %distance_pct,
                    buffer = %self.liquidation_buffer_pct,
                    "Liquidation distance too close"
                );
                return Err(MarginViolation::LiquidationTooClose {
                    distance_pct,
                    buffer: self.liquidation_buffer_pct,
                });
            }
        }
        Ok(())
    }
}

/// Margin policy for spot-only legs: no account leverage gate, no liquidation
/// price concept. Always returns `Ok`.
pub struct SpotMarginPolicy;

impl MarginPolicy for SpotMarginPolicy {
    fn name(&self) -> &'static str {
        "spot_margin_policy"
    }

    fn check_account_inner(&self, _snapshot: &AccountSnapshot) -> Result<(), MarginViolation> {
        Ok(())
    }

    fn check_position_inner(
        &self,
        _position: &Position,
        _spot_price: Decimal,
    ) -> Result<(), MarginViolation> {
        Ok(())
    }
}
