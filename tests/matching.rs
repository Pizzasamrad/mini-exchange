//! Behavioral tests for every [`MatchingEngine`].
//!
//! The suite runs twice: once on [`ReferenceEngine`] and once on [`FastEngine`].
//! Every command goes through `drive`, which checks price-time matching against
//! the previous book, quantity conservation, and the uncrossed-book rule.
//! Setup commands use `apply`, which runs the same checks and drops the report.

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};

use mini_exchange::{
    Book, CancelRejectReason, Command, CommandResult, FastEngine, MatchingEngine, OrderId, Price,
    Qty, ReferenceEngine, RejectReason, RestingOrder, Side, Timestamp, Trade,
};

fn limit(id: u64, side: Side, price: u64, qty: u64, ts: u64) -> Command {
    Command::limit(OrderId(id), side, Price(price), Qty(qty), Timestamp(ts))
}

fn market(id: u64, side: Side, qty: u64, ts: u64) -> Command {
    Command::market(OrderId(id), side, Qty(qty), Timestamp(ts))
}

fn cancel(id: u64) -> Command {
    Command::cancel(OrderId(id))
}

fn trade(maker: u64, taker: u64, price: u64, qty: u64, taker_side: Side, ts: u64) -> Trade {
    Trade {
        maker_id: OrderId(maker),
        taker_id: OrderId(taker),
        price: Price(price),
        qty: Qty(qty),
        taker_side,
        ts: Timestamp(ts),
    }
}

fn accepted(
    id: u64,
    trades: Vec<Trade>,
    filled: u64,
    resting: u64,
    canceled: u64,
) -> CommandResult {
    CommandResult::Accepted {
        order_id: OrderId(id),
        trades,
        filled_qty: Qty(filled),
        resting_qty: Qty(resting),
        canceled_qty: Qty(canceled),
    }
}

fn rejected(id: u64, reason: RejectReason) -> CommandResult {
    CommandResult::Rejected {
        order_id: OrderId(id),
        reason,
    }
}

fn canceled(id: u64, qty: u64) -> CommandResult {
    CommandResult::Canceled {
        order_id: OrderId(id),
        canceled_qty: Qty(qty),
    }
}

fn cancel_rejected(id: u64, reason: CancelRejectReason) -> CommandResult {
    CommandResult::CancelRejected {
        order_id: OrderId(id),
        reason,
    }
}

/// Shared constructor for the engines under test.
trait Engine: MatchingEngine + Clone + PartialEq + Eq + std::fmt::Debug + 'static {
    fn fresh_engine() -> Self;
}

impl Engine for ReferenceEngine {
    fn fresh_engine() -> Self {
        Self::new()
    }
}

impl Engine for FastEngine {
    fn fresh_engine() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Default)]
struct Ledger {
    submitted: HashMap<OrderId, u64>,
    filled: HashMap<OrderId, u64>,
    canceled: HashMap<OrderId, u64>,
}

fn fresh<E: Engine>() -> (E, Ledger) {
    (E::fresh_engine(), Ledger::default())
}

/// Apply `command` and check matching, conservation, and book shape.
fn drive<E: MatchingEngine>(
    engine: &mut E,
    ledger: &mut Ledger,
    command: Command,
) -> CommandResult {
    let before = engine.book();
    let result = engine.execute(command);
    check_result(&before, engine, command, &result);
    note(ledger, command, &result);
    assert_conserved(engine, ledger);
    result
}

fn apply<E: MatchingEngine>(engine: &mut E, ledger: &mut Ledger, command: Command) {
    let _ = drive(engine, ledger, command);
}

fn check_result(
    before: &Book,
    engine: &impl MatchingEngine,
    command: Command,
    result: &CommandResult,
) {
    match (command, result) {
        (
            Command::Limit { id, qty, .. } | Command::Market { id, qty, .. },
            CommandResult::Rejected { order_id, reason },
        ) => {
            assert_eq!(*order_id, id);
            let expected = if qty.is_zero() {
                RejectReason::ZeroQuantity
            } else {
                RejectReason::DuplicateOrderId
            };
            assert_eq!(*reason, expected);
            assert_eq!(&engine.book(), before, "reject changed the book");
        }
        (
            Command::Limit {
                id,
                side,
                price,
                qty,
                ts,
            },
            CommandResult::Accepted { .. },
        ) => check_acceptance(
            before,
            engine,
            Incoming {
                id,
                side,
                limit: Some(price),
                qty,
                ts,
            },
            result,
        ),
        (Command::Market { id, side, qty, ts }, CommandResult::Accepted { .. }) => {
            check_acceptance(
                before,
                engine,
                Incoming {
                    id,
                    side,
                    limit: None,
                    qty,
                    ts,
                },
                result,
            )
        }
        (
            Command::Cancel { id },
            CommandResult::Canceled {
                order_id,
                canceled_qty,
            },
        ) => {
            assert_eq!(*order_id, id);
            let previous = find_order(before, id).expect("cancel target was resting");
            assert_eq!(*canceled_qty, previous.remaining_qty);
            assert!(engine.resting_order(id).is_none());
        }
        (Command::Cancel { id }, CommandResult::CancelRejected { order_id, .. }) => {
            assert_eq!(*order_id, id);
            assert_eq!(&engine.book(), before, "rejected cancel changed the book");
        }
        (command, result) => {
            panic!("command and result do not match: {command:?} -> {result:?}");
        }
    }
}

struct Incoming {
    id: OrderId,
    side: Side,
    limit: Option<Price>,
    qty: Qty,
    ts: Timestamp,
}

fn check_acceptance(
    before: &Book,
    engine: &impl MatchingEngine,
    incoming: Incoming,
    result: &CommandResult,
) {
    let CommandResult::Accepted {
        order_id,
        trades,
        filled_qty,
        resting_qty,
        canceled_qty,
    } = result
    else {
        panic!("expected an acceptance, got {result:?}");
    };
    let (order_id, filled_qty, resting_qty, canceled_qty) =
        (*order_id, *filled_qty, *resting_qty, *canceled_qty);
    let Incoming {
        id,
        side,
        limit,
        qty,
        ts,
    } = incoming;
    assert_eq!(order_id, id);
    let trade_qty = sum_trade_qty(trades);
    assert_eq!(
        filled_qty.get(),
        trade_qty,
        "filled_qty is not the trade sum"
    );
    assert_eq!(
        filled_qty.get() + resting_qty.get() + canceled_qty.get(),
        qty.get(),
        "accepted quantity does not add up"
    );
    assert_price_time(before, side, limit, qty, ts, id, trades);

    match limit {
        Some(price) => {
            assert_eq!(
                canceled_qty,
                Qty::ZERO,
                "a limit order must not discard quantity"
            );
            assert_eq!(resting_qty.get(), qty.get() - trade_qty);
            if resting_qty.is_zero() {
                assert!(engine.resting_order(id).is_none());
            } else {
                let resting = engine.resting_order(id).expect("limit remainder rests");
                assert_eq!(
                    resting,
                    RestingOrder {
                        id,
                        side,
                        price,
                        remaining_qty: resting_qty,
                        original_qty: qty,
                        ts,
                    }
                );
            }
        }
        None => {
            assert_eq!(resting_qty, Qty::ZERO, "a market order must not rest");
            assert_eq!(canceled_qty.get(), qty.get() - trade_qty);
            assert!(engine.resting_order(id).is_none());
            if !canceled_qty.is_zero() {
                let opposite_empty = match side {
                    Side::Buy => engine.book().asks.is_empty(),
                    Side::Sell => engine.book().bids.is_empty(),
                };
                assert!(
                    opposite_empty,
                    "market remainder left opposite liquidity behind"
                );
            }
        }
    }
}

/// Trades must be a prefix of the previous opposite book in price-time order.
fn assert_price_time(
    before: &Book,
    side: Side,
    limit: Option<Price>,
    qty: Qty,
    ts: Timestamp,
    taker_id: OrderId,
    trades: &[Trade],
) {
    let levels = match side {
        Side::Buy => &before.asks,
        Side::Sell => &before.bids,
    };
    let makers: Vec<&RestingOrder> = levels.iter().flat_map(|level| &level.orders).collect();

    for (offset, trade) in trades.iter().enumerate() {
        let maker = makers
            .get(offset)
            .unwrap_or_else(|| panic!("trade {trade:?} has no resting maker in front of it"));
        assert_eq!(trade.maker_id, maker.id);
        assert_eq!(trade.taker_id, taker_id);
        assert_eq!(
            trade.price, maker.price,
            "trade price must be the maker price"
        );
        assert_eq!(trade.taker_side, side);
        assert_eq!(trade.ts, ts);
        assert!(!trade.qty.is_zero());
        assert!(
            trade.qty <= maker.remaining_qty,
            "trade takes more than the maker had"
        );
        assert!(
            crosses(side, limit, trade.price),
            "trade {trade:?} does not cross the limit"
        );
        if trade.qty < maker.remaining_qty {
            assert_eq!(
                offset,
                trades.len() - 1,
                "a partial maker fill must be the last trade"
            );
        }
    }

    let filled = sum_trade_qty(trades);
    assert!(filled <= qty.get(), "fills exceed submitted quantity");
    let unmatched = qty.get() - filled;
    if let Some(last) = trades.last() {
        let maker = makers[trades.len() - 1];
        if last.qty < maker.remaining_qty {
            assert_eq!(unmatched, 0, "taker stopped before filling against a maker");
        }
    }
    if unmatched > 0 {
        if let Some(next) = makers.get(trades.len()) {
            assert!(
                !crosses(side, limit, next.price),
                "crossing liquidity at {} was left unmatched",
                next.price
            );
        }
    }
}

fn crosses(side: Side, limit: Option<Price>, maker_price: Price) -> bool {
    match limit {
        None => true,
        Some(limit) => match side {
            Side::Buy => maker_price <= limit,
            Side::Sell => maker_price >= limit,
        },
    }
}

fn note(ledger: &mut Ledger, command: Command, result: &CommandResult) {
    match result {
        CommandResult::Accepted {
            order_id,
            trades,
            filled_qty,
            canceled_qty,
            ..
        } => {
            let submitted = match command {
                Command::Limit { qty, .. } | Command::Market { qty, .. } => qty.get(),
                Command::Cancel { .. } => panic!("cancel cannot be accepted as an order"),
            };
            for trade in trades {
                *ledger.filled.entry(trade.maker_id).or_insert(0) += trade.qty.get();
            }
            assert!(
                ledger.submitted.insert(*order_id, submitted).is_none(),
                "accepted {order_id} twice"
            );
            assert!(ledger.filled.insert(*order_id, filled_qty.get()).is_none());
            assert!(ledger
                .canceled
                .insert(*order_id, canceled_qty.get())
                .is_none());
        }
        CommandResult::Canceled {
            order_id,
            canceled_qty,
        } => {
            *ledger.canceled.entry(*order_id).or_insert(0) += canceled_qty.get();
        }
        CommandResult::Rejected { .. } | CommandResult::CancelRejected { .. } => {}
    }
}

/// Submitted size of every accepted order equals fills + cancels + resting size.
fn assert_conserved(engine: &impl MatchingEngine, ledger: &Ledger) {
    let book = engine.book();
    assert!(!book.is_crossed(), "crossed book: {book:?}");
    assert_strictly_ordered(&book.bids, true);
    assert_strictly_ordered(&book.asks, false);

    let mut resting = HashMap::<OrderId, u64>::new();
    for (side, levels) in [(Side::Buy, &book.bids), (Side::Sell, &book.asks)] {
        for level in levels {
            let sum = sum_remaining(&level.orders);
            assert_eq!(sum, level.total_qty.get(), "level total at {}", level.price);
            assert!(!level.orders.is_empty(), "empty level in snapshot");
            for order in &level.orders {
                assert_eq!(order.side, side);
                assert_eq!(order.price, level.price);
                assert!(order.remaining_qty.get() > 0);
                assert!(order.remaining_qty <= order.original_qty);
                assert!(
                    resting
                        .insert(order.id, order.remaining_qty.get())
                        .is_none(),
                    "duplicate resting id {}",
                    order.id
                );
            }
        }
    }

    assert_eq!(
        engine
            .best_bid()
            .map(|quote| (quote.price, quote.qty.get())),
        book.bids
            .first()
            .map(|level| (level.price, level.total_qty.get()))
    );
    assert_eq!(
        engine
            .best_ask()
            .map(|quote| (quote.price, quote.qty.get())),
        book.asks
            .first()
            .map(|level| (level.price, level.total_qty.get()))
    );

    for (id, submitted) in &ledger.submitted {
        let filled = ledger.filled.get(id).copied().unwrap_or(0);
        let canceled = ledger.canceled.get(id).copied().unwrap_or(0);
        let live = resting.get(id).copied().unwrap_or(0);
        assert_eq!(
            filled + canceled + live,
            *submitted,
            "quantity not conserved for {id}: filled {filled} canceled {canceled} resting {live} submitted {submitted}"
        );
        match engine.resting_order(*id) {
            Some(order) => assert_eq!(order.remaining_qty.get(), live),
            None => assert_eq!(live, 0, "{id} is resting but lookup missed it"),
        }
    }
    for id in resting.keys() {
        assert!(
            ledger.submitted.contains_key(id),
            "{id} is on the book but was never accepted"
        );
    }
    for id in ledger.filled.keys().chain(ledger.canceled.keys()) {
        assert!(
            ledger.submitted.contains_key(id),
            "{id} was filled or canceled but was never accepted"
        );
    }
}

fn assert_strictly_ordered(levels: &[mini_exchange::Level], descending: bool) {
    for pair in levels.windows(2) {
        if descending {
            assert!(pair[0].price > pair[1].price, "bids are not best-first");
        } else {
            assert!(pair[0].price < pair[1].price, "asks are not best-first");
        }
    }
}

fn find_order(book: &Book, id: OrderId) -> Option<&RestingOrder> {
    book.bids
        .iter()
        .chain(book.asks.iter())
        .flat_map(|level| &level.orders)
        .find(|order| order.id == id)
}

fn sum_trade_qty(trades: &[Trade]) -> u64 {
    trades.iter().map(|trade| trade.qty.get()).sum()
}

fn sum_remaining(orders: &[RestingOrder]) -> u64 {
    orders.iter().map(|order| order.remaining_qty.get()).sum()
}

fn empty_book_has_no_quotes<E: Engine>() {
    let engine = E::fresh_engine();
    assert_eq!(engine.best_bid(), None);
    assert_eq!(engine.best_ask(), None);
    assert!(engine.depth(Side::Buy).is_empty());
    assert!(engine.depth(Side::Sell).is_empty());
    assert!(engine.resting_order(OrderId(1)).is_none());
    assert!(!engine.book().is_crossed());
}

fn exact_fill_prints_at_the_resting_ask<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    assert_eq!(
        drive(&mut engine, &mut ledger, limit(1, Side::Sell, 100, 5, 1)),
        accepted(1, vec![], 0, 5, 0)
    );
    assert_eq!(
        drive(&mut engine, &mut ledger, limit(2, Side::Buy, 100, 5, 2)),
        accepted(2, vec![trade(1, 2, 100, 5, Side::Buy, 2)], 5, 0, 0)
    );
    assert!(engine.book().bids.is_empty());
    assert!(engine.book().asks.is_empty());
}

fn exact_fill_prints_at_the_resting_bid<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Buy, 100, 4, 1));
    assert_eq!(
        drive(&mut engine, &mut ledger, limit(2, Side::Sell, 100, 4, 2)),
        accepted(2, vec![trade(1, 2, 100, 4, Side::Sell, 2)], 4, 0, 0)
    );
    assert!(engine.book().bids.is_empty());
    assert!(engine.book().asks.is_empty());
}

fn aggressive_buy_lifts_a_cheaper_ask_at_the_ask_price<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Sell, 100, 3, 1));
    assert_eq!(
        drive(&mut engine, &mut ledger, limit(2, Side::Buy, 105, 3, 2)),
        accepted(2, vec![trade(1, 2, 100, 3, Side::Buy, 2)], 3, 0, 0)
    );
}

fn aggressive_sell_hits_a_higher_bid_at_the_bid_price<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Buy, 105, 3, 1));
    assert_eq!(
        drive(&mut engine, &mut ledger, limit(2, Side::Sell, 99, 3, 2)),
        accepted(2, vec![trade(1, 2, 105, 3, Side::Sell, 2)], 3, 0, 0)
    );
}

fn equal_prices_cross_instead_of_locking_the_book<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Buy, 50, 2, 1));
    apply(&mut engine, &mut ledger, limit(2, Side::Sell, 50, 2, 2));
    assert!(engine.book().bids.is_empty() && engine.book().asks.is_empty());
    assert!(!engine.book().is_crossed());
}

fn non_crossing_limits_rest_on_both_sides<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Buy, 10, 4, 1));
    apply(&mut engine, &mut ledger, limit(2, Side::Sell, 11, 7, 2));
    let book = engine.book();
    assert_eq!(book.bids.len(), 1);
    assert_eq!(book.asks.len(), 1);
    assert!(book.bids[0].price < book.asks[0].price);
    assert_eq!(engine.best_bid().unwrap().qty, Qty(4));
    assert_eq!(engine.best_ask().unwrap().qty, Qty(7));
}

fn incoming_limit_rests_the_unmatched_remainder<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Sell, 10, 4, 1));
    assert_eq!(
        drive(&mut engine, &mut ledger, limit(2, Side::Buy, 10, 10, 2)),
        accepted(2, vec![trade(1, 2, 10, 4, Side::Buy, 2)], 4, 6, 0)
    );
    let resting = engine.resting_order(OrderId(2)).unwrap();
    assert_eq!(resting.remaining_qty, Qty(6));
    assert_eq!(resting.original_qty, Qty(10));
    assert!(engine.resting_order(OrderId(1)).is_none());
}

fn maker_keeps_its_original_quantity_after_a_partial_fill<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Buy, 10, 9, 1));
    apply(&mut engine, &mut ledger, limit(2, Side::Sell, 10, 2, 2));
    let maker = engine.resting_order(OrderId(1)).unwrap();
    assert_eq!(maker.remaining_qty, Qty(7));
    assert_eq!(maker.original_qty, Qty(9));
    assert!(engine.resting_order(OrderId(2)).is_none());
}

fn sweep_stops_at_the_limit_and_rests_the_remainder<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Sell, 10, 4, 1));
    apply(&mut engine, &mut ledger, limit(2, Side::Sell, 10, 4, 2));
    apply(&mut engine, &mut ledger, limit(3, Side::Sell, 11, 4, 3));
    apply(&mut engine, &mut ledger, limit(4, Side::Sell, 12, 4, 4));

    assert_eq!(
        drive(&mut engine, &mut ledger, limit(5, Side::Buy, 11, 10, 5)),
        accepted(
            5,
            vec![
                trade(1, 5, 10, 4, Side::Buy, 5),
                trade(2, 5, 10, 4, Side::Buy, 5),
                trade(3, 5, 11, 2, Side::Buy, 5),
            ],
            10,
            0,
            0
        )
    );

    let book = engine.book();
    assert!(book.bids.is_empty());
    assert_eq!(book.asks.len(), 2);
    assert_eq!(book.asks[0].price, Price(11));
    assert_eq!(book.asks[0].total_qty, Qty(2));
    assert_eq!(book.asks[0].orders[0].id, OrderId(3));
    assert_eq!(book.asks[1].price, Price(12));
    assert_eq!(book.asks[1].orders[0].id, OrderId(4));
}

fn sweep_that_clears_the_book_rests_the_limit_remainder<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Sell, 10, 2, 1));
    apply(&mut engine, &mut ledger, limit(2, Side::Sell, 11, 2, 2));
    assert_eq!(
        drive(&mut engine, &mut ledger, limit(3, Side::Buy, 11, 10, 3)),
        accepted(
            3,
            vec![
                trade(1, 3, 10, 2, Side::Buy, 3),
                trade(2, 3, 11, 2, Side::Buy, 3),
            ],
            4,
            6,
            0
        )
    );
    assert!(engine.book().asks.is_empty());
    assert_eq!(engine.best_bid().unwrap().price, Price(11));
    assert_eq!(engine.best_bid().unwrap().qty, Qty(6));
}

fn sell_sweep_walks_bids_from_high_to_low<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Buy, 10, 1, 1));
    apply(&mut engine, &mut ledger, limit(2, Side::Buy, 12, 1, 2));
    apply(&mut engine, &mut ledger, limit(3, Side::Buy, 11, 1, 3));
    assert_eq!(
        drive(&mut engine, &mut ledger, limit(4, Side::Sell, 10, 3, 4)),
        accepted(
            4,
            vec![
                trade(2, 4, 12, 1, Side::Sell, 4),
                trade(3, 4, 11, 1, Side::Sell, 4),
                trade(1, 4, 10, 1, Side::Sell, 4),
            ],
            3,
            0,
            0
        )
    );
    assert!(engine.book().bids.is_empty());
}

fn better_price_matches_before_an_earlier_order<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Sell, 100, 5, 1));
    apply(&mut engine, &mut ledger, limit(2, Side::Sell, 99, 5, 2));
    assert_eq!(
        drive(&mut engine, &mut ledger, limit(3, Side::Buy, 100, 5, 3)),
        accepted(3, vec![trade(2, 3, 99, 5, Side::Buy, 3)], 5, 0, 0)
    );
    assert!(engine.resting_order(OrderId(1)).is_some());
    assert!(engine.resting_order(OrderId(2)).is_none());
}

fn fifo_ignores_timestamp_values<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    // Submitted first, despite the later timestamp.
    apply(&mut engine, &mut ledger, limit(1, Side::Buy, 10, 1, 50));
    apply(&mut engine, &mut ledger, limit(2, Side::Buy, 10, 1, 1));
    assert_eq!(
        drive(&mut engine, &mut ledger, limit(3, Side::Sell, 10, 1, 99)),
        accepted(3, vec![trade(1, 3, 10, 1, Side::Sell, 99)], 1, 0, 0)
    );
    assert!(engine.resting_order(OrderId(2)).is_some());
}

fn fifo_partial_fill_reduces_the_front_order_only<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Sell, 7, 5, 1));
    apply(&mut engine, &mut ledger, limit(2, Side::Sell, 7, 5, 2));
    apply(&mut engine, &mut ledger, limit(3, Side::Sell, 7, 5, 3));
    assert_eq!(
        drive(&mut engine, &mut ledger, limit(4, Side::Buy, 7, 12, 4)),
        accepted(
            4,
            vec![
                trade(1, 4, 7, 5, Side::Buy, 4),
                trade(2, 4, 7, 5, Side::Buy, 4),
                trade(3, 4, 7, 2, Side::Buy, 4),
            ],
            12,
            0,
            0
        )
    );
    let front = engine.resting_order(OrderId(3)).unwrap();
    assert_eq!(front.remaining_qty, Qty(3));
    let level = &engine.depth(Side::Sell)[0];
    assert_eq!(level.total_qty, Qty(3));
    assert_eq!(level.orders.len(), 1);
}

fn cancel_in_the_middle_of_a_level_preserves_fifo<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Buy, 15, 2, 1));
    apply(&mut engine, &mut ledger, limit(2, Side::Buy, 15, 2, 2));
    apply(&mut engine, &mut ledger, limit(3, Side::Buy, 15, 2, 3));
    assert_eq!(drive(&mut engine, &mut ledger, cancel(2)), canceled(2, 2));
    let ids: Vec<_> = engine.depth(Side::Buy)[0]
        .orders
        .iter()
        .map(|order| order.id)
        .collect();
    assert_eq!(ids, vec![OrderId(1), OrderId(3)]);
    assert_eq!(engine.best_bid().unwrap().qty, Qty(4));

    assert_eq!(
        drive(&mut engine, &mut ledger, limit(4, Side::Sell, 15, 3, 4)),
        accepted(
            4,
            vec![
                trade(1, 4, 15, 2, Side::Sell, 4),
                trade(3, 4, 15, 1, Side::Sell, 4),
            ],
            3,
            0,
            0
        )
    );
    assert_eq!(
        engine.resting_order(OrderId(3)).unwrap().remaining_qty,
        Qty(1)
    );
}

fn depth_is_best_first_and_aggregates_a_level<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Buy, 9, 1, 1));
    apply(&mut engine, &mut ledger, limit(2, Side::Buy, 11, 2, 2));
    apply(&mut engine, &mut ledger, limit(3, Side::Buy, 11, 3, 3));
    apply(&mut engine, &mut ledger, limit(4, Side::Buy, 10, 4, 4));
    apply(&mut engine, &mut ledger, limit(5, Side::Sell, 20, 1, 5));
    apply(&mut engine, &mut ledger, limit(6, Side::Sell, 18, 8, 6));

    let bids = engine.depth(Side::Buy);
    assert_eq!(
        bids.iter().map(|level| level.price).collect::<Vec<_>>(),
        vec![Price(11), Price(10), Price(9)]
    );
    assert_eq!(bids[0].total_qty, Qty(5));
    assert_eq!(bids[0].orders.len(), 2);
    assert_eq!(engine.best_bid().unwrap().qty, Qty(5));

    let asks = engine.depth(Side::Sell);
    assert_eq!(
        asks.iter().map(|level| level.price).collect::<Vec<_>>(),
        vec![Price(18), Price(20)]
    );
    assert_eq!(engine.best_ask().unwrap().price, Price(18));
}

fn market_buy_fills_in_full<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Sell, 5, 2, 1));
    apply(&mut engine, &mut ledger, limit(2, Side::Sell, 6, 2, 2));
    assert_eq!(
        drive(&mut engine, &mut ledger, market(3, Side::Buy, 3, 3)),
        accepted(
            3,
            vec![
                trade(1, 3, 5, 2, Side::Buy, 3),
                trade(2, 3, 6, 1, Side::Buy, 3),
            ],
            3,
            0,
            0
        )
    );
    assert_eq!(
        engine.resting_order(OrderId(2)).unwrap().remaining_qty,
        Qty(1)
    );
    assert!(engine.resting_order(OrderId(3)).is_none());
}

fn market_sell_fills_in_full<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Buy, 8, 4, 1));
    assert_eq!(
        drive(&mut engine, &mut ledger, market(2, Side::Sell, 4, 2)),
        accepted(2, vec![trade(1, 2, 8, 4, Side::Sell, 2)], 4, 0, 0)
    );
}

fn market_buy_discards_the_remainder_when_the_book_runs_dry<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Sell, 3, 2, 1));
    apply(&mut engine, &mut ledger, limit(2, Side::Sell, 4, 2, 2));
    assert_eq!(
        drive(&mut engine, &mut ledger, market(3, Side::Buy, 10, 3)),
        accepted(
            3,
            vec![
                trade(1, 3, 3, 2, Side::Buy, 3),
                trade(2, 3, 4, 2, Side::Buy, 3),
            ],
            4,
            0,
            6
        )
    );
    assert!(engine.book().asks.is_empty());
    assert!(engine.book().bids.is_empty());
    assert!(engine.resting_order(OrderId(3)).is_none());
}

fn market_on_an_empty_book_cancels_the_whole_quantity<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    assert_eq!(
        drive(&mut engine, &mut ledger, market(1, Side::Buy, 8, 1)),
        accepted(1, vec![], 0, 0, 8)
    );
    assert_eq!(
        drive(&mut engine, &mut ledger, market(2, Side::Sell, 1, 2)),
        accepted(2, vec![], 0, 0, 1)
    );
    assert!(engine.book().bids.is_empty());
    assert!(engine.book().asks.is_empty());
}

fn market_does_not_trade_through_its_own_id_and_leaves_same_side_liquidity<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Buy, 10, 5, 1));
    // A market buy must not match the resting bid, and must not rest.
    assert_eq!(
        drive(&mut engine, &mut ledger, market(2, Side::Buy, 5, 2)),
        accepted(2, vec![], 0, 0, 5)
    );
    assert_eq!(engine.best_bid().unwrap().qty, Qty(5));
}

fn cancel_removes_a_resting_order_and_its_level<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Buy, 10, 4, 1));
    apply(&mut engine, &mut ledger, limit(2, Side::Buy, 9, 1, 2));
    assert_eq!(drive(&mut engine, &mut ledger, cancel(1)), canceled(1, 4));
    assert!(engine.resting_order(OrderId(1)).is_none());
    assert_eq!(engine.best_bid().unwrap().price, Price(9));
    assert_eq!(engine.depth(Side::Buy).len(), 1);
}

fn cancel_removes_only_the_remainder_of_a_partial_fill<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Buy, 10, 10, 1));
    apply(&mut engine, &mut ledger, limit(2, Side::Sell, 10, 4, 2));
    assert_eq!(drive(&mut engine, &mut ledger, cancel(1)), canceled(1, 6));
    assert!(engine.book().bids.is_empty());
    assert_eq!(
        drive(&mut engine, &mut ledger, cancel(1)),
        cancel_rejected(1, CancelRejectReason::AlreadyCanceled)
    );
}

fn cancel_of_a_filled_order_reports_already_filled<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Buy, 10, 2, 1));
    apply(&mut engine, &mut ledger, limit(2, Side::Sell, 10, 2, 2));
    assert_eq!(
        drive(&mut engine, &mut ledger, cancel(1)),
        cancel_rejected(1, CancelRejectReason::AlreadyFilled)
    );
    assert_eq!(
        drive(&mut engine, &mut ledger, cancel(2)),
        cancel_rejected(2, CancelRejectReason::AlreadyFilled)
    );
}

fn cancel_of_an_unknown_id_reports_not_found<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Buy, 3, 1, 1));
    assert_eq!(
        drive(&mut engine, &mut ledger, cancel(99)),
        cancel_rejected(99, CancelRejectReason::NotFound)
    );
    assert!(engine.resting_order(OrderId(1)).is_some());
}

fn cancel_of_a_rejected_id_is_not_found<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    assert_eq!(
        drive(&mut engine, &mut ledger, limit(5, Side::Buy, 3, 0, 1)),
        rejected(5, RejectReason::ZeroQuantity)
    );
    assert_eq!(
        drive(&mut engine, &mut ledger, cancel(5)),
        cancel_rejected(5, CancelRejectReason::NotFound)
    );
}

fn cancel_after_a_partial_market_reports_already_canceled<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Sell, 4, 2, 1));
    apply(&mut engine, &mut ledger, market(2, Side::Buy, 5, 2));
    assert_eq!(
        drive(&mut engine, &mut ledger, cancel(2)),
        cancel_rejected(2, CancelRejectReason::AlreadyCanceled)
    );
}

fn cancel_after_an_empty_market_reports_already_canceled<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, market(1, Side::Sell, 3, 1));
    assert_eq!(
        drive(&mut engine, &mut ledger, cancel(1)),
        cancel_rejected(1, CancelRejectReason::AlreadyCanceled)
    );
}

fn duplicate_id_is_rejected_and_the_book_is_unchanged<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Buy, 10, 2, 1));
    let before = engine.book();
    assert_eq!(
        drive(&mut engine, &mut ledger, limit(1, Side::Sell, 10, 2, 2)),
        rejected(1, RejectReason::DuplicateOrderId)
    );
    assert_eq!(engine.book(), before);
    assert_eq!(
        drive(&mut engine, &mut ledger, market(1, Side::Sell, 2, 3)),
        rejected(1, RejectReason::DuplicateOrderId)
    );
}

fn filled_id_cannot_be_reused<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Buy, 10, 1, 1));
    apply(&mut engine, &mut ledger, limit(2, Side::Sell, 10, 1, 2));
    assert_eq!(
        drive(&mut engine, &mut ledger, limit(1, Side::Buy, 11, 1, 3)),
        rejected(1, RejectReason::DuplicateOrderId)
    );
}

fn canceled_id_cannot_be_reused<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Buy, 10, 1, 1));
    apply(&mut engine, &mut ledger, cancel(1));
    assert_eq!(
        drive(&mut engine, &mut ledger, limit(1, Side::Buy, 10, 1, 2)),
        rejected(1, RejectReason::DuplicateOrderId)
    );
}

fn zero_quantity_does_not_consume_the_id<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    assert_eq!(
        drive(&mut engine, &mut ledger, limit(7, Side::Buy, 10, 0, 1)),
        rejected(7, RejectReason::ZeroQuantity)
    );
    assert_eq!(
        drive(&mut engine, &mut ledger, market(8, Side::Sell, 0, 2)),
        rejected(8, RejectReason::ZeroQuantity)
    );
    assert_eq!(
        drive(&mut engine, &mut ledger, limit(7, Side::Buy, 10, 2, 3)),
        accepted(7, vec![], 0, 2, 0)
    );
    assert_eq!(
        drive(&mut engine, &mut ledger, market(8, Side::Sell, 2, 4)),
        accepted(8, vec![trade(7, 8, 10, 2, Side::Sell, 4)], 2, 0, 0)
    );
}

fn zero_quantity_is_reported_before_a_duplicate_id<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Buy, 4, 1, 1));
    assert_eq!(
        drive(&mut engine, &mut ledger, limit(1, Side::Sell, 4, 0, 2)),
        rejected(1, RejectReason::ZeroQuantity)
    );
    assert_eq!(
        drive(&mut engine, &mut ledger, market(1, Side::Sell, 0, 3)),
        rejected(1, RejectReason::ZeroQuantity)
    );
    assert!(engine.resting_order(OrderId(1)).is_some());
}

fn zero_price_and_zero_timestamp_are_valid<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Buy, 0, 1, 0));
    assert_eq!(
        drive(&mut engine, &mut ledger, limit(2, Side::Sell, 0, 1, 0)),
        accepted(2, vec![trade(1, 2, 0, 1, Side::Sell, 0)], 1, 0, 0)
    );
}

fn quantities_near_u64_max_conserve_shares<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    let max = u64::MAX;
    apply(&mut engine, &mut ledger, limit(1, Side::Buy, 10, max, 1));
    assert_eq!(
        drive(&mut engine, &mut ledger, limit(2, Side::Sell, 10, 3, 2)),
        accepted(2, vec![trade(1, 2, 10, 3, Side::Sell, 2)], 3, 0, 0)
    );
    assert_eq!(
        engine.resting_order(OrderId(1)).unwrap().remaining_qty,
        Qty(max - 3)
    );

    let (mut engine, mut ledger) = fresh::<E>();
    apply(&mut engine, &mut ledger, limit(1, Side::Sell, 5, 1, 1));
    assert_eq!(
        drive(&mut engine, &mut ledger, market(2, Side::Buy, max, 2)),
        accepted(2, vec![trade(1, 2, 5, 1, Side::Buy, 2)], 1, 0, max - 1)
    );
    assert!(engine.book().asks.is_empty());
    assert!(engine.book().bids.is_empty());
}

fn trait_object_can_hold_the_engine<E: Engine>() {
    let mut engine: Box<dyn MatchingEngine> = Box::new(E::fresh_engine());
    let _ = engine.execute(limit(1, Side::Sell, 12, 1, 1));
    let result = engine.execute(limit(2, Side::Buy, 12, 1, 2));
    assert_eq!(
        result,
        accepted(2, vec![trade(1, 2, 12, 1, Side::Buy, 2)], 1, 0, 0)
    );
    assert!(engine.best_ask().is_none());
    assert!(engine.best_bid().is_none());
}

fn identical_sequences_produce_identical_results_and_books<E: Engine>() {
    let commands = sample_sequence();
    let mut first = E::fresh_engine();
    let mut second = E::fresh_engine();
    let mut first_ledger = Ledger::default();
    let mut second_ledger = Ledger::default();

    for command in commands {
        let left = drive(&mut first, &mut first_ledger, command);
        let right = drive(&mut second, &mut second_ledger, command);
        assert_eq!(left, right);
        assert_eq!(first.book(), second.book());
        assert_eq!(first, second);
    }
}

fn scripted_session_matches_this_book<E: Engine>() {
    let (mut engine, mut ledger) = fresh::<E>();
    let mut other = E::fresh_engine();
    let mut other_ledger = Ledger::default();

    let steps = [
        limit(1, Side::Buy, 10, 5, 1),
        limit(2, Side::Buy, 10, 3, 2),
        limit(3, Side::Buy, 9, 4, 3),
        limit(4, Side::Sell, 12, 8, 4),
        limit(5, Side::Sell, 11, 2, 5),
        limit(6, Side::Buy, 11, 6, 6),
        market(7, Side::Sell, 10, 7),
        cancel(2),
        market(8, Side::Buy, 100, 8),
        cancel(1),
        cancel(99),
        limit(8, Side::Sell, 1, 1, 9),
        limit(10, Side::Buy, 1, 0, 10),
        limit(9, Side::Buy, 7, 2, 11),
    ];

    let mut results = Vec::new();
    for command in steps {
        let result = drive(&mut engine, &mut ledger, command);
        let again = drive(&mut other, &mut other_ledger, command);
        assert_eq!(result, again);
        results.push(result);
    }

    assert_eq!(
        results[5],
        accepted(6, vec![trade(5, 6, 11, 2, Side::Buy, 6)], 2, 4, 0)
    );
    assert_eq!(
        results[6],
        accepted(
            7,
            vec![
                trade(6, 7, 11, 4, Side::Sell, 7),
                trade(1, 7, 10, 5, Side::Sell, 7),
                trade(2, 7, 10, 1, Side::Sell, 7),
            ],
            10,
            0,
            0
        )
    );
    assert_eq!(results[7], canceled(2, 2));
    assert_eq!(
        results[8],
        accepted(8, vec![trade(4, 8, 12, 8, Side::Buy, 8)], 8, 0, 92)
    );
    assert_eq!(
        results[9],
        cancel_rejected(1, CancelRejectReason::AlreadyFilled)
    );
    assert_eq!(
        results[10],
        cancel_rejected(99, CancelRejectReason::NotFound)
    );
    assert_eq!(results[11], rejected(8, RejectReason::DuplicateOrderId));
    assert_eq!(results[12], rejected(10, RejectReason::ZeroQuantity));

    let book = engine.book();
    assert_eq!(book.asks.len(), 0);
    assert_eq!(book.bids.len(), 2);
    assert_eq!(book.bids[0].price, Price(9));
    assert_eq!(book.bids[0].orders[0].id, OrderId(3));
    assert_eq!(book.bids[0].total_qty, Qty(4));
    assert_eq!(book.bids[1].price, Price(7));
    assert_eq!(book.bids[1].orders[0].id, OrderId(9));
    assert_eq!(book.bids[1].total_qty, Qty(2));
    assert_eq!(engine, other);
}

fn sample_sequence() -> Vec<Command> {
    vec![
        limit(1, Side::Buy, 100, 5, 1),
        limit(2, Side::Buy, 100, 5, 2),
        limit(3, Side::Sell, 105, 4, 3),
        limit(4, Side::Buy, 105, 2, 4),
        market(5, Side::Sell, 20, 5),
        cancel(2),
        cancel(2),
        cancel(1),
        limit(6, Side::Sell, 90, 3, 6),
        market(7, Side::Buy, 1, 7),
        limit(7, Side::Buy, 90, 1, 8),
        limit(8, Side::Buy, 0, 0, 9),
        market(9, Side::Buy, 0, 10),
        limit(8, Side::Sell, 1, 1, 11),
    ]
}

fn random_streams_are_deterministic_and_preserve_invariants<E: Engine>() {
    for seed in [1_u64, 2, 3, 7, 99, 1_000, 0x00C0_FFEE] {
        run_random_session::<E>(seed, 300);
    }
}

fn run_random_session<E: Engine>(seed: u64, steps: u64) {
    let commands = random_commands(seed, steps);
    let mut primary = E::fresh_engine();
    let mut replica = E::fresh_engine();
    let mut primary_ledger = Ledger::default();
    let mut replica_ledger = Ledger::default();
    let midpoint = (steps / 2) as usize;
    let mut cloned = None;
    let mut cloned_ledger = None;

    for (index, command) in commands.iter().copied().enumerate() {
        let context = format!("seed {seed} step {index} command {command:?}");
        let left = step(&context, || {
            drive(&mut primary, &mut primary_ledger, command)
        });
        let right = step(&context, || {
            drive(&mut replica, &mut replica_ledger, command)
        });
        assert_eq!(left, right, "{context}");
        assert_eq!(primary, replica, "{context}");

        if index + 1 == midpoint {
            cloned = Some(primary.clone());
            cloned_ledger = Some(primary_ledger.clone());
        }
        if let (Some(fork), Some(fork_ledger)) = (cloned.as_mut(), cloned_ledger.as_mut()) {
            if index + 1 > midpoint {
                let forked = step(&context, || drive(fork, fork_ledger, command));
                assert_eq!(left, forked, "clone diverged at {context}");
                assert_eq!(primary, *fork, "clone state diverged at {context}");
            }
        }
    }
}

fn step<T>(context: &str, body: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(value) => value,
        Err(payload) => panic!("{context}\n{}", panic_message(&payload)),
    }
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "engine assertion failed".to_string()
    }
}

fn random_commands(seed: u64, steps: u64) -> Vec<Command> {
    let mut rng = Lcg::new(seed);
    let mut commands = Vec::with_capacity(steps as usize);
    for step in 0..steps {
        let ts = Timestamp(step);
        let kind = rng.below(100);
        if kind < 12 {
            commands.push(Command::cancel(OrderId(rng.below(50) + 1)));
            continue;
        }
        let id = OrderId(rng.below(36) + 1);
        let side = if rng.below(2) == 0 {
            Side::Buy
        } else {
            Side::Sell
        };
        let qty = if rng.below(12) == 0 {
            Qty::ZERO
        } else {
            Qty(rng.below(4) + 1)
        };
        if kind < 35 {
            commands.push(Command::market(id, side, qty, ts));
        } else {
            let price = Price(rng.below(6) + 1);
            commands.push(Command::limit(id, side, price, qty, ts));
        }
    }
    commands
}

/// Fixed numerical-recipes generator. The stream is part of the test, not the engine.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0
    }

    fn below(&mut self, exclusive_end: u64) -> u64 {
        self.next() % exclusive_end
    }
}

macro_rules! instantiate {
    ($($name:ident),* $(,)?) => {
        $(
            mod $name {
                #[test]
                fn reference_engine() {
                    super::$name::<mini_exchange::ReferenceEngine>();
                }

                #[test]
                fn fast_engine() {
                    super::$name::<mini_exchange::FastEngine>();
                }
            }
        )*
    };
}

instantiate!(
    empty_book_has_no_quotes,
    exact_fill_prints_at_the_resting_ask,
    exact_fill_prints_at_the_resting_bid,
    aggressive_buy_lifts_a_cheaper_ask_at_the_ask_price,
    aggressive_sell_hits_a_higher_bid_at_the_bid_price,
    equal_prices_cross_instead_of_locking_the_book,
    non_crossing_limits_rest_on_both_sides,
    incoming_limit_rests_the_unmatched_remainder,
    maker_keeps_its_original_quantity_after_a_partial_fill,
    sweep_stops_at_the_limit_and_rests_the_remainder,
    sweep_that_clears_the_book_rests_the_limit_remainder,
    sell_sweep_walks_bids_from_high_to_low,
    better_price_matches_before_an_earlier_order,
    fifo_ignores_timestamp_values,
    fifo_partial_fill_reduces_the_front_order_only,
    cancel_in_the_middle_of_a_level_preserves_fifo,
    depth_is_best_first_and_aggregates_a_level,
    market_buy_fills_in_full,
    market_sell_fills_in_full,
    market_buy_discards_the_remainder_when_the_book_runs_dry,
    market_on_an_empty_book_cancels_the_whole_quantity,
    market_does_not_trade_through_its_own_id_and_leaves_same_side_liquidity,
    cancel_removes_a_resting_order_and_its_level,
    cancel_removes_only_the_remainder_of_a_partial_fill,
    cancel_of_a_filled_order_reports_already_filled,
    cancel_of_an_unknown_id_reports_not_found,
    cancel_of_a_rejected_id_is_not_found,
    cancel_after_a_partial_market_reports_already_canceled,
    cancel_after_an_empty_market_reports_already_canceled,
    duplicate_id_is_rejected_and_the_book_is_unchanged,
    filled_id_cannot_be_reused,
    canceled_id_cannot_be_reused,
    zero_quantity_does_not_consume_the_id,
    zero_quantity_is_reported_before_a_duplicate_id,
    zero_price_and_zero_timestamp_are_valid,
    quantities_near_u64_max_conserve_shares,
    trait_object_can_hold_the_engine,
    identical_sequences_produce_identical_results_and_books,
    scripted_session_matches_this_book,
    random_streams_are_deterministic_and_preserve_invariants,
);

/// Run one command on both engines and require equal results and books.
fn exchange(
    fast: &mut FastEngine,
    reference: &mut ReferenceEngine,
    fast_ledger: &mut Ledger,
    reference_ledger: &mut Ledger,
    command: Command,
) -> CommandResult {
    let context = format!("{command:?}");
    let left = step(&context, || drive(fast, fast_ledger, command));
    let right = step(&context, || drive(reference, reference_ledger, command));
    assert_eq!(left, right, "{context}");
    assert_eq!(fast.book(), reference.book(), "{context}");
    assert_eq!(fast.best_bid(), reference.best_bid(), "{context}");
    assert_eq!(fast.best_ask(), reference.best_ask(), "{context}");
    left
}

fn agree(
    fast: &mut FastEngine,
    reference: &mut ReferenceEngine,
    fast_ledger: &mut Ledger,
    reference_ledger: &mut Ledger,
    command: Command,
) {
    let _ = exchange(fast, reference, fast_ledger, reference_ledger, command);
}

fn assert_engines_agree(commands: &[Command], label: &str) {
    let mut fast = FastEngine::new();
    let mut reference = ReferenceEngine::new();
    let mut fast_ledger = Ledger::default();
    let mut reference_ledger = Ledger::default();
    for (index, command) in commands.iter().copied().enumerate() {
        let context = format!("{label} step {index}");
        let left = step(&context, || drive(&mut fast, &mut fast_ledger, command));
        let right = step(&context, || {
            drive(&mut reference, &mut reference_ledger, command)
        });
        assert_eq!(left, right, "{context} command {command:?}");
        assert_eq!(fast.book(), reference.book(), "{context}");
        assert_eq!(fast.best_bid(), reference.best_bid(), "{context}");
        assert_eq!(fast.best_ask(), reference.best_ask(), "{context}");
    }
}

#[test]
fn cancel_head_middle_and_tail_keeps_fifo_on_both_engines() {
    for side in [Side::Buy, Side::Sell] {
        let mut fast = FastEngine::new();
        let mut reference = ReferenceEngine::new();
        let mut fast_ledger = Ledger::default();
        let mut reference_ledger = Ledger::default();
        for id in 1..=5 {
            agree(
                &mut fast,
                &mut reference,
                &mut fast_ledger,
                &mut reference_ledger,
                limit(id, side, 40, 10, id),
            );
        }
        agree(
            &mut fast,
            &mut reference,
            &mut fast_ledger,
            &mut reference_ledger,
            cancel(1),
        );
        agree(
            &mut fast,
            &mut reference,
            &mut fast_ledger,
            &mut reference_ledger,
            cancel(5),
        );
        agree(
            &mut fast,
            &mut reference,
            &mut fast_ledger,
            &mut reference_ledger,
            cancel(3),
        );

        let book = fast.book();
        let level = match side {
            Side::Buy => &book.bids[0],
            Side::Sell => &book.asks[0],
        };
        let ids: Vec<_> = level.orders.iter().map(|order| order.id.get()).collect();
        assert_eq!(ids, vec![2, 4]);
        assert_eq!(level.total_qty, Qty(20));

        agree(
            &mut fast,
            &mut reference,
            &mut fast_ledger,
            &mut reference_ledger,
            limit(6, side, 40, 10, 6),
        );
        let result = exchange(
            &mut fast,
            &mut reference,
            &mut fast_ledger,
            &mut reference_ledger,
            limit(7, side.opposite(), 40, 15, 7),
        );
        let aggressor = side.opposite();
        assert_eq!(
            result,
            accepted(
                7,
                vec![
                    trade(2, 7, 40, 10, aggressor, 7),
                    trade(4, 7, 40, 5, aggressor, 7),
                ],
                15,
                0,
                0,
            )
        );
        assert_eq!(
            fast.resting_order(OrderId(4)).unwrap().remaining_qty,
            Qty(5)
        );
        assert_eq!(
            fast.resting_order(OrderId(6)).unwrap().remaining_qty,
            Qty(10)
        );
        assert_eq!(fast.book(), reference.book());
    }
}

#[test]
fn emptying_a_level_updates_best_bid_and_ask() {
    let mut fast = FastEngine::new();
    let mut reference = ReferenceEngine::new();
    let mut fast_ledger = Ledger::default();
    let mut reference_ledger = Ledger::default();

    agree(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        limit(1, Side::Buy, 30, 2, 1),
    );
    agree(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        limit(2, Side::Buy, 30, 3, 2),
    );
    agree(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        limit(3, Side::Buy, 20, 5, 3),
    );
    agree(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        limit(4, Side::Buy, 10, 4, 4),
    );
    agree(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        limit(5, Side::Sell, 40, 2, 5),
    );
    agree(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        limit(6, Side::Sell, 40, 2, 6),
    );
    agree(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        limit(7, Side::Sell, 50, 6, 7),
    );
    assert_eq!(fast.best_bid().unwrap().price, Price(30));
    assert_eq!(fast.best_bid().unwrap().qty, Qty(5));
    assert_eq!(fast.best_ask().unwrap().price, Price(40));
    assert_eq!(fast.best_ask().unwrap().qty, Qty(4));

    agree(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        cancel(1),
    );
    assert_eq!(fast.best_bid().unwrap().qty, Qty(3));
    agree(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        cancel(2),
    );
    assert_eq!(fast.best_bid().unwrap().price, Price(20));
    assert_eq!(fast.best_bid().unwrap().qty, Qty(5));
    assert_eq!(fast.depth(Side::Buy).len(), 2);
    agree(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        cancel(3),
    );
    assert_eq!(fast.best_bid().unwrap().price, Price(10));

    agree(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        cancel(5),
    );
    assert_eq!(fast.best_ask().unwrap().price, Price(40));
    assert_eq!(fast.best_ask().unwrap().qty, Qty(2));
    agree(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        cancel(6),
    );
    assert_eq!(fast.best_ask().unwrap().price, Price(50));
    assert_eq!(fast.depth(Side::Sell).len(), 1);

    agree(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        limit(8, Side::Buy, 25, 1, 8),
    );
    assert_eq!(fast.best_bid().unwrap().price, Price(25));
    agree(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        cancel(8),
    );
    assert_eq!(fast.best_bid().unwrap().price, Price(10));
    agree(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        limit(9, Side::Sell, 45, 1, 9),
    );
    assert_eq!(fast.best_ask().unwrap().price, Price(45));
    agree(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        cancel(9),
    );
    assert_eq!(fast.best_ask().unwrap().price, Price(50));

    let cleared = exchange(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        market(10, Side::Sell, 10, 10),
    );
    assert_eq!(
        cleared,
        accepted(10, vec![trade(4, 10, 10, 4, Side::Sell, 10)], 4, 0, 6)
    );
    assert!(fast.best_bid().is_none());
    assert_eq!(fast.best_ask().unwrap().price, Price(50));
    agree(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        cancel(7),
    );
    assert!(fast.best_ask().is_none());
    assert!(fast.book().bids.is_empty() && fast.book().asks.is_empty());
    assert_eq!(fast.book(), reference.book());
}

#[test]
fn rapid_level_creation_and_eviction_matches_the_reference() {
    let mut fast = FastEngine::new();
    let mut reference = ReferenceEngine::new();
    let mut fast_ledger = Ledger::default();
    let mut reference_ledger = Ledger::default();

    for price in 1..=40 {
        agree(
            &mut fast,
            &mut reference,
            &mut fast_ledger,
            &mut reference_ledger,
            limit(price, Side::Buy, price, 1, price),
        );
        agree(
            &mut fast,
            &mut reference,
            &mut fast_ledger,
            &mut reference_ledger,
            limit(1_000 + price, Side::Sell, 1_000 + price, 1, price),
        );
    }
    assert_eq!(fast.best_bid().unwrap().price, Price(40));
    assert_eq!(fast.best_ask().unwrap().price, Price(1_001));
    assert_eq!(fast.depth(Side::Buy).len(), 40);
    assert_eq!(fast.depth(Side::Sell).len(), 40);

    for price in (1u64..=40).rev() {
        if price.is_multiple_of(2) {
            agree(
                &mut fast,
                &mut reference,
                &mut fast_ledger,
                &mut reference_ledger,
                cancel(price),
            );
            agree(
                &mut fast,
                &mut reference,
                &mut fast_ledger,
                &mut reference_ledger,
                cancel(1_000 + price),
            );
        }
    }
    assert_eq!(fast.best_bid().unwrap().price, Price(39));
    assert_eq!(fast.best_ask().unwrap().price, Price(1_001));
    assert_eq!(fast.depth(Side::Buy).len(), 20);
    assert_eq!(fast.depth(Side::Sell).len(), 20);

    for price in 1u64..=40 {
        if price.is_multiple_of(2) {
            agree(
                &mut fast,
                &mut reference,
                &mut fast_ledger,
                &mut reference_ledger,
                limit(2_000 + price, Side::Buy, price, 3, 2_000 + price),
            );
            agree(
                &mut fast,
                &mut reference,
                &mut fast_ledger,
                &mut reference_ledger,
                limit(3_000 + price, Side::Sell, 1_000 + price, 3, 3_000 + price),
            );
        }
    }
    assert_eq!(fast.best_bid().unwrap().price, Price(40));
    assert_eq!(fast.best_bid().unwrap().qty, Qty(3));
    assert_eq!(fast.best_ask().unwrap().price, Price(1_001));
    assert_eq!(fast.best_ask().unwrap().qty, Qty(1));
    assert_eq!(fast.depth(Side::Buy).len(), 40);

    // 40, 39, 38, and 37 hold 3 + 1 + 3 + 1. A sell of 8 clears them.
    agree(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        limit(9_000, Side::Sell, 1, 8, 9_000),
    );
    assert_eq!(fast.best_bid().unwrap().price, Price(36));
    assert_eq!(fast.best_bid().unwrap().qty, Qty(3));
    assert_eq!(fast.best_ask().unwrap().price, Price(1_001));

    // 1001..=1004 hold 1 + 3 + 1 + 3. A buy of 8 clears them.
    agree(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        limit(9_001, Side::Buy, 10_000, 8, 9_001),
    );
    assert_eq!(fast.best_ask().unwrap().price, Price(1_005));
    assert_eq!(fast.best_ask().unwrap().qty, Qty(1));
    assert_eq!(fast.book(), reference.book());
}

#[test]
fn multi_level_sweep_matches_the_reference() {
    let mut fast = FastEngine::new();
    let mut reference = ReferenceEngine::new();
    let mut fast_ledger = Ledger::default();
    let mut reference_ledger = Ledger::default();

    let mut id = 1u64;
    for price in 10..30 {
        for _ in 0..3 {
            agree(
                &mut fast,
                &mut reference,
                &mut fast_ledger,
                &mut reference_ledger,
                limit(id, Side::Sell, price, 2, id),
            );
            id += 1;
        }
    }
    let swept = exchange(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        limit(id, Side::Buy, 19, 58, id),
    );
    match swept {
        CommandResult::Accepted {
            filled_qty,
            resting_qty,
            canceled_qty,
            trades,
            ..
        } => {
            assert_eq!(filled_qty, Qty(58));
            assert_eq!(resting_qty, Qty::ZERO);
            assert_eq!(canceled_qty, Qty::ZERO);
            assert_eq!(trades.len(), 29);
            assert!(trades.iter().all(|trade| trade.qty == Qty(2)));
        }
        other => panic!("expected the sweep to be accepted, got {other:?}"),
    }
    assert!(fast.best_bid().is_none());
    assert_eq!(fast.best_ask().unwrap().price, Price(19));
    assert_eq!(fast.best_ask().unwrap().qty, Qty(2));
    assert_eq!(fast.depth(Side::Sell).len(), 11);

    id += 1;
    let cleared = exchange(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        market(id, Side::Buy, 1_000, id),
    );
    match cleared {
        CommandResult::Accepted {
            filled_qty,
            resting_qty,
            canceled_qty,
            trades,
            ..
        } => {
            assert_eq!(filled_qty, Qty(62));
            assert_eq!(resting_qty, Qty::ZERO);
            assert_eq!(canceled_qty, Qty(938));
            assert_eq!(trades.len(), 31);
        }
        other => panic!("expected the market sweep to be accepted, got {other:?}"),
    }
    assert!(fast.book().bids.is_empty() && fast.book().asks.is_empty());
    assert_eq!(fast.book(), reference.book());

    let mut fast = FastEngine::new();
    let mut reference = ReferenceEngine::new();
    let mut fast_ledger = Ledger::default();
    let mut reference_ledger = Ledger::default();
    let mut id = 1u64;
    for price in (10..30).rev() {
        for _ in 0..3 {
            agree(
                &mut fast,
                &mut reference,
                &mut fast_ledger,
                &mut reference_ledger,
                limit(id, Side::Buy, price, 2, id),
            );
            id += 1;
        }
    }
    let swept = exchange(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        limit(id, Side::Sell, 20, 58, id),
    );
    match swept {
        CommandResult::Accepted {
            filled_qty,
            resting_qty,
            canceled_qty,
            trades,
            ..
        } => {
            assert_eq!(
                (filled_qty, resting_qty, canceled_qty),
                (Qty(58), Qty::ZERO, Qty::ZERO)
            );
            assert_eq!(trades.len(), 29);
        }
        other => panic!("expected the bid sweep to be accepted, got {other:?}"),
    }
    assert!(fast.best_ask().is_none());
    assert_eq!(fast.best_bid().unwrap().price, Price(20));
    assert_eq!(fast.best_bid().unwrap().qty, Qty(2));
    assert_eq!(fast.depth(Side::Buy).len(), 11);

    id += 1;
    let cleared = exchange(
        &mut fast,
        &mut reference,
        &mut fast_ledger,
        &mut reference_ledger,
        market(id, Side::Sell, 1_000, id),
    );
    match cleared {
        CommandResult::Accepted {
            filled_qty,
            resting_qty,
            canceled_qty,
            ..
        } => {
            assert_eq!(
                (filled_qty, resting_qty, canceled_qty),
                (Qty(62), Qty::ZERO, Qty(938))
            );
        }
        other => panic!("expected the bid market sweep to be accepted, got {other:?}"),
    }
    assert!(fast.book().bids.is_empty() && fast.book().asks.is_empty());
    assert_eq!(fast.book(), reference.book());
}

#[test]
fn fast_engine_matches_reference_on_fixed_seed_streams() {
    assert_engines_agree(&sample_sequence(), "sample");
    for seed in [1_u64, 2, 3, 7, 99, 1_000, 0x00C0_FFEE] {
        assert_engines_agree(&random_commands(seed, 300), &format!("phase1 seed {seed}"));
    }
    // Wider id and price ranges than the Phase 1 generator, so slabs grow and
    // reuse slots. Phase 3 can scale this same shape to a million commands.
    for seed in [1_u64, 7, 99, 0x00C0_FFEE, 42] {
        assert_engines_agree(&stress_commands(seed, 800), &format!("stress seed {seed}"));
    }
}

/// Fixed-seed stream with many distinct ids, overlapping prices, and cancels.
fn stress_commands(seed: u64, steps: usize) -> Vec<Command> {
    let mut rng = Lcg::new(seed);
    let mut next_id = 1u64;
    let mut issued = Vec::new();
    let mut commands = Vec::with_capacity(steps);
    for step in 0..steps {
        let ts = Timestamp(step as u64);
        let roll = rng.below(100);
        if roll < 22 {
            let id = if issued.is_empty() || rng.below(5) == 0 {
                rng.below(next_id.saturating_add(4))
            } else {
                issued[rng.below(issued.len() as u64) as usize]
            };
            commands.push(Command::cancel(OrderId(id)));
            continue;
        }
        let duplicate = !issued.is_empty() && rng.below(30) == 0;
        let (id, fresh_id) = if duplicate {
            (issued[rng.below(issued.len() as u64) as usize], false)
        } else {
            let id = next_id;
            next_id += 1;
            (id, true)
        };
        let side = if rng.below(2) == 0 {
            Side::Buy
        } else {
            Side::Sell
        };
        let qty = if rng.below(20) == 0 {
            Qty::ZERO
        } else {
            Qty(rng.below(6) + 1)
        };
        if fresh_id && !qty.is_zero() {
            issued.push(id);
        }
        if roll < 48 {
            commands.push(Command::market(OrderId(id), side, qty, ts));
        } else {
            let price = match side {
                Side::Buy => Price(90 + rng.below(20)),
                Side::Sell => Price(100 + rng.below(20)),
            };
            commands.push(Command::limit(OrderId(id), side, price, qty, ts));
        }
    }
    commands
}
