//! Integer domain types shared by every matching engine.
//!
//! Prices and quantities are ticks, never floating-point values. Timestamps are
//! caller-supplied logical times (or sequence numbers). They are recorded on
//! orders and trades, but they do not decide priority: when two orders share a
//! price, the one submitted first matches first.

use std::fmt;

/// Price in integer ticks. A higher value is a more expensive price.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Price(pub u64);

impl Price {
    /// Tick value.
    pub const fn ticks(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Price({})", self.0)
    }
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Order or trade quantity in integer units.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Qty(pub u64);

impl Qty {
    /// Zero quantity.
    pub const ZERO: Self = Self(0);

    /// Whether this quantity is zero.
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Unit count.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Checked addition. `None` when the sum does not fit in a `u64`.
    pub const fn checked_add(self, rhs: Self) -> Option<Self> {
        match self.0.checked_add(rhs.0) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Checked subtraction. `None` when `rhs` is greater than `self`.
    pub const fn checked_sub(self, rhs: Self) -> Option<Self> {
        match self.0.checked_sub(rhs.0) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

impl fmt::Debug for Qty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Qty({})", self.0)
    }
}

impl fmt::Display for Qty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Caller-supplied order identifier.
///
/// An id that has been accepted may not be reused, including after the order
/// fills or is canceled. Ids on rejected commands are not consumed.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrderId(pub u64);

impl OrderId {
    /// Raw identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OrderId({})", self.0)
    }
}

impl fmt::Display for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Caller-supplied logical timestamp or sequence number.
///
/// The engine stores this value and copies it onto trades caused by the order.
/// It is not a wall-clock reading, and it is not the price-time tie breaker.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub u64);

impl Timestamp {
    /// Raw timestamp.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Timestamp({})", self.0)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Side of an order. `Buy` rests on the bid book; `Sell` rests on the ask book.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// Bid. Matches against asks, highest resting bid first.
    Buy,
    /// Ask. Matches against bids, lowest resting ask first.
    Sell,
}

impl Side {
    /// The other side of the book.
    pub const fn opposite(self) -> Self {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Side::Buy => write!(f, "buy"),
            Side::Sell => write!(f, "sell"),
        }
    }
}

/// One command applied to a matching engine.
///
/// Commands are pure values. Applying the same sequence to two engines that
/// implement [`crate::MatchingEngine`] must produce the same results and the
/// same book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Rest any unmatched remainder at `price` after crossing the opposite book.
    Limit {
        /// Identifier reserved when the command is accepted.
        id: OrderId,
        /// Bid or ask.
        side: Side,
        /// Limit price. Buys trade at or below it; sells trade at or above it.
        price: Price,
        /// Submitted size. Zero is rejected and does not consume `id`.
        qty: Qty,
        /// Logical time recorded on the order and on trades it initiates.
        ts: Timestamp,
    },
    /// Consume opposite liquidity until filled or the opposite book is empty.
    ///
    /// Unmatched quantity does not rest.
    Market {
        /// Identifier reserved when the command is accepted.
        id: OrderId,
        /// Bid or ask.
        side: Side,
        /// Submitted size. Zero is rejected and does not consume `id`.
        qty: Qty,
        /// Logical time recorded on trades this order initiates.
        ts: Timestamp,
    },
    /// Remove a resting order. Does not panic when the id is unknown or closed.
    Cancel {
        /// Order to remove.
        id: OrderId,
    },
}

impl Command {
    /// Limit order command.
    pub const fn limit(id: OrderId, side: Side, price: Price, qty: Qty, ts: Timestamp) -> Self {
        Self::Limit {
            id,
            side,
            price,
            qty,
            ts,
        }
    }

    /// Market order command.
    pub const fn market(id: OrderId, side: Side, qty: Qty, ts: Timestamp) -> Self {
        Self::Market { id, side, qty, ts }
    }

    /// Cancel command.
    pub const fn cancel(id: OrderId) -> Self {
        Self::Cancel { id }
    }

    /// Id named by this command.
    pub const fn order_id(self) -> OrderId {
        match self {
            Self::Limit { id, .. } | Self::Market { id, .. } | Self::Cancel { id } => id,
        }
    }
}

/// A fill between a resting maker and an incoming taker.
///
/// `price` is always the maker's resting price, never the taker's limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trade {
    /// Resting order that provided liquidity.
    pub maker_id: OrderId,
    /// Incoming order that took liquidity.
    pub taker_id: OrderId,
    /// Maker's price.
    pub price: Price,
    /// Size of this fill. Always greater than zero.
    pub qty: Qty,
    /// Side of the incoming order.
    pub taker_side: Side,
    /// Timestamp of the incoming order.
    pub ts: Timestamp,
}

/// Why a limit or market command was not applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// Quantity was zero. The order id is not consumed.
    ZeroQuantity,
    /// The order id was already accepted. The book is unchanged.
    DuplicateOrderId,
}

/// Why a cancel did not remove a resting order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelRejectReason {
    /// No accepted order has used this id.
    NotFound,
    /// The order traded completely and is no longer resting.
    AlreadyFilled,
    /// The order was accepted and is no longer resting, but it did not trade
    /// in full. That is either a prior cancel or a market order with a remainder.
    AlreadyCanceled,
}

/// Result of applying one [`Command`].
///
/// Trades inside an acceptance are the public events for that command, in
/// match order: best price first, and FIFO within a price.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "discarding a command result drops trades and reject reasons"]
pub enum CommandResult {
    /// The order was accepted. `filled_qty + resting_qty + canceled_qty` equals
    /// the submitted quantity.
    Accepted {
        /// Accepted order id.
        order_id: OrderId,
        /// Fills produced by this order, in match order.
        trades: Vec<Trade>,
        /// Sum of `trades` quantities.
        filled_qty: Qty,
        /// Quantity left on the book. Always zero for a market order.
        resting_qty: Qty,
        /// Quantity neither filled nor resting.
        ///
        /// Always zero for a limit order, whose remainder rests. For a market
        /// order this is the unfilled remainder.
        canceled_qty: Qty,
    },
    /// The command was ignored. The book is unchanged and the id is not consumed.
    Rejected {
        /// Id from the command.
        order_id: OrderId,
        /// Why the command was ignored.
        reason: RejectReason,
    },
    /// A resting order was removed.
    Canceled {
        /// Id that was removed.
        order_id: OrderId,
        /// Quantity that was still resting.
        canceled_qty: Qty,
    },
    /// The cancel did not change the book.
    CancelRejected {
        /// Id from the cancel command.
        order_id: OrderId,
        /// Why nothing was removed.
        reason: CancelRejectReason,
    },
}

/// Best price on one side and the aggregate quantity resting there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quote {
    /// Best bid or best ask price.
    pub price: Price,
    /// Sum of remaining quantities at `price`.
    pub qty: Qty,
}

/// One price level, in FIFO order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Level {
    /// Price of every order in this level.
    pub price: Price,
    /// Sum of remaining quantities. Never zero; empty levels are omitted.
    pub total_qty: Qty,
    /// Resting orders, oldest (front of the queue) first.
    pub orders: Vec<RestingOrder>,
}

/// A passive order currently on the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestingOrder {
    /// Order id.
    pub id: OrderId,
    /// Book this order rests on.
    pub side: Side,
    /// Limit price.
    pub price: Price,
    /// Quantity still available to trade.
    pub remaining_qty: Qty,
    /// Quantity originally accepted, before any fills.
    pub original_qty: Qty,
    /// Timestamp supplied when the order was submitted.
    pub ts: Timestamp,
}

/// Owned snapshot of both books.
///
/// Bids are highest price first. Asks are lowest price first. Within each
/// level, orders are oldest first. Two engines are in the same observable
/// book state when their snapshots compare equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Book {
    /// Bid levels, best (highest) price first.
    pub bids: Vec<Level>,
    /// Ask levels, best (lowest) price first.
    pub asks: Vec<Level>,
}

impl Book {
    /// Whether the best bid is at or through the best ask.
    ///
    /// Equal prices count as crossed: they should already have matched.
    /// An empty side is not a cross.
    pub fn is_crossed(&self) -> bool {
        match (self.bids.first(), self.asks.first()) {
            (Some(bid), Some(ask)) => bid.price >= ask.price,
            _ => false,
        }
    }
}
