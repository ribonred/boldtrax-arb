use crate::arbitrage::policy::DecisionPolicy;
use crate::arbitrage::spot_perp::types::SpotPerpPair;
use crate::arbitrage::types::{DeciderAction, PairState, PairStatus};
use rust_decimal::Decimal;
use tracing::{debug, info, warn};

pub struct SpotRebalanceDecider {
    /// Minimum perp funding rate to justify entering (e.g. 0.001 = 0.1%).
    pub min_funding_threshold: Decimal,
    pub max_position_size: Decimal,
    pub target_notional: Decimal,
    /// Delta drift threshold as a percentage of `target_notional`.
    /// E.g. 10 = trigger rebalance when delta exceeds 10% of target.
    pub rebalance_drift_pct: Decimal,
}

impl SpotRebalanceDecider {
    pub fn new(
        min_funding_threshold: Decimal,
        max_position_size: Decimal,
        target_notional: Decimal,
        rebalance_drift_pct: Decimal,
    ) -> Self {
        Self {
            min_funding_threshold,
            max_position_size,
            target_notional,
            rebalance_drift_pct,
        }
    }

    pub fn evaluate_inner_impl(&self, pair: &SpotPerpPair) -> DeciderAction {
        let funding = pair.funding_rate();
        let total_delta = pair.total_delta();

        debug!(
            funding_rate = %funding,
            total_delta = %total_delta,
            status = ?pair.status,
            "Evaluating spot-perp pair"
        );

        match pair.status {
            PairStatus::Inactive => {
                // Guard: if perp already has a position but spot doesn't (orphaned
                // from a failed prior entry), don't try to enter again — the
                // position fills will trigger the transition to Active, and
                // the next evaluation will handle rebalancing.
                if !pair.perp.position_size.is_zero() || !pair.spot.quantity.is_zero() {
                    warn!(
                        spot_qty = %pair.spot.quantity,
                        perp_size = %pair.perp.position_size,
                        "Inactive but legs have positions, waiting for status transition"
                    );
                    return DeciderAction::DoNothing;
                }

                if funding >= self.min_funding_threshold {
                    let spot_price = pair.spot.current_price;
                    let perp_price = pair.perp.current_price;

                    if spot_price.is_zero() || perp_price.is_zero() {
                        warn!(
                            spot_price = %spot_price,
                            perp_price = %perp_price,
                            "Cannot enter: prices not yet available"
                        );
                        return DeciderAction::DoNothing;
                    }

                    let long_qty = self.target_notional / spot_price;
                    let short_qty = self.target_notional / perp_price;

                    info!(
                        "Perp funding {} >= threshold {}, entering trade (spot_qty={}, perp_qty={})",
                        funding, self.min_funding_threshold, long_qty, short_qty
                    );
                    DeciderAction::Enter {
                        size_long: long_qty,
                        size_short: -short_qty,
                    }
                } else {
                    DeciderAction::DoNothing
                }
            }
            PairStatus::Active => {
                if funding < Decimal::ZERO {
                    info!("Perp funding {} < 0, exiting trade", funding);
                    DeciderAction::Exit
                } else if total_delta.abs()
                    > self.target_notional * self.rebalance_drift_pct / Decimal::new(100, 0)
                {
                    let spot_price = pair.spot.current_price;
                    let perp_price = pair.perp.current_price;

                    if spot_price.is_zero() || perp_price.is_zero() {
                        warn!(
                            spot_price = %spot_price,
                            perp_price = %perp_price,
                            "Cannot rebalance: prices not yet available"
                        );
                        return DeciderAction::DoNothing;
                    }

                    // The delta is the net notional imbalance.
                    // To rebalance, each leg corrects half the delta toward zero:
                    //   - Positive delta → we're net long → sell spot, buy back perp
                    //   - Negative delta → we're net short → buy spot, sell perp
                    // Both legs move in the SAME direction: -half_delta / price.
                    let half_delta = total_delta / Decimal::new(2, 0);
                    // Spot: -half_delta means buy when delta<0, sell when delta>0
                    let spot_correction_qty = -half_delta / spot_price;
                    // Perp: -half_delta means buy back when delta<0, sell when delta>0
                    let perp_correction_qty = -half_delta / perp_price;

                    info!(
                        total_delta = %total_delta,
                        spot_correction = %spot_correction_qty,
                        perp_correction = %perp_correction_qty,
                        "Delta drift detected, rebalancing"
                    );
                    DeciderAction::Rebalance {
                        size_long: spot_correction_qty,
                        size_short: perp_correction_qty,
                    }
                } else {
                    DeciderAction::DoNothing
                }
            }
        }
    }
}

impl DecisionPolicy<SpotPerpPair> for SpotRebalanceDecider {
    fn name(&self) -> &'static str {
        "funding_rate_decider"
    }

    fn evaluate_inner(&self, pair: &SpotPerpPair) -> DeciderAction {
        self.evaluate_inner_impl(pair)
    }
}
