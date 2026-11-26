//! Deterministic backtester.
//!
//! [`run`] replays an event stream through a [`MatchingEngine`] and, after
//! each event, applies the actions returned by a [`Strategy`]. Strategy orders
//! are tagged with ids from [`StrategyIds`], so they never reuse an id from
//! the stream. Fills update the integer-tick [`Portfolio`]. The same stream
//! and the same strategy produce the same [`BacktestReport`], including the
//! FNV-1a digest.
//!
//! [`verify`] runs the stream through [`FastEngine`] and [`ReferenceEngine`]
//! and requires the reports to be equal.
//!
//! ```
//! use mini_exchange::backtest::{run, BacktestConfig};
//! use mini_exchange::sim::{generate, Preset, SimConfig};
//! use mini_exchange::strategy::ComplementArbStrategy;
//! use mini_exchange::FastEngine;
//!
//! let events: Vec<_> = generate(SimConfig::new(Preset::PredictionMarket, 1, 30)).collect();
//! let report = run(
//!     events,
//!     |_| FastEngine::new(),
//!     ComplementArbStrategy::default(),
//!     BacktestConfig::default(),
//! )
//! .unwrap();
//! assert_eq!(report.events, 30);
//! ```

use std::borrow::Borrow;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::io::BufRead;

use crate::engine::MatchingEngine;
use crate::event_log::{Event, EventReader, MarketId, ReadError};
use crate::fast::FastEngine;
use crate::portfolio::{
    rate_bps, AccountSnapshot, DrawdownTracker, MarkMethod, Portfolio, PortfolioError,
    PositionSnapshot, TopOfBook,
};
use crate::reference::ReferenceEngine;
use crate::strategy::{Action, ActionUpdate, LiveOrder, MarketUpdate, Strategy, StrategyIds};
use crate::types::{Command, CommandResult, OrderId, Price, Qty, Quote, Side, Timestamp, Trade};

const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x100_0000_01B3;

/// Fees, starting cash, and the mark used for unrealized PnL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BacktestConfig {
    /// Cash before any fill, in ticks.
    pub initial_cash: i128,
    /// Fee charged on each share when the strategy is the taker.
    pub taker_fee: u64,
    /// Fee charged on each share when the strategy is the maker.
    pub maker_fee: u64,
    /// How open inventory is marked.
    pub mark: MarkMethod,
    /// Most actions applied in response to one market event, including
    /// follow-ups from [`Strategy::on_action`].
    pub max_actions_per_event: u32,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            initial_cash: 1_000_000,
            taker_fee: 0,
            maker_fee: 0,
            mark: MarkMethod::Mid,
            max_actions_per_event: 64,
        }
    }
}

impl BacktestConfig {
    /// `initial_cash` ticks and zero fees, marked at the mid.
    pub const fn new(initial_cash: i128) -> Self {
        Self {
            initial_cash,
            taker_fee: 0,
            maker_fee: 0,
            mark: MarkMethod::Mid,
            max_actions_per_event: 64,
        }
    }

    /// Set both the taker and the maker fee, in ticks per share.
    pub const fn with_fee(mut self, fee_per_share: u64) -> Self {
        self.taker_fee = fee_per_share;
        self.maker_fee = fee_per_share;
        self
    }
}

/// Top of one market at the end of the backtest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopSnapshot {
    /// Market symbol.
    pub market: MarketId,
    /// Best bid.
    pub bid: Option<Quote>,
    /// Best ask.
    pub ask: Option<Quote>,
    /// Last trade price.
    pub last: Option<Price>,
}

/// Outcome of one deterministic backtest.
///
/// PnL fields are ticks. `digest` is FNV-1a over the fee schedule, every
/// strategy fill, the equity observation after each event and action, and the
/// final portfolio and books. Two runs match when their reports are equal.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "discarding a backtest report drops the digest and the PnL"]
pub struct BacktestReport {
    /// Market events applied.
    pub events: u64,
    /// Actions returned by the strategy, including follow-ups.
    pub strategy_actions: u64,
    /// Limit and market orders submitted by the strategy.
    pub strategy_orders: u64,
    /// Cancel actions submitted by the strategy.
    pub strategy_cancels: u64,
    /// Strategy commands the engine rejected.
    pub strategy_rejects: u64,
    /// Fills where the strategy was the taker or the maker.
    pub strategy_fills: u64,
    /// Shares on those fills.
    pub strategy_volume: u128,
    /// Fees paid, in ticks.
    pub fees_paid: i128,
    /// PnL locked in by closing lots, in ticks.
    pub realized_pnl: i128,
    /// Mark of open inventory minus its cost, in ticks.
    pub unrealized_pnl: i128,
    /// `realized_pnl + unrealized_pnl`.
    pub net_pnl: i128,
    /// Cash the backtest started with.
    pub initial_cash: i128,
    /// Cash after fills and fees.
    pub final_cash: i128,
    /// Settlement value of YES/NO pairs plus the mark of every other share.
    pub inventory_value: i128,
    /// `final_cash + inventory_value`.
    pub final_equity: i128,
    /// Highest equity observed.
    pub peak_equity: i128,
    /// Worst peak-to-trough drop, in ticks.
    pub max_drawdown_ticks: i128,
    /// Basis points of that drop against the peak then in force. 10_000 is 100%.
    pub max_drawdown_bps: u64,
    /// YES/NO shares scored as pairs.
    pub paired_qty: u64,
    /// Paired shares with a positive edge.
    pub pair_wins: u64,
    /// Paired shares with a negative edge.
    pub pair_losses: u64,
    /// Paired shares with a zero edge.
    pub pair_scratches: u64,
    /// Sum of pair edge, in ticks.
    pub pair_edge_sum: i128,
    /// Average edge per paired share, rounded toward zero.
    pub avg_edge_ticks: Option<i128>,
    /// Pair win rate when any pairs were scored, otherwise the win rate of
    /// closing fills. Basis points, 10_000 is 100%.
    pub win_rate_bps: Option<u64>,
    /// Shares that closed an existing lot.
    pub closed_qty: u64,
    /// Closed shares whose realized PnL was positive.
    pub close_wins: u64,
    /// Closed shares whose realized PnL was negative.
    pub close_losses: u64,
    /// Closed shares whose realized PnL was zero.
    pub close_scratches: u64,
    /// Largest `|yes_position - no_position|` after a strategy fill.
    pub max_leg_imbalance: u64,
    /// Taker fee used for this run, in ticks per share.
    pub taker_fee: u64,
    /// Maker fee used for this run, in ticks per share.
    pub maker_fee: u64,
    /// Mark used for unrealized PnL.
    pub mark: MarkMethod,
    /// One row per market the strategy traded, sorted by market id.
    pub positions: Vec<PositionSnapshot>,
    /// Final top of book for every market in the stream, sorted by market id.
    pub books: Vec<TopSnapshot>,
    /// FNV-1a fingerprint of the run.
    pub digest: u64,
}

/// Both engines produced the same backtest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedBacktest {
    /// [`FastEngine`] report.
    pub fast: BacktestReport,
    /// [`ReferenceEngine`] report.
    pub reference: BacktestReport,
}

/// A backtest could not finish, or the two engines disagreed.
#[derive(Debug)]
pub enum BacktestError {
    /// Tick arithmetic overflowed.
    Portfolio(PortfolioError),
    /// A market event used an id reserved for the strategy.
    OrderIdCollision {
        /// Logical sequence of the event.
        seq: u64,
        /// Market on the event.
        market: MarketId,
        /// Id that landed in the strategy namespace.
        id: OrderId,
    },
    /// The strategy asked to cancel an id outside its namespace.
    ForeignCancel {
        /// Logical sequence of the event that produced the cancel.
        seq: u64,
        /// Id that was not allocated by [`StrategyIds`].
        id: OrderId,
    },
    /// One market event produced more actions than the config allows.
    ActionLimit {
        /// Logical sequence of the event.
        seq: u64,
        /// `max_actions_per_event` from the config.
        limit: u32,
    },
    /// An internal bookkeeping check failed.
    Accounting(String),
    /// The event log could not be read.
    Read(ReadError),
    /// Fast and reference reports differ.
    Mismatch {
        /// Which fields disagreed.
        detail: String,
    },
}

impl fmt::Display for BacktestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Portfolio(err) => write!(f, "{err}"),
            Self::OrderIdCollision { seq, market, id } => write!(
                f,
                "event {seq} on {market} uses order id {id}, which is reserved for strategy orders (ids >= {})",
                StrategyIds::BASE
            ),
            Self::ForeignCancel { seq, id } => write!(
                f,
                "strategy cancel at sequence {seq} targets order id {id}, which is outside the strategy namespace"
            ),
            Self::ActionLimit { seq, limit } => write!(
                f,
                "strategy produced more than {limit} actions for event {seq}"
            ),
            Self::Accounting(detail) => write!(f, "backtest accounting failed: {detail}"),
            Self::Read(err) => write!(f, "{err}"),
            Self::Mismatch { detail } => write!(f, "backtest verify failed: {detail}"),
        }
    }
}

impl std::error::Error for BacktestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Portfolio(err) => Some(err),
            Self::Read(err) => Some(err),
            _ => None,
        }
    }
}

impl From<PortfolioError> for BacktestError {
    fn from(err: PortfolioError) -> Self {
        Self::Portfolio(err)
    }
}

impl From<ReadError> for BacktestError {
    fn from(err: ReadError) -> Self {
        Self::Read(err)
    }
}

/// Replay `events` through engines from `factory` while `strategy` trades.
///
/// `events` may be a `Vec<Event>`, a `&[Event]`, or any iterator of [`Event`]
/// or `&Event`.
pub fn run<E, S, I, F>(
    events: I,
    factory: F,
    strategy: S,
    config: BacktestConfig,
) -> Result<BacktestReport, BacktestError>
where
    I: IntoIterator,
    I::Item: Borrow<Event>,
    E: MatchingEngine,
    S: Strategy,
    F: FnMut(&MarketId) -> E,
{
    let mut session = Session::new(factory, strategy, config);
    for event in events {
        session.apply_event(event.borrow())?;
    }
    session.finish()
}

/// Stream a log through `strategy`.
pub fn run_reader<R, E, S, F>(
    reader: EventReader<R>,
    factory: F,
    strategy: S,
    config: BacktestConfig,
) -> Result<BacktestReport, BacktestError>
where
    R: BufRead,
    E: MatchingEngine,
    S: Strategy,
    F: FnMut(&MarketId) -> E,
{
    let mut session = Session::new(factory, strategy, config);
    for item in reader {
        session.apply_event(&item?)?;
    }
    session.finish()
}

/// Replay `events` through [`FastEngine`] and [`ReferenceEngine`].
///
/// `make_strategy` is called twice, once per engine. It must build the same
/// deterministic strategy each time.
pub fn verify<S, I, Fs>(
    events: I,
    make_strategy: Fs,
    config: BacktestConfig,
) -> Result<VerifiedBacktest, BacktestError>
where
    I: IntoIterator + Clone,
    I::Item: Borrow<Event>,
    S: Strategy,
    Fs: Fn() -> S,
{
    let fast = run(events.clone(), new_fast, make_strategy(), config)?;
    let reference = run(events, new_reference, make_strategy(), config)?;
    if fast != reference {
        return Err(BacktestError::Mismatch {
            detail: explain(&fast, &reference),
        });
    }
    Ok(VerifiedBacktest { fast, reference })
}

fn new_fast(_: &MarketId) -> FastEngine {
    FastEngine::new()
}

fn new_reference(_: &MarketId) -> ReferenceEngine {
    ReferenceEngine::new()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Taker,
    Maker,
}

struct Stats {
    events: u64,
    actions: u64,
    orders: u64,
    cancels: u64,
    rejects: u64,
    fills: u64,
    volume: u128,
    closed_qty: u64,
    close_wins: u64,
    close_losses: u64,
    close_scratches: u64,
    max_imbalance: u64,
}

impl Stats {
    fn new() -> Self {
        Self {
            events: 0,
            actions: 0,
            orders: 0,
            cancels: 0,
            rejects: 0,
            fills: 0,
            volume: 0,
            closed_qty: 0,
            close_wins: 0,
            close_losses: 0,
            close_scratches: 0,
            max_imbalance: 0,
        }
    }
}

struct Session<E, S, F> {
    factory: F,
    engines: BTreeMap<MarketId, E>,
    strategy: S,
    portfolio: Portfolio,
    books: BTreeMap<MarketId, TopOfBook>,
    live: Vec<LiveOrder>,
    ids: StrategyIds,
    config: BacktestConfig,
    drawdown: DrawdownTracker,
    digest: Digest,
    stats: Stats,
}

impl<E, S, F> Session<E, S, F>
where
    E: MatchingEngine,
    S: Strategy,
    F: FnMut(&MarketId) -> E,
{
    fn new(factory: F, strategy: S, config: BacktestConfig) -> Self {
        let mut digest = Digest::new();
        digest.mix(1);
        digest.mix_i128(config.initial_cash);
        digest.mix(config.taker_fee);
        digest.mix(config.maker_fee);
        digest.mix(mark_code(config.mark));
        digest.mix_equity(0, config.initial_cash);
        Self {
            factory,
            engines: BTreeMap::new(),
            strategy,
            portfolio: Portfolio::new(config.initial_cash),
            books: BTreeMap::new(),
            live: Vec::new(),
            ids: StrategyIds::new(),
            drawdown: DrawdownTracker::new(config.initial_cash),
            config,
            digest,
            stats: Stats::new(),
        }
    }

    fn apply_event(&mut self, event: &Event) -> Result<(), BacktestError> {
        if StrategyIds::contains(event.command.order_id()) {
            return Err(BacktestError::OrderIdCollision {
                seq: event.seq,
                market: event.market.clone(),
                id: event.command.order_id(),
            });
        }
        let result = self.engine_mut(&event.market).execute(event.command);
        self.account_result(&event.market, &result, false, event.seq)?;
        self.refresh(&event.market, last_price(&result));
        self.observe(event.seq)?;
        let actions = self.pull_actions(event.seq, &event.market, &result)?;
        self.drain(event.seq, actions)?;
        self.note_imbalance();
        self.stats.events += 1;
        Ok(())
    }

    fn finish(mut self) -> Result<BacktestReport, BacktestError> {
        let account = self.account()?;
        self.check_identity(&account)?;
        let positions = self.portfolio.snapshots();
        let books = self.book_snapshots();
        let arb = self.portfolio.arb_stats();
        self.digest
            .mix_summary(&account, &self.drawdown, arb, &self.stats);
        self.digest.mix_positions(&positions);
        self.digest.mix_books(&books);
        let digest = self.digest.finish();
        let win_rate = if arb.qty > 0 {
            arb.win_rate_bps()
        } else {
            rate_bps(
                self.stats.close_wins,
                self.stats.close_losses,
                self.stats.close_scratches,
            )
        };
        Ok(BacktestReport {
            events: self.stats.events,
            strategy_actions: self.stats.actions,
            strategy_orders: self.stats.orders,
            strategy_cancels: self.stats.cancels,
            strategy_rejects: self.stats.rejects,
            strategy_fills: self.stats.fills,
            strategy_volume: self.stats.volume,
            fees_paid: account.fees_paid,
            realized_pnl: account.realized_pnl,
            unrealized_pnl: account.unrealized_pnl,
            net_pnl: account.net_pnl,
            initial_cash: self.config.initial_cash,
            final_cash: account.cash,
            inventory_value: account.inventory_value,
            final_equity: account.equity,
            peak_equity: self.drawdown.peak(),
            max_drawdown_ticks: self.drawdown.max_drawdown_ticks(),
            max_drawdown_bps: self.drawdown.max_drawdown_bps(),
            paired_qty: arb.qty,
            pair_wins: arb.wins,
            pair_losses: arb.losses,
            pair_scratches: arb.scratches,
            pair_edge_sum: arb.edge_sum,
            avg_edge_ticks: arb.avg_edge_ticks(),
            win_rate_bps: win_rate,
            closed_qty: self.stats.closed_qty,
            close_wins: self.stats.close_wins,
            close_losses: self.stats.close_losses,
            close_scratches: self.stats.close_scratches,
            max_leg_imbalance: self.stats.max_imbalance,
            taker_fee: self.config.taker_fee,
            maker_fee: self.config.maker_fee,
            mark: self.config.mark,
            positions,
            books,
            digest,
        })
    }

    fn drain(&mut self, seq: u64, actions: Vec<Action>) -> Result<(), BacktestError> {
        let mut queue: VecDeque<Action> = actions.into();
        let mut budget = self.config.max_actions_per_event;
        while let Some(action) = queue.pop_front() {
            if budget == 0 {
                return Err(BacktestError::ActionLimit {
                    seq,
                    limit: self.config.max_actions_per_event,
                });
            }
            budget -= 1;
            self.stats.actions += 1;
            let result = self.apply_action(seq, &action)?;
            self.observe(seq)?;
            let more = self.pull_followups(seq, &action, &result)?;
            queue.extend(more);
        }
        Ok(())
    }

    fn apply_action(&mut self, seq: u64, action: &Action) -> Result<CommandResult, BacktestError> {
        match *action {
            Action::Limit {
                ref market,
                side,
                price,
                qty,
                ioc,
            } => self.submit_limit(seq, market, side, price, qty, ioc),
            Action::Market {
                ref market,
                side,
                qty,
            } => self.submit_market(seq, market, side, qty),
            Action::Cancel { ref market, id } => self.submit_cancel(seq, market, id),
        }
    }

    fn submit_limit(
        &mut self,
        seq: u64,
        market: &MarketId,
        side: Side,
        price: Price,
        qty: Qty,
        ioc: bool,
    ) -> Result<CommandResult, BacktestError> {
        self.stats.orders += 1;
        let id = self.ids.alloc();
        let command = Command::limit(id, side, price, qty, Timestamp(seq));
        let result = self.engine_mut(market).execute(command);
        self.note_reject(&result);
        self.account_result(market, &result, true, seq)?;
        let reported = if let CommandResult::Accepted {
            resting_qty,
            order_id,
            filled_qty,
            canceled_qty,
            ref trades,
        } = result
        {
            if resting_qty.is_zero() {
                result_accepted(
                    order_id,
                    trades.clone(),
                    filled_qty,
                    Qty::ZERO,
                    canceled_qty,
                )
            } else if ioc {
                self.cancel_remainder(market, order_id)?;
                let canceled = canceled_qty
                    .get()
                    .checked_add(resting_qty.get())
                    .ok_or(PortfolioError)?;
                result_accepted(
                    order_id,
                    trades.clone(),
                    filled_qty,
                    Qty::ZERO,
                    Qty(canceled),
                )
            } else {
                self.live.push(LiveOrder {
                    id: order_id,
                    market: market.clone(),
                    side,
                    price,
                    remaining: resting_qty.get(),
                });
                result_accepted(
                    order_id,
                    trades.clone(),
                    filled_qty,
                    resting_qty,
                    canceled_qty,
                )
            }
        } else {
            result
        };
        self.refresh(market, last_price(&reported));
        Ok(reported)
    }

    fn submit_market(
        &mut self,
        seq: u64,
        market: &MarketId,
        side: Side,
        qty: Qty,
    ) -> Result<CommandResult, BacktestError> {
        self.stats.orders += 1;
        let id = self.ids.alloc();
        let command = Command::market(id, side, qty, Timestamp(seq));
        let result = self.engine_mut(market).execute(command);
        self.note_reject(&result);
        self.account_result(market, &result, true, seq)?;
        self.refresh(market, last_price(&result));
        Ok(result)
    }

    fn submit_cancel(
        &mut self,
        seq: u64,
        market: &MarketId,
        id: OrderId,
    ) -> Result<CommandResult, BacktestError> {
        if !StrategyIds::contains(id) {
            return Err(BacktestError::ForeignCancel { seq, id });
        }
        self.stats.cancels += 1;
        let result = self.engine_mut(market).execute(Command::cancel(id));
        self.note_reject(&result);
        if matches!(result, CommandResult::Canceled { .. }) {
            self.live.retain(|order| order.id != id);
        }
        self.refresh(market, None);
        Ok(result)
    }

    fn cancel_remainder(&mut self, market: &MarketId, id: OrderId) -> Result<(), BacktestError> {
        let result = self.engine_mut(market).execute(Command::cancel(id));
        match result {
            CommandResult::Canceled { .. } => Ok(()),
            other => Err(BacktestError::Accounting(format!(
                "immediate-or-cancel remainder for {id} did not cancel: {other:?}"
            ))),
        }
    }

    fn account_result(
        &mut self,
        market: &MarketId,
        result: &CommandResult,
        taker_ours: bool,
        seq: u64,
    ) -> Result<(), BacktestError> {
        let CommandResult::Accepted { trades, .. } = result else {
            return Ok(());
        };
        for trade in trades {
            let maker_ours = StrategyIds::contains(trade.maker_id);
            let taker_is_ours = taker_ours || StrategyIds::contains(trade.taker_id);
            if taker_is_ours {
                self.apply_fill(
                    market,
                    trade.taker_side,
                    trade,
                    self.config.taker_fee,
                    Role::Taker,
                    seq,
                )?;
            }
            if maker_ours {
                self.apply_fill(
                    market,
                    trade.taker_side.opposite(),
                    trade,
                    self.config.maker_fee,
                    Role::Maker,
                    seq,
                )?;
                self.reduce_live(trade.maker_id, trade.qty.get());
            }
        }
        Ok(())
    }

    fn apply_fill(
        &mut self,
        market: &MarketId,
        side: Side,
        trade: &Trade,
        fee: u64,
        role: Role,
        seq: u64,
    ) -> Result<(), BacktestError> {
        let effect = self
            .portfolio
            .apply_fill(market, side, trade.price, trade.qty, fee)?;
        self.stats.fills = self.stats.fills.checked_add(1).ok_or(PortfolioError)?;
        self.stats.volume = self
            .stats
            .volume
            .checked_add(u128::from(trade.qty.get()))
            .ok_or(PortfolioError)?;
        if effect.closed_qty > 0 {
            self.stats.closed_qty = self
                .stats
                .closed_qty
                .checked_add(effect.closed_qty)
                .ok_or(PortfolioError)?;
            let bucket = match effect.realized_pnl.cmp(&0) {
                std::cmp::Ordering::Greater => &mut self.stats.close_wins,
                std::cmp::Ordering::Less => &mut self.stats.close_losses,
                std::cmp::Ordering::Equal => &mut self.stats.close_scratches,
            };
            *bucket = bucket
                .checked_add(effect.closed_qty)
                .ok_or(PortfolioError)?;
        }
        self.digest.mix_fill(seq, market, trade, fee, role);
        Ok(())
    }

    fn reduce_live(&mut self, id: OrderId, qty: u64) {
        if let Some(order) = self.live.iter_mut().find(|order| order.id == id) {
            order.remaining = order
                .remaining
                .checked_sub(qty)
                .expect("fill exceeds the resting strategy quantity");
        }
        self.live.retain(|order| order.remaining > 0);
    }

    fn note_imbalance(&mut self) {
        let gap = crate::portfolio::abs_diff_pos(
            self.portfolio.net(&MarketId::yes()),
            self.portfolio.net(&MarketId::no()),
        );
        if gap > self.stats.max_imbalance {
            self.stats.max_imbalance = gap;
        }
    }

    fn note_reject(&mut self, result: &CommandResult) {
        if matches!(
            result,
            CommandResult::Rejected { .. } | CommandResult::CancelRejected { .. }
        ) {
            self.stats.rejects += 1;
        }
    }

    fn observe(&mut self, seq: u64) -> Result<(), BacktestError> {
        let account = self.account()?;
        self.drawdown.observe(account.equity);
        self.digest.mix_equity(seq, account.equity);
        self.check_identity(&account)
    }

    fn account(&self) -> Result<AccountSnapshot, BacktestError> {
        Ok(self.portfolio.account(&self.books, self.config.mark)?)
    }

    fn check_identity(&self, account: &AccountSnapshot) -> Result<(), BacktestError> {
        let open = self.portfolio.total_open_cashflow()?;
        let expected_cash = self
            .config
            .initial_cash
            .checked_add(account.realized_pnl)
            .and_then(|value| value.checked_sub(open))
            .ok_or(PortfolioError)?;
        if account.cash != expected_cash {
            return Err(BacktestError::Accounting(format!(
                "cash {} does not match initial + realized - open cost {expected_cash}",
                account.cash
            )));
        }
        let net = account
            .realized_pnl
            .checked_add(account.unrealized_pnl)
            .ok_or(PortfolioError)?;
        if account.net_pnl != net {
            return Err(BacktestError::Accounting(
                "net pnl is not realized plus unrealized".to_string(),
            ));
        }
        let expected_equity = self
            .config
            .initial_cash
            .checked_add(net)
            .ok_or(PortfolioError)?;
        if account.equity != expected_equity {
            return Err(BacktestError::Accounting(format!(
                "equity {} does not match initial cash + net pnl {expected_equity}",
                account.equity
            )));
        }
        let cash_plus_inventory = account
            .cash
            .checked_add(account.inventory_value)
            .ok_or(PortfolioError)?;
        if cash_plus_inventory != account.equity {
            return Err(BacktestError::Accounting(
                "equity is not cash plus inventory value".to_string(),
            ));
        }
        Ok(())
    }

    fn pull_actions(
        &mut self,
        seq: u64,
        market: &MarketId,
        result: &CommandResult,
    ) -> Result<Vec<Action>, BacktestError> {
        let account = self.account()?;
        let update = MarketUpdate {
            seq,
            market,
            result,
            books: &self.books,
            portfolio: &self.portfolio,
            account,
            live_orders: &self.live,
            taker_fee: self.config.taker_fee,
            maker_fee: self.config.maker_fee,
            mark: self.config.mark,
        };
        Ok(self.strategy.on_market(&update))
    }

    fn pull_followups(
        &mut self,
        seq: u64,
        action: &Action,
        result: &CommandResult,
    ) -> Result<Vec<Action>, BacktestError> {
        let account = self.account()?;
        let update = ActionUpdate {
            seq,
            action,
            result,
            books: &self.books,
            portfolio: &self.portfolio,
            account,
            live_orders: &self.live,
            taker_fee: self.config.taker_fee,
            maker_fee: self.config.maker_fee,
            mark: self.config.mark,
        };
        Ok(self.strategy.on_action(&update))
    }

    fn refresh(&mut self, market: &MarketId, last: Option<Price>) {
        let (bid, ask) = {
            let engine = self
                .engines
                .get(market)
                .expect("market engine exists after execute");
            (engine.best_bid(), engine.best_ask())
        };
        let book = self.books.entry(market.clone()).or_default();
        book.bid = bid;
        book.ask = ask;
        if let Some(price) = last {
            book.last = Some(price);
        }
    }

    fn engine_mut(&mut self, market: &MarketId) -> &mut E {
        if !self.engines.contains_key(market) {
            let engine = (self.factory)(market);
            self.engines.insert(market.clone(), engine);
        }
        self.engines
            .get_mut(market)
            .expect("market engine exists after insert")
    }

    fn book_snapshots(&self) -> Vec<TopSnapshot> {
        self.books
            .iter()
            .map(|(market, book)| TopSnapshot {
                market: market.clone(),
                bid: book.bid,
                ask: book.ask,
                last: book.last,
            })
            .collect()
    }
}

fn result_accepted(
    order_id: OrderId,
    trades: Vec<Trade>,
    filled_qty: Qty,
    resting_qty: Qty,
    canceled_qty: Qty,
) -> CommandResult {
    CommandResult::Accepted {
        order_id,
        trades,
        filled_qty,
        resting_qty,
        canceled_qty,
    }
}

fn last_price(result: &CommandResult) -> Option<Price> {
    match result {
        CommandResult::Accepted { trades, .. } => trades.last().map(|trade| trade.price),
        _ => None,
    }
}

fn mark_code(mark: MarkMethod) -> u64 {
    match mark {
        MarkMethod::Mid => 1,
        MarkMethod::Touch => 2,
    }
}

fn explain(fast: &BacktestReport, reference: &BacktestReport) -> String {
    let mut parts = Vec::new();
    push_diff(&mut parts, "digest", fast.digest, reference.digest);
    push_diff(&mut parts, "events", fast.events, reference.events);
    push_diff(
        &mut parts,
        "strategy_fills",
        fast.strategy_fills,
        reference.strategy_fills,
    );
    push_diff(
        &mut parts,
        "strategy_volume",
        fast.strategy_volume,
        reference.strategy_volume,
    );
    push_diff(&mut parts, "net_pnl", fast.net_pnl, reference.net_pnl);
    push_diff(
        &mut parts,
        "realized_pnl",
        fast.realized_pnl,
        reference.realized_pnl,
    );
    push_diff(
        &mut parts,
        "unrealized_pnl",
        fast.unrealized_pnl,
        reference.unrealized_pnl,
    );
    push_diff(
        &mut parts,
        "final_equity",
        fast.final_equity,
        reference.final_equity,
    );
    push_diff(
        &mut parts,
        "peak_equity",
        fast.peak_equity,
        reference.peak_equity,
    );
    push_diff(
        &mut parts,
        "max_drawdown_ticks",
        fast.max_drawdown_ticks,
        reference.max_drawdown_ticks,
    );
    push_diff(
        &mut parts,
        "paired_qty",
        fast.paired_qty,
        reference.paired_qty,
    );
    if fast.positions != reference.positions {
        parts.push("positions differ".to_string());
    }
    if fast.books != reference.books {
        parts.push("final books differ".to_string());
    }
    if parts.is_empty() {
        parts.push("reports are not equal".to_string());
    }
    parts.join("; ")
}

fn push_diff<T: fmt::Debug + PartialEq>(parts: &mut Vec<String>, name: &str, left: T, right: T) {
    if left != right {
        parts.push(format!("{name} {left:?} != {right:?}"));
    }
}

struct Digest {
    hash: u64,
}

impl Digest {
    fn new() -> Self {
        Self { hash: FNV_OFFSET }
    }

    fn mix(&mut self, value: u64) {
        self.hash = (self.hash ^ value).wrapping_mul(FNV_PRIME);
    }

    fn mix_i128(&mut self, value: i128) {
        let bytes = value.to_le_bytes();
        self.mix(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]));
        self.mix(u64::from_le_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ]));
    }

    fn mix_str(&mut self, text: &str) {
        self.mix(u64::try_from(text.len()).unwrap_or(u64::MAX));
        for byte in text.bytes() {
            self.mix(u64::from(byte));
        }
    }

    fn mix_equity(&mut self, seq: u64, equity: i128) {
        self.mix(3);
        self.mix(seq);
        self.mix_i128(equity);
    }

    fn mix_fill(&mut self, seq: u64, market: &MarketId, trade: &Trade, fee: u64, role: Role) {
        self.mix(2);
        self.mix(seq);
        self.mix_str(market.as_str());
        self.mix(trade.maker_id.get());
        self.mix(trade.taker_id.get());
        self.mix(trade.price.ticks());
        self.mix(trade.qty.get());
        self.mix(side_code(trade.taker_side));
        self.mix(trade.ts.get());
        self.mix(fee);
        self.mix(match role {
            Role::Taker => 1,
            Role::Maker => 2,
        });
    }

    fn mix_summary(
        &mut self,
        account: &AccountSnapshot,
        drawdown: &DrawdownTracker,
        arb: crate::portfolio::ArbStats,
        stats: &Stats,
    ) {
        self.mix(4);
        self.mix_i128(account.cash);
        self.mix_i128(account.realized_pnl);
        self.mix_i128(account.unrealized_pnl);
        self.mix_i128(account.equity);
        self.mix_i128(account.inventory_value);
        self.mix_i128(account.fees_paid);
        self.mix_i128(drawdown.peak());
        self.mix_i128(drawdown.max_drawdown_ticks());
        self.mix(drawdown.max_drawdown_bps());
        self.mix(arb.qty);
        self.mix(arb.wins);
        self.mix(arb.losses);
        self.mix(arb.scratches);
        self.mix_i128(arb.edge_sum);
        self.mix(stats.fills);
        self.mix(u64::try_from(stats.volume).unwrap_or(u64::MAX));
        self.mix(stats.closed_qty);
        self.mix(stats.max_imbalance);
    }

    fn mix_positions(&mut self, positions: &[PositionSnapshot]) {
        self.mix(5);
        self.mix(u64::try_from(positions.len()).unwrap_or(u64::MAX));
        for pos in positions {
            self.mix_str(pos.market.as_str());
            self.mix_i128(i128::from(pos.net));
            self.mix_i128(pos.open_cashflow);
            self.mix(pos.max_abs_net);
        }
    }

    fn mix_books(&mut self, books: &[TopSnapshot]) {
        self.mix(6);
        self.mix(u64::try_from(books.len()).unwrap_or(u64::MAX));
        for book in books {
            self.mix_str(book.market.as_str());
            self.mix_quote(book.bid);
            self.mix_quote(book.ask);
            self.mix(book.last.map(Price::ticks).unwrap_or(0));
        }
    }

    fn mix_quote(&mut self, quote: Option<Quote>) {
        match quote {
            Some(quote) => {
                self.mix(1);
                self.mix(quote.price.ticks());
                self.mix(quote.qty.get());
            }
            None => self.mix(0),
        }
    }

    fn finish(mut self) -> u64 {
        self.mix(u64::MAX);
        self.hash
    }
}

fn side_code(side: Side) -> u64 {
    match side {
        Side::Buy => 1,
        Side::Sell => 2,
    }
}
