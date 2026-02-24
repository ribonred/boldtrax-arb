use boldtrax_core::types::{InstrumentKey, Position};
use boldtrax_core::zmq::protocol::ZmqEvent;
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
}

// ---------------------------------------------------------------------------
// Pair status and actions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairStatus {
    /// No open position — waiting for entry signal.
    Inactive,
    /// Both legs filled and hedged.
    Active,
}

/// Action produced by a [`DecisionPolicy`](super::policy::DecisionPolicy).
/// Generic across all pair types — every arb strategy has a long side
/// and a short side that can enter, rebalance, or exit.
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
}
