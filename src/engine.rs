//! The contract shared by [`crate::ReferenceEngine`] and [`crate::FastEngine`].
//!
//! Any two engines that follow this trait must, for the same command sequence,
//! return equal [`CommandResult`] values and equal [`Book`] snapshots. The
//! [`crate::fuzz`] harness is the differential check.

use crate::types::{
    Book, Command, CommandResult, Level, OrderId, Price, Quote, RestingOrder, Side,
};

/// Price-time limit order book.
///
/// Implementations must follow these rules:
///
/// * **Priority.** Bids match from highest price to lowest. Asks match from
///   lowest price to highest. At one price, the order submitted earlier
///   matches first. The caller-supplied timestamp is not the tie breaker.
/// * **Trade price.** A fill prints at the resting maker's price.
/// * **Limit orders.** A buy trades with asks priced at or below its limit. A
///   sell trades with bids priced at or above its limit. Unmatched quantity
///   rests at the limit price.
/// * **Market orders.** The order consumes opposite liquidity until it is
///   filled or that side is empty. Unmatched quantity does not rest.
/// * **Cancel.** A resting order is removed. Canceling an unknown id, a fully
///   filled order, or an order that has already left the book returns a
///   [`CommandResult::CancelRejected`] and does not panic.
/// * **Rejections.** Zero quantity is rejected and does not consume the id.
///   An id that was already accepted is rejected and does not change the book.
/// * **Determinism.** No wall clock and no random source. Observable state is
///   fully determined by the sequence of commands.
/// * **Uncrossed book.** After every command, if both sides are non-empty, the
///   best bid is strictly below the best ask.
///
/// [`depth`](MatchingEngine::depth) is the canonical book view. The default
/// query methods are defined from it, so a snapshot cannot disagree with
/// best bid, best ask, or order lookup. An override must describe that same book.
pub trait MatchingEngine {
    /// Apply one command.
    fn execute(&mut self, command: Command) -> CommandResult;

    /// Price levels from best to worst.
    ///
    /// Bids are descending in price. Asks are ascending. Empty levels are
    /// omitted. Within a level, [`Level::orders`] is oldest first.
    fn depth(&self, side: Side) -> Vec<Level>;

    /// Highest bid and the aggregate quantity resting at that price.
    fn best_bid(&self) -> Option<Quote> {
        top_quote(&self.depth(Side::Buy))
    }

    /// Lowest ask and the aggregate quantity resting at that price.
    fn best_ask(&self) -> Option<Quote> {
        top_quote(&self.depth(Side::Sell))
    }

    /// The resting order with this id, if it is still on the book.
    fn resting_order(&self, id: OrderId) -> Option<RestingOrder> {
        let book = self.book();
        book.bids
            .into_iter()
            .chain(book.asks)
            .flat_map(|level| level.orders)
            .find(|order| order.id == id)
    }

    /// Owned snapshot of both books, suitable for equality comparison.
    fn book(&self) -> Book {
        Book {
            bids: self.depth(Side::Buy),
            asks: self.depth(Side::Sell),
        }
    }
}

fn top_quote(levels: &[Level]) -> Option<Quote> {
    levels.first().map(|level| Quote {
        price: level.price,
        qty: level.total_qty,
    })
}

/// Whether `maker_price` can fill an incoming order.
///
/// A market order (`limit == None`) crosses every resting price. A buy limit
/// crosses asks at or below its limit. A sell limit crosses bids at or above
/// its limit.
pub(crate) fn price_crosses(taker_side: Side, limit: Option<Price>, maker_price: Price) -> bool {
    let Some(limit) = limit else {
        return true;
    };
    match taker_side {
        Side::Buy => maker_price <= limit,
        Side::Sell => maker_price >= limit,
    }
}
