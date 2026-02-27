use std::collections::HashMap;
use std::time::{Duration, Instant};

use boldtrax_core::types::{FundingRateSnapshot, InstrumentKey, Position};
use boldtrax_core::zmq::protocol::ZmqEvent;
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::arbitrage::types::{OrderTracker, PairState, PairStatus, PerpLeg};

/// (`Positive` — most common) or paying funding (`Negative` — rare,
/// useful for hedging or when funding is inverted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CarryDirection {
    /// Long leg has higher funding rate → receives net funding payment.
    Positive,
    /// Long leg has lower funding rate → pays net funding (inverse arb).
    Negative,
}

// ---------------------------------------------------------------------------
// Funding rate cache — in-memory, per-strategy
// ---------------------------------------------------------------------------

/// Simple in-memory cache for funding rate snapshots with staleness tracking.
///
/// On each poll tick the poller checks `get_if_fresh()` first. If the cached
/// value is within `staleness_threshold` it is reused; otherwise the poller
/// falls back to querying the exchange's ZMQ `GetFundingRate` command.
#[derive(Debug, Clone)]
pub struct FundingCache {
    entries: HashMap<InstrumentKey, (FundingRateSnapshot, Instant)>,
    staleness_threshold: Duration,
}

impl FundingCache {
    pub fn new(staleness_threshold: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            staleness_threshold,
        }
    }

    pub fn get_if_fresh(&self, key: &InstrumentKey) -> Option<&FundingRateSnapshot> {
        self.entries.get(key).and_then(|(snap, ts)| {
            if ts.elapsed() < self.staleness_threshold {
                Some(snap)
            } else {
                None
            }
        })
    }

    pub fn update(&mut self, key: InstrumentKey, snapshot: FundingRateSnapshot) {
        self.entries.insert(key, (snapshot, Instant::now()));
    }
}

#[derive(Debug, Clone)]
pub struct PerpPerpPair {
    /// funding rate (receives funding in positive-carry mode).
    pub long_leg: PerpLeg,
    /// funding rate (pays funding in positive-carry mode).
    pub short_leg: PerpLeg,
    /// How the spread is interpreted — set by user config at startup.
    pub carry_direction: CarryDirection,
    /// Target dollar exposure per side.
    pub target_notional: Decimal,
    /// Current lifecycle state.
    pub status: PairStatus,
    /// Live position for the long leg (updated from PositionUpdate events).
    pub position_long: Option<Position>,
    /// Live position for the short leg (updated from PositionUpdate events).
    pub position_short: Option<Position>,
    /// In-memory funding rate cache.
    pub funding_cache: FundingCache,
    /// In-flight order tracker for both legs.
    pub order_tracker: OrderTracker,
}

impl PerpPerpPair {
    pub fn new(
        long_key: InstrumentKey,
        short_key: InstrumentKey,
        carry_direction: CarryDirection,
        cache_staleness: Duration,
    ) -> Self {
        Self {
            long_leg: PerpLeg::new(long_key),
            short_leg: PerpLeg::new(short_key),
            carry_direction,
            target_notional: Decimal::ZERO,
            status: PairStatus::Inactive,
            position_long: None,
            position_short: None,
            funding_cache: FundingCache::new(cache_staleness),
            order_tracker: OrderTracker::new(),
        }
    }

    pub fn funding_spread(&self) -> Decimal {
        match self.carry_direction {
            CarryDirection::Positive => self.long_leg.funding_rate - self.short_leg.funding_rate,
            CarryDirection::Negative => self.short_leg.funding_rate - self.long_leg.funding_rate,
        }
    }

    /// Update a leg's funding rate + cache from a snapshot.
    /// Returns `true` if the snapshot matched one of our legs.
    fn apply_funding(&mut self, snapshot: FundingRateSnapshot) -> bool {
        if snapshot.key == self.long_leg.key {
            self.long_leg.funding_rate = snapshot.funding_rate;
            if self.long_leg.current_price.is_zero() && !snapshot.mark_price.is_zero() {
                self.long_leg.current_price = snapshot.mark_price;
            }
            self.funding_cache.update(self.long_leg.key, snapshot);
            true
        } else if snapshot.key == self.short_leg.key {
            self.short_leg.funding_rate = snapshot.funding_rate;
            if self.short_leg.current_price.is_zero() && !snapshot.mark_price.is_zero() {
                self.short_leg.current_price = snapshot.mark_price;
            }
            self.funding_cache.update(self.short_leg.key, snapshot);
            true
        } else {
            false
        }
    }

    fn apply_position(&mut self, position: Position) -> bool {
        if position.key == self.long_leg.key {
            self.long_leg.position_size = position.size;
            self.long_leg.entry_price = position.entry_price;
            self.position_long = Some(position);
            tracing::info!(
                leg = "long",
                key = %self.long_leg.key,
                size = %self.long_leg.position_size,
                entry = %self.long_leg.entry_price,
                "Long leg position updated"
            );
            self.maybe_transition_to_active();
            true
        } else if position.key == self.short_leg.key {
            self.short_leg.position_size = position.size;
            self.short_leg.entry_price = position.entry_price;
            self.position_short = Some(position);
            tracing::info!(
                leg = "short",
                key = %self.short_leg.key,
                size = %self.short_leg.position_size,
                entry = %self.short_leg.entry_price,
                "Short leg position updated"
            );
            self.maybe_transition_to_active();
            true
        } else {
            false
        }
    }

    /// Return a mutable reference to the leg matching `key`.
    /// Panics if the key matches neither leg (programming error).
    pub fn leg_mut(&mut self, key: &InstrumentKey) -> &mut PerpLeg {
        if *key == self.long_leg.key {
            &mut self.long_leg
        } else if *key == self.short_leg.key {
            &mut self.short_leg
        } else {
            unreachable!("key {:?} doesn't match either leg", key)
        }
    }

    pub fn maybe_transition_to_active(&mut self) {
        if matches!(self.status, PairStatus::Inactive | PairStatus::Entering)
            && !self.long_leg.position_size.is_zero()
            && !self.short_leg.position_size.is_zero()
        {
            tracing::info!(
                long_size = %self.long_leg.position_size,
                short_size = %self.short_leg.position_size,
                "Both legs filled, transitioning to Active"
            );
            self.status = PairStatus::Active;
        }
    }
}

impl PairState for PerpPerpPair {
    fn total_delta(&self) -> Decimal {
        self.long_leg.notional() + self.short_leg.notional()
    }

    fn status(&self) -> &PairStatus {
        &self.status
    }

    fn set_status(&mut self, status: PairStatus) {
        self.status = status;
    }

    fn leg_keys(&self) -> Vec<InstrumentKey> {
        vec![self.long_leg.key, self.short_leg.key]
    }

    fn order_tracker(&self) -> &OrderTracker {
        &self.order_tracker
    }

    fn order_tracker_mut(&mut self) -> &mut OrderTracker {
        &mut self.order_tracker
    }

    /// Process ZMQ events that arrive via the event stream.
    ///
    /// `FundingRate` updates the matching leg's rate + cache and returns
    /// `true` to trigger evaluation. `PositionUpdate` updates position
    /// state but returns `false` — the poller drives evaluation timing.
    fn apply_event(&mut self, event: ZmqEvent) -> bool {
        match event {
            ZmqEvent::FundingRate(snapshot) => {
                if self.apply_funding(snapshot) {
                    tracing::info!(
                        long_rate = %self.long_leg.funding_rate,
                        short_rate = %self.short_leg.funding_rate,
                        spread = %self.funding_spread(),
                        "Funding rates updated via event"
                    );
                    // Return true to allow optional event-triggered evaluation
                    true
                } else {
                    false
                }
            }
            ZmqEvent::PositionUpdate(position) => {
                self.apply_position(position);
                false
            }
            _ => false,
        }
    }

    fn refresh_prices(&mut self, mid_price: &dyn Fn(&InstrumentKey) -> Option<Decimal>) {
        if let Some(price) = mid_price(&self.long_leg.key) {
            self.long_leg.current_price = price;
        }
        if let Some(price) = mid_price(&self.short_leg.key) {
            self.short_leg.current_price = price;
        }
    }

    fn refresh_orderbook(
        &mut self,
        best_bid: &dyn Fn(&InstrumentKey) -> Option<Decimal>,
        best_ask: &dyn Fn(&InstrumentKey) -> Option<Decimal>,
        ask_depth: &dyn Fn(&InstrumentKey) -> Option<Decimal>,
        bid_depth: &dyn Fn(&InstrumentKey) -> Option<Decimal>,
    ) {
        self.long_leg.best_bid = best_bid(&self.long_leg.key);
        self.long_leg.best_ask = best_ask(&self.long_leg.key);
        self.long_leg.ask_depth = ask_depth(&self.long_leg.key);
        self.long_leg.bid_depth = bid_depth(&self.long_leg.key);

        self.short_leg.best_bid = best_bid(&self.short_leg.key);
        self.short_leg.best_ask = best_ask(&self.short_leg.key);
        self.short_leg.ask_depth = ask_depth(&self.short_leg.key);
        self.short_leg.bid_depth = bid_depth(&self.short_leg.key);
    }

    fn positions_for_margin_check(&self) -> Vec<(&Position, Decimal)> {
        let mut checks = Vec::new();
        if let Some(pos) = &self.position_long {
            checks.push((pos, self.long_leg.current_price));
        }
        if let Some(pos) = &self.position_short {
            checks.push((pos, self.short_leg.current_price));
        }
        checks
    }

    fn is_fully_entered(&self) -> bool {
        !self.long_leg.position_size.is_zero() && !self.short_leg.position_size.is_zero()
    }

    fn is_fully_exited(&self) -> bool {
        self.long_leg.position_size.is_zero() && self.short_leg.position_size.is_zero()
    }

    fn close_sizes(&self) -> (Decimal, Decimal) {
        (-self.long_leg.position_size, -self.short_leg.position_size)
    }

    fn has_sufficient_depth(&self, size_long: Decimal, size_short: Decimal) -> bool {
        self.long_leg.depth_sufficient(size_long) && self.short_leg.depth_sufficient(size_short)
    }

    fn recovery_sizes(&self) -> Option<(Decimal, Decimal)> {
        let long_has_pos = !self.long_leg.position_size.is_zero();
        let short_has_pos = !self.short_leg.position_size.is_zero();

        if long_has_pos == short_has_pos {
            return None; // Both present or both empty — nothing to recover.
        }

        let long_price = self
            .long_leg
            .best_bid
            .unwrap_or(self.long_leg.current_price);
        let short_price = self
            .short_leg
            .best_ask
            .unwrap_or(self.short_leg.current_price);

        if (!long_has_pos && long_price.is_zero()) || (!short_has_pos && short_price.is_zero()) {
            return None; // Missing leg price not available yet.
        }

        let size_long = if long_has_pos {
            Decimal::ZERO
        } else {
            let target = self.short_leg.position_size.abs() * short_price;
            target / long_price
        };
        let size_short = if short_has_pos {
            Decimal::ZERO
        } else {
            let target = self.long_leg.position_size.abs() * long_price;
            -(target / short_price)
        };

        Some((size_long, size_short))
    }
}
