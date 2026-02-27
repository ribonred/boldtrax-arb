use std::collections::HashMap;

use boldtrax_core::types::{InstrumentKey, Order, OrderEvent, Position};
use boldtrax_core::zmq::protocol::ZmqEvent;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Shared leg types — used by multiple pair strategies
// ---------------------------------------------------------------------------

/// A perpetual/swap leg of an arbitrage pair.
///
/// Tracks position size, entry price, funding rate, and current mark price.
/// Used by both spot-perp and perp-perp strategies.
#[derive(Debug, Clone)]
pub struct PerpLeg {
    /// Swap/perp instrument key (exchange, pair, InstrumentType::Swap).
    pub key: InstrumentKey,
    /// Position size — negative when short.
    pub position_size: Decimal,
    /// Entry price from PositionUpdate.
    pub entry_price: Decimal,
    /// Mark / mid price from the oracle.
    pub current_price: Decimal,
    /// Current funding rate — only perps have this.
    pub funding_rate: Decimal,
    /// Best bid from the latest orderbook snapshot.
    pub best_bid: Option<Decimal>,
    /// Best ask from the latest orderbook snapshot.
    pub best_ask: Option<Decimal>,
    /// Sum of quantity across top 3 ask levels.
    pub ask_depth: Option<Decimal>,
    /// Sum of quantity across top 3 bid levels.
    pub bid_depth: Option<Decimal>,
}

impl PerpLeg {
    pub fn new(key: InstrumentKey) -> Self {
        Self {
            key,
            position_size: Decimal::ZERO,
            entry_price: Decimal::ZERO,
            current_price: Decimal::ZERO,
            funding_rate: Decimal::ZERO,
            best_bid: None,
            best_ask: None,
            ask_depth: None,
            bid_depth: None,
        }
    }

    /// Notional value of the perp position (negative when short).
    pub fn notional(&self) -> Decimal {
        self.position_size * self.current_price
    }

    /// Check if available depth can absorb `size`.
    ///
    /// Positive `size` checks ask depth (buying); negative checks bid depth (selling).
    /// Returns `true` if depth >= |size| **or** if depth is unknown.
    pub fn depth_sufficient(&self, size: Decimal) -> bool {
        if size.is_zero() {
            return true;
        }
        let depth = if size.is_sign_positive() {
            self.ask_depth
        } else {
            self.bid_depth
        };
        match depth {
            Some(d) => d >= size.abs(),
            None => true, // unknown depth → allow
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairStatus {
    /// No open position — waiting for entry signal.
    Inactive,
    /// Orders placed, waiting for both legs to fill.
    Entering,
    /// Both legs filled and hedged.
    Active,
    /// Exit orders placed, waiting for both legs to close.
    Exiting,
    /// One leg orphaned — recovery orders in flight to restore the pair.
    Recovering,
    /// Command-driven graceful unwind — chunked close in progress.
    /// No new entries or rebalances accepted until fully closed.
    Unwinding,
}

impl PairStatus {
    /// Returns `true` for statuses where orders are in flight and the
    /// decider should not emit new actions.
    pub fn is_transitional(&self) -> bool {
        matches!(
            self,
            PairStatus::Entering
                | PairStatus::Exiting
                | PairStatus::Recovering
                | PairStatus::Unwinding
        )
    }
}

#[derive(Debug, Clone)]
pub enum DeciderAction {
    DoNothing,
    Enter {
        size_long: Decimal,
        size_short: Decimal,
    },
    Rebalance {
        size_long: Decimal,
        size_short: Decimal,
    },
    Exit,
    /// Re-enter the missing leg of an orphaned pair.
    Recover {
        size_long: Decimal,
        size_short: Decimal,
    },
    /// Command-driven graceful unwind — close a chunk of the position.
    Unwind {
        size_long: Decimal,
        size_short: Decimal,
    },
}

pub trait PairState: std::fmt::Debug + Send + Sync {
    /// Net delta exposure of the hedged position.
    fn total_delta(&self) -> Decimal;

    /// Current lifecycle state.
    fn status(&self) -> &PairStatus;

    /// Transition the pair to a new lifecycle state.
    fn set_status(&mut self, status: PairStatus);

    /// Return the `InstrumentKey` for each leg — used for generic logging
    /// (e.g. by [`PaperExecution`](super::paper::PaperExecution)).
    fn leg_keys(&self) -> Vec<InstrumentKey>;

    /// Access the order tracker (immutable).
    fn order_tracker(&self) -> &OrderTracker;

    /// Access the order tracker (mutable).
    fn order_tracker_mut(&mut self) -> &mut OrderTracker;

    /// Process a pair-specific ZmqEvent, updating internal state.
    /// Returns `true` when the engine should trigger decision evaluation.
    /// Universal events (OrderBook, Ticker) are routed by the engine, not here.
    fn apply_event(&mut self, event: ZmqEvent) -> bool;

    /// Refresh current prices by looking up each leg's key in the oracle.
    fn refresh_prices(&mut self, mid_price: &dyn Fn(&InstrumentKey) -> Option<Decimal>);

    /// Refresh best bid/ask from the oracle for smart order placement.
    /// Default no-op — override in pair types that need orderbook-aware execution.
    fn refresh_orderbook(
        &mut self,
        _best_bid: &dyn Fn(&InstrumentKey) -> Option<Decimal>,
        _best_ask: &dyn Fn(&InstrumentKey) -> Option<Decimal>,
        _ask_depth: &dyn Fn(&InstrumentKey) -> Option<Decimal>,
        _bid_depth: &dyn Fn(&InstrumentKey) -> Option<Decimal>,
    ) {
    }

    /// Return `(position, current_price)` pairs that need margin checks.
    /// Spot legs return nothing; perp legs return their position.
    fn positions_for_margin_check(&self) -> Vec<(&Position, Decimal)>;

    /// Returns `true` when all legs have non-zero positions (fully entered).
    /// Used by the engine to decide when `Entering` → `Active` is safe.
    fn is_fully_entered(&self) -> bool;

    /// Returns `true` when all legs have zero positions (fully closed).
    /// Used by the engine to decide when `Exiting` → `Inactive` is safe.
    fn is_fully_exited(&self) -> bool;

    /// Signed sizes that would fully close both legs.
    /// Used by the engine for command-driven exits (GracefulExit → Unwind).
    /// Returns `(long_close, short_close)` — typically negated position sizes.
    fn close_sizes(&self) -> (Decimal, Decimal);

    /// Check if orderbook depth is sufficient for the given order sizes.
    ///
    /// Buying (+size) checks ask depth; selling (-size) checks bid depth.
    /// Returns `true` if enough liquidity exists **or** if depth data is
    /// unavailable (gracefully degrades when the oracle hasn't reported yet).
    ///
    /// Used by the engine to gate entries and rebalances — exits/unwinds are
    /// never depth-gated because closing is mandatory.
    fn has_sufficient_depth(&self, size_long: Decimal, size_short: Decimal) -> bool;

    /// Compute sizes needed to recover from a one-legged state.
    ///
    /// Returns `Some((size_long, size_short))` where the filled leg's size is
    /// zero and the missing leg's size is computed to match the existing
    /// position's notional. Returns `None` if both legs are present, both
    /// are empty, or prices aren't available yet.
    fn recovery_sizes(&self) -> Option<(Decimal, Decimal)> {
        None
    }
}

/// Lifecycle status of a single tracked order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackedOrderStatus {
    /// Order submitted, waiting for exchange acknowledgement.
    Pending,
    /// Exchange accepted the order; it is resting on the book.
    Open,
    /// At least one partial fill received.
    PartiallyFilled,
    /// Fully filled — terminal state.
    Filled,
    /// Cancelled by us or the exchange — terminal state.
    Cancelled,
    /// Rejected by exchange — terminal state.
    Rejected,
}

impl TrackedOrderStatus {
    /// Terminal order states — no further transitions expected.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TrackedOrderStatus::Filled
                | TrackedOrderStatus::Cancelled
                | TrackedOrderStatus::Rejected
        )
    }

    /// Convert from an [`OrderEvent`] variant to the corresponding tracker status.
    pub fn from_order_event(evt: &OrderEvent) -> Self {
        match evt {
            OrderEvent::New(_) => TrackedOrderStatus::Open,
            OrderEvent::PartiallyFilled(_) => TrackedOrderStatus::PartiallyFilled,
            OrderEvent::Filled(_) => TrackedOrderStatus::Filled,
            OrderEvent::Canceled(_) => TrackedOrderStatus::Cancelled,
            OrderEvent::Rejected(_) => TrackedOrderStatus::Rejected,
        }
    }
}

/// A single order being tracked within a pair.
#[derive(Debug, Clone)]
pub struct TrackedOrder {
    /// Client-assigned or exchange-assigned order ID.
    pub order_id: String,
    /// Which leg this order belongs to.
    pub key: InstrumentKey,
    /// Current lifecycle status.
    pub status: TrackedOrderStatus,
    /// Requested quantity (signed: positive buy, negative sell).
    pub requested_qty: Decimal,
    /// Cumulative filled quantity so far (always positive).
    pub filled_qty: Decimal,
    /// When the order was submitted.
    pub submitted_at: DateTime<Utc>,
    /// Limit price the order was placed at (None for market orders).
    pub order_price: Option<Decimal>,
}

/// Tracks a gradual position unwind across multiple chunks.
///
/// Created when `GracefulExit` is received.  Each chunk closes a fraction
/// of the remaining position, computed at dispatch time from actual
/// `close_sizes()` so partial fills are handled naturally.
#[derive(Debug, Clone)]
pub struct UnwindSchedule {
    /// Total number of chunks to divide the unwind into.
    pub num_chunks: u32,
    /// Number of chunks already dispatched.
    pub chunks_dispatched: u32,
}

impl UnwindSchedule {
    pub fn new(num_chunks: u32) -> Self {
        Self {
            num_chunks: num_chunks.max(1),
            chunks_dispatched: 0,
        }
    }

    /// Compute the next chunk sizes from the current remaining position.
    /// Returns `None` if all chunks have been dispatched.
    pub fn next_chunk(
        &self,
        remaining_long: Decimal,
        remaining_short: Decimal,
    ) -> Option<(Decimal, Decimal)> {
        if self.is_complete() {
            return None;
        }
        let remaining_chunks = Decimal::from(self.num_chunks - self.chunks_dispatched);
        let chunk_long = remaining_long / remaining_chunks;
        let chunk_short = remaining_short / remaining_chunks;
        Some((chunk_long, chunk_short))
    }

    /// Mark one chunk as dispatched.
    pub fn advance(&mut self) {
        self.chunks_dispatched += 1;
    }

    /// `true` if all chunks have been dispatched.
    pub fn is_complete(&self) -> bool {
        self.chunks_dispatched >= self.num_chunks
    }
}

/// Default number of chunks for a gradual unwind.
pub const DEFAULT_UNWIND_CHUNKS: u32 = 4;

/// Tracks all in-flight orders for a single pair.
///
/// The engine feeds order events into this tracker so the pair always
/// knows which orders are pending, open, or partially filled. Deciders
/// and status-transition logic use this to decide when it's safe to
/// act (e.g. transition from `Entering` → `Active`).
#[derive(Debug, Clone)]
pub struct OrderTracker {
    orders: HashMap<String, TrackedOrder>,
    /// Active unwind schedule, if any.
    pub unwind_schedule: Option<UnwindSchedule>,
}

impl OrderTracker {
    pub fn new() -> Self {
        Self {
            orders: HashMap::new(),
            unwind_schedule: None,
        }
    }

    /// Record a newly submitted order.
    pub fn track(&mut self, order: TrackedOrder) {
        self.orders.insert(order.order_id.clone(), order);
    }

    /// Update the status of an existing tracked order by ID.
    /// Returns `false` if the order was not being tracked.
    pub fn update_status(&mut self, order_id: &str, status: TrackedOrderStatus) -> bool {
        if let Some(tracked) = self.orders.get_mut(order_id) {
            tracked.status = status;
            true
        } else {
            false
        }
    }

    /// Update filled quantity and status together.
    pub fn update_fill(
        &mut self,
        order_id: &str,
        filled_qty: Decimal,
        status: TrackedOrderStatus,
    ) -> bool {
        if let Some(tracked) = self.orders.get_mut(order_id) {
            tracked.filled_qty = filled_qty;
            tracked.status = status;
            true
        } else {
            false
        }
    }

    /// All pending/open/partially-filled orders for a specific leg.
    pub fn pending_for_leg(&self, key: &InstrumentKey) -> Vec<&TrackedOrder> {
        self.orders
            .values()
            .filter(|o| &o.key == key && !o.status.is_terminal())
            .collect()
    }

    /// `true` if any order is still in a non-terminal state.
    pub fn has_pending(&self) -> bool {
        self.orders.values().any(|o| !o.status.is_terminal())
    }

    /// Count of orders still in a non-terminal state.
    pub fn pending_count(&self) -> usize {
        self.orders
            .values()
            .filter(|o| !o.status.is_terminal())
            .count()
    }

    /// Orders that have been pending/open longer than `max_age`.
    pub fn stale_orders(&self, max_age: std::time::Duration) -> Vec<&TrackedOrder> {
        let cutoff = Utc::now() - chrono::Duration::from_std(max_age).unwrap_or_default();
        self.orders
            .values()
            .filter(|o| !o.status.is_terminal() && o.submitted_at < cutoff)
            .collect()
    }

    /// Remove all orders in a terminal state (garbage collection).
    pub fn remove_terminal(&mut self) {
        self.orders.retain(|_, o| !o.status.is_terminal());
    }

    /// Create and track a `TrackedOrder` from a submitted [`Order`].
    ///
    /// Uses `client_order_id` as the tracking key — this is the ID that
    /// appears in subsequent [`OrderEvent`]s from the exchange.
    pub fn track_from_order(&mut self, order: &Order, key: InstrumentKey, requested_qty: Decimal) {
        self.track(TrackedOrder {
            order_id: order.client_order_id.clone(),
            key,
            status: TrackedOrderStatus::Pending,
            requested_qty,
            filled_qty: Decimal::ZERO,
            submitted_at: order.created_at,
            order_price: order.request.price,
        });
    }

    /// Convenience: track only if the order was actually placed (non-zero size).
    pub fn track_if_some(
        &mut self,
        order: Option<Order>,
        key: InstrumentKey,
        requested_qty: Decimal,
    ) {
        if let Some(ref o) = order {
            self.track_from_order(o, key, requested_qty);
        }
    }

    /// Get a tracked order by ID.
    pub fn get(&self, order_id: &str) -> Option<&TrackedOrder> {
        self.orders.get(order_id)
    }

    /// Number of tracked orders (including terminal).
    pub fn len(&self) -> usize {
        self.orders.len()
    }

    /// `true` if no orders are being tracked.
    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }

    pub fn all_pending(&self) -> Vec<&TrackedOrder> {
        self.orders
            .values()
            .filter(|o| !o.status.is_terminal())
            .collect()
    }

    pub fn force_cancel(&mut self, order_id: &str) {
        if let Some(tracked) = self.orders.get_mut(order_id) {
            tracked.status = TrackedOrderStatus::Cancelled;
        }
    }
}

impl Default for OrderTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute the next unwind chunk action from the schedule and remaining sizes.
///
/// Shared helper used by both `ArbitrageEngine` and `PerpPerpPoller` to
/// avoid duplicating the unwind-continuation logic.
pub fn next_unwind_action(
    schedule: &mut UnwindSchedule,
    remaining_long: Decimal,
    remaining_short: Decimal,
) -> Option<DeciderAction> {
    if let Some((chunk_long, chunk_short)) = schedule.next_chunk(remaining_long, remaining_short) {
        schedule.advance();
        tracing::info!(
            chunk = schedule.chunks_dispatched,
            total = schedule.num_chunks,
            long = %chunk_long,
            short = %chunk_short,
            "Dispatching unwind chunk"
        );
        Some(DeciderAction::Unwind {
            size_long: chunk_long,
            size_short: chunk_short,
        })
    } else {
        None
    }
}

// Re-export from core — the canonical definition lives in the ZMQ protocol.
pub use boldtrax_core::zmq::protocol::{StrategyCommand, StrategyResponse, StrategyStatus};
