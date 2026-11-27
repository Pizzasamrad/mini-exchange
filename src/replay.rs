//! Deterministic replay of an event stream through a [`MatchingEngine`].
//!
//! [`run`] accepts a slice, a `Vec`, or any iterator of events. [`run_reader`]
//! streams a file. [`verify`] and [`verify_reader`] apply the same stream to
//! [`FastEngine`] and [`ReferenceEngine`] and require equal command results,
//! equal books, and equal digests.
//!
//! The digest is FNV-1a over the trade tape in match order (each trade tagged
//! with its market) and then over each final book in market-id order. Two
//! replays are the same when their digests are equal.
//!
//! ```
//! use mini_exchange::replay::{run, ReplayOptions};
//! use mini_exchange::sim::{generate, Preset, SimConfig};
//! use mini_exchange::FastEngine;
//!
//! let report = run(
//!     generate(SimConfig::new(Preset::Equity, 1, 50)),
//!     |_| FastEngine::new(),
//!     ReplayOptions::default(),
//! );
//! assert_eq!(report.commands, 50);
//! ```

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::fmt;
use std::io::BufRead;

use crate::engine::MatchingEngine;
use crate::event_log::{Event, EventReader, MarketId, ReadError};
use crate::fast::FastEngine;
use crate::reference::ReferenceEngine;
use crate::types::{Book, Command, CommandResult, Level, Price, Quote, Side, Trade};

/// FNV-1a 64-bit offset basis. An empty tape and no books stay at this value
/// until the closing mix in [`Digest::finish`].
const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x100_0000_01B3;

/// How often to sample the inside of the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayOptions {
    /// Sample the market that just changed every this many commands.
    ///
    /// `0` samples only once per market, at the end of the stream. The end
    /// sample is always recorded, including when a command was sampled on the
    /// final step.
    pub sample_every: u64,
}

impl ReplayOptions {
    /// Options with an explicit sample cadence.
    pub const fn new(sample_every: u64) -> Self {
        Self { sample_every }
    }
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self { sample_every: 64 }
    }
}

/// Top-of-book samples taken while replaying.
///
/// `mid_sum` adds best bid and best ask on each two-sided sample. The average
/// mid in ticks is `mid_sum / (2 * two_sided)`, rounded down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BookStats {
    /// Samples recorded, including one-sided and empty books.
    pub samples: u64,
    /// Samples where both a bid and an ask were present.
    pub two_sided: u64,
    /// Sum of `ask - bid` over two-sided samples.
    pub spread_sum: u128,
    /// Smallest two-sided spread, in ticks.
    pub spread_min: Option<u64>,
    /// Largest two-sided spread, in ticks.
    pub spread_max: Option<u64>,
    /// Sum of best bid plus best ask over two-sided samples.
    pub mid_sum: u128,
}

impl BookStats {
    fn observe(&mut self, bid: Option<Quote>, ask: Option<Quote>) {
        self.samples += 1;
        let (Some(bid), Some(ask)) = (bid, ask) else {
            return;
        };
        self.two_sided += 1;
        let spread = ask.price.ticks().saturating_sub(bid.price.ticks());
        self.spread_sum = self.spread_sum.saturating_add(u128::from(spread));
        self.spread_min = Some(self.spread_min.map_or(spread, |min| min.min(spread)));
        self.spread_max = Some(self.spread_max.map_or(spread, |max| max.max(spread)));
        let mid = u128::from(bid.price.ticks()) + u128::from(ask.price.ticks());
        self.mid_sum = self.mid_sum.saturating_add(mid);
    }

    /// Average spread in ticks, rounded down.
    pub fn average_spread_ticks(&self) -> Option<u64> {
        if self.two_sided == 0 {
            None
        } else {
            u64::try_from(self.spread_sum / u128::from(self.two_sided)).ok()
        }
    }

    /// Average mid in ticks, rounded down.
    pub fn average_mid_ticks(&self) -> Option<u64> {
        if self.two_sided == 0 {
            None
        } else {
            let den = u128::from(self.two_sided) * 2;
            u64::try_from(self.mid_sum / den).ok()
        }
    }
}

/// Final state of one market after replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketSnapshot {
    /// Market symbol.
    pub market: MarketId,
    /// Commands applied to this market.
    pub commands: u64,
    /// Limit commands.
    pub limits: u64,
    /// Market-order commands.
    pub markets: u64,
    /// Cancel commands, including cancels the engine rejected.
    pub cancels: u64,
    /// Order rejects plus cancel rejects.
    pub rejects: u64,
    /// Trade prints.
    pub trades: u64,
    /// Sum of trade quantities, in shares.
    pub volume: u128,
    /// Sum of `price * qty` over trades. VWAP is `notional / volume`.
    pub notional: u128,
    /// Price of the last trade on this market.
    pub last_price: Option<Price>,
    /// Inside-quote samples for this market.
    pub top_of_book: BookStats,
    /// Canonical book. Bids are highest price first, asks lowest, FIFO within a level.
    pub book: Book,
}

impl MarketSnapshot {
    /// Volume-weighted average trade price, rounded down to a tick.
    pub fn vwap_ticks(&self) -> Option<u64> {
        vwap_ticks(self.notional, self.volume)
    }
}

/// Totals from one deterministic pass.
///
/// `limits + markets + cancels == commands`. `rejects` counts commands the
/// engine refused. `snapshots` is the canonical final book of every market
/// that appeared, sorted by market id. `digest` covers the trade tape and
/// those books.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "discarding a replay report drops the digest and the final book"]
pub struct ReplayReport {
    /// Events applied.
    pub commands: u64,
    /// Limit commands.
    pub limits: u64,
    /// Market-order commands.
    pub markets: u64,
    /// Cancel commands, including cancels the engine rejected.
    pub cancels: u64,
    /// Order rejects plus cancel rejects.
    pub rejects: u64,
    /// Trade prints across every market.
    pub trades: u64,
    /// Sum of trade quantities, in shares.
    pub volume: u128,
    /// Sum of `price * qty` over every trade.
    pub notional: u128,
    /// Price of the last trade in stream order.
    pub last_price: Option<Price>,
    /// Inside-quote samples across every market.
    pub top_of_book: BookStats,
    /// One snapshot per market, sorted by market id.
    pub snapshots: Vec<MarketSnapshot>,
    /// FNV-1a fingerprint of the trade tape and the final books.
    pub digest: u64,
}

impl ReplayReport {
    /// Final book when the stream contains exactly one market.
    pub fn book(&self) -> Option<&Book> {
        if self.snapshots.len() == 1 {
            Some(&self.snapshots[0].book)
        } else {
            None
        }
    }

    /// Snapshot for `market`.
    pub fn snapshot(&self, market: &MarketId) -> Option<&MarketSnapshot> {
        self.snapshots.iter().find(|snap| &snap.market == market)
    }

    /// Volume-weighted average trade price, rounded down to a tick.
    pub fn vwap_ticks(&self) -> Option<u64> {
        vwap_ticks(self.notional, self.volume)
    }
}

fn vwap_ticks(notional: u128, volume: u128) -> Option<u64> {
    notional
        .checked_div(volume)
        .and_then(|ticks| u64::try_from(ticks).ok())
}

/// Drives one engine per market id.
pub struct Replayer<E, F> {
    factory: F,
    markets: BTreeMap<MarketId, MarketState<E>>,
    options: ReplayOptions,
    commands: u64,
    last_price: Option<Price>,
    top_of_book: BookStats,
    digest: Digest,
}

struct MarketState<E> {
    engine: E,
    commands: u64,
    limits: u64,
    markets: u64,
    cancels: u64,
    rejects: u64,
    trades: u64,
    volume: u128,
    notional: u128,
    last_price: Option<Price>,
    top: BookStats,
}

impl<E> MarketState<E> {
    fn new(engine: E) -> Self {
        Self {
            engine,
            commands: 0,
            limits: 0,
            markets: 0,
            cancels: 0,
            rejects: 0,
            trades: 0,
            volume: 0,
            notional: 0,
            last_price: None,
            top: BookStats::default(),
        }
    }

    fn record_command(&mut self, command: Command) {
        self.commands += 1;
        match command {
            Command::Limit { .. } => self.limits += 1,
            Command::Market { .. } => self.markets += 1,
            Command::Cancel { .. } => self.cancels += 1,
        }
    }

    fn record_result(&mut self, result: &CommandResult) {
        match result {
            CommandResult::Accepted { trades, .. } => {
                for trade in trades {
                    self.trades += 1;
                    let qty = u128::from(trade.qty.get());
                    let px = u128::from(trade.price.ticks());
                    self.volume = self.volume.saturating_add(qty);
                    self.notional = self.notional.saturating_add(px.saturating_mul(qty));
                    self.last_price = Some(trade.price);
                }
            }
            CommandResult::Rejected { .. } | CommandResult::CancelRejected { .. } => {
                self.rejects += 1;
            }
            CommandResult::Canceled { .. } => {}
        }
    }
}

impl<E, F> Replayer<E, F>
where
    E: MatchingEngine,
    F: FnMut(&MarketId) -> E,
{
    /// Empty replayer. `factory` builds the engine for a market on first use.
    pub fn new(factory: F, options: ReplayOptions) -> Self {
        Self {
            factory,
            markets: BTreeMap::new(),
            options,
            commands: 0,
            last_price: None,
            top_of_book: BookStats::default(),
            digest: Digest::new(),
        }
    }

    /// Apply one event. Markets that have not been seen get a fresh engine.
    pub fn apply(&mut self, event: &Event) -> CommandResult {
        self.ensure_market(&event.market);
        self.commands += 1;
        let result = {
            let state = self
                .markets
                .get_mut(&event.market)
                .expect("market was inserted");
            state.record_command(event.command);
            let result = state.engine.execute(event.command);
            state.record_result(&result);
            result
        };
        if let CommandResult::Accepted { trades, .. } = &result {
            for trade in trades {
                self.last_price = Some(trade.price);
                self.digest.observe_trade(&event.market, trade);
            }
        }
        if self.options.sample_every > 0 && self.commands.is_multiple_of(self.options.sample_every)
        {
            self.sample(&event.market);
        }
        result
    }

    /// Take the final inside-quote sample and the canonical books.
    pub fn finish(mut self) -> ReplayReport {
        let mut limits = 0u64;
        let mut markets = 0u64;
        let mut cancels = 0u64;
        let mut rejects = 0u64;
        let mut trades = 0u64;
        let mut volume = 0u128;
        let mut notional = 0u128;
        let mut snapshots = Vec::with_capacity(self.markets.len());

        for (market, mut state) in self.markets {
            let bid = state.engine.best_bid();
            let ask = state.engine.best_ask();
            state.top.observe(bid, ask);
            self.top_of_book.observe(bid, ask);
            let book = state.engine.book();
            self.digest.observe_book(&market, &book);
            limits += state.limits;
            markets += state.markets;
            cancels += state.cancels;
            rejects += state.rejects;
            trades += state.trades;
            volume = volume.saturating_add(state.volume);
            notional = notional.saturating_add(state.notional);
            snapshots.push(MarketSnapshot {
                market,
                commands: state.commands,
                limits: state.limits,
                markets: state.markets,
                cancels: state.cancels,
                rejects: state.rejects,
                trades: state.trades,
                volume: state.volume,
                notional: state.notional,
                last_price: state.last_price,
                top_of_book: state.top,
                book,
            });
        }

        debug_assert_eq!(limits + markets + cancels, self.commands);
        ReplayReport {
            commands: self.commands,
            limits,
            markets,
            cancels,
            rejects,
            trades,
            volume,
            notional,
            last_price: self.last_price,
            top_of_book: self.top_of_book,
            snapshots,
            digest: self.digest.finish(),
        }
    }

    fn ensure_market(&mut self, market: &MarketId) {
        if self.markets.contains_key(market) {
            return;
        }
        let engine = (self.factory)(market);
        self.markets
            .insert(market.clone(), MarketState::new(engine));
    }

    fn sample(&mut self, market: &MarketId) {
        let (bid, ask) = {
            let state = self.markets.get_mut(market).expect("market was inserted");
            let bid = state.engine.best_bid();
            let ask = state.engine.best_ask();
            state.top.observe(bid, ask);
            (bid, ask)
        };
        self.top_of_book.observe(bid, ask);
    }
}

/// Replay `events` through engines from `factory`.
///
/// `events` may be a `Vec<Event>`, a `&[Event]`, or any iterator that yields
/// [`Event`] or `&Event`.
pub fn run<E, I, F>(events: I, factory: F, options: ReplayOptions) -> ReplayReport
where
    I: IntoIterator,
    I::Item: Borrow<Event>,
    E: MatchingEngine,
    F: FnMut(&MarketId) -> E,
{
    let mut replayer = Replayer::new(factory, options);
    for event in events {
        let _ = replayer.apply(event.borrow());
    }
    replayer.finish()
}

/// Stream a log through engines from `factory`.
pub fn run_reader<R, E, F>(
    reader: EventReader<R>,
    factory: F,
    options: ReplayOptions,
) -> Result<ReplayReport, ReadError>
where
    R: BufRead,
    E: MatchingEngine,
    F: FnMut(&MarketId) -> E,
{
    let mut replayer = Replayer::new(factory, options);
    for item in reader {
        let _ = replayer.apply(&item?);
    }
    Ok(replayer.finish())
}

/// Both engines agreed on a stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedReplay {
    /// [`FastEngine`] report.
    pub fast: ReplayReport,
    /// [`ReferenceEngine`] report.
    pub reference: ReplayReport,
}

/// The two engines disagreed, or their reports did.
#[derive(Debug)]
pub enum VerifyError {
    /// One command produced different results.
    Mismatch {
        /// Zero-based index in the stream.
        index: u64,
        /// Event that was applied.
        event: Box<Event>,
        /// [`FastEngine`] result.
        fast: Box<CommandResult>,
        /// [`ReferenceEngine`] result.
        reference: Box<CommandResult>,
    },
    /// Command results matched, but the finished reports did not.
    Report {
        /// Which fields disagreed.
        detail: String,
    },
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mismatch {
                index,
                event,
                fast,
                reference,
            } => write!(
                f,
                "verify mismatch at event {index} (seq {} market {})\n  command: {:?}\n  fast: {:?}\n  reference: {:?}",
                event.seq, event.market, event.command, fast, reference
            ),
            Self::Report { detail } => write!(
                f,
                "verify reports differ after matching command results: {detail}"
            ),
        }
    }
}

impl std::error::Error for VerifyError {}

/// A streamed replay failed to read, or the two engines disagreed.
#[derive(Debug)]
pub enum StreamError {
    /// The log could not be read or parsed.
    Read(ReadError),
    /// Fast and reference diverged.
    Verify(VerifyError),
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(err) => write!(f, "{err}"),
            Self::Verify(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for StreamError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(err) => Some(err),
            Self::Verify(err) => Some(err),
        }
    }
}

impl From<ReadError> for StreamError {
    fn from(err: ReadError) -> Self {
        Self::Read(err)
    }
}

impl From<std::io::Error> for StreamError {
    fn from(err: std::io::Error) -> Self {
        Self::Read(ReadError::Io(err))
    }
}

impl From<VerifyError> for StreamError {
    fn from(err: VerifyError) -> Self {
        Self::Verify(err)
    }
}

/// Replay `events` through [`FastEngine`] and [`ReferenceEngine`].
pub fn verify<I>(events: I, options: ReplayOptions) -> Result<VerifiedReplay, VerifyError>
where
    I: IntoIterator,
    I::Item: Borrow<Event>,
{
    let mut pair = Pair::new(options);
    for event in events {
        pair.apply(event.borrow())?;
    }
    pair.finish()
}

/// Stream a log through both engines in one pass.
pub fn verify_reader<R: BufRead>(
    reader: EventReader<R>,
    options: ReplayOptions,
) -> Result<VerifiedReplay, StreamError> {
    let mut pair = Pair::new(options);
    for item in reader {
        pair.apply(&item?)?;
    }
    Ok(pair.finish()?)
}

struct Pair {
    fast: Replayer<FastEngine, fn(&MarketId) -> FastEngine>,
    reference: Replayer<ReferenceEngine, fn(&MarketId) -> ReferenceEngine>,
    index: u64,
}

fn new_fast(_: &MarketId) -> FastEngine {
    FastEngine::new()
}

fn new_reference(_: &MarketId) -> ReferenceEngine {
    ReferenceEngine::new()
}

impl Pair {
    fn new(options: ReplayOptions) -> Self {
        Self {
            fast: Replayer::new(new_fast, options),
            reference: Replayer::new(new_reference, options),
            index: 0,
        }
    }

    fn apply(&mut self, event: &Event) -> Result<(), VerifyError> {
        let fast = self.fast.apply(event);
        let reference = self.reference.apply(event);
        let index = self.index;
        self.index += 1;
        if fast != reference {
            return Err(VerifyError::Mismatch {
                index,
                event: Box::new(event.clone()),
                fast: Box::new(fast),
                reference: Box::new(reference),
            });
        }
        Ok(())
    }

    fn finish(self) -> Result<VerifiedReplay, VerifyError> {
        let fast = self.fast.finish();
        let reference = self.reference.finish();
        if let Some(detail) = diff_reports(&fast, &reference) {
            return Err(VerifyError::Report { detail });
        }
        Ok(VerifiedReplay { fast, reference })
    }
}

fn diff_reports(fast: &ReplayReport, reference: &ReplayReport) -> Option<String> {
    if fast == reference {
        return None;
    }
    let mut parts = Vec::new();
    if fast.digest != reference.digest {
        parts.push(format!(
            "digest {:016x} != {:016x}",
            fast.digest, reference.digest
        ));
    }
    if fast.trades != reference.trades {
        parts.push(format!("trades {} != {}", fast.trades, reference.trades));
    }
    if fast.volume != reference.volume {
        parts.push(format!("volume {} != {}", fast.volume, reference.volume));
    }
    if fast.snapshots != reference.snapshots {
        parts.push("final books differ".to_string());
    }
    if fast.top_of_book != reference.top_of_book {
        parts.push("top-of-book statistics differ".to_string());
    }
    if parts.is_empty() {
        parts.push("reports are not equal".to_string());
    }
    Some(parts.join("; "))
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

    fn mix_str(&mut self, text: &str) {
        self.mix(u64::try_from(text.len()).unwrap_or(u64::MAX));
        for byte in text.bytes() {
            self.mix(u64::from(byte));
        }
    }

    fn observe_trade(&mut self, market: &MarketId, trade: &Trade) {
        self.mix(1);
        self.mix_str(market.as_str());
        self.mix(trade.maker_id.get());
        self.mix(trade.taker_id.get());
        self.mix(trade.price.ticks());
        self.mix(trade.qty.get());
        self.mix(side_code(trade.taker_side));
        self.mix(trade.ts.get());
    }

    fn observe_book(&mut self, market: &MarketId, book: &Book) {
        self.mix(2);
        self.mix_str(market.as_str());
        self.observe_levels(&book.bids);
        self.observe_levels(&book.asks);
    }

    fn observe_levels(&mut self, levels: &[Level]) {
        self.mix(u64::try_from(levels.len()).unwrap_or(u64::MAX));
        for level in levels {
            self.mix(level.price.ticks());
            self.mix(level.total_qty.get());
            self.mix(u64::try_from(level.orders.len()).unwrap_or(u64::MAX));
            for order in &level.orders {
                self.mix(order.id.get());
                self.mix(side_code(order.side));
                self.mix(order.price.ticks());
                self.mix(order.remaining_qty.get());
                self.mix(order.original_qty.get());
                self.mix(order.ts.get());
            }
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
