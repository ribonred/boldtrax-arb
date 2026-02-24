use crate::arbitrage::perp_perp::types::PerpPerpPair;
use crate::arbitrage::policy::DecisionPolicy;
use crate::arbitrage::types::{DeciderAction, PairState, PairStatus};
use rust_decimal::Decimal;
use tracing::{debug, info, warn};

/// Spread-based decision policy for perp-vs-perp funding rate arbitrage.
///
/// The "spread" is `funding_spread()` on the pair — directional based on
/// the user's configured [`CarryDirection`](super::types::CarryDirection).
/// A positive spread means the carry is profitable.
pub struct PerpPerpDecider {
    /// Minimum `|spread|` to justify opening a new position.
    pub min_spread_threshold: Decimal,
    /// Optional exit threshold. If set, exit when spread drops below this.
    /// If `None`, exit immediately when spread < 0 (sign flip).
    pub exit_threshold: Option<Decimal>,
    /// Maximum position size per symbol (USD).
    pub max_position_size: Decimal,
    /// Target dollar exposure per side.
    pub target_notional: Decimal,
    /// Delta drift threshold as a percentage of `target_notional`.
    pub rebalance_drift_pct: Decimal,
}

impl PerpPerpDecider {
    pub fn new(
        min_spread_threshold: Decimal,
        exit_threshold: Option<Decimal>,
        max_position_size: Decimal,
        target_notional: Decimal,
        rebalance_drift_pct: Decimal,
    ) -> Self {
        Self {
            min_spread_threshold,
            exit_threshold,
            max_position_size,
            target_notional,
            rebalance_drift_pct,
        }
    }

    pub fn evaluate_inner_impl(&self, pair: &PerpPerpPair) -> DeciderAction {
        let spread = pair.funding_spread();
        let total_delta = pair.total_delta();

        debug!(
            funding_spread = %spread,
            long_rate = %pair.long_leg.funding_rate,
            short_rate = %pair.short_leg.funding_rate,
            total_delta = %total_delta,
            status = ?pair.status,
            "Evaluating perp-perp pair"
        );

        match pair.status {
            PairStatus::Inactive => {
                // Guard: orphaned positions from a failed prior entry.
                if !pair.long_leg.position_size.is_zero() || !pair.short_leg.position_size.is_zero()
                {
                    warn!(
                        long_size = %pair.long_leg.position_size,
                        short_size = %pair.short_leg.position_size,
                        "Inactive but legs have positions, waiting for status transition"
                    );
                    return DeciderAction::DoNothing;
                }

                if spread >= self.min_spread_threshold {
                    let long_price = pair.long_leg.current_price;
                    let short_price = pair.short_leg.current_price;

                    if long_price.is_zero() || short_price.is_zero() {
                        warn!(
                            long_price = %long_price,
                            short_price = %short_price,
                            "Cannot enter: prices not yet available"
                        );
                        return DeciderAction::DoNothing;
                    }

                    let long_qty = self.target_notional / long_price;
                    let short_qty = self.target_notional / short_price;

                    info!(
                        spread = %spread,
                        threshold = %self.min_spread_threshold,
                        long_qty = %long_qty,
                        short_qty = %short_qty,
                        "Spread above threshold, entering trade"
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
                // Exit check: use exit_threshold if configured, otherwise exit on sign flip.
                let should_exit = match self.exit_threshold {
                    Some(threshold) => spread < threshold,
                    None => spread < Decimal::ZERO,
                };

                if should_exit {
                    info!(
                        spread = %spread,
                        exit_threshold = ?self.exit_threshold,
                        "Spread below exit threshold, exiting trade"
                    );
                    return DeciderAction::Exit;
                }

                // Rebalance on delta drift.
                let drift_limit =
                    self.target_notional * self.rebalance_drift_pct / Decimal::new(100, 0);

                if total_delta.abs() > drift_limit {
                    let long_price = pair.long_leg.current_price;
                    let short_price = pair.short_leg.current_price;

                    if long_price.is_zero() || short_price.is_zero() {
                        warn!(
                            long_price = %long_price,
                            short_price = %short_price,
                            "Cannot rebalance: prices not yet available"
                        );
                        return DeciderAction::DoNothing;
                    }

                    let half_delta = total_delta / Decimal::new(2, 0);
                    let long_correction = -half_delta / long_price;
                    let short_correction = -half_delta / short_price;

                    info!(
                        total_delta = %total_delta,
                        long_correction = %long_correction,
                        short_correction = %short_correction,
                        "Delta drift detected, rebalancing"
                    );

                    DeciderAction::Rebalance {
                        size_long: long_correction,
                        size_short: short_correction,
                    }
                } else {
                    DeciderAction::DoNothing
                }
            }
        }
    }
}

impl DecisionPolicy<PerpPerpPair> for PerpPerpDecider {
    fn name(&self) -> &'static str {
        "perp_perp_spread_decider"
    }

    fn evaluate_inner(&self, pair: &PerpPerpPair) -> DeciderAction {
        self.evaluate_inner_impl(pair)
    }
}
