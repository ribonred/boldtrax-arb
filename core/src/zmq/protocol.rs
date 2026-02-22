use crate::{
    AccountSnapshot, Instrument, InstrumentKey,
    types::{
        FundingRateSnapshot, Order, OrderBookSnapshot, OrderBookUpdate, OrderEvent, OrderRequest,
        Position, Ticker, Trade,
    },
};

/// Commands sent from a Strategy (DEALER) to the ExchangeRunner (ROUTER).
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
#[repr(u8)]
pub enum ZmqCommand {
    /// Submit a new order to the exchange.
    SubmitOrder(OrderRequest),
    /// Cancel an existing order by its internal ID.
    CancelOrder(String),
    /// Request the current position for a specific instrument.
    GetPosition(InstrumentKey),
    /// Request all current positions.
    GetAllPositions,
    /// Request the current account snapshot (balances, etc).
    GetAccountSnapshot,
    /// Request instrument information by key.
    GetInstrument(InstrumentKey),
    /// Request all instruments for the exchange.
    GetAllInstruments,
}

/// Responses sent from the ExchangeRunner (ROUTER) back to the Strategy (DEALER).
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
#[repr(u8)]
pub enum ZmqResponse {
    /// Acknowledgment that an order was successfully submitted (returns the pending/new Order).
    SubmitAck(Order),
    /// Acknowledgment that an order was successfully canceled.
    CancelAck(Order),
    /// Response containing a specific position.
    Position(Option<Position>),
    /// Response containing all positions.
    AllPositions(Vec<Position>),
    /// Response containing the account snapshot.
    AccountSnapshot(AccountSnapshot),
    /// Response containing instrument information.
    Instrument(Option<Instrument>),
    /// Response containing all instruments.
    AllInstruments(Vec<Instrument>),
    /// An error occurred while processing the command.
    Error(String),
}

/// Events broadcasted from the ExchangeRunner (PUB) to all subscribed Strategies (SUB).
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
#[repr(u8)]
pub enum ZmqEvent {
    /// A new order book snapshot.
    OrderBook(OrderBookSnapshot),
    /// A new order book update.
    OrderBookUpdate(OrderBookUpdate),
    /// A new ticker update (price, volume).
    Ticker(Ticker),
    /// A new public trade.
    Trade(Trade),
    /// A new funding rate snapshot.
    FundingRate(FundingRateSnapshot),
    /// An update to an order's lifecycle (New, Filled, Canceled, Rejected).
    OrderUpdate(OrderEvent),
    /// An update to a position (e.g., after a fill).
    PositionUpdate(Position),
}

impl ZmqEvent {
    /// Helper to generate the ZMQ topic prefix for this event.
    /// This allows subscribers to filter events efficiently.
    pub fn topic(&self) -> String {
        match self {
            ZmqEvent::OrderBook(ob) => format!("MD.OB.{:?}.{:?}", ob.key.exchange, ob.key.pair),
            ZmqEvent::OrderBookUpdate(ob) => format!(
                "MD.OBU.{:?}.{:?}",
                ob.snapshot.key.exchange, ob.snapshot.key.pair
            ),
            ZmqEvent::Ticker(t) => format!("MD.TICKER.{:?}.{:?}", t.key.exchange, t.key.pair),
            ZmqEvent::Trade(t) => format!("MD.TRADE.{:?}.{:?}", t.key.exchange, t.key.pair),
            ZmqEvent::FundingRate(f) => format!("MD.FUNDING.{:?}.{:?}", f.key.exchange, f.key.pair),
            ZmqEvent::OrderUpdate(event) => {
                let order = match event {
                    OrderEvent::New(o) => o,
                    OrderEvent::PartiallyFilled(o) => o,
                    OrderEvent::Filled(o) => o,
                    OrderEvent::Canceled(o) => o,
                    OrderEvent::Rejected(o) => o,
                };
                format!(
                    "EXEC.{:?}.{:?}",
                    order.request.key.exchange, order.request.key.pair
                )
            }
            ZmqEvent::PositionUpdate(pos) => {
                format!("POS.{:?}.{:?}", pos.key.exchange, pos.key.pair)
            }
        }
    }
}
