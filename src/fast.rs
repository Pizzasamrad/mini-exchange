//! Cache-friendly price-time matching engine.
//!
//! [`FastEngine`] follows the same rules as [`crate::ReferenceEngine`]. The
//! layout is built for a tight match and cancel loop:
//!
//! * Resting orders live in a slab. Each price level is an intrusive
//!   doubly-linked FIFO of slab indices, so canceling any order rewrites two
//!   links and a quantity. The hot path does not shift a queue and does not
//!   allocate a heap object per order.
//! * An order-id map points at the live slab slot, or records that the id
//!   already filled or left the book. Lookup, duplicate detection, and cancel
//!   are one hash probe. The hasher has fixed keys, so it adds no randomness.
//! * Each side keeps its best level and links levels from best to worst.
//!   Emptying the inside price promotes the next level without a scan. A
//!   [`BTreeMap`] resolves a price to its level when a level is created.
//! * Slabs reuse slots after fills and cancels. Steady-state matching does not
//!   allocate order storage. Links are indices, so the engine stays in safe
//!   Rust.
//!
//! Accepted ids are never forgotten: the id map grows with every accept. That
//! is what makes a later duplicate or cancel deterministic. Resting storage
//! does not grow with them.
//!
//! Debug builds walk the book after every command and panic if a link,
//! quantity, or index disagrees. Release builds omit that walk.

use std::collections::{BTreeMap, HashMap};
use std::hash::{BuildHasherDefault, Hasher};
use std::ops::Bound::{Excluded, Unbounded};

use crate::engine::{price_crosses, MatchingEngine};
use crate::types::{
    CancelRejectReason, Command, CommandResult, Level, OrderId, Price, Qty, Quote, RejectReason,
    RestingOrder, Side, Timestamp, Trade,
};

/// Price-time order book tuned for throughput.
///
/// Observable behavior matches [`crate::ReferenceEngine`]. See the
/// [module docs](self) for the layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FastEngine {
    orders: Arena<StoredOrder>,
    price_levels: Arena<PriceLevel>,
    bids: SideBook,
    asks: SideBook,
    /// Every accepted id. Live orders point at a slab slot. Filled and
    /// canceled ids stay so the id cannot be reused and a later cancel can
    /// say why.
    index: OrderIndex,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct SlotId(u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct LevelId(u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OrderLoc {
    Live(SlotId),
    Filled,
    Canceled,
}

type OrderIndex = HashMap<OrderId, OrderLoc, BuildHasherDefault<FxHasher>>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct StoredOrder {
    id: OrderId,
    side: Side,
    price: Price,
    remaining_qty: Qty,
    original_qty: Qty,
    ts: Timestamp,
    prev: Option<SlotId>,
    next: Option<SlotId>,
    level: LevelId,
}

/// One price on one side. `better` is toward the top of the book (higher bid,
/// lower ask). `head` is the oldest resting order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct PriceLevel {
    price: Price,
    total_qty: Qty,
    head: Option<SlotId>,
    tail: Option<SlotId>,
    better: Option<LevelId>,
    worse: Option<LevelId>,
    count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SideBook {
    by_price: BTreeMap<Price, LevelId>,
    best: Option<LevelId>,
}

impl SideBook {
    fn new() -> Self {
        Self {
            by_price: BTreeMap::new(),
            best: None,
        }
    }
}

impl FastEngine {
    /// Empty book.
    pub fn new() -> Self {
        Self {
            orders: Arena::new(),
            price_levels: Arena::new(),
            bids: SideBook::new(),
            asks: SideBook::new(),
            index: HashMap::default(),
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
        if self.index.contains_key(&id) {
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
            self.close_taker(id, OrderLoc::Filled);
            (Qty::ZERO, Qty::ZERO)
        } else if let Some(price) = limit {
            self.rest(id, side, price, unmatched, qty, ts);
            (unmatched, Qty::ZERO)
        } else {
            // Market remainder does not rest.
            self.close_taker(id, OrderLoc::Canceled);
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
        let Some(loc) = self.index.get(&id).copied() else {
            return CommandResult::CancelRejected {
                order_id: id,
                reason: CancelRejectReason::NotFound,
            };
        };
        match loc {
            OrderLoc::Live(slot) => {
                let order = self.take_resting_slot(slot, OrderLoc::Canceled);
                CommandResult::Canceled {
                    order_id: id,
                    canceled_qty: order.remaining_qty,
                }
            }
            OrderLoc::Filled => CommandResult::CancelRejected {
                order_id: id,
                reason: CancelRejectReason::AlreadyFilled,
            },
            OrderLoc::Canceled => CommandResult::CancelRejected {
                order_id: id,
                reason: CancelRejectReason::AlreadyCanceled,
            },
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
        let maker_side = side.opposite();

        while !remaining.is_zero() {
            let Some(level_id) = self.side(maker_side).best else {
                break;
            };
            let maker_price = self.price_levels.get(level_id.0).price;
            if !price_crosses(side, limit, maker_price) {
                break;
            }

            let (maker_id, fill) = self.fill_best(level_id, remaining);
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

    /// Fill against the oldest order on `level_id`, which is the current best.
    fn fill_best(&mut self, level_id: LevelId, taker_qty: Qty) -> (OrderId, Qty) {
        let slot = self
            .price_levels
            .get(level_id.0)
            .head
            .expect("best level is non-empty");
        let maker = *self.orders.get(slot.0);
        debug_assert_eq!(maker.level, level_id);
        debug_assert_eq!(self.side(maker.side).best, Some(level_id));
        let fill = std::cmp::min(maker.remaining_qty, taker_qty);
        assert!(!fill.is_zero(), "refusing to print a zero-quantity trade");

        if fill == maker.remaining_qty {
            let removed = self.take_resting_slot(slot, OrderLoc::Filled);
            debug_assert_eq!(removed.id, maker.id);
        } else {
            self.orders.get_mut(slot.0).remaining_qty = maker
                .remaining_qty
                .checked_sub(fill)
                .expect("fill exceeds resting quantity");
            let level = self.price_levels.get_mut(level_id.0);
            level.total_qty = level
                .total_qty
                .checked_sub(fill)
                .expect("fill exceeds level quantity");
        }

        (maker.id, fill)
    }

    fn rest(
        &mut self,
        id: OrderId,
        side: Side,
        price: Price,
        remaining_qty: Qty,
        original_qty: Qty,
        ts: Timestamp,
    ) {
        let level_id = self.ensure_level(side, price);
        let (new_total, new_count) = {
            let level = self.price_levels.get(level_id.0);
            let new_total = level
                .total_qty
                .checked_add(remaining_qty)
                .expect("price-level quantity overflowed u64");
            let new_count = level
                .count
                .checked_add(1)
                .expect("level order count overflow");
            (new_total, new_count)
        };
        let slot = SlotId(self.orders.alloc(StoredOrder {
            id,
            side,
            price,
            remaining_qty,
            original_qty,
            ts,
            prev: None,
            next: None,
            level: level_id,
        }));
        self.link_tail(level_id, slot);
        let level = self.price_levels.get_mut(level_id.0);
        level.total_qty = new_total;
        level.count = new_count;
        debug_assert!(level.head.is_some() && level.tail.is_some());
        self.insert_live(id, slot);
    }

    /// Unlink `slot`, update its level, and record `terminal`.
    ///
    /// The slot returns to the free list only after the index no longer calls
    /// it live, so a reused slot cannot be confused with this order.
    fn take_resting_slot(&mut self, slot: SlotId, terminal: OrderLoc) -> StoredOrder {
        debug_assert!(matches!(terminal, OrderLoc::Filled | OrderLoc::Canceled));
        let order = *self.orders.get(slot.0);
        debug_assert!(!order.remaining_qty.is_zero());
        self.unlink(order.level, slot);
        let empty = {
            let level = self.price_levels.get_mut(order.level.0);
            level.total_qty = level
                .total_qty
                .checked_sub(order.remaining_qty)
                .expect("level quantity underflow");
            level.count = level.count.checked_sub(1).expect("level count underflow");
            let empty = level.count == 0;
            if empty {
                assert!(
                    level.total_qty.is_zero(),
                    "empty level at {} still has quantity",
                    level.price
                );
                assert!(
                    level.head.is_none() && level.tail.is_none(),
                    "empty level at {} still has orders",
                    level.price
                );
            } else {
                assert!(
                    !level.total_qty.is_zero(),
                    "level at {} has orders but no quantity",
                    level.price
                );
                assert!(
                    level.head.is_some() && level.tail.is_some(),
                    "level at {} lost its FIFO ends",
                    level.price
                );
            }
            empty
        };
        let previous = self.index.insert(order.id, terminal);
        assert!(
            matches!(previous, Some(OrderLoc::Live(live)) if live == slot),
            "order {} was not live in this slot",
            order.id
        );
        if empty {
            self.remove_level(order.side, order.level);
        }
        self.orders.free(slot.0);
        order
    }

    fn close_taker(&mut self, id: OrderId, state: OrderLoc) {
        debug_assert!(matches!(state, OrderLoc::Filled | OrderLoc::Canceled));
        let previous = self.index.insert(id, state);
        assert!(previous.is_none(), "order {id} left the book twice");
    }

    fn insert_live(&mut self, id: OrderId, slot: SlotId) {
        let previous = self.index.insert(id, OrderLoc::Live(slot));
        assert!(previous.is_none(), "order {id} was already accepted");
    }

    fn ensure_level(&mut self, side: Side, price: Price) -> LevelId {
        if let Some(id) = self.side(side).by_price.get(&price).copied() {
            return id;
        }
        self.insert_level(side, price)
    }

    fn insert_level(&mut self, side: Side, price: Price) -> LevelId {
        debug_assert!(!self.side(side).by_price.contains_key(&price));
        let (better, worse) = self.neighbors(side, price);
        let id = LevelId(self.price_levels.alloc(PriceLevel {
            price,
            total_qty: Qty::ZERO,
            head: None,
            tail: None,
            better,
            worse,
            count: 0,
        }));
        debug_assert_ne!(Some(id), better);
        debug_assert_ne!(Some(id), worse);
        if let Some(better_id) = better {
            self.price_levels.get_mut(better_id.0).worse = Some(id);
        }
        if let Some(worse_id) = worse {
            self.price_levels.get_mut(worse_id.0).better = Some(id);
        }
        {
            let book = self.side_mut(side);
            book.by_price.insert(price, id);
            // `better == None` means nothing on this side is priced better.
            if better.is_none() {
                book.best = Some(id);
            }
        }
        id
    }

    fn remove_level(&mut self, side: Side, id: LevelId) {
        let level = *self.price_levels.get(id.0);
        debug_assert_eq!(level.count, 0);
        debug_assert!(level.head.is_none() && level.tail.is_none());
        debug_assert!(level.total_qty.is_zero());
        if let Some(better) = level.better {
            self.price_levels.get_mut(better.0).worse = level.worse;
        }
        if let Some(worse) = level.worse {
            self.price_levels.get_mut(worse.0).better = level.better;
        }
        {
            let book = self.side_mut(side);
            assert_eq!(
                book.by_price.remove(&level.price),
                Some(id),
                "level {} missing from the price map",
                level.price
            );
            if book.best == Some(id) {
                book.best = level.worse;
            }
        }
        self.price_levels.free(id.0);
    }

    /// `(better, worse)` neighbors of a price that is not on the book yet.
    fn neighbors(&self, side: Side, price: Price) -> (Option<LevelId>, Option<LevelId>) {
        let by_price = &self.side(side).by_price;
        let higher = by_price
            .range((Excluded(price), Unbounded))
            .next()
            .map(|(_, id)| *id);
        let lower = by_price.range(..price).next_back().map(|(_, id)| *id);
        match side {
            // Higher bids are better. Lower asks are better.
            Side::Buy => (higher, lower),
            Side::Sell => (lower, higher),
        }
    }

    fn link_tail(&mut self, level_id: LevelId, slot: SlotId) {
        let tail = self.price_levels.get(level_id.0).tail;
        match tail {
            None => {
                let level = self.price_levels.get_mut(level_id.0);
                debug_assert!(level.head.is_none());
                level.head = Some(slot);
                level.tail = Some(slot);
            }
            Some(tail) => {
                debug_assert!(self.orders.get(tail.0).next.is_none());
                self.orders.get_mut(tail.0).next = Some(slot);
                self.orders.get_mut(slot.0).prev = Some(tail);
                self.price_levels.get_mut(level_id.0).tail = Some(slot);
            }
        }
    }

    fn unlink(&mut self, level_id: LevelId, slot: SlotId) {
        let order = *self.orders.get(slot.0);
        debug_assert_eq!(order.level, level_id);
        match order.prev {
            Some(prev) => {
                debug_assert_eq!(self.orders.get(prev.0).next, Some(slot));
                self.orders.get_mut(prev.0).next = order.next;
            }
            None => {
                debug_assert_eq!(self.price_levels.get(level_id.0).head, Some(slot));
                self.price_levels.get_mut(level_id.0).head = order.next;
            }
        }
        match order.next {
            Some(next) => {
                debug_assert_eq!(self.orders.get(next.0).prev, Some(slot));
                self.orders.get_mut(next.0).prev = order.prev;
            }
            None => {
                debug_assert_eq!(self.price_levels.get(level_id.0).tail, Some(slot));
                self.price_levels.get_mut(level_id.0).tail = order.prev;
            }
        }
    }

    fn side(&self, side: Side) -> &SideBook {
        match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        }
    }

    fn side_mut(&mut self, side: Side) -> &mut SideBook {
        match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        }
    }

    fn top(&self, side: Side) -> Option<Quote> {
        let level_id = self.side(side).best?;
        let level = self.price_levels.get(level_id.0);
        Some(Quote {
            price: level.price,
            qty: level.total_qty,
        })
    }
}

impl Default for FastEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl MatchingEngine for FastEngine {
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
        #[cfg(debug_assertions)]
        self.check_invariants();
        result
    }

    fn depth(&self, side: Side) -> Vec<Level> {
        let mut levels = Vec::with_capacity(self.side(side).by_price.len());
        let mut cursor = self.side(side).best;
        let level_limit = self.price_levels.slots.len();
        let mut seen_levels = 0usize;
        while let Some(level_id) = cursor {
            seen_levels += 1;
            assert!(seen_levels <= level_limit, "cycle in {side} price levels");
            let level = self.price_levels.get(level_id.0);
            let price = level.price;
            let total_qty = level.total_qty;
            let next_level = level.worse;
            let mut order_cursor = level.head;
            let mut orders = Vec::with_capacity(level.count as usize);
            let order_limit = self.orders.slots.len();
            let mut seen_orders = 0usize;
            while let Some(slot) = order_cursor {
                seen_orders += 1;
                assert!(
                    seen_orders <= order_limit,
                    "cycle in the FIFO list at {price}"
                );
                let order = self.orders.get(slot.0);
                orders.push(RestingOrder {
                    id: order.id,
                    side: order.side,
                    price: order.price,
                    remaining_qty: order.remaining_qty,
                    original_qty: order.original_qty,
                    ts: order.ts,
                });
                order_cursor = order.next;
            }
            levels.push(Level {
                price,
                total_qty,
                orders,
            });
            cursor = next_level;
        }
        levels
    }

    fn best_bid(&self) -> Option<Quote> {
        self.top(Side::Buy)
    }

    fn best_ask(&self) -> Option<Quote> {
        self.top(Side::Sell)
    }

    fn resting_order(&self, id: OrderId) -> Option<RestingOrder> {
        let OrderLoc::Live(slot) = self.index.get(&id).copied()? else {
            return None;
        };
        let order = self.orders.get(slot.0);
        Some(RestingOrder {
            id: order.id,
            side: order.side,
            price: order.price,
            remaining_qty: order.remaining_qty,
            original_qty: order.original_qty,
            ts: order.ts,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Arena<T> {
    slots: Vec<Slot<T>>,
    free_head: Option<u32>,
    live: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Slot<T> {
    Vacant { next: Option<u32> },
    Occupied(T),
}

impl<T> Arena<T> {
    fn new() -> Self {
        Self {
            slots: Vec::new(),
            free_head: None,
            live: 0,
        }
    }

    #[inline]
    fn alloc(&mut self, value: T) -> u32 {
        let index = if let Some(index) = self.free_head.take() {
            let next = match &self.slots[index as usize] {
                Slot::Vacant { next } => *next,
                Slot::Occupied(_) => panic!("free list points at occupied arena slot {index}"),
            };
            self.free_head = next;
            self.slots[index as usize] = Slot::Occupied(value);
            index
        } else {
            let index = u32::try_from(self.slots.len()).expect("arena exceeded u32::MAX slots");
            self.slots.push(Slot::Occupied(value));
            index
        };
        self.live = self.live.checked_add(1).expect("arena live count overflow");
        index
    }

    #[inline]
    fn free(&mut self, index: u32) -> T {
        let Some(slot_ref) = self.slots.get_mut(index as usize) else {
            panic!("arena slot {index} is out of range");
        };
        let slot = std::mem::replace(
            slot_ref,
            Slot::Vacant {
                next: self.free_head,
            },
        );
        self.free_head = Some(index);
        self.live = self
            .live
            .checked_sub(1)
            .expect("arena live count underflow");
        match slot {
            Slot::Occupied(value) => value,
            Slot::Vacant { .. } => panic!("arena slot {index} was already free"),
        }
    }

    #[inline]
    fn get(&self, index: u32) -> &T {
        match self.slots.get(index as usize) {
            Some(Slot::Occupied(value)) => value,
            Some(Slot::Vacant { .. }) => panic!("arena slot {index} is vacant"),
            None => panic!("arena slot {index} is out of range"),
        }
    }

    #[inline]
    fn get_mut(&mut self, index: u32) -> &mut T {
        match self.slots.get_mut(index as usize) {
            Some(Slot::Occupied(value)) => value,
            Some(Slot::Vacant { .. }) => panic!("arena slot {index} is vacant"),
            None => panic!("arena slot {index} is out of range"),
        }
    }

    #[cfg(debug_assertions)]
    fn check_free_list(&self) {
        let mut seen = vec![false; self.slots.len()];
        let mut cursor = self.free_head;
        let mut free_count = 0usize;
        while let Some(index) = cursor {
            let position = index as usize;
            assert!(
                position < self.slots.len(),
                "free list index {index} is out of range"
            );
            assert!(!seen[position], "cycle in the arena free list at {index}");
            seen[position] = true;
            match &self.slots[position] {
                Slot::Vacant { next } => cursor = *next,
                Slot::Occupied(_) => panic!("free list points at occupied arena slot {index}"),
            }
            free_count += 1;
        }
        let mut occupied = 0usize;
        for (position, slot) in self.slots.iter().enumerate() {
            match slot {
                Slot::Occupied(_) => {
                    assert!(!seen[position], "occupied slot {position} is also free");
                    occupied += 1;
                }
                Slot::Vacant { .. } => {
                    assert!(
                        seen[position],
                        "vacant slot {position} is missing from the free list"
                    );
                }
            }
        }
        assert_eq!(occupied, self.live as usize, "arena live count is wrong");
        assert_eq!(occupied + free_count, self.slots.len());
    }
}

/// Fixed-key hasher. `HashMap`'s default hasher is seeded from the OS, which
/// the engine is not allowed to read. Order ids are not a secret, so a fast
/// integer mix is enough.
#[derive(Clone, Default)]
struct FxHasher {
    hash: u64,
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, mut bytes: &[u8]) {
        while bytes.len() >= 8 {
            let mut buf = [0; 8];
            buf.copy_from_slice(&bytes[..8]);
            self.write_u64(u64::from_ne_bytes(buf));
            bytes = &bytes[8..];
        }
        if !bytes.is_empty() {
            let mut buf = [0; 8];
            buf[..bytes.len()].copy_from_slice(bytes);
            self.write_u64(u64::from_ne_bytes(buf));
        }
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.hash = (self.hash.rotate_left(5) ^ value).wrapping_mul(0x517c_c1b7_2722_0a95);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

#[cfg(debug_assertions)]
impl FastEngine {
    fn check_invariants(&self) {
        if let (Some(bid), Some(ask)) = (self.top(Side::Buy), self.top(Side::Sell)) {
            assert!(
                bid.price < ask.price,
                "crossed book: best bid {} is not below best ask {}",
                bid.price,
                ask.price
            );
        }

        let mut resting = Vec::new();
        self.check_side(Side::Buy, &mut resting);
        self.check_side(Side::Sell, &mut resting);
        resting.sort_unstable();
        for pair in resting.windows(2) {
            assert_ne!(
                pair[0], pair[1],
                "order {} rests in more than one place",
                pair[0]
            );
        }

        let mut live = 0usize;
        for (id, loc) in &self.index {
            match loc {
                OrderLoc::Live(slot) => {
                    live += 1;
                    assert!(
                        resting.binary_search(id).is_ok(),
                        "order {id} is indexed live but not on a book"
                    );
                    assert_eq!(
                        self.orders.get(slot.0).id,
                        *id,
                        "live index for {id} points at the wrong order"
                    );
                }
                OrderLoc::Filled | OrderLoc::Canceled => {
                    assert!(
                        resting.binary_search(id).is_err(),
                        "order {id} is closed but still resting"
                    );
                }
            }
        }
        assert_eq!(live, resting.len(), "live index does not match the book");
        assert_eq!(
            live, self.orders.live as usize,
            "arena live count does not match the index"
        );
        assert_eq!(
            self.price_levels.live as usize,
            self.bids.by_price.len() + self.asks.by_price.len(),
            "level arena does not match the price maps"
        );
        self.orders.check_free_list();
        self.price_levels.check_free_list();
    }

    fn check_side(&self, side: Side, resting: &mut Vec<OrderId>) {
        let (best, expected) = {
            let book = match side {
                Side::Buy => &self.bids,
                Side::Sell => &self.asks,
            };
            let extreme = match side {
                Side::Buy => book
                    .by_price
                    .iter()
                    .next_back()
                    .map(|(price, id)| (*price, *id)),
                Side::Sell => book.by_price.iter().next().map(|(price, id)| (*price, *id)),
            };
            match (book.best, extreme) {
                (None, None) => {}
                (Some(best_id), Some((_price, id))) => {
                    assert_eq!(best_id, id, "{side} best pointer is not the best price");
                }
                (best_ptr, extreme_entry) => {
                    panic!(
                        "{side} best pointer {best_ptr:?} disagrees with the price map {extreme_entry:?}"
                    );
                }
            }
            let entries: Vec<(Price, LevelId)> = book
                .by_price
                .iter()
                .map(|(price, id)| (*price, *id))
                .collect();
            let expected = match side {
                Side::Buy => entries.into_iter().rev().collect(),
                Side::Sell => entries,
            };
            (book.best, expected)
        };

        let mut cursor = best;
        for (index, (price, id)) in expected.iter().copied().enumerate() {
            let Some(cursor_id) = cursor else {
                panic!("{side} level list ended before {price}");
            };
            assert_eq!(cursor_id, id, "{side} level list diverged at {price}");
            let level = *self.price_levels.get(cursor_id.0);
            assert_eq!(
                level.price, price,
                "{side} level price does not match the map"
            );
            if index == 0 {
                assert_eq!(
                    level.better, None,
                    "{side} best level has a better neighbor"
                );
            } else {
                assert_eq!(level.better, Some(expected[index - 1].1));
            }
            if index + 1 == expected.len() {
                assert_eq!(level.worse, None, "{side} worst level has a worse neighbor");
            } else {
                assert_eq!(level.worse, Some(expected[index + 1].1));
            }
            self.check_orders(side, cursor_id, &level, resting);
            cursor = level.worse;
        }
        assert!(
            cursor.is_none(),
            "{side} level list is longer than the price map"
        );
    }

    fn check_orders(
        &self,
        side: Side,
        level_id: LevelId,
        level: &PriceLevel,
        resting: &mut Vec<OrderId>,
    ) {
        assert!(level.count > 0, "empty {side} level at {}", level.price);
        assert!(level.head.is_some() && level.tail.is_some());
        let mut cursor = level.head;
        let mut prev = None;
        let mut count = 0u32;
        let mut qty = Qty::ZERO;
        while let Some(slot) = cursor {
            count += 1;
            assert!(
                count <= level.count,
                "cycle in the FIFO list at {}",
                level.price
            );
            let order = *self.orders.get(slot.0);
            assert_eq!(order.prev, prev, "prev link mismatch at {}", level.price);
            assert_eq!(order.level, level_id);
            assert_eq!(order.side, side, "order {} is on the wrong book", order.id);
            assert_eq!(
                order.price, level.price,
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
            match self.index.get(&order.id) {
                Some(OrderLoc::Live(live)) => {
                    assert_eq!(*live, slot, "order {} index points elsewhere", order.id);
                }
                other => panic!("order {} index is {other:?} while it is resting", order.id),
            }
            resting.push(order.id);
            qty = qty
                .checked_add(order.remaining_qty)
                .expect("price-level quantity overflowed u64");
            prev = Some(slot);
            cursor = order.next;
        }
        assert_eq!(
            count, level.count,
            "order count mismatch at {}",
            level.price
        );
        assert_eq!(
            qty, level.total_qty,
            "level total mismatch at {}",
            level.price
        );
        assert_eq!(level.tail, prev, "tail pointer mismatch at {}", level.price);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(id: u64, side: Side, price: u64, qty: u64, ts: u64) -> Command {
        Command::limit(OrderId(id), side, Price(price), Qty(qty), Timestamp(ts))
    }

    fn cancel(id: u64) -> Command {
        Command::cancel(OrderId(id))
    }

    fn apply(engine: &mut FastEngine, command: Command) {
        let result = engine.execute(command);
        if matches!(
            result,
            CommandResult::Rejected { .. } | CommandResult::CancelRejected { .. }
        ) {
            panic!("setup command failed: {command:?} -> {result:?}");
        }
    }

    fn live_slot(engine: &FastEngine, id: u64) -> SlotId {
        match engine.index.get(&OrderId(id)) {
            Some(OrderLoc::Live(slot)) => *slot,
            other => panic!("order {id} is {other:?}"),
        }
    }

    fn level_id(engine: &FastEngine, side: Side, price: u64) -> LevelId {
        engine
            .side(side)
            .by_price
            .get(&Price(price))
            .copied()
            .unwrap_or_else(|| panic!("missing {side} level {price}"))
    }

    fn fifo(engine: &FastEngine, side: Side, price: u64) -> Vec<u64> {
        let level = engine.price_levels.get(level_id(engine, side, price).0);
        let mut ids = Vec::new();
        let mut cursor = level.head;
        while let Some(slot) = cursor {
            let order = engine.orders.get(slot.0);
            ids.push(order.id.get());
            cursor = order.next;
        }
        ids
    }

    #[test]
    fn default_engine_matches_new() {
        assert_eq!(FastEngine::default(), FastEngine::new());
        assert!(FastEngine::new().best_bid().is_none());
        assert!(FastEngine::new().best_ask().is_none());
    }

    #[test]
    fn fifo_links_survive_head_middle_tail_cancels_and_slot_reuse() {
        for side in [Side::Buy, Side::Sell] {
            let mut engine = FastEngine::new();
            for id in 1..=5 {
                apply(&mut engine, limit(id, side, 15, 2, id));
            }
            let slots: Vec<SlotId> = (1..=5).map(|id| live_slot(&engine, id)).collect();
            let level = level_id(&engine, side, 15);
            assert_eq!(engine.price_levels.get(level.0).head, Some(slots[0]));
            assert_eq!(engine.price_levels.get(level.0).tail, Some(slots[4]));
            assert_eq!(fifo(&engine, side, 15), vec![1, 2, 3, 4, 5]);

            apply(&mut engine, cancel(1));
            assert_eq!(engine.price_levels.get(level.0).head, Some(slots[1]));
            assert_eq!(engine.orders.get(slots[1].0).prev, None);
            assert_eq!(engine.price_levels.get(level.0).tail, Some(slots[4]));
            assert_eq!(fifo(&engine, side, 15), vec![2, 3, 4, 5]);

            apply(&mut engine, cancel(5));
            assert_eq!(engine.price_levels.get(level.0).tail, Some(slots[3]));
            assert_eq!(engine.orders.get(slots[3].0).next, None);
            assert_eq!(engine.orders.get(slots[2].0).next, Some(slots[3]));
            assert_eq!(fifo(&engine, side, 15), vec![2, 3, 4]);

            apply(&mut engine, cancel(3));
            assert_eq!(engine.orders.get(slots[1].0).next, Some(slots[3]));
            assert_eq!(engine.orders.get(slots[3].0).prev, Some(slots[1]));
            assert_eq!(engine.price_levels.get(level.0).head, Some(slots[1]));
            assert_eq!(engine.price_levels.get(level.0).tail, Some(slots[3]));
            assert_eq!(fifo(&engine, side, 15), vec![2, 4]);
            assert_eq!(engine.price_levels.get(level.0).total_qty, Qty(4));

            let high_water = engine.orders.slots.len();
            assert_eq!(high_water, 5);
            apply(&mut engine, limit(6, side, 15, 2, 6));
            assert_eq!(
                engine.orders.slots.len(),
                high_water,
                "order slot was not reused"
            );
            let slot6 = live_slot(&engine, 6);
            assert!(
                slot6 == slots[0] || slot6 == slots[2] || slot6 == slots[4],
                "new order did not occupy a freed slot"
            );
            assert_eq!(engine.price_levels.get(level.0).tail, Some(slot6));
            assert_eq!(engine.orders.get(slots[3].0).next, Some(slot6));
            assert_eq!(engine.orders.get(slot6.0).prev, Some(slots[3]));
            assert_eq!(engine.orders.get(slot6.0).next, None);
            assert_eq!(fifo(&engine, side, 15), vec![2, 4, 6]);

            // The freed slot now belongs to order 6. Order 1 must stay canceled.
            assert!(matches!(
                engine.execute(cancel(1)),
                CommandResult::CancelRejected {
                    reason: CancelRejectReason::AlreadyCanceled,
                    ..
                }
            ));
            assert_eq!(live_slot(&engine, 6), slot6);
            assert_eq!(engine.orders.get(slot6.0).id, OrderId(6));

            let head = live_slot(&engine, 2);
            apply(&mut engine, limit(7, side.opposite(), 15, 1, 7));
            assert_eq!(
                live_slot(&engine, 2),
                head,
                "a partial fill moved the head order"
            );
            assert_eq!(engine.orders.get(head.0).remaining_qty, Qty(1));
            assert_eq!(engine.price_levels.get(level.0).head, Some(head));
            assert_eq!(engine.price_levels.get(level.0).total_qty, Qty(5));

            apply(&mut engine, limit(8, side.opposite(), 15, 1, 8));
            assert!(engine.resting_order(OrderId(2)).is_none());
            assert_eq!(engine.price_levels.get(level.0).head, Some(slots[3]));
            assert_eq!(fifo(&engine, side, 15), vec![4, 6]);

            apply(&mut engine, limit(9, side, 15, 2, 9));
            assert_eq!(
                engine.orders.slots.len(),
                high_water,
                "a filled order's slot was not reused"
            );
            let slot9 = live_slot(&engine, 9);
            assert_eq!(slot9, head);
            assert_eq!(fifo(&engine, side, 15), vec![4, 6, 9]);
            assert!(matches!(
                engine.execute(cancel(2)),
                CommandResult::CancelRejected {
                    reason: CancelRejectReason::AlreadyFilled,
                    ..
                }
            ));
            assert_eq!(engine.orders.get(slot9.0).id, OrderId(9));
            assert_eq!(
                engine.resting_order(OrderId(9)).unwrap().remaining_qty,
                Qty(2)
            );
        }
    }

    #[test]
    fn best_pointer_is_not_a_reused_level_slot() {
        for side in [Side::Buy, Side::Sell] {
            let (worst, mid, best, reused) = match side {
                Side::Buy => (10, 20, 30, 15),
                Side::Sell => (30, 20, 10, 25),
            };
            let mut engine = FastEngine::new();
            apply(&mut engine, limit(1, side, worst, 1, 1));
            apply(&mut engine, limit(2, side, mid, 1, 2));
            apply(&mut engine, limit(3, side, best, 1, 3));
            let best_before = engine.side(side).best.expect("best");
            assert_eq!(engine.price_levels.get(best_before.0).price, Price(best));
            let level_slots = engine.price_levels.slots.len();

            apply(&mut engine, cancel(3));
            let mid_level = engine.side(side).best.expect("mid");
            assert_ne!(mid_level, best_before);
            assert_eq!(engine.price_levels.get(mid_level.0).price, Price(mid));
            assert_eq!(engine.top(side).unwrap().price, Price(mid));

            apply(&mut engine, limit(4, side, reused, 1, 4));
            assert_eq!(
                engine.price_levels.slots.len(),
                level_slots,
                "level slot was not reused"
            );
            assert_eq!(engine.side(side).best, Some(mid_level));
            assert_eq!(engine.top(side).unwrap().price, Price(mid));
            assert_eq!(
                engine.price_levels.get(best_before.0).price,
                Price(reused),
                "freed best slot was not reused for the new worse price"
            );

            apply(&mut engine, cancel(2));
            let remaining_best = match side {
                Side::Buy => worst.max(reused),
                Side::Sell => worst.min(reused),
            };
            assert_eq!(engine.top(side).unwrap().price, Price(remaining_best));
        }
    }

    #[test]
    fn order_and_level_slots_are_reused_after_a_sweep() {
        let mut engine = FastEngine::new();
        for id in 1..=8 {
            apply(&mut engine, limit(id, Side::Sell, 10 + (id % 3), 1, id));
        }
        let order_slots = engine.orders.slots.len();
        let level_slots = engine.price_levels.slots.len();
        assert_eq!(order_slots, 8);
        assert_eq!(level_slots, 3);
        apply(
            &mut engine,
            Command::market(OrderId(100), Side::Buy, Qty(8), Timestamp(100)),
        );
        assert_eq!(engine.orders.live, 0);
        assert_eq!(engine.price_levels.live, 0);

        for id in 101..=108 {
            apply(&mut engine, limit(id, Side::Sell, 10 + (id % 3), 1, id));
        }
        assert_eq!(engine.orders.slots.len(), order_slots);
        assert_eq!(engine.price_levels.slots.len(), level_slots);
        assert_eq!(engine.orders.live, 8);
        assert_eq!(engine.price_levels.live, 3);
        assert_eq!(engine.best_ask().unwrap().price, Price(10));
    }
}
