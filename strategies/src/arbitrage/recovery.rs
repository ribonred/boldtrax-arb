//! Smart recovery order management for arbitrage strategies.
//!
//! When one leg fills but the other fails or gets cancelled, the strategy
//! enters `Recovering`.  Instead of blindly retrying limit orders on a
//! fixed timer, this module makes an economic decision each tick:
//!
//! | Decision      | Trigger                                           |
//! |---------------|---------------------------------------------------|
//! | Rest          | No adverse move, order is competitive             |
//! | Reprice       | Resting order drifted from current best           |
//! | Chase         | Unrealized loss exceeds crossing cost             |
//! | AbortAndReset | Loss exceeds safety cap, profit exceeds crossing  |
//! |               | cost, or hard time backstop                       |
//!
//! All thresholds are computed dynamically from position notional —
//! larger positions automatically get tighter risk behavior.

use std::time::Duration;

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Hard time backstop — close the filled leg and reset if recovery
/// takes longer than this.  Prevents sitting one-legged indefinitely
/// in a dead market.
pub const MAX_RECOVERY_SECS: u64 = 60;

/// Maximum acceptable loss as a fraction of position notional.
/// 0.3% — at $500 notional that's $1.50, at $50K that's $150.
const ABORT_NOTIONAL_FRACTION: Decimal = dec!(0.003);

/// Assumed taker fee rate for crossing-cost calculations.
/// Used to decide when chasing (market order) is cheaper than waiting.
const TAKER_FEE_RATE: Decimal = dec!(0.0005); // 5 bps

/// Inputs for the recovery assessment — computed by the caller each tick.
#[derive(Debug)]
pub struct RecoveryContext {
    /// Unrealized P&L of the filled leg (positive = profit, negative = loss).
    pub filled_pnl: Decimal,
    /// Absolute notional value of the filled leg position.
    pub filled_notional: Decimal,
    /// Whether the resting limit order is worse than current market best.
    /// Ignored when `has_resting_order` is false.
    pub order_is_stale: bool,
    /// Whether there is currently a limit order resting on the book.
    pub has_resting_order: bool,
    /// Time elapsed since recovery started.
    pub elapsed: Duration,
}

/// What the recovery manager should do this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryDecision {
    /// Let the current limit order rest — nothing to do.
    Rest,
    /// Cancel the resting order and re-place at current best price.
    /// Also used when no order exists yet (place fresh limit).
    Reprice,
    /// Market order the missing leg — loss exceeds crossing cost, so
    /// chasing is cheaper than resetting.
    Chase,
    /// Close the filled leg at market and reset to Inactive.
    /// Triggered by profit (bank it), large loss (cut it), or timeout.
    AbortAndReset,
}

/// Pure economic assessment — no side effects.
///
/// Priority order (first matching rule wins):
/// 1. Time backstop         → AbortAndReset
/// 2. Large loss (> 0.3%)   → AbortAndReset
/// 3. Profit > crossing cost → AbortAndReset (bank the free money)
/// 4. Loss > crossing cost   → Chase (cheaper than resetting)
/// 5. No resting order       → Reprice (place fresh limit)
/// 6. Stale order            → Reprice
/// 7. Otherwise              → Rest
pub fn assess_recovery(ctx: &RecoveryContext) -> RecoveryDecision {
    let crossing_cost = ctx.filled_notional * TAKER_FEE_RATE;
    let abort_threshold = ctx.filled_notional * ABORT_NOTIONAL_FRACTION;

    // 1. Hard time backstop — don't sit one-legged forever.
    if ctx.elapsed.as_secs() >= MAX_RECOVERY_SECS {
        return RecoveryDecision::AbortAndReset;
    }

    // 2. Large loss — cut the position before it gets worse.
    if ctx.filled_pnl < -abort_threshold {
        return RecoveryDecision::AbortAndReset;
    }

    // 3. Profit exceeds crossing cost — bank the free money and re-enter clean.
    if ctx.filled_pnl > crossing_cost {
        return RecoveryDecision::AbortAndReset;
    }

    // 4. Small loss exceeds crossing cost — cheaper to market the missing
    //    leg than to close + re-enter both sides.
    if ctx.filled_pnl < -crossing_cost {
        return RecoveryDecision::Chase;
    }

    // 5. No resting order — need to place a fresh limit.
    if !ctx.has_resting_order {
        return RecoveryDecision::Reprice;
    }

    // 6. Resting order is worse than current market — reprice it.
    if ctx.order_is_stale {
        return RecoveryDecision::Reprice;
    }

    // 7. Everything looks fine — let the maker order rest.
    RecoveryDecision::Rest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(pnl: Decimal, notional: Decimal, stale: bool, has_order: bool, secs: u64) -> RecoveryContext {
        RecoveryContext {
            filled_pnl: pnl,
            filled_notional: notional,
            order_is_stale: stale,
            has_resting_order: has_order,
            elapsed: Duration::from_secs(secs),
        }
    }

    #[test]
    fn time_backstop_triggers_abort() {
        let c = ctx(Decimal::ZERO, dec!(1000), false, true, 61);
        assert_eq!(assess_recovery(&c), RecoveryDecision::AbortAndReset);
    }

    #[test]
    fn large_loss_triggers_abort() {
        // 0.3% of 1000 = 3.0, pnl = -4.0 → abort
        let c = ctx(dec!(-4.0), dec!(1000), false, true, 5);
        assert_eq!(assess_recovery(&c), RecoveryDecision::AbortAndReset);
    }

    #[test]
    fn profit_above_crossing_cost_triggers_abort() {
        // crossing = 1000 × 0.0005 = 0.5, pnl = 0.6 → abort (bank it)
        let c = ctx(dec!(0.6), dec!(1000), false, true, 5);
        assert_eq!(assess_recovery(&c), RecoveryDecision::AbortAndReset);
    }

    #[test]
    fn loss_above_crossing_cost_triggers_chase() {
        // crossing = 1000 × 0.0005 = 0.5, pnl = -0.8 → chase
        // (but not abort because 0.8 < 3.0 abort threshold)
        let c = ctx(dec!(-0.8), dec!(1000), false, true, 5);
        assert_eq!(assess_recovery(&c), RecoveryDecision::Chase);
    }

    #[test]
    fn no_resting_order_triggers_reprice() {
        let c = ctx(Decimal::ZERO, dec!(1000), false, false, 5);
        assert_eq!(assess_recovery(&c), RecoveryDecision::Reprice);
    }

    #[test]
    fn stale_order_triggers_reprice() {
        let c = ctx(Decimal::ZERO, dec!(1000), true, true, 5);
        assert_eq!(assess_recovery(&c), RecoveryDecision::Reprice);
    }

    #[test]
    fn small_pnl_competitive_order_rests() {
        // pnl = -0.2, crossing = 0.5 → loss < crossing → rest
        let c = ctx(dec!(-0.2), dec!(1000), false, true, 5);
        assert_eq!(assess_recovery(&c), RecoveryDecision::Rest);
    }

    #[test]
    fn zero_pnl_competitive_order_rests() {
        let c = ctx(Decimal::ZERO, dec!(1000), false, true, 5);
        assert_eq!(assess_recovery(&c), RecoveryDecision::Rest);
    }

    #[test]
    fn small_profit_below_crossing_cost_rests() {
        // profit = 0.3, crossing = 0.5 → not enough to bank → rest
        let c = ctx(dec!(0.3), dec!(1000), false, true, 5);
        assert_eq!(assess_recovery(&c), RecoveryDecision::Rest);
    }

    #[test]
    fn time_backstop_overrides_small_loss() {
        // loss = -0.3 (< crossing), but time = 65s → abort
        let c = ctx(dec!(-0.3), dec!(1000), false, true, 65);
        assert_eq!(assess_recovery(&c), RecoveryDecision::AbortAndReset);
    }

    #[test]
    fn large_loss_overrides_time() {
        // Both triggers fire — large loss checked before time in code,
        // but time is checked first. Either way → abort.
        let c = ctx(dec!(-10.0), dec!(1000), false, true, 65);
        assert_eq!(assess_recovery(&c), RecoveryDecision::AbortAndReset);
    }

    #[test]
    fn chase_not_triggered_when_loss_below_crossing_cost() {
        // crossing = 0.5, loss = 0.4 → too small to chase → rest
        let c = ctx(dec!(-0.4), dec!(1000), false, true, 5);
        assert_eq!(assess_recovery(&c), RecoveryDecision::Rest);
    }

    #[test]
    fn large_notional_scales_thresholds() {
        // notional = 50_000 → crossing = 25, abort = 150
        // loss = -30 → above crossing (25) but below abort (150) → chase
        let c = ctx(dec!(-30), dec!(50000), false, true, 5);
        assert_eq!(assess_recovery(&c), RecoveryDecision::Chase);
    }

    #[test]
    fn large_notional_abort_threshold() {
        // notional = 50_000 → abort = 150, loss = -200 → abort
        let c = ctx(dec!(-200), dec!(50000), false, true, 5);
        assert_eq!(assess_recovery(&c), RecoveryDecision::AbortAndReset);
    }
}
