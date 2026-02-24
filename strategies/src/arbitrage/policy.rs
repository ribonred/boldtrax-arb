use async_trait::async_trait;
use boldtrax_core::AccountSnapshot;
use boldtrax_core::types::{InstrumentKey, OrderBookSnapshot, Position, Ticker};
use rust_decimal::Decimal;
use thiserror::Error;

use crate::arbitrage::types::{DeciderAction, PairState};

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum MarginViolation {
    #[error("leverage {leverage} exceeds max {max}")]
    LeverageExceeded { leverage: Decimal, max: Decimal },

    #[error("liquidation distance {distance_pct:.2}% is below safety buffer {buffer:.2}%")]
    LiquidationTooClose {
        distance_pct: Decimal,
        buffer: Decimal,
    },
}

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("order submission failed for {key}: {reason}")]
    OrderFailed { key: String, reason: String },
}

// ---------------------------------------------------------------------------
// DecisionPolicy
// ---------------------------------------------------------------------------

/// Evaluates the current state of an arbitrage pair and returns the action
/// that should be taken. Implementors provide `evaluate_inner`; the default
/// `evaluate` wrapper adds structured tracing and metrics counters.
pub trait DecisionPolicy<P: PairState>: Send + Sync {
    fn name(&self) -> &'static str;

    /// Core decision logic — implement this.
    fn evaluate_inner(&self, pair: &P) -> DeciderAction;

    /// Template method: calls `evaluate_inner` and emits a structured trace
    /// event as well as a `monotonic_counter` field for metrics subscribers.
    /// Do NOT override this unless you need to replace the logging contract.
    fn evaluate(&self, pair: &P) -> DeciderAction {
        let action = self.evaluate_inner(pair);
        tracing::info!(
            policy = self.name(),
            action = ?action,
            total_delta = %pair.total_delta(),
            status = ?pair.status(),
            monotonic_counter.decisions_evaluated = 1,
            "Decision evaluated"
        );
        action
    }
}

// ---------------------------------------------------------------------------
// MarginPolicy
// ---------------------------------------------------------------------------

/// Guards execution by checking account-level health and individual position
/// safety margins. Two-phase: account (leverage ratios) and position
/// (liquidation distance). Implementors provide both `_inner` methods; the
/// default wrappers add structured tracing and metrics counters.
///
/// Swap in `SpotMarginPolicy` (where `check_position_inner` is always `Ok`)
/// to handle strategies with no liquidation price concept.
pub trait MarginPolicy: Send + Sync {
    fn name(&self) -> &'static str;

    /// Account-level check (leverage ratios, total exposure). Implement this.
    fn check_account_inner(&self, snapshot: &AccountSnapshot) -> Result<(), MarginViolation>;

    /// Position-level check (liquidation distance). Implement this.
    /// `spot_price` is the current live mid-price for the instrument.
    fn check_position_inner(
        &self,
        position: &Position,
        spot_price: Decimal,
    ) -> Result<(), MarginViolation>;

    /// Template method: wraps `check_account_inner` with tracing.
    fn check_account(&self, snapshot: &AccountSnapshot) -> Result<(), MarginViolation> {
        let result = self.check_account_inner(snapshot);
        match &result {
            Ok(()) => {
                tracing::debug!(
                    policy = self.name(),
                    exchange = ?snapshot.exchange,
                    monotonic_counter.margin_account_checks_passed = 1,
                    "Account margin check passed"
                );
            }
            Err(v) => {
                tracing::warn!(
                    policy = self.name(),
                    exchange = ?snapshot.exchange,
                    violation = %v,
                    monotonic_counter.margin_violations = 1,
                    "Account margin violation"
                );
            }
        }
        result
    }

    fn check_position(
        &self,
        position: &Position,
        spot_price: Decimal,
    ) -> Result<(), MarginViolation> {
        let result = self.check_position_inner(position, spot_price);
        match &result {
            Ok(()) => {
                tracing::debug!(
                    policy = self.name(),
                    key = ?position.key,
                    spot_price = %spot_price,
                    monotonic_counter.margin_position_checks_passed = 1,
                    "Position margin check passed"
                );
            }
            Err(v) => {
                tracing::warn!(
                    policy = self.name(),
                    key = ?position.key,
                    spot_price = %spot_price,
                    violation = %v,
                    monotonic_counter.margin_violations = 1,
                    "Position margin violation"
                );
            }
        }
        result
    }
}

// ---------------------------------------------------------------------------
// PriceSource
// ---------------------------------------------------------------------------

/// Provides price data for instruments, both from a local cache (sync) and
/// via a remote fetch (async). Implementors provide the `_inner` methods and
/// update methods; templates add cache-miss logging and fetch metrics.
pub trait PriceSource: Send + Sync {
    fn name(&self) -> &'static str;

    /// Ingest a fresh order-book snapshot into the cache.
    fn update_snapshot(&mut self, snapshot: OrderBookSnapshot);

    /// Ingest a fresh ticker into the cache.
    fn update_ticker(&mut self, ticker: Ticker);

    /// Inner mid-price lookup — implement this.
    fn get_mid_price_inner(&self, key: &InstrumentKey) -> Option<Decimal>;

    /// Inner best-bid lookup — implement this.
    fn get_best_bid_inner(&self, key: &InstrumentKey) -> Option<Decimal>;

    /// Inner best-ask lookup — implement this.
    fn get_best_ask_inner(&self, key: &InstrumentKey) -> Option<Decimal>;

    /// Template method: returns mid-price from cache with cache-miss logging.
    fn get_mid_price(&self, key: &InstrumentKey) -> Option<Decimal> {
        let price = self.get_mid_price_inner(key);
        if price.is_none() {
            tracing::debug!(
                source = self.name(),
                key = ?key,
                monotonic_counter.price_cache_misses = 1,
                "Mid-price cache miss"
            );
        }
        price
    }

    fn get_best_bid(&self, key: &InstrumentKey) -> Option<Decimal> {
        self.get_best_bid_inner(key)
    }

    fn get_best_ask(&self, key: &InstrumentKey) -> Option<Decimal> {
        self.get_best_ask_inner(key)
    }

    /// Sum of quantity across the top N ask levels (default 3).
    fn get_ask_depth(&self, _key: &InstrumentKey) -> Option<Decimal> {
        None
    }

    /// Sum of quantity across the top N bid levels (default 3).
    fn get_bid_depth(&self, _key: &InstrumentKey) -> Option<Decimal> {
        None
    }
}

// ---------------------------------------------------------------------------
// ExecutionPolicy
// ---------------------------------------------------------------------------

/// Translates a `DeciderAction` into exchange orders. Implementors provide
/// `execute_inner`; the default `execute` wrapper logs the action, calls
/// `execute_inner`, and emits success/failure metrics.
///
/// Wrap with `PaperExecution<E>` to intercept real orders for simulation.
#[async_trait]
pub trait ExecutionPolicy<P: PairState>: Send + Sync {
    fn name(&self) -> &'static str;

    /// Core execution logic — implement this.
    async fn execute_inner(
        &mut self,
        action: &DeciderAction,
        pair: &mut P,
    ) -> Result<(), ExecutionError>;

    /// Template method: logs the action, delegates to `execute_inner`, and
    /// records outcome metrics. Do NOT override unless replacing the logging
    /// contract entirely.
    async fn execute(
        &mut self,
        action: &DeciderAction,
        pair: &mut P,
    ) -> Result<(), ExecutionError> {
        tracing::info!(
            policy = self.name(),
            action = ?action,
            pair_status = ?pair.status(),
            "Executing strategy action"
        );
        let result = self.execute_inner(action, pair).await;
        match &result {
            Ok(()) => {
                tracing::info!(
                    policy = self.name(),
                    monotonic_counter.orders_executed = 1,
                    "Execution succeeded"
                );
            }
            Err(e) => {
                tracing::error!(
                    policy = self.name(),
                    error = %e,
                    monotonic_counter.execution_errors = 1,
                    "Execution failed"
                );
            }
        }
        result
    }
}
