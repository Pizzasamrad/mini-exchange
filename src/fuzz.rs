//! Differential checks between [`ReferenceEngine`](crate::ReferenceEngine) and
//! [`FastEngine`](crate::FastEngine).
//!
//! [`Workload`] is a pure function of a [`Profile`] and a seed. It never reads
//! the operating system's random source. [`run`] feeds that stream to both
//! engines. Every command must return the same [`CommandResult`]. On a fixed
//! cadence, and again after the last command, the books must compare equal and
//! satisfy the quantity and ordering invariants. A second pass replays the
//! seed and requires the same final book and the same trade-stream digest.
//!
//! ```
//! use mini_exchange::fuzz::{run, FuzzConfig, Profile};
//!
//! let report = run(&FuzzConfig::new(Profile::Balanced, 1, 200)).unwrap();
//! assert_eq!(report.orders, 200);
//! ```

use std::any::Any;
use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::str::FromStr;

use crate::engine::MatchingEngine;
use crate::fast::FastEngine;
use crate::reference::ReferenceEngine;
use crate::types::{
    Book, CancelRejectReason, Command, CommandResult, Level, OrderId, Price, Qty, Quote,
    RejectReason, Side, Timestamp, Trade,
};

/// Moving-mid band for every profile except the adversarial price probes.
///
/// Prices `0..=3` are reserved. Adversarial cases park a zero bid, a one-tick
/// bid, and a single `u64::MAX` quantity at price 3. Keeping the mid above
/// this band means a normal order never joins those levels, so the huge order
/// is always alone and a level total cannot overflow.
const MID_MIN: u64 = 5_000;
const MID_MAX: u64 = 80_000;
const INITIAL_MID: u64 = 10_000;
/// Generator-side cap on ids that might still be resting. The real book stays
/// at or below this, which keeps the reference scan cheap for multi-million
/// command runs.
const MAX_LIVE: usize = 160;
/// Commands per sub-profile when [`Profile::Mixed`] is selected.
const PHASE_LEN: u64 = 500;

/// Kind of deterministic order flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Profile {
    /// Limit, market, and cancel mix around a mid that takes small steps.
    Balanced,
    /// A few prices, long FIFO queues, and cancels at the head, middle, and tail.
    TightSpread,
    /// Many new levels, partial multi-level sweeps, then empty-book markets.
    WideSweep,
    /// Duplicate ids, zero quantities, stale cancels, and price extremes.
    Adversarial,
    /// Rotates through the four profiles so one seed exercises all of them.
    Mixed,
}

/// Rejected profile name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseProfileError;

impl Profile {
    /// Stable name used by the CLI and by diagnostics.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Balanced => "balanced",
            Self::TightSpread => "tight-spread",
            Self::WideSweep => "wide-sweep",
            Self::Adversarial => "adversarial",
            Self::Mixed => "mixed",
        }
    }

    fn active(self, generated: u64) -> Self {
        match self {
            Self::Mixed => {
                const PHASES: [Profile; 4] = [
                    Profile::Balanced,
                    Profile::TightSpread,
                    Profile::WideSweep,
                    Profile::Adversarial,
                ];
                PHASES[((generated / PHASE_LEN) as usize) % PHASES.len()]
            }
            other => other,
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Profile {
    type Err = ParseProfileError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "balanced" => Ok(Self::Balanced),
            "tight" | "tight-spread" | "tight_spread" => Ok(Self::TightSpread),
            "wide" | "wide-sweep" | "wide_sweep" => Ok(Self::WideSweep),
            "adversarial" | "edge" => Ok(Self::Adversarial),
            "mixed" => Ok(Self::Mixed),
            _ => Err(ParseProfileError),
        }
    }
}

impl fmt::Display for ParseProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "unknown profile (expected balanced, tight-spread, wide-sweep, adversarial, or mixed)",
        )
    }
}

impl std::error::Error for ParseProfileError {}

/// How to compare the two engines on one command stream.
///
/// `seed` and `profile` are copied into diagnostics. They do not have to be
/// the seed that produced `commands` when the stream is hand-written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffOptions {
    /// Diagnostic label.
    pub seed: u64,
    /// Diagnostic label.
    pub profile: Profile,
    /// Compare snapshots every this many commands.
    ///
    /// `0` compares only after the final command. The end of the stream is
    /// always checked.
    pub check_every: u64,
}

impl DiffOptions {
    /// Options that check the book after every command.
    pub const fn new(profile: Profile, seed: u64) -> Self {
        Self {
            seed,
            profile,
            check_every: 1,
        }
    }
}

/// A generated differential run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuzzConfig {
    /// Commands to draw from the workload.
    pub orders: u64,
    /// Workload seed.
    pub seed: u64,
    /// Workload profile.
    pub profile: Profile,
    /// Snapshot cadence. See [`DiffOptions::check_every`].
    pub check_every: u64,
    /// Replay the seed and require the same book and digests.
    pub check_determinism: bool,
}

impl FuzzConfig {
    /// Mixed-sized run: snapshots every 1024 commands, with a replay.
    pub const fn new(profile: Profile, seed: u64, orders: u64) -> Self {
        Self {
            orders,
            seed,
            profile,
            check_every: 1_024,
            check_determinism: true,
        }
    }

    fn options(self) -> DiffOptions {
        DiffOptions {
            seed: self.seed,
            profile: self.profile,
            check_every: self.check_every,
        }
    }
}

/// Totals from one pass that found no divergence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuzzReport {
    /// Profile named by the options.
    pub profile: Profile,
    /// Seed named by the options.
    pub seed: u64,
    /// Commands applied.
    pub orders: u64,
    /// Trade prints.
    pub trades: u64,
    /// Sum of trade quantities.
    pub shares_filled: u128,
    /// Cancels that removed a resting order.
    pub cancels: u64,
    /// Order rejects plus cancel rejects.
    pub rejects: u64,
    /// Times the book was compared and the invariants were checked.
    pub snapshot_checks: u64,
    /// Fingerprint of the trade stream, in match order.
    pub trade_digest: u64,
    /// Fingerprint of every command result, including rejects.
    pub result_digest: u64,
}

/// The engines disagreed, or they agreed on a book that breaks an invariant.
#[derive(Debug)]
pub struct Divergence {
    /// Workload seed, or the label passed in for a hand-written stream.
    pub seed: u64,
    /// Workload profile, or the label passed in for a hand-written stream.
    pub profile: Profile,
    /// Index of the failing command in the stream.
    pub command_index: u64,
    /// Command that was applied.
    pub command: Command,
    /// What disagreed.
    pub kind: DivergenceKind,
}

/// The specific mismatch inside a [`Divergence`].
#[derive(Debug)]
pub enum DivergenceKind {
    /// The two engines returned different results for one command.
    Result {
        /// [`crate::ReferenceEngine`] result.
        reference: Box<CommandResult>,
        /// [`crate::FastEngine`] result.
        fast: Box<CommandResult>,
    },
    /// Snapshots differed after a command both engines had accepted the same way.
    Book {
        /// [`crate::ReferenceEngine`] book.
        reference: Box<Book>,
        /// [`crate::FastEngine`] book.
        fast: Box<Book>,
    },
    /// Top-of-book quotes differed. `FastEngine` does not derive these from `depth`.
    Quotes {
        /// Reference best bid.
        reference_bid: Option<Quote>,
        /// Fast best bid.
        fast_bid: Option<Quote>,
        /// Reference best ask.
        reference_ask: Option<Quote>,
        /// Fast best ask.
        fast_ask: Option<Quote>,
    },
    /// Both engines produced a result or a book the matching rules forbid.
    Invariant {
        /// What failed.
        detail: String,
    },
    /// An engine panicked while applying the command.
    Panic {
        /// Panic payload and, when the other engine finished, its result.
        detail: String,
    },
    /// Replaying the same seed did not reproduce the first pass.
    Replay {
        /// Digests, and the first command or book that moved.
        detail: String,
    },
}

impl fmt::Display for Divergence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "differential divergence")?;
        writeln!(f, "  seed: {}", self.seed)?;
        writeln!(f, "  profile: {}", self.profile)?;
        writeln!(f, "  command_index: {}", self.command_index)?;
        writeln!(f, "  command: {:?}", self.command)?;
        match &self.kind {
            DivergenceKind::Result { reference, fast } => {
                writeln!(f, "  reference: {}", fmt_result(reference))?;
                write!(f, "  fast:      {}", fmt_result(fast))
            }
            DivergenceKind::Book { reference, fast } => {
                writeln!(f, "  book mismatch:")?;
                write!(f, "{}", fmt_book_diff(reference, fast))
            }
            DivergenceKind::Quotes {
                reference_bid,
                fast_bid,
                reference_ask,
                fast_ask,
            } => {
                writeln!(f, "  best bid reference: {reference_bid:?}")?;
                writeln!(f, "  best bid fast:      {fast_bid:?}")?;
                writeln!(f, "  best ask reference: {reference_ask:?}")?;
                write!(f, "  best ask fast:      {fast_ask:?}")
            }
            DivergenceKind::Invariant { detail }
            | DivergenceKind::Panic { detail }
            | DivergenceKind::Replay { detail } => write!(f, "  {detail}"),
        }
    }
}

impl std::error::Error for Divergence {}

/// Run `config` against both engines.
///
/// When `check_determinism` is set, the same seed is generated and applied a
/// second time. The final books and both digests must match.
pub fn run(config: &FuzzConfig) -> Result<FuzzReport, Box<Divergence>> {
    let first = drive_workload(config)?;
    if config.check_determinism {
        let second = match drive_workload(config) {
            Ok(pass) => pass,
            Err(err) => return Err(replay_of_failure(err)),
        };
        if first.report.trade_digest != second.report.trade_digest
            || first.report.result_digest != second.report.result_digest
            || first.book != second.book
        {
            return Err(locate_replay(config, &first, &second));
        }
    }
    Ok(first.report)
}

/// Compare both engines on a caller-supplied stream.
///
/// `options.seed` and `options.profile` are diagnostic labels only.
pub fn check_stream(
    commands: &[Command],
    options: &DiffOptions,
) -> Result<FuzzReport, Box<Divergence>> {
    Ok(drive(commands.iter().copied(), options)?.report)
}

/// First `count` commands of a workload.
pub fn generate(profile: Profile, seed: u64, count: usize) -> Vec<Command> {
    Workload::new(profile, seed).take(count).collect()
}

/// Deterministic command source.
///
/// The sequence is fixed by [`Profile`] and the seed. [`Iterator`] does not
/// end; stop it with [`Iterator::take`] or by counting [`Workload::next_command`].
#[derive(Debug)]
pub struct Workload {
    rng: Rng,
    profile: Profile,
    pending: VecDeque<Command>,
    /// Ids of non-zero limits this generator has not yet canceled.
    ///
    /// Fills are not visible here, so the vec can mention an id the engine has
    /// already removed. Canceling it then is a stale-id probe, and the cap
    /// still bounds how large the real book can grow.
    live: Vec<u64>,
    next_id: u64,
    generated: u64,
    mid: u64,
}

impl Workload {
    /// Stream for `profile` starting at `seed`.
    pub fn new(profile: Profile, seed: u64) -> Self {
        Self {
            rng: Rng::new(seed),
            profile,
            pending: VecDeque::new(),
            live: Vec::new(),
            next_id: 1,
            generated: 0,
            mid: INITIAL_MID,
        }
    }

    /// Next command. The stream is infinite.
    pub fn next_command(&mut self) -> Command {
        if self.pending.is_empty() {
            self.fill();
            assert!(
                !self.pending.is_empty(),
                "workload chunk did not produce a command"
            );
        }
        self.generated += 1;
        self.pending
            .pop_front()
            .expect("pending command was just produced")
    }

    fn fill(&mut self) {
        match self.profile.active(self.generated) {
            Profile::Balanced => self.emit_balanced(),
            Profile::TightSpread => self.emit_tight(),
            Profile::WideSweep => self.emit_wide(),
            Profile::Adversarial => self.emit_adversarial(),
            Profile::Mixed => unreachable!("mixed resolves to a concrete profile"),
        }
    }

    fn emit_balanced(&mut self) {
        if self.live.len() >= MAX_LIVE {
            let cancel = self.take_cancel();
            self.pending.push_back(cancel);
            return;
        }
        self.walk_mid();
        let side = self.random_side();
        let roll = self.rng.below(100);
        if roll < 15 {
            let id = self.alloc();
            let qty = 1 + self.rng.below(6);
            self.emit_market(id, side, qty);
            return;
        }
        if roll < 30 {
            let cancel = self.take_cancel();
            self.pending.push_back(cancel);
            return;
        }
        let price = if self.rng.below(8) == 0 {
            self.crossing_price(side)
        } else {
            self.passive_price(side)
        };
        // A retried id and a zero-quantity submit that is reused on the next
        // command. Both are rare; the book is mostly ordinary flow.
        if self.rng.below(32) == 0 {
            let id = self.alloc();
            let qty = 1 + self.rng.below(8);
            self.emit_limit_raw(id, side, price, 0);
            self.emit_limit(id, side, price, qty);
            return;
        }
        if self.rng.below(32) == 0 && !self.live.is_empty() {
            let index = self.rng.below(self.live.len() as u64) as usize;
            let id = self.live[index];
            let qty = 1 + self.rng.below(8);
            self.emit_limit_raw(id, side, price, qty);
            return;
        }
        let id = self.alloc();
        let qty = self.realistic_qty();
        self.emit_limit(id, side, price, qty);
    }

    /// Several orders at one price, then cancels of the oldest, newest, and
    /// middle id in that burst. A smaller opposite order follows so the new
    /// head is only partly filled.
    fn emit_tight(&mut self) {
        self.nudge_mid();
        self.emit_queue_churn(Side::Buy, self.mid);
        self.emit_queue_churn(Side::Sell, self.mid + 2);
        let id = self.alloc();
        let side = self.random_side();
        self.emit_market(id, side, 1);
        self.drain_to_cap();
    }

    fn emit_queue_churn(&mut self, side: Side, price: u64) {
        let n = 5 + self.rng.below(4) as usize;
        let qty = 4 + self.rng.below(5);
        let mut ids = Vec::with_capacity(n);
        for _ in 0..n {
            let id = self.alloc();
            ids.push(id);
            self.emit_limit(id, side, price, qty);
        }
        self.cancel_live_id(ids[0]);
        self.cancel_live_id(ids[n - 1]);
        self.cancel_live_id(ids[n / 2]);

        let nibble = 1 + self.rng.below(qty - 1);
        let aggressor = self.alloc();
        self.emit_limit(aggressor, side.opposite(), price, nibble);
        let joiner = self.alloc();
        self.emit_limit(joiner, side, price, qty);
    }

    /// Build a price ladder, delete every other bid level, sweep the inside
    /// asks through a partial FIFO fill, then hit the emptied side with a
    /// second market.
    fn emit_wide(&mut self) {
        let span = 8 + self.rng.below(9);
        let qty = 2 + self.rng.below(3);
        let mut bids = Vec::new();
        for offset in 1..=span {
            let price = self.mid - offset;
            debug_assert!(price > 3, "wide ladder entered the reserved prices");
            let id = self.alloc();
            bids.push(id);
            self.emit_limit(id, Side::Buy, price, qty);
        }
        for (index, id) in bids.into_iter().enumerate() {
            if index % 2 == 0 {
                self.cancel_live_id(id);
            }
        }

        const INSIDE_LEVELS: u64 = 3;
        const QUEUE: u64 = 2;
        for offset in 1..=span {
            let copies = if offset <= INSIDE_LEVELS { QUEUE } else { 1 };
            for _ in 0..copies {
                let id = self.alloc();
                self.emit_limit(id, Side::Sell, self.mid + offset, qty);
            }
        }
        // Fill the first two inside queues and one share of the second order
        // on the third level. `qty >= 2`, so this stops inside that order.
        let submitted = (INSIDE_LEVELS - 1) * QUEUE * qty + qty + 1;
        let taker = self.alloc();
        self.emit_limit(taker, Side::Buy, self.mid + INSIDE_LEVELS, submitted);

        // Larger than any book this generator keeps, so the market finishes
        // against an empty side. The one-lot that follows is an empty-book order.
        let clearing = self.alloc();
        self.emit_market(clearing, Side::Buy, 50_000);
        let empty = self.alloc();
        self.emit_market(empty, Side::Buy, 1);
        let clearing = self.alloc();
        self.emit_market(clearing, Side::Sell, 50_000);
        let empty = self.alloc();
        self.emit_market(empty, Side::Sell, 1);

        self.jump_mid();
        self.drain_to_cap();
    }

    /// Each chunk forces the reject reasons and the price extremes. Orders
    /// that might rest are canceled before the chunk ends, so this profile
    /// does not leave a giant book behind for the next profile.
    fn emit_adversarial(&mut self) {
        let reused = self.alloc();
        self.emit_limit_raw(reused, Side::Buy, 10, 0);
        self.emit_limit(reused, Side::Buy, 10, 1);
        // Zero quantity is reported even when the id is already consumed.
        self.emit_limit_raw(reused, Side::Sell, 10, 0);
        self.emit_limit_raw(reused, Side::Sell, 10, 1);
        self.cancel_live_id(reused);
        self.push_cancel(reused);

        let market_id = self.alloc();
        self.emit_market(market_id, Side::Sell, 0);
        self.emit_market(market_id, Side::Buy, 1);
        self.push_cancel(market_id);

        self.push_cancel(0);
        self.push_cancel(u64::MAX);
        self.push_cancel(self.next_id.saturating_add(50_000));

        // A one-lot buy at the maximum price either takes one share or rests.
        // The following sell at that price finishes the job, so the cancel is
        // `AlreadyFilled` on every book state. The sell is canceled too.
        let maker = self.alloc();
        let taker = self.alloc();
        self.emit_limit(maker, Side::Buy, u64::MAX, 1);
        self.emit_limit(taker, Side::Sell, u64::MAX, 1);
        self.cancel_live_id(maker);
        self.cancel_live_id(taker);

        // No ask is ever placed at 0, so this bid rests. The second cancel is
        // `AlreadyCanceled` and does not sweep the book.
        let resting = self.alloc();
        self.emit_limit(resting, Side::Buy, 0, 1);
        self.cancel_live_id(resting);
        self.push_cancel(resting);

        let low = self.alloc();
        self.emit_limit(low, Side::Buy, 1, 1);
        self.cancel_live_id(low);
        let high = self.alloc();
        self.emit_limit(high, Side::Sell, u64::MAX - 1, 1);
        self.cancel_live_id(high);

        // One maximum-sized order, alone at the reserved price, then removed.
        let huge = self.alloc();
        self.emit_limit(huge, Side::Buy, 3, u64::MAX);
        self.cancel_live_id(huge);

        // A little real matching below the moving mid, then cleaned up.
        let bid = self.alloc();
        let ask = self.alloc();
        let cross = self.alloc();
        self.emit_limit(bid, Side::Buy, 4_000, 2);
        self.emit_limit(ask, Side::Sell, 4_002, 2);
        self.emit_limit(cross, Side::Buy, 4_002, 1);
        self.cancel_live_id(bid);
        self.cancel_live_id(ask);
        self.cancel_live_id(cross);
        self.drain_to_cap();
    }

    fn alloc(&mut self) -> u64 {
        assert!(
            self.next_id > 0 && self.next_id < u64::MAX,
            "order id space exhausted"
        );
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn ts_for_next(&self) -> Timestamp {
        Timestamp(
            self.generated
                .saturating_add(u64::try_from(self.pending.len()).unwrap_or(u64::MAX)),
        )
    }

    fn emit_limit(&mut self, id: u64, side: Side, price: u64, qty: u64) {
        self.emit_limit_raw(id, side, price, qty);
        if qty > 0 {
            self.live.push(id);
        }
    }

    fn emit_limit_raw(&mut self, id: u64, side: Side, price: u64, qty: u64) {
        let ts = self.ts_for_next();
        self.pending.push_back(Command::limit(
            OrderId(id),
            side,
            Price(price),
            Qty(qty),
            ts,
        ));
    }

    fn emit_market(&mut self, id: u64, side: Side, qty: u64) {
        let ts = self.ts_for_next();
        self.pending
            .push_back(Command::market(OrderId(id), side, Qty(qty), ts));
    }

    fn push_cancel(&mut self, id: u64) {
        self.pending.push_back(Command::cancel(OrderId(id)));
    }

    fn cancel_live_id(&mut self, id: u64) {
        if let Some(pos) = self.live.iter().position(|live| *live == id) {
            self.live.remove(pos);
        }
        self.push_cancel(id);
    }

    fn take_cancel(&mut self) -> Command {
        if self.live.is_empty() {
            return Command::cancel(OrderId(
                self.next_id.saturating_add(1 + self.rng.below(10_000)),
            ));
        }
        let n = self.live.len();
        let roll = self.rng.below(100);
        let index = if roll < 25 {
            0
        } else if roll < 45 {
            n - 1
        } else if roll < 65 {
            n / 2
        } else {
            self.rng.below(n as u64) as usize
        };
        let id = self.live.remove(index);
        Command::cancel(OrderId(id))
    }

    fn drain_to_cap(&mut self) {
        while self.live.len() > MAX_LIVE {
            let id = self.live.remove(0);
            self.push_cancel(id);
        }
    }

    fn random_side(&mut self) -> Side {
        if self.rng.below(2) == 0 {
            Side::Buy
        } else {
            Side::Sell
        }
    }

    fn realistic_qty(&mut self) -> u64 {
        match self.rng.below(10) {
            0 => 1,
            1..=7 => 1 + self.rng.below(8),
            _ => 1 + self.rng.below(16),
        }
    }

    fn passive_price(&mut self, side: Side) -> u64 {
        let offset = self.rng.below(8);
        match side {
            Side::Buy => self.mid - 1 - offset,
            Side::Sell => self.mid + 1 + offset,
        }
    }

    fn crossing_price(&mut self, side: Side) -> u64 {
        let extra = self.rng.below(3);
        match side {
            Side::Buy => self.mid + extra,
            Side::Sell => self.mid.saturating_sub(extra),
        }
    }

    fn walk_mid(&mut self) {
        let delta = self.rng.below(5) as i64 - 2;
        self.shift_mid(delta);
    }

    fn nudge_mid(&mut self) {
        if self.rng.below(2) == 0 {
            let delta = self.rng.below(3) as i64 - 1;
            self.shift_mid(delta);
        }
    }

    fn jump_mid(&mut self) {
        let magnitude = (200 + self.rng.below(2_000)) as i64;
        let delta = if self.rng.below(2) == 0 {
            -magnitude
        } else {
            magnitude
        };
        self.shift_mid(delta);
    }

    fn shift_mid(&mut self, delta: i64) {
        let next = (self.mid as i64 + delta).clamp(MID_MIN as i64, MID_MAX as i64);
        self.mid = u64::try_from(next).expect("mid stays non-negative");
    }
}

impl Iterator for Workload {
    type Item = Command;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.next_command())
    }
}

/// SplitMix64. The state lives entirely in this struct.
struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut mixed = self.state;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        mixed ^ (mixed >> 31)
    }

    fn below(&mut self, exclusive_end: u64) -> u64 {
        debug_assert!(exclusive_end > 0);
        self.next_u64() % exclusive_end
    }
}

impl fmt::Debug for Rng {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Rng")
    }
}

struct Pass {
    report: FuzzReport,
    book: Book,
}

struct Totals {
    trades: u64,
    shares_filled: u128,
    cancels: u64,
    rejects: u64,
    snapshot_checks: u64,
}

/// Limit shares:
/// `submitted == taker_fills + maker_fills + canceled + resting`.
///
/// A trade consumes one share of the maker and one share of the taker. Those
/// are different orders, so both are counted. Resting size is read from the
/// book. Market orders never rest: `submitted == filled + canceled`, and
/// `filled <= submitted`.
struct Conservation {
    limit_submitted: u128,
    limit_taker_filled: u128,
    maker_filled: u128,
    limit_canceled: u128,
    market_submitted: u128,
    market_filled: u128,
    market_canceled: u128,
}

impl Conservation {
    fn new() -> Self {
        Self {
            limit_submitted: 0,
            limit_taker_filled: 0,
            maker_filled: 0,
            limit_canceled: 0,
            market_submitted: 0,
            market_filled: 0,
            market_canceled: 0,
        }
    }

    fn observe(&mut self, command: Command, result: &CommandResult) -> Result<(), String> {
        match (command, result) {
            (
                Command::Limit { qty, .. },
                CommandResult::Accepted {
                    filled_qty,
                    canceled_qty,
                    trades,
                    ..
                },
            ) => {
                add("limit submitted", &mut self.limit_submitted, qty.get())?;
                add(
                    "limit taker filled",
                    &mut self.limit_taker_filled,
                    filled_qty.get(),
                )?;
                add(
                    "limit canceled",
                    &mut self.limit_canceled,
                    canceled_qty.get(),
                )?;
                self.add_maker_fills(trades)?;
            }
            (
                Command::Market { qty, .. },
                CommandResult::Accepted {
                    filled_qty,
                    canceled_qty,
                    trades,
                    ..
                },
            ) => {
                add("market submitted", &mut self.market_submitted, qty.get())?;
                add("market filled", &mut self.market_filled, filled_qty.get())?;
                add(
                    "market canceled",
                    &mut self.market_canceled,
                    canceled_qty.get(),
                )?;
                if u128::from(filled_qty.get()) > u128::from(qty.get()) {
                    return Err(format!(
                        "market fill {} exceeds submitted {}",
                        filled_qty, qty
                    ));
                }
                self.add_maker_fills(trades)?;
            }
            (_, CommandResult::Canceled { canceled_qty, .. }) => {
                add(
                    "limit canceled",
                    &mut self.limit_canceled,
                    canceled_qty.get(),
                )?;
            }
            _ => {}
        }
        Ok(())
    }

    fn add_maker_fills(&mut self, trades: &[Trade]) -> Result<(), String> {
        for trade in trades {
            add("maker filled", &mut self.maker_filled, trade.qty.get())?;
        }
        Ok(())
    }

    fn check(&self, book: &Book) -> Result<(), String> {
        let resting = resting_qty(book)?;
        let limit_filled = self
            .limit_taker_filled
            .checked_add(self.maker_filled)
            .ok_or("limit filled quantity overflowed u128")?;
        let accounted = limit_filled
            .checked_add(self.limit_canceled)
            .and_then(|sum| sum.checked_add(resting))
            .ok_or("limit conservation sum overflowed u128")?;
        if accounted != self.limit_submitted {
            return Err(format!(
                "limit quantity not conserved: submitted {} != taker fills {} + maker fills {} + canceled {} + resting {}",
                self.limit_submitted,
                self.limit_taker_filled,
                self.maker_filled,
                self.limit_canceled,
                resting
            ));
        }
        if self.market_filled > self.market_submitted {
            return Err(format!(
                "market fills {} exceed submitted {}",
                self.market_filled, self.market_submitted
            ));
        }
        let market_accounted = self
            .market_filled
            .checked_add(self.market_canceled)
            .ok_or("market conservation sum overflowed u128")?;
        if market_accounted != self.market_submitted {
            return Err(format!(
                "market quantity not conserved: submitted {} != filled {} + canceled {}",
                self.market_submitted, self.market_filled, self.market_canceled
            ));
        }
        Ok(())
    }
}

struct Digest {
    trades: u64,
    results: u64,
}

impl Digest {
    /// FNV-1a offset basis. A stream with no trades stays at this value.
    const OFFSET: u64 = 0xCBF2_9CE4_8422_2325;

    fn new() -> Self {
        Self {
            trades: Self::OFFSET,
            results: Self::OFFSET,
        }
    }

    fn observe(&mut self, result: &CommandResult) {
        match result {
            CommandResult::Accepted {
                order_id,
                trades,
                filled_qty,
                resting_qty,
                canceled_qty,
            } => {
                self.mix_result(1);
                self.mix_result(order_id.get());
                self.mix_result(filled_qty.get());
                self.mix_result(resting_qty.get());
                self.mix_result(canceled_qty.get());
                self.mix_result(trades.len() as u64);
                for trade in trades {
                    self.observe_trade(trade);
                }
            }
            CommandResult::Rejected { order_id, reason } => {
                self.mix_result(2);
                self.mix_result(order_id.get());
                self.mix_result(match reason {
                    RejectReason::ZeroQuantity => 1,
                    RejectReason::DuplicateOrderId => 2,
                });
            }
            CommandResult::Canceled {
                order_id,
                canceled_qty,
            } => {
                self.mix_result(3);
                self.mix_result(order_id.get());
                self.mix_result(canceled_qty.get());
            }
            CommandResult::CancelRejected { order_id, reason } => {
                self.mix_result(4);
                self.mix_result(order_id.get());
                self.mix_result(match reason {
                    CancelRejectReason::NotFound => 1,
                    CancelRejectReason::AlreadyFilled => 2,
                    CancelRejectReason::AlreadyCanceled => 3,
                });
            }
        }
    }

    fn observe_trade(&mut self, trade: &Trade) {
        self.trades = mix(self.trades, trade.maker_id.get());
        self.trades = mix(self.trades, trade.taker_id.get());
        self.trades = mix(self.trades, trade.price.ticks());
        self.trades = mix(self.trades, trade.qty.get());
        self.trades = mix(
            self.trades,
            match trade.taker_side {
                Side::Buy => 1,
                Side::Sell => 2,
            },
        );
        self.trades = mix(self.trades, trade.ts.get());
        self.mix_result(self.trades);
    }

    fn mix_result(&mut self, value: u64) {
        self.results = mix(self.results, value);
    }
}

fn mix(hash: u64, value: u64) -> u64 {
    (hash ^ value).wrapping_mul(0x100_0000_01B3)
}

fn drive_workload(config: &FuzzConfig) -> Result<Pass, Box<Divergence>> {
    let count = usize::try_from(config.orders).expect("orders fit in usize");
    drive(
        Workload::new(config.profile, config.seed).take(count),
        &config.options(),
    )
}

fn drive(
    commands: impl Iterator<Item = Command>,
    options: &DiffOptions,
) -> Result<Pass, Box<Divergence>> {
    let mut reference = ReferenceEngine::new();
    let mut fast = FastEngine::new();
    let mut conservation = Conservation::new();
    let mut digest = Digest::new();
    let mut totals = Totals {
        trades: 0,
        shares_filled: 0,
        cancels: 0,
        rejects: 0,
        snapshot_checks: 0,
    };
    let mut since_check = 0u64;
    let mut applied = 0u64;
    let mut last_command = Command::cancel(OrderId(0));
    let mut last_index = 0u64;
    let mut last_book = Book {
        bids: Vec::new(),
        asks: Vec::new(),
    };

    for (index, command) in commands.enumerate() {
        let command_index = u64::try_from(index).expect("command index fits in u64");
        last_command = command;
        last_index = command_index;
        let reference_result = execute(&mut reference, command);
        let fast_result = execute(&mut fast, command);
        let result = match (reference_result, fast_result) {
            (Err(reference_panic), Err(fast_panic)) => {
                return Err(diverge(
                    options,
                    command_index,
                    command,
                    DivergenceKind::Panic {
                        detail: format!(
                            "both engines panicked\n  reference: {reference_panic}\n  fast: {fast_panic}"
                        ),
                    },
                ));
            }
            (Err(message), Ok(fast)) => {
                return Err(diverge(
                    options,
                    command_index,
                    command,
                    DivergenceKind::Panic {
                        detail: format!(
                            "reference panicked: {message}\n  fast: {}",
                            fmt_result(&fast)
                        ),
                    },
                ));
            }
            (Ok(reference), Err(message)) => {
                return Err(diverge(
                    options,
                    command_index,
                    command,
                    DivergenceKind::Panic {
                        detail: format!(
                            "fast panicked: {message}\n  reference: {}",
                            fmt_result(&reference)
                        ),
                    },
                ));
            }
            (Ok(reference), Ok(fast)) => {
                if reference != fast {
                    return Err(diverge(
                        options,
                        command_index,
                        command,
                        DivergenceKind::Result {
                            reference: Box::new(reference),
                            fast: Box::new(fast),
                        },
                    ));
                }
                reference
            }
        };
        if let Err(detail) = check_result_shape(command, &result) {
            return Err(diverge(
                options,
                command_index,
                command,
                DivergenceKind::Invariant {
                    detail: format!("{detail}\n  result: {}", fmt_result(&result)),
                },
            ));
        }
        if let Err(detail) = conservation.observe(command, &result) {
            return Err(diverge(
                options,
                command_index,
                command,
                DivergenceKind::Invariant { detail },
            ));
        }
        digest.observe(&result);
        note_totals(&mut totals, &result);
        applied += 1;
        since_check += 1;
        if options.check_every > 0 && since_check == options.check_every {
            last_book = checkpoint(
                options,
                &reference,
                &fast,
                &conservation,
                &mut totals,
                last_index,
                last_command,
            )?;
            since_check = 0;
        }
    }
    if applied == 0 || since_check != 0 {
        last_book = checkpoint(
            options,
            &reference,
            &fast,
            &conservation,
            &mut totals,
            last_index,
            last_command,
        )?;
    }

    Ok(Pass {
        report: FuzzReport {
            profile: options.profile,
            seed: options.seed,
            orders: applied,
            trades: totals.trades,
            shares_filled: totals.shares_filled,
            cancels: totals.cancels,
            rejects: totals.rejects,
            snapshot_checks: totals.snapshot_checks,
            trade_digest: digest.trades,
            result_digest: digest.results,
        },
        book: last_book,
    })
}

fn checkpoint(
    options: &DiffOptions,
    reference: &ReferenceEngine,
    fast: &FastEngine,
    conservation: &Conservation,
    totals: &mut Totals,
    command_index: u64,
    command: Command,
) -> Result<Book, Box<Divergence>> {
    let reference_book = reference.book();
    let fast_book = fast.book();
    if reference_book != fast_book {
        return Err(diverge(
            options,
            command_index,
            command,
            DivergenceKind::Book {
                reference: Box::new(reference_book),
                fast: Box::new(fast_book),
            },
        ));
    }
    let reference_bid = reference.best_bid();
    let fast_bid = fast.best_bid();
    let reference_ask = reference.best_ask();
    let fast_ask = fast.best_ask();
    if reference_bid != fast_bid || reference_ask != fast_ask {
        return Err(diverge(
            options,
            command_index,
            command,
            DivergenceKind::Quotes {
                reference_bid,
                fast_bid,
                reference_ask,
                fast_ask,
            },
        ));
    }
    if let Err(detail) = inspect_book(&reference_book) {
        return Err(diverge(
            options,
            command_index,
            command,
            DivergenceKind::Invariant { detail },
        ));
    }
    if let Err(detail) = conservation.check(&reference_book) {
        return Err(diverge(
            options,
            command_index,
            command,
            DivergenceKind::Invariant { detail },
        ));
    }
    totals.snapshot_checks += 1;
    Ok(reference_book)
}

fn note_totals(totals: &mut Totals, result: &CommandResult) {
    match result {
        CommandResult::Accepted {
            trades, filled_qty, ..
        } => {
            totals.trades += trades.len() as u64;
            totals.shares_filled += u128::from(filled_qty.get());
        }
        CommandResult::Canceled { .. } => totals.cancels += 1,
        CommandResult::Rejected { .. } | CommandResult::CancelRejected { .. } => {
            totals.rejects += 1;
        }
    }
}

fn execute(engine: &mut impl MatchingEngine, command: Command) -> Result<CommandResult, String> {
    match catch_unwind(AssertUnwindSafe(|| engine.execute(command))) {
        Ok(result) => Ok(result),
        Err(payload) => Err(panic_message(payload)),
    }
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "engine assertion failed".to_string()
    }
}

fn diverge(
    options: &DiffOptions,
    command_index: u64,
    command: Command,
    kind: DivergenceKind,
) -> Box<Divergence> {
    Box::new(Divergence {
        seed: options.seed,
        profile: options.profile,
        command_index,
        command,
        kind,
    })
}

fn check_result_shape(command: Command, result: &CommandResult) -> Result<(), String> {
    match (command, result) {
        (
            Command::Limit { id, qty, .. } | Command::Market { id, qty, .. },
            CommandResult::Rejected { order_id, reason },
        ) => {
            if *order_id != id {
                return Err(format!("reject id {order_id} does not match {id}"));
            }
            let expected = if qty.is_zero() {
                RejectReason::ZeroQuantity
            } else {
                RejectReason::DuplicateOrderId
            };
            if *reason != expected {
                return Err(format!(
                    "reject reason {reason:?} does not match {expected:?}"
                ));
            }
            Ok(())
        }
        (
            Command::Limit {
                id, side, qty, ts, ..
            },
            CommandResult::Accepted {
                order_id,
                trades,
                filled_qty,
                resting_qty,
                canceled_qty,
            },
        ) => {
            check_fill_identity(
                id,
                qty,
                *order_id,
                trades,
                *filled_qty,
                *resting_qty,
                *canceled_qty,
            )?;
            check_trade_identity(id, side, ts, trades)?;
            if !canceled_qty.is_zero() {
                return Err("limit order discarded quantity".to_string());
            }
            Ok(())
        }
        (
            Command::Market { id, side, qty, ts },
            CommandResult::Accepted {
                order_id,
                trades,
                filled_qty,
                resting_qty,
                canceled_qty,
            },
        ) => {
            check_fill_identity(
                id,
                qty,
                *order_id,
                trades,
                *filled_qty,
                *resting_qty,
                *canceled_qty,
            )?;
            check_trade_identity(id, side, ts, trades)?;
            if !resting_qty.is_zero() {
                return Err("market order rested".to_string());
            }
            if filled_qty.get() > qty.get() {
                return Err(format!(
                    "market fill {} exceeds submitted {}",
                    filled_qty, qty
                ));
            }
            Ok(())
        }
        (
            Command::Cancel { id },
            CommandResult::Canceled {
                order_id,
                canceled_qty,
            },
        ) => {
            if *order_id != id {
                return Err(format!("cancel id {order_id} does not match {id}"));
            }
            if canceled_qty.is_zero() {
                return Err("cancel removed zero quantity".to_string());
            }
            Ok(())
        }
        (Command::Cancel { id }, CommandResult::CancelRejected { order_id, .. }) => {
            if *order_id != id {
                return Err(format!("cancel reject id {order_id} does not match {id}"));
            }
            Ok(())
        }
        (command, result) => Err(format!(
            "command and result do not match: {command:?} -> {}",
            fmt_result(result)
        )),
    }
}

fn check_fill_identity(
    id: OrderId,
    qty: Qty,
    order_id: OrderId,
    trades: &[Trade],
    filled_qty: Qty,
    resting_qty: Qty,
    canceled_qty: Qty,
) -> Result<(), String> {
    if order_id != id {
        return Err(format!("accepted id {order_id} does not match {id}"));
    }
    let trade_qty = trades
        .iter()
        .fold(0u128, |sum, trade| sum + u128::from(trade.qty.get()));
    if trade_qty != u128::from(filled_qty.get()) {
        return Err(format!(
            "filled quantity {filled_qty} is not the trade sum {trade_qty}"
        ));
    }
    let parts = u128::from(filled_qty.get())
        + u128::from(resting_qty.get())
        + u128::from(canceled_qty.get());
    if parts != u128::from(qty.get()) {
        return Err(format!(
            "accepted quantity does not add up: filled {filled_qty} + resting {resting_qty} + canceled {canceled_qty} != {qty}"
        ));
    }
    Ok(())
}

fn check_trade_identity(
    id: OrderId,
    side: Side,
    ts: Timestamp,
    trades: &[Trade],
) -> Result<(), String> {
    for trade in trades {
        if trade.qty.is_zero() {
            return Err(format!("zero-quantity trade {trade:?}"));
        }
        if trade.taker_id != id || trade.taker_side != side || trade.ts != ts {
            return Err(format!(
                "trade {trade:?} does not belong to {side} order {id} at {ts}"
            ));
        }
    }
    Ok(())
}

fn inspect_book(book: &Book) -> Result<(), String> {
    if let (Some(bid), Some(ask)) = (book.bids.first(), book.asks.first()) {
        if bid.price >= ask.price {
            return Err(format!(
                "crossed book: best bid {} is not below best ask {}",
                bid.price, ask.price
            ));
        }
    }
    let mut seen = BTreeSet::new();
    inspect_side(&book.bids, Side::Buy, &mut seen)?;
    inspect_side(&book.asks, Side::Sell, &mut seen)?;
    Ok(())
}

fn inspect_side(levels: &[Level], side: Side, seen: &mut BTreeSet<OrderId>) -> Result<(), String> {
    for (index, level) in levels.iter().enumerate() {
        if level.orders.is_empty() {
            return Err(format!("empty {side} level at {}", level.price));
        }
        if index > 0 {
            let previous = levels[index - 1].price;
            let ordered = match side {
                Side::Buy => previous > level.price,
                Side::Sell => previous < level.price,
            };
            if !ordered {
                return Err(format!(
                    "{side} prices are not strictly ordered: {previous} then {}",
                    level.price
                ));
            }
        }
        let mut sum = 0u128;
        for order in &level.orders {
            if order.remaining_qty.is_zero() {
                return Err(format!("order {} rests with zero quantity", order.id));
            }
            if order.side != side {
                return Err(format!("order {} is on the wrong book", order.id));
            }
            if order.price != level.price {
                return Err(format!(
                    "order {} price {} does not match its level {}",
                    order.id, order.price, level.price
                ));
            }
            if order.remaining_qty > order.original_qty {
                return Err(format!(
                    "order {} remaining {} exceeds original {}",
                    order.id, order.remaining_qty, order.original_qty
                ));
            }
            if !seen.insert(order.id) {
                return Err(format!("order {} rests in more than one place", order.id));
            }
            sum += u128::from(order.remaining_qty.get());
        }
        if sum != u128::from(level.total_qty.get()) {
            return Err(format!(
                "{side} level {} total {} does not match the order sum {sum}",
                level.price, level.total_qty
            ));
        }
    }
    Ok(())
}

fn resting_qty(book: &Book) -> Result<u128, String> {
    let mut total = 0u128;
    for level in book.bids.iter().chain(book.asks.iter()) {
        total = total
            .checked_add(u128::from(level.total_qty.get()))
            .ok_or("resting quantity overflowed u128")?;
    }
    Ok(total)
}

fn add(label: &str, slot: &mut u128, value: u64) -> Result<(), String> {
    *slot = slot
        .checked_add(u128::from(value))
        .ok_or_else(|| format!("{label} overflowed u128"))?;
    Ok(())
}

fn replay_of_failure(err: Box<Divergence>) -> Box<Divergence> {
    let detail = format!("first pass passed but the replay failed:\n{err}");
    Box::new(Divergence {
        seed: err.seed,
        profile: err.profile,
        command_index: err.command_index,
        command: err.command,
        kind: DivergenceKind::Replay { detail },
    })
}

fn locate_replay(config: &FuzzConfig, first: &Pass, second: &Pass) -> Box<Divergence> {
    let header = format!(
        "replay diverged: trade digest {:016x} vs {:016x}, result digest {:016x} vs {:016x}",
        first.report.trade_digest,
        second.report.trade_digest,
        first.report.result_digest,
        second.report.result_digest
    );
    let mut left_workload = Workload::new(config.profile, config.seed);
    let mut right_workload = Workload::new(config.profile, config.seed);
    let mut left_reference = ReferenceEngine::new();
    let mut right_reference = ReferenceEngine::new();
    let mut left_fast = FastEngine::new();
    let mut right_fast = FastEngine::new();
    let mut last_command = Command::cancel(OrderId(0));
    let mut last_index = 0u64;

    for index in 0..config.orders {
        let command = left_workload.next_command();
        let again = right_workload.next_command();
        last_command = command;
        last_index = index;
        if command != again {
            return replay_at(
                config,
                index,
                command,
                format!("{header}\n  workload emitted {command:?} then {again:?}"),
            );
        }
        if let Some(err) = replay_step(
            config,
            index,
            command,
            &header,
            "reference",
            &mut left_reference,
            &mut right_reference,
        ) {
            return err;
        }
        if let Some(err) = replay_step(
            config,
            index,
            command,
            &header,
            "fast",
            &mut left_fast,
            &mut right_fast,
        ) {
            return err;
        }
    }

    let left_book = left_reference.book();
    let right_book = right_reference.book();
    if left_book != right_book {
        return replay_at(
            config,
            last_index,
            last_command,
            format!(
                "{header}\n  reference books:\n{}",
                fmt_book_diff(&left_book, &right_book)
            ),
        );
    }
    let left_fast_book = left_fast.book();
    let right_fast_book = right_fast.book();
    if left_fast_book != right_fast_book {
        return replay_at(
            config,
            last_index,
            last_command,
            format!(
                "{header}\n  fast books:\n{}",
                fmt_book_diff(&left_fast_book, &right_fast_book)
            ),
        );
    }
    replay_at(
        config,
        last_index,
        last_command,
        format!("{header}\n  per-command results matched; digest or snapshot compare did not"),
    )
}

fn replay_step(
    config: &FuzzConfig,
    index: u64,
    command: Command,
    header: &str,
    engine: &str,
    left: &mut impl MatchingEngine,
    right: &mut impl MatchingEngine,
) -> Option<Box<Divergence>> {
    let left_result = execute(left, command);
    let right_result = execute(right, command);
    if left_result == right_result {
        return None;
    }
    Some(replay_at(
        config,
        index,
        command,
        format!(
            "{header}\n  {engine} replay:\n  first:  {}\n  second: {}",
            fmt_execute(&left_result),
            fmt_execute(&right_result)
        ),
    ))
}

fn replay_at(
    config: &FuzzConfig,
    command_index: u64,
    command: Command,
    detail: String,
) -> Box<Divergence> {
    Box::new(Divergence {
        seed: config.seed,
        profile: config.profile,
        command_index,
        command,
        kind: DivergenceKind::Replay { detail },
    })
}

fn fmt_execute(result: &Result<CommandResult, String>) -> String {
    match result {
        Ok(result) => fmt_result(result),
        Err(message) => format!("panicked: {message}"),
    }
}

fn fmt_result(result: &CommandResult) -> String {
    match result {
        CommandResult::Accepted {
            order_id,
            trades,
            filled_qty,
            resting_qty,
            canceled_qty,
        } => {
            let mut text = format!(
                "Accepted {{ order_id: {order_id}, filled: {filled_qty}, resting: {resting_qty}, canceled: {canceled_qty}, trades: ["
            );
            for (index, trade) in trades.iter().enumerate().take(6) {
                if index > 0 {
                    text.push_str(", ");
                }
                text.push_str(&format!(
                    "{{ maker: {}, taker: {}, price: {}, qty: {}, side: {}, ts: {} }}",
                    trade.maker_id,
                    trade.taker_id,
                    trade.price,
                    trade.qty,
                    trade.taker_side,
                    trade.ts
                ));
            }
            if trades.len() > 6 {
                text.push_str(&format!(", ... {} more", trades.len() - 6));
            }
            text.push_str("] }");
            text
        }
        other => format!("{other:?}"),
    }
}

fn fmt_book_diff(reference: &Book, fast: &Book) -> String {
    let mut text = String::new();
    diff_side(&mut text, "bids", &reference.bids, &fast.bids);
    diff_side(&mut text, "asks", &reference.asks, &fast.asks);
    if text.is_empty() {
        text.push_str("  books differ but no level delta was isolated\n");
    }
    text
}

fn diff_side(text: &mut String, name: &str, left: &[Level], right: &[Level]) {
    let len = left.len().max(right.len());
    for index in 0..len {
        let left_level = left.get(index);
        let right_level = right.get(index);
        if left_level == right_level {
            continue;
        }
        text.push_str(&format!("  {name}[{index}]:\n"));
        text.push_str(&format!("    reference: {}\n", fmt_level(left_level)));
        text.push_str(&format!("    fast:      {}\n", fmt_level(right_level)));
        return;
    }
}

fn fmt_level(level: Option<&Level>) -> String {
    let Some(level) = level else {
        return "missing".to_string();
    };
    let mut orders = String::new();
    for (index, order) in level.orders.iter().enumerate().take(6) {
        if index > 0 {
            orders.push_str(", ");
        }
        orders.push_str(&format!("{}:{}", order.id, order.remaining_qty));
    }
    if level.orders.len() > 6 {
        orders.push_str(&format!(", ... {} more", level.orders.len() - 6));
    }
    format!(
        "price {} total {} orders [{}]",
        level.price, level.total_qty, orders
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RestingOrder;

    fn resting(id: u64, side: Side, price: u64, qty: u64) -> RestingOrder {
        RestingOrder {
            id: OrderId(id),
            side,
            price: Price(price),
            remaining_qty: Qty(qty),
            original_qty: Qty(qty),
            ts: Timestamp(0),
        }
    }

    fn level(price: u64, orders: Vec<RestingOrder>) -> Level {
        let total = orders
            .iter()
            .fold(0u64, |sum, order| sum + order.remaining_qty.get());
        Level {
            price: Price(price),
            total_qty: Qty(total),
            orders,
        }
    }

    #[test]
    fn same_seed_replays_the_same_commands_and_profiles_differ() {
        for profile in [
            Profile::Balanced,
            Profile::TightSpread,
            Profile::WideSweep,
            Profile::Adversarial,
            Profile::Mixed,
        ] {
            assert_eq!(
                generate(profile, 42, 400),
                generate(profile, 42, 400),
                "{profile}"
            );
        }
        assert_ne!(
            generate(Profile::Mixed, 1, 80),
            generate(Profile::Mixed, 2, 80)
        );
        assert_ne!(
            generate(Profile::Balanced, 1, 40),
            generate(Profile::TightSpread, 1, 40)
        );
        assert_ne!(
            generate(Profile::WideSweep, 1, 40),
            generate(Profile::Adversarial, 1, 40)
        );
    }

    #[test]
    fn profile_names_round_trip() {
        for profile in [
            Profile::Balanced,
            Profile::TightSpread,
            Profile::WideSweep,
            Profile::Adversarial,
            Profile::Mixed,
        ] {
            assert_eq!(profile.name().parse::<Profile>(), Ok(profile));
        }
        assert_eq!("tight".parse::<Profile>(), Ok(Profile::TightSpread));
        assert_eq!("wide_sweep".parse::<Profile>(), Ok(Profile::WideSweep));
        assert!("nope".parse::<Profile>().is_err());
    }

    #[test]
    fn structural_invariants_reject_a_broken_book() {
        let sound = Book {
            bids: vec![
                level(11, vec![resting(1, Side::Buy, 11, 2)]),
                level(10, vec![resting(2, Side::Buy, 10, 3)]),
            ],
            asks: vec![level(12, vec![resting(3, Side::Sell, 12, 4)])],
        };
        assert!(inspect_book(&sound).is_ok());

        let crossed = Book {
            bids: vec![level(12, vec![resting(1, Side::Buy, 12, 1)])],
            asks: vec![level(12, vec![resting(2, Side::Sell, 12, 1)])],
        };
        assert!(inspect_book(&crossed).unwrap_err().contains("crossed"));

        let empty = Book {
            bids: vec![Level {
                price: Price(5),
                total_qty: Qty(1),
                orders: Vec::new(),
            }],
            asks: Vec::new(),
        };
        assert!(inspect_book(&empty).unwrap_err().contains("empty"));

        let zero = level(
            4,
            vec![RestingOrder {
                remaining_qty: Qty::ZERO,
                ..resting(1, Side::Buy, 4, 1)
            }],
        );
        let zero_book = Book {
            bids: vec![Level {
                total_qty: Qty::ZERO,
                ..zero
            }],
            asks: Vec::new(),
        };
        assert!(inspect_book(&zero_book)
            .unwrap_err()
            .contains("zero quantity"));

        let unordered = Book {
            bids: vec![
                level(5, vec![resting(1, Side::Buy, 5, 1)]),
                level(6, vec![resting(2, Side::Buy, 6, 1)]),
            ],
            asks: Vec::new(),
        };
        assert!(inspect_book(&unordered)
            .unwrap_err()
            .contains("not strictly ordered"));

        let asks_descending = Book {
            bids: Vec::new(),
            asks: vec![
                level(8, vec![resting(1, Side::Sell, 8, 1)]),
                level(7, vec![resting(2, Side::Sell, 7, 1)]),
            ],
        };
        assert!(inspect_book(&asks_descending).is_err());

        let mut wrong_total = level(9, vec![resting(1, Side::Sell, 9, 2)]);
        wrong_total.total_qty = Qty(9);
        assert!(inspect_book(&Book {
            bids: Vec::new(),
            asks: vec![wrong_total],
        })
        .unwrap_err()
        .contains("does not match"));

        let wrong_side = level(9, vec![resting(1, Side::Buy, 9, 1)]);
        assert!(inspect_book(&Book {
            bids: Vec::new(),
            asks: vec![wrong_side],
        })
        .unwrap_err()
        .contains("wrong book"));

        let duplicate = Book {
            bids: vec![level(3, vec![resting(1, Side::Buy, 3, 1)])],
            asks: vec![level(4, vec![resting(1, Side::Sell, 4, 1)])],
        };
        assert!(inspect_book(&duplicate)
            .unwrap_err()
            .contains("more than one place"));
    }

    #[test]
    fn scripted_stream_conserves_quantity_including_extremes() {
        let commands = vec![
            Command::limit(OrderId(1), Side::Buy, Price(10), Qty(5), Timestamp(0)),
            Command::limit(OrderId(2), Side::Sell, Price(10), Qty(5), Timestamp(1)),
            Command::limit(OrderId(3), Side::Buy, Price(10), Qty(9), Timestamp(2)),
            Command::limit(OrderId(4), Side::Sell, Price(10), Qty(4), Timestamp(3)),
            Command::cancel(OrderId(3)),
            Command::limit(OrderId(5), Side::Buy, Price(8), Qty(2), Timestamp(5)),
            Command::market(OrderId(6), Side::Sell, Qty(5), Timestamp(6)),
            Command::limit(OrderId(7), Side::Buy, Price(3), Qty(u64::MAX), Timestamp(7)),
            Command::cancel(OrderId(7)),
            Command::market(OrderId(8), Side::Buy, Qty(u64::MAX), Timestamp(9)),
            Command::limit(OrderId(9), Side::Buy, Price(1), Qty(0), Timestamp(10)),
            Command::cancel(OrderId(0)),
            Command::limit(OrderId(1), Side::Sell, Price(1), Qty(1), Timestamp(12)),
        ];
        let report = check_stream(&commands, &DiffOptions::new(Profile::Balanced, 0))
            .unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(report.orders, commands.len() as u64);
        assert_eq!(report.snapshot_checks, commands.len() as u64);
        assert!(report.trades >= 2);
        assert!(report.rejects >= 2);
    }

    #[test]
    fn divergence_names_the_seed_profile_command_and_both_outputs() {
        let command = Command::limit(OrderId(7), Side::Buy, Price(11), Qty(3), Timestamp(4));
        let err = Divergence {
            seed: 42,
            profile: Profile::TightSpread,
            command_index: 19,
            command,
            kind: DivergenceKind::Result {
                reference: Box::new(CommandResult::Accepted {
                    order_id: OrderId(7),
                    trades: vec![Trade {
                        maker_id: OrderId(3),
                        taker_id: OrderId(7),
                        price: Price(11),
                        qty: Qty(3),
                        taker_side: Side::Buy,
                        ts: Timestamp(4),
                    }],
                    filled_qty: Qty(3),
                    resting_qty: Qty::ZERO,
                    canceled_qty: Qty::ZERO,
                }),
                fast: Box::new(CommandResult::Rejected {
                    order_id: OrderId(7),
                    reason: RejectReason::DuplicateOrderId,
                }),
            },
        };
        let text = err.to_string();
        assert!(text.contains("seed: 42"), "{text}");
        assert!(text.contains("profile: tight-spread"), "{text}");
        assert!(text.contains("command_index: 19"), "{text}");
        assert!(text.contains("OrderId(7)"), "{text}");
        assert!(text.contains("reference:"), "{text}");
        assert!(text.contains("fast:"), "{text}");
        assert!(text.contains("DuplicateOrderId"), "{text}");

        let books = Divergence {
            seed: 1,
            profile: Profile::WideSweep,
            command_index: 4,
            command: Command::cancel(OrderId(2)),
            kind: DivergenceKind::Book {
                reference: Box::new(Book {
                    bids: vec![level(10, vec![resting(2, Side::Buy, 10, 1)])],
                    asks: Vec::new(),
                }),
                fast: Box::new(Book {
                    bids: Vec::new(),
                    asks: Vec::new(),
                }),
            },
        };
        let text = books.to_string();
        assert!(text.contains("bids[0]"), "{text}");
        assert!(text.contains("reference:"), "{text}");
        assert!(text.contains("fast:"), "{text}");
    }
}
