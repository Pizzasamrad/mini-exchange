//! Obviously-correct price-time matching engine.
//!
//! Orders live in a [`BTreeMap`] keyed by price, so the best bid is the last
//! key and the best ask is the first. Each level is a [`VecDeque`]: the front
//! is the oldest order. Nothing here is tuned for speed. Fills, cancels, and
//! book queries all walk that structure directly, which keeps a single source
//! of truth for resting quantity.
//!
//! Quantity arithmetic is checked. A `u64` overflow panics, because wrapping
//! would silently break quantity conservation.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use crate::engine::MatchingEngine;
use crate::types::{
    CancelRejectReason, Command, CommandResult, Level, OrderId, Price, Qty, RejectReason,
    RestingOrder, Side, Timestamp, Trade,
};

/// Why an accepted order is no longer on the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Terminal {
    /// Traded for its full submitted quantity.
    Filled,
    /// Left the book with some quantity unfilled.
    Canceled,
}

/// Reference limit order book.
///
/// See [`MatchingEngine`] for the matching rules. This type adds no behavior
/// beyond that contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceEngine {
    bids: BTreeMap<Price, VecDeque<RestingOrder>>,
    asks: BTreeMap<Price, VecDeque<RestingOrder>>,
    /// Accepted ids that are no longer resting, and why they left.
    terminal: HashMap<OrderId, Terminal>,
}

impl ReferenceEngine {
    /// Empty book.
    pub fn new() -> Self {
        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            terminal: HashMap::new(),
        }
    }

    fn place(
        &mut self,
        id: OrderId,
        side: Side,
        limit: Option<Price>,
        qty: Qty,
        ts: Timestamp,
    ) -> CommandResult {
        if qty.is_zero() {
            return CommandResult::Rejected {
                order_id: id,
                reason: RejectReason::ZeroQuantity,
            };
        }
        if self.id_in_use(id) {
            return CommandResult::Rejected {
                order_id: id,
                reason: RejectReason::DuplicateOrderId,
            };
        }

        let (trades, unmatched) = self.match_incoming(id, side, limit, qty, ts);
        let filled_qty = qty
            .checked_sub(unmatched)
            .expect("filled quantity exceeds submitted quantity");

        // Rest only after matching, so the new order cannot trade with itself.
        let (resting_qty, canceled_qty) = if unmatched.is_zero() {
            assert!(!trades.is_empty(), "a full fill must produce trades");
            self.mark_terminal(id, Terminal::Filled);
            (Qty::ZERO, Qty::ZERO)
        } else if let Some(price) = limit {
            self.rest(RestingOrder {
                id,
                side,
                price,
                remaining_qty: unmatched,
                original_qty: qty,
                ts,
            });
            (unmatched, Qty::ZERO)
        } else {
            // Market remainder does not rest.
            self.mark_terminal(id, Terminal::Canceled);
            (Qty::ZERO, unmatched)
        };

        CommandResult::Accepted {
            order_id: id,
            trades,
            filled_qty,
            resting_qty,
            canceled_qty,
        }
    }

    fn cancel(&mut self, id: OrderId) -> CommandResult {
        if let Some(order) = self.take_resting(id) {
            self.mark_terminal(id, Terminal::Canceled);
            return CommandResult::Canceled {
                order_id: id,
                canceled_qty: order.remaining_qty,
            };
        }
        let reason = match self.terminal.get(&id).copied() {
            Some(Terminal::Filled) => CancelRejectReason::AlreadyFilled,
            Some(Terminal::Canceled) => CancelRejectReason::AlreadyCanceled,
            None => CancelRejectReason::NotFound,
        };
        CommandResult::CancelRejected {
            order_id: id,
            reason,
        }
    }

    /// Match `qty` against the opposite book. Returns fills and the unmatched size.
    fn match_incoming(
        &mut self,
        taker_id: OrderId,
        side: Side,
        limit: Option<Price>,
        qty: Qty,
        ts: Timestamp,
    ) -> (Vec<Trade>, Qty) {
        let mut trades = Vec::new();
        let mut remaining = qty;

        while !remaining.is_zero() {
            let Some(maker_price) = self.best_price(side.opposite()) else {
                break;
            };
            if !price_crosses(side, limit, maker_price) {
                break;
            }

            let (maker_id, fill) = self.consume_front(side.opposite(), maker_price, remaining);
            remaining = remaining
                .checked_sub(fill)
                .expect("fill exceeds incoming quantity");
            trades.push(Trade {
                maker_id,
                taker_id,
                // The print is the resting order's price.
                price: maker_price,
                qty: fill,
                taker_side: side,
                ts,
            });
        }

        (trades, remaining)
    }

    /// Fill against the oldest order at `price` on `maker_side`.
    fn consume_front(&mut self, maker_side: Side, price: Price, taker_qty: Qty) -> (OrderId, Qty) {
        let (maker_id, fill, done) = {
            let queue = self
                .levels_mut(maker_side)
                .get_mut(&price)
                .expect("matched price level exists");
            let maker = queue.front_mut().expect("matched price level is non-empty");
            let fill = std::cmp::min(maker.remaining_qty, taker_qty);
            assert!(!fill.is_zero(), "refusing to print a zero-quantity trade");
            maker.remaining_qty = maker
                .remaining_qty
                .checked_sub(fill)
                .expect("fill exceeds resting quantity");
            (maker.id, fill, maker.remaining_qty.is_zero())
        };

        if done {
            let (removed_id, level_empty) = {
                let queue = self
                    .levels_mut(maker_side)
                    .get_mut(&price)
                    .expect("matched price level exists");
                let removed = queue
                    .pop_front()
                    .expect("exhausted order is at the front of the queue");
                (removed.id, queue.is_empty())
            };
            assert_eq!(removed_id, maker_id, "queue front changed during a fill");
            if level_empty {
                self.levels_mut(maker_side).remove(&price);
            }
            self.mark_terminal(maker_id, Terminal::Filled);
        }

        (maker_id, fill)
    }

    fn rest(&mut self, order: RestingOrder) {
        let side = order.side;
        let price = order.price;
        self.levels_mut(side)
            .entry(price)
            .or_default()
            .push_back(order);
    }

    fn take_resting(&mut self, id: OrderId) -> Option<RestingOrder> {
        let (side, price, index) = self.locate(id)?;
        let order = {
            let queue = self
                .levels_mut(side)
                .get_mut(&price)
                .expect("located level exists");
            queue.remove(index).expect("located order exists")
        };
        if self
            .levels(side)
            .get(&price)
            .is_some_and(|queue| queue.is_empty())
        {
            self.levels_mut(side).remove(&price);
        }
        Some(order)
    }

    fn locate(&self, id: OrderId) -> Option<(Side, Price, usize)> {
        for side in [Side::Buy, Side::Sell] {
            for (price, queue) in self.levels(side) {
                if let Some(index) = queue.iter().position(|order| order.id == id) {
                    return Some((side, *price, index));
                }
            }
        }
        None
    }

    fn id_in_use(&self, id: OrderId) -> bool {
        self.terminal.contains_key(&id) || self.locate(id).is_some()
    }

    fn mark_terminal(&mut self, id: OrderId, state: Terminal) {
        let previous = self.terminal.insert(id, state);
        assert!(previous.is_none(), "order {id} left the book twice");
    }

    fn best_price(&self, side: Side) -> Option<Price> {
        match side {
            Side::Buy => self.bids.keys().next_back().copied(),
            Side::Sell => self.asks.keys().next().copied(),
        }
    }

    fn levels(&self, side: Side) -> &BTreeMap<Price, VecDeque<RestingOrder>> {
        match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        }
    }

    fn levels_mut(&mut self, side: Side) -> &mut BTreeMap<Price, VecDeque<RestingOrder>> {
        match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        }
    }

    /// Structural checks that do not depend on command history.
    ///
    /// Quantity conservation needs the original commands, so it lives in the
    /// test suite. These checks catch a broken book on every command.
    fn check_invariants(&self) {
        if let (Some(bid), Some(ask)) = (self.best_price(Side::Buy), self.best_price(Side::Sell)) {
            assert!(
                bid < ask,
                "crossed book: best bid {bid} is not below best ask {ask}"
            );
        }

        let mut seen = HashSet::new();
        for side in [Side::Buy, Side::Sell] {
            for (price, queue) in self.levels(side) {
                assert!(!queue.is_empty(), "empty {side} level at {price}");
                for order in queue {
                    assert_eq!(order.side, side, "order {} is on the wrong book", order.id);
                    assert_eq!(
                        order.price, *price,
                        "order {} price does not match its level",
                        order.id
                    );
                    assert!(
                        !order.remaining_qty.is_zero(),
                        "order {} rests with zero quantity",
                        order.id
                    );
                    assert!(
                        order.remaining_qty <= order.original_qty,
                        "order {} remaining exceeds original quantity",
                        order.id
                    );
                    assert!(
                        !self.terminal.contains_key(&order.id),
                        "order {} is both resting and closed",
                        order.id
                    );
                    assert!(
                        seen.insert(order.id),
                        "order {} rests in more than one place",
                        order.id
                    );
                }
            }
        }
    }
}

impl Default for ReferenceEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl MatchingEngine for ReferenceEngine {
    fn execute(&mut self, command: Command) -> CommandResult {
        let result = match command {
            Command::Limit {
                id,
                side,
                price,
                qty,
                ts,
            } => self.place(id, side, Some(price), qty, ts),
            Command::Market { id, side, qty, ts } => self.place(id, side, None, qty, ts),
            Command::Cancel { id } => self.cancel(id),
        };
        self.check_invariants();
        result
    }

    fn depth(&self, side: Side) -> Vec<Level> {
        match side {
            Side::Buy => snapshot(self.bids.iter().rev()),
            Side::Sell => snapshot(self.asks.iter()),
        }
    }
}

fn snapshot<'a>(
    levels: impl Iterator<Item = (&'a Price, &'a VecDeque<RestingOrder>)>,
) -> Vec<Level> {
    levels
        .map(|(price, queue)| {
            let orders: Vec<RestingOrder> = queue.iter().copied().collect();
            Level {
                price: *price,
                total_qty: sum_remaining(&orders),
                orders,
            }
        })
        .collect()
}

fn sum_remaining(orders: &[RestingOrder]) -> Qty {
    orders.iter().fold(Qty::ZERO, |total, order| {
        total
            .checked_add(order.remaining_qty)
            .expect("price-level quantity overflowed u64")
    })
}

/// A market order (`limit == None`) crosses any resting price.
fn price_crosses(taker_side: Side, limit: Option<Price>, maker_price: Price) -> bool {
    let Some(limit) = limit else {
        return true;
    };
    match taker_side {
        Side::Buy => maker_price <= limit,
        Side::Sell => maker_price >= limit,
    }
}
