//! Reproducible throughput and latency measurements.
//!
//! Every workload is a pure function of a name, a seed, and a length. The
//! benchmark builds that stream before it starts the clock, runs one warmup
//! pass, then times a throughput pass and a latency pass separately. The
//! throughput loop does not read the clock. The latency loop times each
//! operation with [`std::time::Instant`] and does not hash the result until
//! the clock has stopped.
//!
//! [`Workload`] covers the four matching-engine streams. The prediction-market
//! workload is a full [`ComplementArbStrategy`](crate::ComplementArbStrategy)
//! backtest on [`Preset::PredictionMarket`](crate::Preset::PredictionMarket).
//! Its orders/sec figure is market events per second. Its latency samples are
//! one market event each, including the strategy and the portfolio update.

use std::time::Instant;

use crate::backtest::{self, BacktestConfig, BacktestError, BacktestReport, EventStepper};
use crate::engine::MatchingEngine;
use crate::event_log::Event;
use crate::fuzz::{self, Profile};
use crate::sim::{generate, Preset, SimConfig};
use crate::strategy::{ComplementArbConfig, ComplementArbStrategy};
use crate::types::{
    Book, CancelRejectReason, Command, CommandResult, Level, OrderId, Price, Qty, RejectReason,
    Side, Timestamp, Trade,
};

const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x100_0000_01B3;

/// Passive bid ceiling for [`Workload::AddOnly`]. Asks start at
/// [`ADD_ONLY_ASK_BOTTOM`], so the two sides cannot cross.
const ADD_ONLY_BID_TOP: u64 = 8_000;
const ADD_ONLY_ASK_BOTTOM: u64 = 12_000;
const ADD_ONLY_LEVELS: u64 = 256;

/// Distinct prices in [`Workload::AddCancel`]. Two bids, then two asks.
const ADD_CANCEL_LEVELS: usize = 4;

/// Complement-arb knobs used by the prediction-market benchmark.
///
/// These match [`ComplementArbConfig::default`]: a one-tick edge, a position
/// cap of 200, and a clip of 20. Fees stay at zero and cash starts at
/// 1_000_000 ticks, which is [`BacktestConfig::default`].
pub const ARB_MIN_EDGE_TICKS: u64 = 1;
pub const ARB_MAX_POSITION: u64 = 200;
pub const ARB_MAX_ORDER_QTY: u64 = 20;

/// A matching-engine workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Workload {
    /// Passive limits spread across many prices. Nothing crosses.
    AddOnly,
    /// Passive limits, then cancels of a live order at the head, middle, or tail.
    AddCancel,
    /// Resting liquidity swept by crossing limits and market orders.
    MatchHeavy,
    /// [`Profile::Mixed`] from the differential fuzzer.
    Mixed,
}

impl Workload {
    /// Workloads in benchmark order.
    pub const ALL: [Self; 4] = [
        Self::AddOnly,
        Self::AddCancel,
        Self::MatchHeavy,
        Self::Mixed,
    ];

    /// Stable CLI name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::AddOnly => "add-only",
            Self::AddCancel => "add-cancel",
            Self::MatchHeavy => "match-heavy",
            Self::Mixed => "mixed",
        }
    }

    /// One-line description for the help text.
    pub const fn description(self) -> &'static str {
        match self {
            Self::AddOnly => "passive limit inserts across price levels",
            Self::AddCancel => "limit inserts paired with head, middle, and tail cancels",
            Self::MatchHeavy => "crossing limits and market orders that take resting liquidity",
            Self::Mixed => "fuzz profile mixed (balanced, tight-spread, wide-sweep, adversarial)",
        }
    }
}

impl std::fmt::Display for Workload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// First `ops` commands of `workload` at `seed`.
///
/// The stream does not read the operating system's random source. `ops` of
/// zero yields an empty vector.
pub fn generate_commands(workload: Workload, seed: u64, ops: usize) -> Vec<Command> {
    match workload {
        Workload::AddOnly => generate_add_only(seed, ops),
        Workload::AddCancel => generate_add_cancel(seed, ops),
        Workload::MatchHeavy => generate_match_heavy(seed, ops),
        Workload::Mixed => fuzz::generate(Profile::Mixed, seed, ops),
    }
}

/// Prediction-market events for the complement-arb benchmark.
///
/// The iterator is collected here so a later timed pass does not generate
/// events while the clock is running.
pub fn generate_prediction_market(seed: u64, ops: u64) -> Vec<Event> {
    generate(SimConfig::new(Preset::PredictionMarket, seed, ops)).collect()
}

/// Strategy measured by the prediction-market workload.
pub fn complement_arb_strategy() -> ComplementArbStrategy {
    ComplementArbStrategy::new(ComplementArbConfig::new(
        ARB_MIN_EDGE_TICKS,
        ARB_MAX_POSITION,
        ARB_MAX_ORDER_QTY,
    ))
}

/// Fee schedule and starting cash for the prediction-market workload.
pub fn complement_arb_backtest() -> BacktestConfig {
    BacktestConfig::default()
}

/// Nearest-rank latency percentiles, in the same unit as `samples`.
///
/// The rank for fraction `numer/denom` is `ceil(numer/denom * n)` (1-based).
/// The reported value is the sample at that rank after sorting, so every
/// percentile is one of the inputs. `p99_9` uses 999/1000. [`None`] when
/// `samples` is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Percentiles {
    /// 50th percentile.
    pub p50: u64,
    /// 95th percentile.
    pub p95: u64,
    /// 99th percentile.
    pub p99: u64,
    /// 99.9th percentile.
    pub p99_9: u64,
    /// Largest sample.
    pub max: u64,
}

/// Percentiles of `samples`. The slice does not need to be sorted.
pub fn percentiles(samples: &[u64]) -> Option<Percentiles> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    Some(Percentiles {
        p50: ranked(&sorted, 500, 1000),
        p95: ranked(&sorted, 950, 1000),
        p99: ranked(&sorted, 990, 1000),
        p99_9: ranked(&sorted, 999, 1000),
        max: *sorted.last().expect("sample slice was checked non-empty"),
    })
}

fn ranked(sorted: &[u64], numer: u128, denom: u128) -> u64 {
    sorted[rank_index(sorted.len(), numer, denom)]
}

/// 0-based index of the nearest rank. `len` must be non-zero.
fn rank_index(len: usize, numer: u128, denom: u128) -> usize {
    let n = u128::try_from(len).expect("sample count fits in u128");
    let rank = numer.saturating_mul(n).div_ceil(denom).max(1);
    let index = usize::try_from(rank - 1).unwrap_or(usize::MAX);
    index.min(len - 1)
}

/// FNV-1a fingerprint of one engine pass, plus the trade count and final book.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamDigest {
    /// Fingerprint of every command result and the final book.
    pub digest: u64,
    /// Trade prints.
    pub trades: u64,
    /// Book after the last command.
    pub book: Book,
}

/// Run `commands` and fingerprint the results.
///
/// Two engines that follow [`MatchingEngine`] produce the same digest and the
/// same book. This pass is not a timing measurement.
pub fn digest_commands<E: MatchingEngine>(engine: &mut E, commands: &[Command]) -> StreamDigest {
    let mut digest = RunningDigest::new();
    for command in commands {
        let result = engine.execute(*command);
        digest.observe(&result);
    }
    let book = engine.book();
    let (hash, trades) = digest.finish(&book);
    StreamDigest {
        digest: hash,
        trades,
        book,
    }
}

/// Throughput and per-operation latency for one engine.
///
/// `new_engine` is called three times: warmup, throughput, and latency. Each
/// pass starts from an empty book. `commands` must be non-empty.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "benchmark measurements are the result of the run"]
pub struct EngineBench {
    /// Commands applied.
    pub ops: u64,
    /// Wall time of the throughput pass, in nanoseconds.
    pub elapsed_ns: u128,
    /// `ops` per second, rounded to the nearest integer.
    pub ops_per_sec: u64,
    /// Per-`execute` latency, in nanoseconds.
    pub latency: Percentiles,
    /// Trade prints observed on the latency pass.
    pub trades: u64,
    /// FNV-1a digest of the latency pass, including the final book.
    pub digest: u64,
}

/// Warm up, then measure throughput and latency of `commands`.
pub fn bench_commands<E, F>(commands: &[Command], mut new_engine: F) -> EngineBench
where
    E: MatchingEngine,
    F: FnMut() -> E,
{
    assert!(!commands.is_empty(), "benchmark workload is empty");
    warmup_commands(&mut new_engine, commands);
    let elapsed_ns = time_commands(&mut new_engine, commands);
    let mut engine = new_engine();
    let (latency, trades, digest) = time_command_latency(&mut engine, commands);
    let ops = u64::try_from(commands.len()).expect("command count fits in u64");
    EngineBench {
        ops,
        elapsed_ns,
        ops_per_sec: ops_per_sec(ops, elapsed_ns),
        latency,
        trades,
        digest,
    }
}

/// End-to-end complement-arb measurement.
///
/// `orders_per_sec` counts market events, matching the backtest CLI. Latency
/// samples are one call to the backtest stepper: the market command, the
/// portfolio update, and every strategy action that follows.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "benchmark measurements are the result of the run"]
pub struct ArbBench {
    /// Market events applied.
    pub events: u64,
    /// Wall time of one full backtest, in nanoseconds.
    pub elapsed_ns: u128,
    /// Market events per second, rounded to the nearest integer.
    pub orders_per_sec: u64,
    /// Per-event latency, in nanoseconds.
    pub latency: Percentiles,
    /// Report from the throughput pass.
    pub report: BacktestReport,
}

/// Warm up, time one backtest, then time each event on a fresh run.
///
/// The latency pass must reproduce the throughput report. A mismatch is an
/// accounting error: the clock must not change the strategy.
pub fn bench_complement_arb<E, F>(
    events: &[Event],
    new_engine: F,
) -> Result<ArbBench, BacktestError>
where
    E: MatchingEngine,
    F: Fn(&crate::event_log::MarketId) -> E + Copy,
{
    assert!(!events.is_empty(), "prediction-market workload is empty");
    let warmed = backtest::run(
        events,
        new_engine,
        complement_arb_strategy(),
        complement_arb_backtest(),
    )?;
    std::hint::black_box(warmed.digest);

    let start = Instant::now();
    let report = backtest::run(
        events,
        new_engine,
        complement_arb_strategy(),
        complement_arb_backtest(),
    )?;
    let elapsed_ns = nanos(start);
    std::hint::black_box(report.digest);

    let mut stepper = EventStepper::new(
        new_engine,
        complement_arb_strategy(),
        complement_arb_backtest(),
    );
    let mut samples = Vec::with_capacity(events.len());
    for event in events {
        let start = Instant::now();
        stepper.apply(event)?;
        samples.push(elapsed_ns_u64(start));
    }
    let replayed = stepper.finish()?;
    if replayed != report {
        return Err(BacktestError::Accounting(
            "latency pass diverged from the throughput backtest".to_string(),
        ));
    }
    let events_n = u64::try_from(events.len()).expect("event count fits in u64");
    Ok(ArbBench {
        events: events_n,
        elapsed_ns,
        orders_per_sec: ops_per_sec(events_n, elapsed_ns),
        latency: percentiles(&samples).expect("event stream was checked non-empty"),
        report,
    })
}

/// `ops` per second. Zero elapsed time saturates at [`u64::MAX`] when `ops`
/// is non-zero, and stays zero when both are zero.
pub fn ops_per_sec(ops: u64, elapsed_ns: u128) -> u64 {
    let numer = u128::from(ops).saturating_mul(1_000_000_000);
    let Some(rounded) = elapsed_ns
        .checked_div(2)
        .and_then(|half| numer.saturating_add(half).checked_div(elapsed_ns))
    else {
        return if ops == 0 { 0 } else { u64::MAX };
    };
    u64::try_from(rounded).unwrap_or(u64::MAX)
}

fn warmup_commands<E, F>(new_engine: &mut F, commands: &[Command])
where
    E: MatchingEngine,
    F: FnMut() -> E,
{
    let mut engine = new_engine();
    let mut sink = 0u64;
    for command in commands {
        let result = engine.execute(*command);
        sink = sink.wrapping_add(result_stamp(&result));
    }
    std::hint::black_box(sink);
    std::hint::black_box(engine);
}

fn time_commands<E, F>(new_engine: &mut F, commands: &[Command]) -> u128
where
    E: MatchingEngine,
    F: FnMut() -> E,
{
    let mut engine = new_engine();
    let mut sink = 0u64;
    let start = Instant::now();
    for command in commands {
        let result = engine.execute(*command);
        sink = sink.wrapping_add(result_stamp(&result));
    }
    let elapsed = nanos(start);
    std::hint::black_box(sink);
    std::hint::black_box(engine);
    elapsed
}

fn time_command_latency<E: MatchingEngine>(
    engine: &mut E,
    commands: &[Command],
) -> (Percentiles, u64, u64) {
    let mut samples = Vec::with_capacity(commands.len());
    let mut digest = RunningDigest::new();
    for command in commands {
        let start = Instant::now();
        let result = engine.execute(*command);
        samples.push(elapsed_ns_u64(start));
        digest.observe(&result);
    }
    let book = engine.book();
    let (hash, trades) = digest.finish(&book);
    let latency = percentiles(&samples).expect("command stream was checked non-empty");
    (latency, trades, hash)
}

fn nanos(start: Instant) -> u128 {
    start.elapsed().as_nanos()
}

fn elapsed_ns_u64(start: Instant) -> u64 {
    u64::try_from(nanos(start)).unwrap_or(u64::MAX)
}

fn result_stamp(result: &CommandResult) -> u64 {
    match result {
        CommandResult::Accepted {
            order_id,
            trades,
            filled_qty,
            resting_qty,
            canceled_qty,
        } => {
            let tail = trades
                .last()
                .map_or(0, |trade| trade.price.ticks().wrapping_add(trade.qty.get()));
            order_id
                .get()
                .wrapping_add(filled_qty.get())
                .wrapping_add(resting_qty.get())
                .wrapping_add(canceled_qty.get())
                .wrapping_add(u64::try_from(trades.len()).unwrap_or(u64::MAX))
                .wrapping_add(tail)
        }
        CommandResult::Rejected { order_id, reason } => order_id.get().wrapping_add(match reason {
            RejectReason::ZeroQuantity => 1,
            RejectReason::DuplicateOrderId => 2,
        }),
        CommandResult::Canceled {
            order_id,
            canceled_qty,
        } => order_id.get().wrapping_add(canceled_qty.get()),
        CommandResult::CancelRejected { order_id, reason } => {
            order_id.get().wrapping_add(match reason {
                CancelRejectReason::NotFound => 1,
                CancelRejectReason::AlreadyFilled => 2,
                CancelRejectReason::AlreadyCanceled => 3,
            })
        }
    }
}

struct RunningDigest {
    hash: u64,
    trades: u64,
}

impl RunningDigest {
    fn new() -> Self {
        Self {
            hash: FNV_OFFSET,
            trades: 0,
        }
    }

    fn observe(&mut self, result: &CommandResult) {
        self.hash = mix_result(self.hash, result);
        self.trades = self.trades.saturating_add(trade_count(result));
    }

    fn finish(self, book: &Book) -> (u64, u64) {
        let hash = mix(mix_book(self.hash, book), u64::MAX);
        (hash, self.trades)
    }
}

fn trade_count(result: &CommandResult) -> u64 {
    match result {
        CommandResult::Accepted { trades, .. } => u64::try_from(trades.len()).unwrap_or(u64::MAX),
        _ => 0,
    }
}

fn mix(hash: u64, value: u64) -> u64 {
    (hash ^ value).wrapping_mul(FNV_PRIME)
}

fn mix_result(mut hash: u64, result: &CommandResult) -> u64 {
    match result {
        CommandResult::Accepted {
            order_id,
            trades,
            filled_qty,
            resting_qty,
            canceled_qty,
        } => {
            hash = mix(hash, 1);
            hash = mix(hash, order_id.get());
            hash = mix(hash, filled_qty.get());
            hash = mix(hash, resting_qty.get());
            hash = mix(hash, canceled_qty.get());
            hash = mix(hash, u64::try_from(trades.len()).unwrap_or(u64::MAX));
            for trade in trades {
                hash = mix_trade(hash, trade);
            }
        }
        CommandResult::Rejected { order_id, reason } => {
            hash = mix(hash, 2);
            hash = mix(hash, order_id.get());
            hash = mix(
                hash,
                match reason {
                    RejectReason::ZeroQuantity => 1,
                    RejectReason::DuplicateOrderId => 2,
                },
            );
        }
        CommandResult::Canceled {
            order_id,
            canceled_qty,
        } => {
            hash = mix(hash, 3);
            hash = mix(hash, order_id.get());
            hash = mix(hash, canceled_qty.get());
        }
        CommandResult::CancelRejected { order_id, reason } => {
            hash = mix(hash, 4);
            hash = mix(hash, order_id.get());
            hash = mix(
                hash,
                match reason {
                    CancelRejectReason::NotFound => 1,
                    CancelRejectReason::AlreadyFilled => 2,
                    CancelRejectReason::AlreadyCanceled => 3,
                },
            );
        }
    }
    hash
}

fn mix_trade(mut hash: u64, trade: &Trade) -> u64 {
    hash = mix(hash, trade.maker_id.get());
    hash = mix(hash, trade.taker_id.get());
    hash = mix(hash, trade.price.ticks());
    hash = mix(hash, trade.qty.get());
    hash = mix(hash, side_code(trade.taker_side));
    mix(hash, trade.ts.get())
}

fn mix_book(mut hash: u64, book: &Book) -> u64 {
    hash = mix(hash, 5);
    hash = mix_levels(hash, &book.bids);
    mix_levels(hash, &book.asks)
}

fn mix_levels(mut hash: u64, levels: &[Level]) -> u64 {
    hash = mix(hash, u64::try_from(levels.len()).unwrap_or(u64::MAX));
    for level in levels {
        hash = mix(hash, level.price.ticks());
        hash = mix(hash, level.total_qty.get());
        hash = mix(hash, u64::try_from(level.orders.len()).unwrap_or(u64::MAX));
        for order in &level.orders {
            hash = mix(hash, order.id.get());
            hash = mix(hash, side_code(order.side));
            hash = mix(hash, order.price.ticks());
            hash = mix(hash, order.remaining_qty.get());
            hash = mix(hash, order.original_qty.get());
            hash = mix(hash, order.ts.get());
        }
    }
    hash
}

fn side_code(side: Side) -> u64 {
    match side {
        Side::Buy => 1,
        Side::Sell => 2,
    }
}

fn generate_add_only(seed: u64, ops: usize) -> Vec<Command> {
    let mut rng = Rng::new(seed);
    let mut commands = Vec::with_capacity(ops);
    for step in 0..ops {
        let buy = step.is_multiple_of(2);
        let level = rng.below(ADD_ONLY_LEVELS);
        let price = if buy {
            ADD_ONLY_BID_TOP - level
        } else {
            ADD_ONLY_ASK_BOTTOM + level
        };
        let qty = 1 + rng.below(20);
        let id = u64::try_from(step).expect("op index fits in u64") + 1;
        let side = if buy { Side::Buy } else { Side::Sell };
        commands.push(Command::limit(
            OrderId(id),
            side,
            Price(price),
            Qty(qty),
            Timestamp(u64::try_from(step).expect("op index fits in u64")),
        ));
    }
    commands
}

fn generate_add_cancel(seed: u64, ops: usize) -> Vec<Command> {
    let target = (ops / 4).clamp(8, 512);
    let cap = target.div_ceil(ADD_CANCEL_LEVELS).saturating_add(8);
    let mut rng = Rng::new(seed);
    let mut queues = vec![Vec::<OrderId>::new(); ADD_CANCEL_LEVELS];
    let mut next_id = 1u64;
    let mut commands = Vec::with_capacity(ops);
    for step in 0..ops {
        let live: usize = queues.iter().map(Vec::len).sum();
        if live >= target && !step.is_multiple_of(2) {
            let start = below_usize(&mut rng, ADD_CANCEL_LEVELS);
            let level = (0..ADD_CANCEL_LEVELS)
                .map(|offset| (start + offset) % ADD_CANCEL_LEVELS)
                .find(|level| !queues[*level].is_empty())
                .expect("a live order implies a non-empty level");
            push_cancel(&mut queues[level], &mut rng, &mut commands);
        } else {
            let level = below_usize(&mut rng, ADD_CANCEL_LEVELS);
            if queues[level].len() >= cap {
                push_cancel(&mut queues[level], &mut rng, &mut commands);
            } else {
                let (side, price) = add_cancel_level(level);
                let qty = 1 + rng.below(12);
                let id = OrderId(next_id);
                next_id += 1;
                let ts = Timestamp(u64::try_from(step).expect("op index fits in u64"));
                commands.push(Command::limit(id, side, price, Qty(qty), ts));
                queues[level].push(id);
            }
        }
    }
    commands
}

fn add_cancel_level(level: usize) -> (Side, Price) {
    if level < 2 {
        (
            Side::Buy,
            Price(9_000 + u64::try_from(level).expect("level fits")),
        )
    } else {
        let offset = u64::try_from(level - 2).expect("level fits");
        (Side::Sell, Price(11_000 + offset))
    }
}

fn push_cancel(queue: &mut Vec<OrderId>, rng: &mut Rng, commands: &mut Vec<Command>) {
    let slot = match rng.below(3) {
        0 => 0,
        1 => queue.len() / 2,
        _ => queue.len() - 1,
    };
    let id = queue.remove(slot);
    commands.push(Command::cancel(id));
}

fn generate_match_heavy(seed: u64, ops: usize) -> Vec<Command> {
    let mut rng = Rng::new(seed);
    let mut commands = Vec::with_capacity(ops);
    let mut next_id = 1u64;
    let mut cycle = 0u64;
    while commands.len() < ops {
        let before = commands.len();
        let qty = 1 + rng.below(4);
        let market = !cycle.is_multiple_of(2);
        for offset in 0..3 {
            if commands.len() == ops {
                break;
            }
            push_limit(
                &mut commands,
                &mut next_id,
                Side::Sell,
                10_000 + offset,
                qty,
            );
        }
        if commands.len() < ops {
            push_taker(
                &mut commands,
                &mut next_id,
                Side::Buy,
                10_002,
                qty.saturating_mul(3),
                market,
            );
        }
        for offset in 0..3 {
            if commands.len() == ops {
                break;
            }
            push_limit(&mut commands, &mut next_id, Side::Buy, 9_000 + offset, qty);
        }
        if commands.len() < ops {
            push_taker(
                &mut commands,
                &mut next_id,
                Side::Sell,
                9_000,
                qty.saturating_mul(3),
                market,
            );
        }
        cycle += 1;
        if commands.len() == before {
            break;
        }
    }
    commands
}

fn push_taker(
    out: &mut Vec<Command>,
    next_id: &mut u64,
    side: Side,
    price: u64,
    qty: u64,
    market: bool,
) {
    if market {
        push_market(out, next_id, side, qty);
    } else {
        push_limit(out, next_id, side, price, qty);
    }
}

fn push_limit(out: &mut Vec<Command>, next_id: &mut u64, side: Side, price: u64, qty: u64) {
    let id = OrderId(*next_id);
    *next_id += 1;
    let ts = Timestamp(u64::try_from(out.len()).expect("command index fits in u64"));
    out.push(Command::limit(id, side, Price(price), Qty(qty), ts));
}

fn push_market(out: &mut Vec<Command>, next_id: &mut u64, side: Side, qty: u64) {
    let id = OrderId(*next_id);
    *next_id += 1;
    let ts = Timestamp(u64::try_from(out.len()).expect("command index fits in u64"));
    out.push(Command::market(id, side, Qty(qty), ts));
}

fn below_usize(rng: &mut Rng, exclusive_end: usize) -> usize {
    usize::try_from(rng.below(u64::try_from(exclusive_end).expect("bound fits in u64")))
        .expect("draw is below the bound")
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        complement_arb_strategy, digest_commands, generate_commands, generate_prediction_market,
        ops_per_sec, percentiles, Workload, ADD_ONLY_ASK_BOTTOM, ADD_ONLY_BID_TOP,
    };
    use crate::backtest::{self, BacktestConfig};
    use crate::event_log::MarketId;
    use crate::fast::FastEngine;
    use crate::fuzz::{self, Profile};
    use crate::reference::ReferenceEngine;
    use crate::strategy::ComplementArbStrategy;
    use crate::types::{Command, Price, Side};

    #[test]
    fn percentiles_use_nearest_rank() {
        let samples: Vec<u64> = (1..=1000).collect();
        let stats = percentiles(&samples).expect("non-empty");
        assert_eq!(stats.p50, 500);
        assert_eq!(stats.p95, 950);
        assert_eq!(stats.p99, 990);
        assert_eq!(stats.p99_9, 999);
        assert_eq!(stats.max, 1000);
    }

    #[test]
    fn percentiles_sort_and_stay_ordered() {
        let shuffled = [9_u64, 1, 8, 2, 7, 3, 10, 4, 6, 5];
        let stats = percentiles(&shuffled).expect("non-empty");
        let sorted = percentiles(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]).expect("non-empty");
        assert_eq!(stats, sorted);
        assert!(stats.p50 <= stats.p95);
        assert!(stats.p95 <= stats.p99);
        assert!(stats.p99 <= stats.p99_9);
        assert!(stats.p99_9 <= stats.max);
        assert_eq!(stats.max, 10);
        assert_eq!(percentiles(&[42]).expect("one sample").p99_9, 42);
        assert!(percentiles(&[]).is_none());
    }

    #[test]
    fn ops_per_sec_rounds_to_nearest() {
        assert_eq!(ops_per_sec(1_000, 1_000_000_000), 1_000);
        assert_eq!(ops_per_sec(2, 500), 4_000_000);
        assert_eq!(ops_per_sec(0, 0), 0);
        assert_eq!(ops_per_sec(5, 0), u64::MAX);
    }

    #[test]
    fn workloads_match_the_requested_length_and_repeat() {
        for ops in [1_usize, 7, 8, 64] {
            for workload in Workload::ALL {
                let commands = generate_commands(workload, 11, ops);
                assert_eq!(commands.len(), ops, "{workload}");
                assert_eq!(commands, generate_commands(workload, 11, ops));
            }
        }
        let events = generate_prediction_market(11, 25);
        assert_eq!(events.len(), 25);
        assert_eq!(events, generate_prediction_market(11, 25));
        assert!(events
            .iter()
            .all(|event| { event.market == MarketId::yes() || event.market == MarketId::no() }));
    }

    #[test]
    fn seed_changes_every_workload() {
        for workload in Workload::ALL {
            assert_ne!(
                generate_commands(workload, 1, 64),
                generate_commands(workload, 2, 64),
                "{workload}"
            );
        }
        assert_ne!(
            generate_prediction_market(1, 40),
            generate_prediction_market(2, 40)
        );
    }

    #[test]
    fn mixed_uses_the_fuzz_profile() {
        let commands = generate_commands(Workload::Mixed, 42, 350);
        assert_eq!(commands, fuzz::generate(Profile::Mixed, 42, 350));
        assert!(!commands.is_empty());
    }

    #[test]
    fn add_only_rests_without_crossing() {
        let commands = generate_commands(Workload::AddOnly, 3, 300);
        for command in &commands {
            match *command {
                Command::Limit {
                    side, price, qty, ..
                } => {
                    assert!(!qty.is_zero());
                    match side {
                        Side::Buy => assert!(price.ticks() <= ADD_ONLY_BID_TOP),
                        Side::Sell => assert!(price.ticks() >= ADD_ONLY_ASK_BOTTOM),
                    }
                }
                _ => panic!("add-only emitted {command:?}"),
            }
        }
        let fast = digest_commands(&mut FastEngine::new(), &commands);
        let reference = digest_commands(&mut ReferenceEngine::new(), &commands);
        assert_eq!(fast.trades, 0);
        assert!(!fast.book.is_crossed());
        assert_eq!(fast, reference);
    }

    #[test]
    fn add_cancel_cancels_live_ids() {
        let commands = generate_commands(Workload::AddCancel, 5, 80);
        let mut live = BTreeSet::new();
        let mut cancels = 0_usize;
        for command in &commands {
            match *command {
                Command::Limit { id, qty, price, .. } => {
                    assert!(!qty.is_zero());
                    assert!(passive_add_cancel(price));
                    assert!(live.insert(id), "duplicate limit id {id}");
                }
                Command::Cancel { id } => {
                    assert!(live.remove(&id), "cancel of an id that is not live");
                    cancels += 1;
                }
                Command::Market { .. } => panic!("add-cancel emitted a market order"),
            }
        }
        assert!(cancels > 0, "expected paired cancels");
        assert!(!live.is_empty(), "the book keeps a resting window");
    }

    fn passive_add_cancel(price: Price) -> bool {
        (9_000..9_002).contains(&price.ticks()) || (11_000..11_002).contains(&price.ticks())
    }

    #[test]
    fn match_heavy_cycle_prints_six_trades() {
        let commands = generate_commands(Workload::MatchHeavy, 1, 8);
        let fast = digest_commands(&mut FastEngine::new(), &commands);
        let reference = digest_commands(&mut ReferenceEngine::new(), &commands);
        assert_eq!(fast.trades, 6);
        assert_eq!(fast, reference);
        assert!(!fast.book.is_crossed());
    }

    #[test]
    fn engine_digests_match_on_benchmark_workloads() {
        for workload in Workload::ALL {
            let ops = if workload == Workload::Mixed {
                2_100
            } else {
                400
            };
            for seed in [1_u64, 42] {
                let commands = generate_commands(workload, seed, ops);
                assert!(!commands.is_empty());
                let fast = digest_commands(&mut FastEngine::new(), &commands);
                let again = digest_commands(&mut FastEngine::new(), &commands);
                let reference = digest_commands(&mut ReferenceEngine::new(), &commands);
                assert_eq!(fast, again, "{workload} seed {seed} is deterministic");
                assert_eq!(fast.trades, reference.trades, "{workload} seed {seed}");
                assert_eq!(fast.digest, reference.digest, "{workload} seed {seed}");
                assert_eq!(fast.book, reference.book, "{workload} seed {seed}");
            }
        }
    }

    #[test]
    fn prediction_market_arb_reports_match() {
        let config = complement_arb_strategy().config().clone();
        assert_eq!(config.min_edge_ticks, 1);
        assert_eq!(config.max_position, 200);
        assert_eq!(config.max_order_qty, 20);
        assert_eq!(config.max_unhedged, 0);
        assert!(config.allow_unwind);
        assert_eq!(
            ComplementArbStrategy::default().config(),
            complement_arb_strategy().config()
        );

        let events = generate_prediction_market(1, 250);
        assert!(!events.is_empty());
        let backtest = BacktestConfig::default();
        let fast = backtest::run(
            events.clone(),
            |_| FastEngine::new(),
            complement_arb_strategy(),
            backtest,
        )
        .expect("fast complement arb");
        let reference = backtest::run(
            events,
            |_| ReferenceEngine::new(),
            complement_arb_strategy(),
            backtest,
        )
        .expect("reference complement arb");
        assert_eq!(fast, reference);
    }
}
