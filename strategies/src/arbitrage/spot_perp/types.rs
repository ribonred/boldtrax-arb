use boldtrax_core::types::{InstrumentKey, InstrumentType, OrderEvent, OrderSide, Position};
use boldtrax_core::zmq::protocol::ZmqEvent;
use rust_decimal::Decimal;

use crate::arbitrage::types::{PairState, PairStatus, PerpLeg};

#[derive(Debug, Clone)]
pub struct SpotLeg {
    /// Spot instrument key (exchange, pair, InstrumentType::Spot).
    pub key: InstrumentKey,
    /// Held quantity — sourced from account balance, not a Position.
    pub quantity: Decimal,
    /// Average acquisition cost per unit.
    pub avg_cost: Decimal,
    /// Latest mid-price from the oracle.
    pub current_price: Decimal,
    /// Best bid from the latest orderbook snapshot.
    pub best_bid: Option<Decimal>,
    /// Best ask from the latest orderbook snapshot.
    pub best_ask: Option<Decimal>,
    /// Sum of quantity across top 3 ask levels (available for buying into).
    pub ask_depth: Option<Decimal>,
    /// Sum of quantity across top 3 bid levels (available for selling into).
    pub bid_depth: Option<Decimal>,
}

impl SpotLeg {
    pub fn new(key: InstrumentKey) -> Self {
        Self {
            key,
            quantity: Decimal::ZERO,
            avg_cost: Decimal::ZERO,
            current_price: Decimal::ZERO,
            best_bid: None,
            best_ask: None,
            ask_depth: None,
            bid_depth: None,
        }
    }

    /// Notional value of the spot holding (positive when long).
    pub fn notional(&self) -> Decimal {
        self.quantity * self.current_price
    }
}

#[derive(Debug, Clone)]
pub struct SpotPerpPair {
    /// Long leg: buy-and-hold spot asset (tracked via balance).
    pub spot: SpotLeg,
    /// Short leg: short the perpetual (tracked via Position).
    pub perp: PerpLeg,
    /// Target dollar exposure per side.
    pub target_notional: Decimal,
    /// Current lifecycle state.
    pub status: PairStatus,
    /// Live perp position (updated from PositionUpdate events).
    pub perp_position: Option<Position>,
    /// Tracks cumulative signed fill for the current spot order's partial fills.
    /// Reset to zero when the order reaches Filled state.
    last_spot_partial_fill: Decimal,
}

impl SpotPerpPair {
    pub fn new(spot_key: InstrumentKey, perp_key: InstrumentKey) -> Self {
        Self {
            spot: SpotLeg::new(spot_key),
            perp: PerpLeg::new(perp_key),
            target_notional: Decimal::ZERO,
            status: PairStatus::Inactive,
            perp_position: None,
            last_spot_partial_fill: Decimal::ZERO,
        }
    }

    /// The funding rate we capture — only the perp leg has one.
    /// Positive means shorts receive funding (profitable for us).
    pub fn funding_rate(&self) -> Decimal {
        self.perp.funding_rate
    }
}

impl PairState for SpotPerpPair {
    fn total_delta(&self) -> Decimal {
        self.spot.notional() + self.perp.notional()
    }

    fn status(&self) -> &PairStatus {
        &self.status
    }

    fn set_status(&mut self, status: PairStatus) {
        self.status = status;
    }

    fn leg_keys(&self) -> Vec<InstrumentKey> {
        vec![self.spot.key, self.perp.key]
    }

    #[tracing::instrument(name = "SpotPerpPair::apply_event", skip(self), fields(event = ?event))]
    fn apply_event(&mut self, event: ZmqEvent) -> bool {
        match event {
            ZmqEvent::FundingRate(snapshot) if snapshot.key == self.perp.key => {
                tracing::info!(
                    "{} Funding rate updated: {}",
                    self.perp.key,
                    snapshot.funding_rate
                );
                self.perp.funding_rate = snapshot.funding_rate;
                // Seed prices from mark_price when the oracle hasn't provided
                // a price yet — ensures the decider can do notional→qty conversion
                // even before orderbook data arrives.
                if self.perp.current_price.is_zero() && !snapshot.mark_price.is_zero() {
                    self.perp.current_price = snapshot.mark_price;
                }
                if self.spot.current_price.is_zero() && !snapshot.index_price.is_zero() {
                    self.spot.current_price = snapshot.index_price;
                }
                true
            }
            ZmqEvent::PositionUpdate(position) if position.key == self.perp.key => {
                self.perp.position_size = position.size;
                self.perp.entry_price = position.entry_price;
                self.perp_position = Some(position);
                tracing::info!(
                    "Perp position updated: size={}, entry_price={}",
                    self.perp.position_size,
                    self.perp.entry_price
                );
                // Auto-transition to Active when both legs are filled.
                if self.status == PairStatus::Inactive
                    && !self.perp.position_size.is_zero()
                    && !self.spot.quantity.is_zero()
                {
                    tracing::info!(
                        "Both legs filled (spot={}, perp={}), transitioning to Active",
                        self.spot.quantity,
                        self.perp.position_size
                    );
                    self.status = PairStatus::Active;
                }
                false
            }
            // Track spot balance from order fills.
            ZmqEvent::OrderUpdate(ref event) => {
                let order = event.inner();
                if order.request.key.instrument_type == InstrumentType::Spot
                    && order.request.key.pair == self.spot.key.pair
                    && order.request.key.exchange == self.spot.key.exchange
                    && order.filled_size > Decimal::ZERO
                {
                    let signed_fill = match order.request.side {
                        OrderSide::Buy => order.filled_size,
                        OrderSide::Sell => -order.filled_size,
                    };
                    // For Filled events, set cumulative qty; for partial, track latest.
                    match event {
                        OrderEvent::Filled(_) => {
                            self.spot.quantity += signed_fill - self.last_spot_partial_fill;
                            self.last_spot_partial_fill = Decimal::ZERO;
                            if let Some(avg_price) = order.avg_fill_price {
                                self.spot.avg_cost = avg_price;
                            }
                            tracing::info!(
                                "Spot fill completed: qty={}, total_held={}",
                                signed_fill,
                                self.spot.quantity
                            );
                            // Auto-transition to Active when both legs are filled.
                            if self.status == PairStatus::Inactive
                                && !self.perp.position_size.is_zero()
                                && !self.spot.quantity.is_zero()
                            {
                                tracing::info!(
                                    "Both legs filled (spot={}, perp={}), transitioning to Active",
                                    self.spot.quantity,
                                    self.perp.position_size
                                );
                                self.status = PairStatus::Active;
                            }
                        }
                        OrderEvent::PartiallyFilled(_) => {
                            let incremental = signed_fill - self.last_spot_partial_fill;
                            self.spot.quantity += incremental;
                            self.last_spot_partial_fill = signed_fill;
                            tracing::debug!(
                                "Spot partial fill: incremental={}, total_held={}",
                                incremental,
                                self.spot.quantity
                            );
                        }
                        _ => {}
                    }
                }
                false
            }
            _ => false,
        }
    }

    #[tracing::instrument(name = "SpotPerpPair::refresh_prices", skip(self, mid_price))]
    fn refresh_prices(&mut self, mid_price: &dyn Fn(&InstrumentKey) -> Option<Decimal>) {
        if let Some(price) = mid_price(&self.spot.key) {
            tracing::info!("{} Spot price updated: {}", self.spot.key, price);
            self.spot.current_price = price;
        }
        if let Some(price) = mid_price(&self.perp.key) {
            tracing::info!("{} Perp price updated: {}", self.perp.key, price);
            self.perp.current_price = price;
        }
    }

    fn refresh_orderbook(
        &mut self,
        best_bid: &dyn Fn(&InstrumentKey) -> Option<Decimal>,
        best_ask: &dyn Fn(&InstrumentKey) -> Option<Decimal>,
        ask_depth: &dyn Fn(&InstrumentKey) -> Option<Decimal>,
        bid_depth: &dyn Fn(&InstrumentKey) -> Option<Decimal>,
    ) {
        self.spot.best_bid = best_bid(&self.spot.key);
        self.spot.best_ask = best_ask(&self.spot.key);
        self.spot.ask_depth = ask_depth(&self.spot.key);
        self.spot.bid_depth = bid_depth(&self.spot.key);
        self.perp.best_bid = best_bid(&self.perp.key);
        self.perp.best_ask = best_ask(&self.perp.key);
        self.perp.ask_depth = ask_depth(&self.perp.key);
        self.perp.bid_depth = bid_depth(&self.perp.key);
    }

    #[tracing::instrument(name = "SpotPerpPair::positions_for_margin_check", skip(self))]
    fn positions_for_margin_check(&self) -> Vec<(&Position, Decimal)> {
        match &self.perp_position {
            Some(pos) => vec![(pos, self.perp.current_price)],
            None => vec![],
        }
    }
}
