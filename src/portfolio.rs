//! Integer-tick portfolio, PnL, and drawdown accounting.
//!
//! Cash, cost basis, fees, and marks are ticks. Nothing here uses floating
//! point, so a replay cannot drift. A fill updates cash and a FIFO lot queue.
//! Realized PnL is the difference between the closing cashflow and the cost of
//! the lots it consumes. Unrealized PnL marks whatever is still open.
//!
//! A long YES position and a long NO position (or a short pair) are one binary
//! contract. Each matched pair is marked at [`COMPLEMENT_PAYOUT`] ticks, which
//! is what the pair pays at settlement. Only the unmatched remainder is marked
//! to the book. That split does not change total equity: equity is cash plus
//! the settlement value of the pairs plus the mark of everything else.
//!
//! The cash identity, checked at the end of a backtest, is
//!
//! ```text
//! cash = initial_cash + realized_pnl - open_cashflow
//! equity = cash + inventory_value
//!        = initial_cash + realized_pnl + unrealized_pnl
//! ```
//!
//! `open_cashflow` is the net cash spent to open the current inventory. It is
//! positive for a long and negative for a short. Fees are part of that
//! cashflow and are also accumulated in `fees_paid`.

use std::collections::{BTreeMap, VecDeque};

use crate::event_log::MarketId;
use crate::types::{Price, Qty, Quote, Side};

/// Settlement value, in ticks, of one YES share plus one NO share.
pub const COMPLEMENT_PAYOUT: u64 = 100;

/// How an open share is marked when it is not part of a YES/NO pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarkMethod {
    /// Two-sided books use `(bid + ask) / 2`, rounded down. One side uses that
    /// side. An empty book uses the last trade, then the position's own cost.
    Mid,
    /// Long inventory is marked at the bid, short inventory at the ask. A
    /// missing side falls back to the other side, then the last trade, then cost.
    Touch,
}

impl std::fmt::Display for MarkMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mid => f.write_str("mid"),
            Self::Touch => f.write_str("touch"),
        }
    }
}

/// Inside quote used to mark a market.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TopOfBook {
    /// Best bid, if any.
    pub bid: Option<Quote>,
    /// Best ask, if any.
    pub ask: Option<Quote>,
    /// Last trade price on this market.
    pub last: Option<Price>,
}

/// A fill could not be applied without overflowing tick arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortfolioError;

impl std::fmt::Display for PortfolioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("portfolio arithmetic overflowed")
    }
}

impl std::error::Error for PortfolioError {}

/// What one fill did to the inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillEffect {
    /// Realized PnL, in ticks, from the quantity that closed an existing lot.
    pub realized_pnl: i128,
    /// Shares that reduced an open position.
    pub closed_qty: u64,
    /// Shares that opened a new lot, including a flip's remainder.
    pub opened_qty: u64,
    /// Signed position after the fill. Positive is long.
    pub net: i64,
}

/// Point-in-time account, in ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountSnapshot {
    /// Cash after every fill and fee.
    pub cash: i128,
    /// Locked-in PnL from closed lots.
    pub realized_pnl: i128,
    /// Mark minus the cost of inventory that is still open.
    pub unrealized_pnl: i128,
    /// `cash + inventory_value`.
    pub equity: i128,
    /// Settlement value of pairs plus the mark of every other open share.
    pub inventory_value: i128,
    /// Taker and maker fees paid, in ticks. Already included in cash and PnL.
    pub fees_paid: i128,
    /// `realized_pnl + unrealized_pnl`, which equals `equity - initial_cash`.
    pub net_pnl: i128,
}

/// One market at the end of a backtest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionSnapshot {
    /// Market symbol.
    pub market: MarketId,
    /// Signed shares. Positive is long.
    pub net: i64,
    /// Cash spent to open the current lots. Negative when the open position is short.
    pub open_cashflow: i128,
    /// `open_cashflow / net`, rounded toward zero. `None` when the position is flat.
    pub avg_cost_ticks: Option<i128>,
    /// Largest absolute position seen on this market.
    pub max_abs_net: u64,
}

/// Complement pairs the portfolio has scored.
///
/// A pair is scored when the overlapping YES and NO inventory grows. The edge
/// is `payout * qty` minus the FIFO cost of the shares that formed the pair
/// (or, for a short pair, the credits received minus the payout).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ArbStats {
    /// Cumulative paired shares.
    pub qty: u64,
    /// Paired shares whose edge was strictly positive.
    pub wins: u64,
    /// Paired shares whose edge was strictly negative.
    pub losses: u64,
    /// Paired shares whose edge was zero.
    pub scratches: u64,
    /// Sum of per-pair edge, in ticks.
    pub edge_sum: i128,
}

impl ArbStats {
    /// Average edge per paired share, rounded toward zero.
    pub fn avg_edge_ticks(self) -> Option<i128> {
        let qty = i128::from(self.qty);
        self.edge_sum.checked_div(qty)
    }

    /// `wins / (wins + losses + scratches)`, in basis points. 10_000 is 100%.
    pub fn win_rate_bps(self) -> Option<u64> {
        rate_bps(self.wins, self.losses, self.scratches)
    }

    fn record(&mut self, qty: u64, edge: i128) -> Result<(), PortfolioError> {
        self.qty = self.qty.checked_add(qty).ok_or(PortfolioError)?;
        self.edge_sum = self.edge_sum.checked_add(edge).ok_or(PortfolioError)?;
        let bucket = match edge.cmp(&0) {
            std::cmp::Ordering::Greater => &mut self.wins,
            std::cmp::Ordering::Less => &mut self.losses,
            std::cmp::Ordering::Equal => &mut self.scratches,
        };
        *bucket = bucket.checked_add(qty).ok_or(PortfolioError)?;
        Ok(())
    }
}

/// Wins over wins, losses, and scratches, in basis points.
pub fn rate_bps(wins: u64, losses: u64, scratches: u64) -> Option<u64> {
    let total = wins.checked_add(losses)?.checked_add(scratches)?;
    if total == 0 {
        return None;
    }
    let num = u128::from(wins).checked_mul(10_000)?;
    num.checked_div(u128::from(total))
        .and_then(|bps| u64::try_from(bps).ok())
}

/// Peak equity and the worst peak-to-trough drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawdownTracker {
    peak: i128,
    max_drawdown: i128,
    /// Peak that was in force when `max_drawdown` was recorded.
    peak_at_max: i128,
}

impl DrawdownTracker {
    /// Tracker whose first observation is `equity`.
    pub fn new(equity: i128) -> Self {
        Self {
            peak: equity,
            max_drawdown: 0,
            peak_at_max: equity,
        }
    }

    /// Record an equity observation. The peak updates before the drawdown.
    pub fn observe(&mut self, equity: i128) {
        if equity > self.peak {
            self.peak = equity;
        }
        let dd = self.peak.saturating_sub(equity);
        if dd > self.max_drawdown {
            self.max_drawdown = dd;
            self.peak_at_max = self.peak;
        }
    }

    /// Highest equity observed.
    pub fn peak(&self) -> i128 {
        self.peak
    }

    /// Largest drop from a peak to a later equity, in ticks.
    pub fn max_drawdown_ticks(&self) -> i128 {
        self.max_drawdown
    }

    /// Drawdown of the worst tick episode, in basis points of the peak that
    /// was in force during that episode. 10_000 basis points is 100%. A
    /// non-positive peak has no basis-point reading and yields 0.
    pub fn max_drawdown_bps(&self) -> u64 {
        drawdown_bps(self.max_drawdown, self.peak_at_max)
    }
}

/// `drawdown * 10_000 / peak`, rounded down. Zero when the peak is not positive.
pub fn drawdown_bps(drawdown: i128, peak: i128) -> u64 {
    if drawdown <= 0 || peak <= 0 {
        return 0;
    }
    let Some(drawdown) = u128::try_from(drawdown).ok() else {
        return u64::MAX;
    };
    let Some(peak) = u128::try_from(peak).ok() else {
        return u64::MAX;
    };
    let Some(scaled) = drawdown.checked_mul(10_000) else {
        return u64::MAX;
    };
    let Some(bps) = scaled.checked_div(peak) else {
        return u64::MAX;
    };
    u64::try_from(bps).unwrap_or(u64::MAX)
}

#[derive(Debug, Clone)]
struct Lot {
    qty: u64,
    /// Cash paid per share to open the lot. Negative when opening a short
    /// received more cash than the fee.
    cash_per_share: i128,
}

#[derive(Debug, Clone)]
struct Position {
    lots: VecDeque<Lot>,
    net: i64,
    open_cashflow: i128,
    max_abs_net: u64,
}

impl Position {
    fn new() -> Self {
        Self {
            lots: VecDeque::new(),
            net: 0,
            open_cashflow: 0,
            max_abs_net: 0,
        }
    }

    fn note_net(&mut self) {
        let abs = self.net.unsigned_abs();
        if abs > self.max_abs_net {
            self.max_abs_net = abs;
        }
    }
}

/// Cash, FIFO inventory, and complement-pair statistics.
#[derive(Debug, Clone)]
pub struct Portfolio {
    initial_cash: i128,
    cash: i128,
    realized: i128,
    fees: i128,
    positions: BTreeMap<MarketId, Position>,
    arb: ArbStats,
    /// YES/NO shares currently counted as an open pair. Grows only when the
    /// overlap grows, so closing a pair does not score those shares again.
    paired_open: u64,
}

impl Portfolio {
    /// Flat portfolio with `initial_cash` ticks.
    pub fn new(initial_cash: i128) -> Self {
        Self {
            initial_cash,
            cash: initial_cash,
            realized: 0,
            fees: 0,
            positions: BTreeMap::new(),
            arb: ArbStats::default(),
            paired_open: 0,
        }
    }

    /// Cash the portfolio was created with.
    pub fn initial_cash(&self) -> i128 {
        self.initial_cash
    }

    /// Cash after fills and fees.
    pub fn cash(&self) -> i128 {
        self.cash
    }

    /// Realized PnL, in ticks.
    pub fn realized_pnl(&self) -> i128 {
        self.realized
    }

    /// Fees paid, in ticks.
    pub fn fees_paid(&self) -> i128 {
        self.fees
    }

    /// Signed position. Zero when the market has never traded.
    pub fn net(&self, market: &MarketId) -> i64 {
        self.positions.get(market).map(|pos| pos.net).unwrap_or(0)
    }

    /// Cash spent to hold the current lots on `market`.
    pub fn open_cashflow(&self, market: &MarketId) -> i128 {
        self.positions
            .get(market)
            .map(|pos| pos.open_cashflow)
            .unwrap_or(0)
    }

    /// Average open cash per share, rounded toward zero.
    pub fn avg_cost_ticks(&self, market: &MarketId) -> Option<i128> {
        let pos = self.positions.get(market)?;
        if pos.net == 0 {
            None
        } else {
            pos.open_cashflow.checked_div(i128::from(pos.net))
        }
    }

    /// Sum of open cashflows.
    pub fn total_open_cashflow(&self) -> Result<i128, PortfolioError> {
        let mut total = 0i128;
        for pos in self.positions.values() {
            total = total.checked_add(pos.open_cashflow).ok_or(PortfolioError)?;
        }
        Ok(total)
    }

    /// Pairs scored so far.
    pub fn arb_stats(&self) -> ArbStats {
        self.arb
    }

    /// Apply one fill. `fee_per_share` is charged on every share, in ticks.
    ///
    /// Buying pays `price + fee` per share. Selling receives `price - fee`.
    /// A fill that reduces the position realizes PnL against the oldest lots.
    /// Any remainder opens a new lot, which is how a position flips.
    pub fn apply_fill(
        &mut self,
        market: &MarketId,
        side: Side,
        price: Price,
        qty: Qty,
        fee_per_share: u64,
    ) -> Result<FillEffect, PortfolioError> {
        let qty = qty.get();
        if qty == 0 {
            return Ok(FillEffect {
                realized_pnl: 0,
                closed_qty: 0,
                opened_qty: 0,
                net: self.net(market),
            });
        }
        let qty_i = i64::try_from(qty).map_err(|_| PortfolioError)?;
        let per_share = cash_per_share(side, price.ticks(), fee_per_share)?;
        let notional = per_share
            .checked_mul(i128::from(qty_i))
            .ok_or(PortfolioError)?;
        let fee = i128::from(fee_per_share)
            .checked_mul(i128::from(qty_i))
            .ok_or(PortfolioError)?;
        self.cash = self.cash.checked_sub(notional).ok_or(PortfolioError)?;
        self.fees = self.fees.checked_add(fee).ok_or(PortfolioError)?;

        let (realized, closed, left) = {
            let pos = self
                .positions
                .entry(market.clone())
                .or_insert_with(Position::new);
            let closing = match side {
                Side::Buy => pos.net < 0,
                Side::Sell => pos.net > 0,
            };
            let mut left = qty;
            let mut closed = 0u64;
            let mut realized = 0i128;
            if closing {
                while left > 0 {
                    let Some(lot) = pos.lots.front_mut() else {
                        break;
                    };
                    let take = left.min(lot.qty);
                    let take_i = i128::from(take);
                    let open_cash = lot
                        .cash_per_share
                        .checked_mul(take_i)
                        .ok_or(PortfolioError)?;
                    let close_cash = per_share.checked_mul(take_i).ok_or(PortfolioError)?;
                    // Closing cashflow is money out. Profit is minus that outflow
                    // minus the cash that was spent (or received) to open the lot.
                    let pnl = close_cash
                        .checked_neg()
                        .and_then(|v| v.checked_sub(open_cash))
                        .ok_or(PortfolioError)?;
                    realized = realized.checked_add(pnl).ok_or(PortfolioError)?;
                    pos.open_cashflow = pos
                        .open_cashflow
                        .checked_sub(open_cash)
                        .ok_or(PortfolioError)?;
                    lot.qty -= take;
                    left -= take;
                    closed += take;
                    if lot.qty == 0 {
                        pos.lots.pop_front();
                    }
                }
            }
            if left > 0 {
                let left_i = i128::from(left);
                let open_cash = per_share.checked_mul(left_i).ok_or(PortfolioError)?;
                pos.open_cashflow = pos
                    .open_cashflow
                    .checked_add(open_cash)
                    .ok_or(PortfolioError)?;
                push_lot(&mut pos.lots, left, per_share);
            }
            let signed = match side {
                Side::Buy => qty_i,
                Side::Sell => qty_i.checked_neg().ok_or(PortfolioError)?,
            };
            pos.net = pos.net.checked_add(signed).ok_or(PortfolioError)?;
            pos.note_net();
            debug_assert!(lots_match(pos));
            (realized, closed, left)
        };
        self.realized = self.realized.checked_add(realized).ok_or(PortfolioError)?;
        self.score_pairs()?;
        let net = self.net(market);
        Ok(FillEffect {
            realized_pnl: realized,
            closed_qty: closed,
            opened_qty: left,
            net,
        })
    }

    /// Mark every open position.
    pub fn account(
        &self,
        books: &BTreeMap<MarketId, TopOfBook>,
        mark: MarkMethod,
    ) -> Result<AccountSnapshot, PortfolioError> {
        let inventory_value = self.inventory_value(books, mark)?;
        let open = self.total_open_cashflow()?;
        let unrealized = inventory_value.checked_sub(open).ok_or(PortfolioError)?;
        let equity = self
            .cash
            .checked_add(inventory_value)
            .ok_or(PortfolioError)?;
        let net = self
            .realized
            .checked_add(unrealized)
            .ok_or(PortfolioError)?;
        let expected_cash = self
            .initial_cash
            .checked_add(self.realized)
            .and_then(|v| v.checked_sub(open))
            .ok_or(PortfolioError)?;
        debug_assert_eq!(self.cash, expected_cash, "cash identity");
        debug_assert_eq!(
            equity,
            self.initial_cash.wrapping_add(net),
            "equity identity"
        );
        Ok(AccountSnapshot {
            cash: self.cash,
            realized_pnl: self.realized,
            unrealized_pnl: unrealized,
            equity,
            inventory_value,
            fees_paid: self.fees,
            net_pnl: net,
        })
    }

    /// One snapshot per market that has traded, in market-id order.
    pub fn snapshots(&self) -> Vec<PositionSnapshot> {
        self.positions
            .iter()
            .map(|(market, pos)| PositionSnapshot {
                market: market.clone(),
                net: pos.net,
                open_cashflow: pos.open_cashflow,
                avg_cost_ticks: if pos.net == 0 {
                    None
                } else {
                    pos.open_cashflow.checked_div(i128::from(pos.net))
                },
                max_abs_net: pos.max_abs_net,
            })
            .collect()
    }

    fn inventory_value(
        &self,
        books: &BTreeMap<MarketId, TopOfBook>,
        mark: MarkMethod,
    ) -> Result<i128, PortfolioError> {
        let yes = MarketId::yes();
        let no = MarketId::no();
        let yes_net = self.net(&yes);
        let no_net = self.net(&no);
        let pairs = overlap(yes_net, no_net);
        let mut value = 0i128;
        if pairs > 0 {
            let payout = i128::from(COMPLEMENT_PAYOUT)
                .checked_mul(i128::from(pairs))
                .ok_or(PortfolioError)?;
            let signed = if yes_net > 0 {
                payout
            } else {
                payout.checked_neg().ok_or(PortfolioError)?
            };
            value = signed;
        }
        value = value
            .checked_add(self.residual_value(&yes, yes_net, pairs, books, mark)?)
            .ok_or(PortfolioError)?;
        value = value
            .checked_add(self.residual_value(&no, no_net, pairs, books, mark)?)
            .ok_or(PortfolioError)?;
        for (market, pos) in &self.positions {
            if market == &yes || market == &no {
                continue;
            }
            value = value
                .checked_add(plain_value(pos, books.get(market), mark)?)
                .ok_or(PortfolioError)?;
        }
        Ok(value)
    }

    fn residual_value(
        &self,
        market: &MarketId,
        net: i64,
        pairs: u64,
        books: &BTreeMap<MarketId, TopOfBook>,
        mark: MarkMethod,
    ) -> Result<i128, PortfolioError> {
        if net == 0 {
            return Ok(0);
        }
        let abs = net.unsigned_abs();
        let residual = abs.saturating_sub(pairs);
        if residual == 0 {
            return Ok(0);
        }
        let long = net > 0;
        if let Some(px) = books
            .get(market)
            .and_then(|book| mark_price(book, long, mark))
        {
            return signed_mark(long, px, residual);
        }
        // No quote and no last trade: the residual contributes zero unrealized.
        self.range_cash(market, pairs, residual)
    }

    /// Cashflow of the FIFO share range `[skip, skip + qty)`.
    fn range_cash(&self, market: &MarketId, skip: u64, qty: u64) -> Result<i128, PortfolioError> {
        let Some(pos) = self.positions.get(market) else {
            return if qty == 0 { Ok(0) } else { Err(PortfolioError) };
        };
        let end = skip.checked_add(qty).ok_or(PortfolioError)?;
        let mut seen = 0u64;
        let mut cash = 0i128;
        for lot in &pos.lots {
            let lot_start = seen;
            let lot_end = seen.checked_add(lot.qty).ok_or(PortfolioError)?;
            seen = lot_end;
            let from = lot_start.max(skip);
            let to = lot_end.min(end);
            if to > from {
                let n = i128::from(to - from);
                let part = lot.cash_per_share.checked_mul(n).ok_or(PortfolioError)?;
                cash = cash.checked_add(part).ok_or(PortfolioError)?;
            }
        }
        if seen < end {
            return Err(PortfolioError);
        }
        Ok(cash)
    }

    fn score_pairs(&mut self) -> Result<(), PortfolioError> {
        let yes = MarketId::yes();
        let no = MarketId::no();
        let yes_net = self.net(&yes);
        let no_net = self.net(&no);
        let overlap = overlap(yes_net, no_net);
        if overlap <= self.paired_open {
            self.paired_open = overlap;
            return Ok(());
        }
        let mut left = overlap - self.paired_open;
        let mut skip = self.paired_open;
        let long = yes_net > 0;
        while left > 0 {
            let yes_slice = self.next_slice(&yes, skip)?;
            let no_slice = self.next_slice(&no, skip)?;
            let qty = left.min(yes_slice.0).min(no_slice.0);
            if qty == 0 {
                return Err(PortfolioError);
            }
            let edge = pair_edge(long, yes_slice.1, no_slice.1, qty)?;
            self.arb.record(qty, edge)?;
            skip = skip.checked_add(qty).ok_or(PortfolioError)?;
            left -= qty;
        }
        self.paired_open = overlap;
        Ok(())
    }

    /// `(qty, cash_per_share)` of the first lot that still has shares after `skip`.
    fn next_slice(&self, market: &MarketId, skip: u64) -> Result<(u64, i128), PortfolioError> {
        let pos = self.positions.get(market).ok_or(PortfolioError)?;
        let mut skipped = 0u64;
        for lot in &pos.lots {
            if skipped.saturating_add(lot.qty) <= skip {
                skipped = skipped.saturating_add(lot.qty);
                continue;
            }
            let into = skip.saturating_sub(skipped);
            return Ok((lot.qty - into, lot.cash_per_share));
        }
        Err(PortfolioError)
    }
}

fn plain_value(
    pos: &Position,
    book: Option<&TopOfBook>,
    mark: MarkMethod,
) -> Result<i128, PortfolioError> {
    if pos.net == 0 {
        return Ok(0);
    }
    let long = pos.net > 0;
    if let Some(px) = book.and_then(|book| mark_price(book, long, mark)) {
        return signed_mark(long, px, pos.net.unsigned_abs());
    }
    Ok(pos.open_cashflow)
}

fn signed_mark(long: bool, price: u64, qty: u64) -> Result<i128, PortfolioError> {
    let magnitude = i128::from(price)
        .checked_mul(i128::from(qty))
        .ok_or(PortfolioError)?;
    if long {
        Ok(magnitude)
    } else {
        magnitude.checked_neg().ok_or(PortfolioError)
    }
}

fn mark_price(book: &TopOfBook, long: bool, method: MarkMethod) -> Option<u64> {
    match method {
        MarkMethod::Mid => match (book.bid, book.ask) {
            (Some(bid), Some(ask)) => Some(bid.price.ticks().midpoint(ask.price.ticks())),
            (Some(bid), None) => Some(bid.price.ticks()),
            (None, Some(ask)) => Some(ask.price.ticks()),
            (None, None) => book.last.map(Price::ticks),
        },
        MarkMethod::Touch => {
            let primary = if long { book.bid } else { book.ask };
            let secondary = if long { book.ask } else { book.bid };
            primary
                .map(|quote| quote.price.ticks())
                .or_else(|| secondary.map(|quote| quote.price.ticks()))
                .or_else(|| book.last.map(Price::ticks))
        }
    }
}

fn cash_per_share(side: Side, price: u64, fee_per_share: u64) -> Result<i128, PortfolioError> {
    let price = i128::from(price);
    let fee = i128::from(fee_per_share);
    match side {
        Side::Buy => price.checked_add(fee).ok_or(PortfolioError),
        Side::Sell => price
            .checked_neg()
            .and_then(|v| v.checked_add(fee))
            .ok_or(PortfolioError),
    }
}

fn push_lot(lots: &mut VecDeque<Lot>, qty: u64, cash_per_share: i128) {
    if let Some(back) = lots.back_mut() {
        if back.cash_per_share == cash_per_share {
            if let Some(sum) = back.qty.checked_add(qty) {
                back.qty = sum;
                return;
            }
        }
    }
    lots.push_back(Lot {
        qty,
        cash_per_share,
    });
}

fn lots_match(pos: &Position) -> bool {
    let mut qty = 0u64;
    let mut cash = 0i128;
    for lot in &pos.lots {
        qty = qty.saturating_add(lot.qty);
        let Some(part) = lot.cash_per_share.checked_mul(i128::from(lot.qty)) else {
            return false;
        };
        let Some(sum) = cash.checked_add(part) else {
            return false;
        };
        cash = sum;
    }
    qty == pos.net.unsigned_abs() && cash == pos.open_cashflow
}

fn pair_edge(long: bool, yes_cps: i128, no_cps: i128, qty: u64) -> Result<i128, PortfolioError> {
    let qty = i128::from(qty);
    let payout = i128::from(COMPLEMENT_PAYOUT)
        .checked_mul(qty)
        .ok_or(PortfolioError)?;
    let yes_cost = yes_cps.checked_mul(qty).ok_or(PortfolioError)?;
    let no_cost = no_cps.checked_mul(qty).ok_or(PortfolioError)?;
    if long {
        payout
            .checked_sub(yes_cost)
            .and_then(|v| v.checked_sub(no_cost))
            .ok_or(PortfolioError)
    } else {
        // Short credits are negative cash per share, so negating them is the
        // cash received. Edge is that credit minus the 100-tick settlement.
        yes_cost
            .checked_neg()
            .and_then(|v| v.checked_sub(no_cost))
            .and_then(|v| v.checked_sub(payout))
            .ok_or(PortfolioError)
    }
}

/// Overlapping absolute inventory when both legs have the same sign.
pub fn overlap(yes: i64, no: i64) -> u64 {
    if (yes > 0 && no > 0) || (yes < 0 && no < 0) {
        yes.unsigned_abs().min(no.unsigned_abs())
    } else {
        0
    }
}

/// Absolute difference of two signed positions, saturating at `u64::MAX`.
pub fn abs_diff_pos(left: i64, right: i64) -> u64 {
    left.checked_sub(right)
        .map(|delta| delta.unsigned_abs())
        .unwrap_or(u64::MAX)
}
