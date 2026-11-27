//! Strategy interface and the two built-in strategies.
//!
//! A [`Strategy`] sees each market event after it has been applied and may
//! return limit, market, or cancel actions. The backtester allocates order ids
//! from [`StrategyIds`], which starts at `1 << 62`. Simulator ids and ordinary
//! recorded ids stay below that line, so a strategy order cannot collide with
//! the stream it is trading against.
//!
//! [`ComplementArbStrategy`] is the binary-contract strategy. One YES share
//! plus one NO share settles at 100 ticks. It buys both when the asks sum to
//! `100 - min_edge` or less after taker fees, and it can sell the pair when
//! the bids pay `100 + min_edge` or more. Size is the minimum of the two top
//! quantities, `max_order_qty`, and the room under `max_position`.
//!
//! [`TopOfBookMarketMaker`] is the single-book baseline. It joins the bid and
//! the ask, cancels a quote that is no longer at the touch, and stops quoting
//! a side that would push inventory past `max_position`.

use std::collections::BTreeMap;

use crate::event_log::MarketId;
use crate::portfolio::{
    abs_diff_pos, AccountSnapshot, MarkMethod, Portfolio, TopOfBook, COMPLEMENT_PAYOUT,
};
use crate::types::{CommandResult, OrderId, Price, Qty, Side};

/// Strategy order ids occupy `[BASE, u64::MAX]`.
///
/// [`StrategyIds::alloc`] walks up from [`StrategyIds::BASE`]. A market event
/// whose id is already in this range is rejected by the backtester instead of
/// being allowed to collide with a later strategy order.
#[derive(Debug, Clone)]
pub struct StrategyIds {
    next: u64,
}

impl StrategyIds {
    /// First id the allocator hands out. `1 << 62`.
    pub const BASE: u64 = 1 << 62;

    /// Allocator positioned at [`StrategyIds::BASE`].
    pub const fn new() -> Self {
        Self { next: Self::BASE }
    }

    /// Next id. Panics if the `u64` space is exhausted, which a backtest cannot reach.
    pub fn alloc(&mut self) -> OrderId {
        let id = OrderId(self.next);
        self.next = self
            .next
            .checked_add(1)
            .expect("strategy order id space exhausted");
        id
    }

    /// Whether `id` was reserved for strategy orders.
    pub const fn contains(id: OrderId) -> bool {
        id.get() >= Self::BASE
    }
}

impl Default for StrategyIds {
    fn default() -> Self {
        Self::new()
    }
}

/// One order or cancel the strategy wants applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Limit order. `ioc` cancels any remainder after the immediate match.
    Limit {
        /// Book to trade.
        market: MarketId,
        /// Bid or ask.
        side: Side,
        /// Limit price, in ticks.
        price: Price,
        /// Submitted size.
        qty: Qty,
        /// Cancel the unmatched remainder immediately.
        ioc: bool,
    },
    /// Market order. The unmatched remainder does not rest.
    Market {
        /// Book to trade.
        market: MarketId,
        /// Bid or ask.
        side: Side,
        /// Submitted size.
        qty: Qty,
    },
    /// Cancel a resting strategy order.
    Cancel {
        /// Book the order was sent to.
        market: MarketId,
        /// Strategy order id.
        id: OrderId,
    },
}

impl Action {
    /// Immediate-or-cancel limit.
    pub fn ioc_limit(market: MarketId, side: Side, price: Price, qty: Qty) -> Self {
        Self::Limit {
            market,
            side,
            price,
            qty,
            ioc: true,
        }
    }

    /// Resting limit.
    pub fn limit(market: MarketId, side: Side, price: Price, qty: Qty) -> Self {
        Self::Limit {
            market,
            side,
            price,
            qty,
            ioc: false,
        }
    }

    /// Market order.
    pub fn market(market: MarketId, side: Side, qty: Qty) -> Self {
        Self::Market { market, side, qty }
    }

    /// Cancel.
    pub fn cancel(market: MarketId, id: OrderId) -> Self {
        Self::Cancel { market, id }
    }

    /// Book this action targets.
    pub fn target(&self) -> &MarketId {
        match self {
            Self::Limit { market, .. }
            | Self::Market { market, .. }
            | Self::Cancel { market, .. } => market,
        }
    }
}

/// A strategy order that is still resting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveOrder {
    /// Strategy order id.
    pub id: OrderId,
    /// Book the order rests on.
    pub market: MarketId,
    /// Bid or ask.
    pub side: Side,
    /// Limit price.
    pub price: Price,
    /// Quantity still on the book.
    pub remaining: u64,
}

/// Read-only view of the book, the account, and one command result.
#[derive(Debug, Clone, Copy)]
pub struct MarketUpdate<'a> {
    /// Logical sequence of the market event that just applied.
    pub seq: u64,
    /// Market the event was applied to.
    pub market: &'a MarketId,
    /// Result of that event, including any trades.
    pub result: &'a CommandResult,
    /// Top of book for every market that has been seen, in market-id order.
    pub books: &'a BTreeMap<MarketId, TopOfBook>,
    /// Inventory, cash, and pair statistics. Marks are in `account`.
    pub portfolio: &'a Portfolio,
    /// Equity and PnL after the event, before the actions about to be returned.
    pub account: AccountSnapshot,
    /// Strategy orders still resting, oldest first.
    pub live_orders: &'a [LiveOrder],
    /// Taker fee, in ticks per share.
    pub taker_fee: u64,
    /// Maker fee, in ticks per share.
    pub maker_fee: u64,
    /// Mark used for `account`.
    pub mark: MarkMethod,
}

/// Result of one strategy action, with the same read-only context.
#[derive(Debug, Clone, Copy)]
pub struct ActionUpdate<'a> {
    /// Logical sequence of the market event that caused this action.
    pub seq: u64,
    /// Action that was applied.
    pub action: &'a Action,
    /// Engine result. An IOC limit is reported with the remainder already canceled.
    pub result: &'a CommandResult,
    /// Top of book after the action.
    pub books: &'a BTreeMap<MarketId, TopOfBook>,
    /// Portfolio after fills from this action.
    pub portfolio: &'a Portfolio,
    /// Equity and PnL after the action.
    pub account: AccountSnapshot,
    /// Strategy orders still resting.
    pub live_orders: &'a [LiveOrder],
    /// Taker fee, in ticks per share.
    pub taker_fee: u64,
    /// Maker fee, in ticks per share.
    pub maker_fee: u64,
    /// Mark used for `account`.
    pub mark: MarkMethod,
}

/// A deterministic trading strategy.
///
/// `on_market` runs after each recorded or simulated event. `on_action` runs
/// after each action returned by the strategy, so a strategy can react to a
/// partial fill before the next market event. The default `on_action` returns
/// nothing. Both methods must be pure functions of their arguments and of the
/// strategy's own state: no clock and no random source.
pub trait Strategy {
    /// Actions to apply after one market event.
    fn on_market(&mut self, update: &MarketUpdate<'_>) -> Vec<Action>;

    /// Further actions after one strategy action has been applied.
    fn on_action(&mut self, update: &ActionUpdate<'_>) -> Vec<Action> {
        let _ = update;
        Vec::new()
    }
}

/// Knobs for [`ComplementArbStrategy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComplementArbConfig {
    /// YES leg. Defaults to [`MarketId::yes`].
    pub yes: MarketId,
    /// NO leg. Defaults to [`MarketId::no`].
    pub no: MarketId,
    /// Minimum locked edge, in ticks per pair, after taker fees.
    pub min_edge_ticks: u64,
    /// Largest long position on either leg.
    pub max_position: u64,
    /// Largest quantity on one order.
    pub max_order_qty: u64,
    /// How far `|yes - no|` may grow before the strategy stops opening pairs
    /// and tries to buy the lagging leg.
    pub max_unhedged: u64,
    /// Sell both legs when the bids pay `payout + min_edge` after fees.
    pub allow_unwind: bool,
}

impl Default for ComplementArbConfig {
    fn default() -> Self {
        Self {
            yes: MarketId::yes(),
            no: MarketId::no(),
            min_edge_ticks: 1,
            max_position: 200,
            max_order_qty: 20,
            max_unhedged: 0,
            allow_unwind: true,
        }
    }
}

impl ComplementArbConfig {
    /// Config with the three sizing knobs set and the rest left at the defaults.
    pub fn new(min_edge_ticks: u64, max_position: u64, max_order_qty: u64) -> Self {
        Self {
            min_edge_ticks,
            max_position,
            max_order_qty,
            ..Self::default()
        }
    }
}

/// Buy YES and NO when their asks are cheap relative to the 100-tick payout,
/// and optionally sell the pair when the bids are rich.
///
/// Orders are immediate-or-cancel limits at the current touch, sized to the
/// quantity actually displayed there. A partial touch therefore cannot leave
/// a resting leg. If a previous clip did leave the legs uneven, the next
/// decision buys the smaller leg when that hedge still locks a non-negative
/// edge, and it will not open a new pair while `|yes - no|` exceeds
/// `max_unhedged`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComplementArbStrategy {
    config: ComplementArbConfig,
}

impl ComplementArbStrategy {
    /// Strategy with an explicit config.
    pub fn new(config: ComplementArbConfig) -> Self {
        Self { config }
    }

    /// Config this strategy was built with.
    pub fn config(&self) -> &ComplementArbConfig {
        &self.config
    }
}

impl Default for ComplementArbStrategy {
    fn default() -> Self {
        Self::new(ComplementArbConfig::default())
    }
}

impl Strategy for ComplementArbStrategy {
    fn on_market(&mut self, update: &MarketUpdate<'_>) -> Vec<Action> {
        decide(&self.config, update)
    }
}

fn decide(config: &ComplementArbConfig, update: &MarketUpdate<'_>) -> Vec<Action> {
    let yes_net = update.portfolio.net(&config.yes);
    let no_net = update.portfolio.net(&config.no);
    let gap = abs_diff_pos(yes_net, no_net);
    if gap > config.max_unhedged {
        return hedge(config, update, yes_net, no_net);
    }
    if let Some(actions) = open_pair(config, update, yes_net, no_net) {
        return actions;
    }
    if config.allow_unwind {
        if let Some(actions) = unwind_pair(config, update, yes_net, no_net) {
            return actions;
        }
    }
    Vec::new()
}

fn hedge(
    config: &ComplementArbConfig,
    update: &MarketUpdate<'_>,
    yes_net: i64,
    no_net: i64,
) -> Vec<Action> {
    // Only the long-pair case has a well-defined cheap hedge: buy the smaller
    // leg if the excess leg's average cost plus this ask still pays 100.
    if yes_net < 0 || no_net < 0 || yes_net == no_net {
        return Vec::new();
    }
    let (lagging_is_yes, excess) = if yes_net > no_net {
        (false, yes_net - no_net)
    } else {
        (true, no_net - yes_net)
    };
    let Some(excess_qty) = u64::try_from(excess).ok() else {
        return Vec::new();
    };
    let Some(need) = excess_qty.checked_sub(config.max_unhedged) else {
        return Vec::new();
    };
    if need == 0 {
        return Vec::new();
    }
    let (market, other) = if lagging_is_yes {
        (&config.yes, &config.no)
    } else {
        (&config.no, &config.yes)
    };
    let Some(ask) = update.books.get(market).and_then(|book| book.ask) else {
        return Vec::new();
    };
    let Some(avg) = avg_cost_ceil(update.portfolio, other) else {
        return Vec::new();
    };
    let fee = i128::from(update.taker_fee);
    let ask_px = i128::from(ask.price.ticks());
    let payout = i128::from(COMPLEMENT_PAYOUT);
    let Some(locked) = avg.checked_add(ask_px).and_then(|v| v.checked_add(fee)) else {
        return Vec::new();
    };
    if locked > payout {
        return Vec::new();
    }
    let room = long_room(update.portfolio.net(market), config.max_position);
    let qty = need.min(ask.qty.get()).min(config.max_order_qty).min(room);
    if qty == 0 {
        return Vec::new();
    }
    vec![Action::ioc_limit(
        market.clone(),
        Side::Buy,
        ask.price,
        Qty(qty),
    )]
}

fn open_pair(
    config: &ComplementArbConfig,
    update: &MarketUpdate<'_>,
    yes_net: i64,
    no_net: i64,
) -> Option<Vec<Action>> {
    if yes_net < 0 || no_net < 0 {
        return None;
    }
    let yes_book = update.books.get(&config.yes)?;
    let no_book = update.books.get(&config.no)?;
    let yes_ask = yes_book.ask?;
    let no_ask = no_book.ask?;
    if !buy_is_cheap(
        yes_ask.price.ticks(),
        no_ask.price.ticks(),
        update.taker_fee,
        config.min_edge_ticks,
    ) {
        return None;
    }
    let qty = yes_ask
        .qty
        .get()
        .min(no_ask.qty.get())
        .min(config.max_order_qty)
        .min(long_room(yes_net, config.max_position))
        .min(long_room(no_net, config.max_position));
    if qty == 0 {
        return None;
    }
    Some(vec![
        Action::ioc_limit(config.yes.clone(), Side::Buy, yes_ask.price, Qty(qty)),
        Action::ioc_limit(config.no.clone(), Side::Buy, no_ask.price, Qty(qty)),
    ])
}

fn unwind_pair(
    config: &ComplementArbConfig,
    update: &MarketUpdate<'_>,
    yes_net: i64,
    no_net: i64,
) -> Option<Vec<Action>> {
    if yes_net <= 0 || no_net <= 0 {
        return None;
    }
    let yes_book = update.books.get(&config.yes)?;
    let no_book = update.books.get(&config.no)?;
    let yes_bid = yes_book.bid?;
    let no_bid = no_book.bid?;
    if !sell_is_rich(
        yes_bid.price.ticks(),
        no_bid.price.ticks(),
        update.taker_fee,
        config.min_edge_ticks,
    ) {
        return None;
    }
    let yes_qty = u64::try_from(yes_net).ok()?;
    let no_qty = u64::try_from(no_net).ok()?;
    let qty = yes_qty
        .min(no_qty)
        .min(yes_bid.qty.get())
        .min(no_bid.qty.get())
        .min(config.max_order_qty);
    if qty == 0 {
        return None;
    }
    Some(vec![
        Action::ioc_limit(config.yes.clone(), Side::Sell, yes_bid.price, Qty(qty)),
        Action::ioc_limit(config.no.clone(), Side::Sell, no_bid.price, Qty(qty)),
    ])
}

fn buy_is_cheap(yes_ask: u64, no_ask: u64, fee: u64, min_edge: u64) -> bool {
    let cost = u128::from(yes_ask)
        .saturating_add(u128::from(no_ask))
        .saturating_add(u128::from(fee).saturating_mul(2))
        .saturating_add(u128::from(min_edge));
    cost <= u128::from(COMPLEMENT_PAYOUT)
}

fn sell_is_rich(yes_bid: u64, no_bid: u64, fee: u64, min_edge: u64) -> bool {
    let proceeds = u128::from(yes_bid).saturating_add(u128::from(no_bid));
    let need = u128::from(COMPLEMENT_PAYOUT)
        .saturating_add(u128::from(min_edge))
        .saturating_add(u128::from(fee).saturating_mul(2));
    proceeds >= need
}

fn long_room(net: i64, max_position: u64) -> u64 {
    let Ok(max) = i64::try_from(max_position) else {
        return u64::MAX;
    };
    if net >= max {
        0
    } else {
        u64::try_from(max - net).unwrap_or(u64::MAX)
    }
}

/// Average cost rounded up, so a hedge check does not ignore a truncated tick.
fn avg_cost_ceil(portfolio: &Portfolio, market: &MarketId) -> Option<i128> {
    let net = portfolio.net(market);
    if net <= 0 {
        return None;
    }
    let open = portfolio.open_cashflow(market);
    let net = i128::from(net);
    let quotient = open.checked_div(net)?;
    let remainder = open.checked_rem(net)?;
    if remainder == 0 {
        Some(quotient)
    } else {
        quotient.checked_add(1)
    }
}

/// Knobs for [`TopOfBookMarketMaker`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketMakerConfig {
    /// Quote only this market. `None` quotes whichever market just updated.
    pub market: Option<MarketId>,
    /// Largest absolute inventory.
    pub max_position: u64,
    /// Size of each resting quote.
    pub order_qty: u64,
    /// Do not quote when `ask - bid` is below this many ticks.
    pub min_spread: u64,
}

impl Default for MarketMakerConfig {
    fn default() -> Self {
        Self {
            market: None,
            max_position: 100,
            order_qty: 10,
            min_spread: 1,
        }
    }
}

impl MarketMakerConfig {
    /// Quote every market with these size limits. `min_spread` stays 1.
    pub fn new(max_position: u64, order_qty: u64) -> Self {
        Self {
            max_position,
            order_qty,
            ..Self::default()
        }
    }
}

/// Join the best bid and the best ask, one resting order per side.
///
/// Cancels are emitted before new orders, so a replace cannot trade against
/// the strategy's own stale quote. Inventory past `max_position` drops the
/// side that would add to it. Fills happen when the simulated flow hits the
/// resting quote; the strategy does not cross the spread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopOfBookMarketMaker {
    config: MarketMakerConfig,
}

impl TopOfBookMarketMaker {
    /// Market maker with an explicit config.
    pub fn new(config: MarketMakerConfig) -> Self {
        Self { config }
    }

    /// Config this strategy was built with.
    pub fn config(&self) -> &MarketMakerConfig {
        &self.config
    }
}

impl Default for TopOfBookMarketMaker {
    fn default() -> Self {
        Self::new(MarketMakerConfig::default())
    }
}

impl Strategy for TopOfBookMarketMaker {
    fn on_market(&mut self, update: &MarketUpdate<'_>) -> Vec<Action> {
        if let Some(only) = &self.config.market {
            if only != update.market {
                return Vec::new();
            }
        }
        let market = update.market.clone();
        let Some(book) = update.books.get(&market) else {
            return Vec::new();
        };
        let (Some(bid), Some(ask)) = (book.bid, book.ask) else {
            return cancel_market(update, &market);
        };
        let spread = ask.price.ticks().saturating_sub(bid.price.ticks());
        if spread < self.config.min_spread {
            return cancel_market(update, &market);
        }
        let net = update.portfolio.net(&market);
        let buy_qty = side_qty(
            net,
            self.config.max_position,
            self.config.order_qty,
            Side::Buy,
        );
        let sell_qty = side_qty(
            net,
            self.config.max_position,
            self.config.order_qty,
            Side::Sell,
        );
        let mut cancels = Vec::new();
        let mut places = Vec::new();
        sync_side(
            update,
            &market,
            Side::Buy,
            (buy_qty > 0).then_some((bid.price, buy_qty)),
            &mut cancels,
            &mut places,
        );
        sync_side(
            update,
            &market,
            Side::Sell,
            (sell_qty > 0).then_some((ask.price, sell_qty)),
            &mut cancels,
            &mut places,
        );
        cancels.append(&mut places);
        cancels
    }
}

fn side_qty(net: i64, max_position: u64, order_qty: u64, side: Side) -> u64 {
    let Ok(max) = i64::try_from(max_position) else {
        return order_qty;
    };
    let room = match side {
        Side::Buy => max.saturating_sub(net),
        Side::Sell => max.saturating_add(net),
    };
    if room <= 0 {
        0
    } else {
        order_qty.min(u64::try_from(room).unwrap_or(u64::MAX))
    }
}

fn cancel_market(update: &MarketUpdate<'_>, market: &MarketId) -> Vec<Action> {
    update
        .live_orders
        .iter()
        .filter(|order| &order.market == market)
        .map(|order| Action::cancel(market.clone(), order.id))
        .collect()
}

fn sync_side(
    update: &MarketUpdate<'_>,
    market: &MarketId,
    side: Side,
    target: Option<(Price, u64)>,
    cancels: &mut Vec<Action>,
    places: &mut Vec<Action>,
) {
    let mut kept = false;
    for order in update
        .live_orders
        .iter()
        .filter(|order| &order.market == market && order.side == side)
    {
        let same_price = target.is_some_and(|(price, _)| price == order.price);
        if same_price && !kept {
            kept = true;
        } else {
            cancels.push(Action::cancel(market.clone(), order.id));
        }
    }
    if kept {
        return;
    }
    if let Some((price, qty)) = target {
        if qty > 0 {
            places.push(Action::limit(market.clone(), side, price, Qty(qty)));
        }
    }
}
