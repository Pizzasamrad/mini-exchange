//! Deterministic synthetic order flow.
//!
//! [`Simulator`] is a pure function of a [`Preset`] and a seed. It never reads
//! the operating system's random source. The latent fair value is an integer
//! mean-reverting random walk. Liquidity providers post bids and asks in
//! layers around that value, takers send crossing limits and market orders,
//! and cancels prefer orders that are old or far from the fair value.
//!
//! [`Preset::Equity`] is one book centered on 10_000 ticks. [`Preset::PredictionMarket`]
//! is a YES/NO pair whose prices stay in `1..=99`. NO tracks `100 - YES`
//! except during short shocks that open a complement gap for a later
//! arbitrage backtester.

use std::fmt;
use std::str::FromStr;

use crate::event_log::{Event, MarketId};
use crate::types::{Command, OrderId, Price, Qty, Side, Timestamp};

/// How much synthetic flow to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimConfig {
    /// Equity book or binary YES/NO pair.
    pub preset: Preset,
    /// Events to emit.
    pub orders: u64,
    /// Generator seed. The stream does not use OS randomness.
    pub seed: u64,
}

impl SimConfig {
    /// A stream of `orders` events from `preset` and `seed`.
    pub const fn new(preset: Preset, seed: u64, orders: u64) -> Self {
        Self {
            preset,
            orders,
            seed,
        }
    }
}

/// Kind of market the generator builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Preset {
    /// One book whose fair value mean-reverts around 10_000 ticks.
    Equity,
    /// YES and NO books, prices strictly inside `1..=99`, with complement shocks.
    PredictionMarket,
}

/// Rejected preset name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsePresetError;

impl Preset {
    /// Stable name used by the CLI and by log comments.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Equity => "equity",
            Self::PredictionMarket => "prediction-market",
        }
    }
}

impl fmt::Display for Preset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Preset {
    type Err = ParsePresetError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "equity" => Ok(Self::Equity),
            "prediction-market" | "prediction_market" | "prediction" => Ok(Self::PredictionMarket),
            _ => Err(ParsePresetError),
        }
    }
}

impl fmt::Display for ParsePresetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown preset (expected equity or prediction-market)")
    }
}

impl std::error::Error for ParsePresetError {}

/// Build a simulator. The iterator yields exactly `config.orders` events.
pub fn generate(config: SimConfig) -> Simulator {
    Simulator::new(config)
}

/// Seeded microstructure generator.
#[derive(Debug)]
pub struct Simulator {
    rng: Rng,
    remaining: u64,
    next_seq: u64,
    next_id: u64,
    preset: Preset,
    anchor: u64,
    latent: u64,
    shock: i64,
    shock_left: u64,
    markets: Vec<MarketSim>,
}

#[derive(Debug)]
struct MarketSim {
    id: MarketId,
    live: Vec<Tracked>,
}

#[derive(Debug, Clone, Copy)]
struct Tracked {
    id: OrderId,
    side: Side,
    price: u64,
    born: u64,
}

#[derive(Clone, Copy)]
struct Style {
    min_price: u64,
    max_price: u64,
    fair_min: u64,
    fair_max: u64,
    reversion: i64,
    noise: u64,
    jump_every: u64,
    jump: u64,
    levels: u64,
    lifetime: u64,
    far: u64,
    max_live: usize,
    qty: &'static [u64],
    take_qty: &'static [u64],
}

const EQUITY_QTY: &[u64] = &[5, 10, 10, 20, 25, 50, 100, 200];
const EQUITY_TAKE: &[u64] = &[5, 10, 10, 20, 50];
const PRED_QTY: &[u64] = &[1, 2, 5, 5, 10, 10, 20, 50];
const PRED_TAKE: &[u64] = &[1, 2, 5, 10];

const EQUITY: Style = Style {
    min_price: 1,
    max_price: 1_000_000,
    fair_min: 6_000,
    fair_max: 14_000,
    reversion: 48,
    noise: 60,
    jump_every: 400,
    jump: 150,
    levels: 8,
    lifetime: 80,
    far: 60,
    max_live: 48,
    qty: EQUITY_QTY,
    take_qty: EQUITY_TAKE,
};

/// Fair value stays in `8..=92` so a shock of a few cents always moves NO off
/// the complement and every quote still lands in `1..=99`.
const PREDICTION: Style = Style {
    min_price: 1,
    max_price: 99,
    fair_min: 8,
    fair_max: 92,
    reversion: 8,
    noise: 3,
    jump_every: 180,
    jump: 5,
    levels: 4,
    lifetime: 45,
    far: 8,
    max_live: 28,
    qty: PRED_QTY,
    take_qty: PRED_TAKE,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Passive,
    Take,
    Market,
    Cancel,
}

impl Simulator {
    /// Simulator for `config`. The first event is sequence 1 and order id 1.
    pub fn new(config: SimConfig) -> Self {
        let anchor = match config.preset {
            Preset::Equity => 10_000,
            Preset::PredictionMarket => prediction_anchor(config.seed),
        };
        let markets = match config.preset {
            Preset::Equity => vec![MarketSim::new(MarketId::default_market())],
            Preset::PredictionMarket => vec![
                MarketSim::new(MarketId::yes()),
                MarketSim::new(MarketId::no()),
            ],
        };
        Self {
            rng: Rng::new(config.seed),
            remaining: config.orders,
            next_seq: 1,
            next_id: 1,
            preset: config.preset,
            anchor,
            latent: anchor,
            shock: 0,
            shock_left: 0,
            markets,
        }
    }

    fn style(&self) -> &'static Style {
        match self.preset {
            Preset::Equity => &EQUITY,
            Preset::PredictionMarket => &PREDICTION,
        }
    }

    fn next_event(&mut self) -> Event {
        self.step_latent();
        let index = self.pick_market_index();
        let fair = self.fair_for(index);
        let action = self.choose_action(index, fair);
        let command = self.command_for(index, fair, action);
        let seq = self.next_seq;
        let market = self.markets[index].id.clone();
        self.next_seq = self
            .next_seq
            .checked_add(1)
            .expect("sequence number exhausted");
        Event {
            seq,
            market,
            command,
        }
    }

    fn step_latent(&mut self) {
        let style = self.style();
        let fair = i64::try_from(self.latent).expect("latent fair fits in i64");
        let anchor = i64::try_from(self.anchor).expect("anchor fits in i64");
        let pull = (anchor - fair) / style.reversion;
        let noise = self.rng.noise(style.noise);
        let jump = if self.rng.below(style.jump_every) == 0 {
            self.rng.noise(style.jump)
        } else {
            0
        };
        self.latent = clamp_fair(fair + pull + noise + jump, style.fair_min, style.fair_max);
        if self.preset == Preset::PredictionMarket {
            self.step_shock();
        }
    }

    fn step_shock(&mut self) {
        if self.shock_left > 0 {
            self.shock_left -= 1;
            if self.shock_left == 0 {
                self.shock = 0;
            }
            return;
        }
        // Let the pair print a clean complement before the first gap opens.
        if self.next_seq > 30 && self.rng.below(36) == 0 {
            let magnitude = 1 + self.rng.below(4);
            let sign = if self.rng.below(2) == 0 { -1i64 } else { 1 };
            self.shock = sign * i64::try_from(magnitude).expect("shock fits in i64");
            self.shock_left = 16 + self.rng.below(36);
        }
    }

    fn pick_market_index(&mut self) -> usize {
        if self.markets.len() == 1 {
            0
        } else {
            self.rng.below_usize(self.markets.len())
        }
    }

    fn fair_for(&self, index: usize) -> u64 {
        match self.preset {
            Preset::Equity => self.latent,
            Preset::PredictionMarket if index == 0 => self.latent,
            Preset::PredictionMarket => no_fair(self.latent, self.shock),
        }
    }

    fn choose_action(&mut self, index: usize, fair: u64) -> Action {
        let style = self.style();
        let now = self.next_seq;
        let live = &self.markets[index].live;
        let stale = live.iter().any(|order| {
            now.saturating_sub(order.born) >= style.lifetime
                || order.price.abs_diff(fair) >= style.far
        });
        let full = live.len() >= style.max_live;
        let empty = live.is_empty();
        let roll = self.rng.below(100);
        if full {
            Action::Cancel
        } else if empty {
            if roll < 90 {
                Action::Passive
            } else {
                Action::Market
            }
        } else if stale && roll < 70 {
            Action::Cancel
        } else if roll < 10 {
            Action::Market
        } else if roll < 28 {
            Action::Take
        } else if roll < 46 {
            Action::Cancel
        } else {
            Action::Passive
        }
    }

    fn command_for(&mut self, index: usize, fair: u64, action: Action) -> Command {
        let order = match action {
            Action::Cancel => [Action::Cancel, Action::Passive, Action::Market],
            Action::Passive => [Action::Passive, Action::Cancel, Action::Market],
            Action::Take => [Action::Take, Action::Passive, Action::Market],
            Action::Market => [Action::Market, Action::Passive, Action::Cancel],
        };
        for candidate in order {
            if let Some(command) = self.try_action(index, fair, candidate) {
                return command;
            }
        }
        unreachable!("a market order can always be emitted");
    }

    fn try_action(&mut self, index: usize, fair: u64, action: Action) -> Option<Command> {
        match action {
            Action::Cancel => self.cancel_command(index, fair),
            Action::Passive => self.passive_command(index, fair),
            Action::Take => self.take_command(index, fair),
            Action::Market => Some(self.market_command(index)),
        }
    }

    fn cancel_command(&mut self, index: usize, fair: u64) -> Option<Command> {
        let now = self.next_seq;
        let needle = self.rng.next_u64();
        let slot = pick_cancel(&self.markets[index].live, fair, now, needle)?;
        let tracked = self.markets[index].live.swap_remove(slot);
        Some(Command::cancel(tracked.id))
    }

    fn passive_command(&mut self, index: usize, fair: u64) -> Option<Command> {
        let prefer = self.passive_side(index);
        let (side, price) = self.find_passive(fair, prefer)?;
        let qty = Qty(self.pick_qty(false));
        self.resting_limit(index, side, price, qty)
    }

    fn take_command(&mut self, index: usize, fair: u64) -> Option<Command> {
        let side = self.taker_side(index);
        let through = 1 + self.rng.below(3);
        let style = self.style();
        let price = ranged_price(fair, side, through, style.min_price, style.max_price)
            .or_else(|| ranged_price(fair, side, 0, style.min_price, style.max_price))?;
        let qty = Qty(self.pick_qty(true));
        self.resting_limit(index, side, price, qty)
    }

    fn resting_limit(&mut self, index: usize, side: Side, price: u64, qty: Qty) -> Option<Command> {
        let id = self.alloc_id();
        let born = self.next_seq;
        self.markets[index].live.push(Tracked {
            id,
            side,
            price,
            born,
        });
        Some(Command::limit(id, side, Price(price), qty, Timestamp(born)))
    }

    fn market_command(&mut self, index: usize) -> Command {
        let side = self.taker_side(index);
        let qty = Qty(self.pick_qty(true));
        let id = self.alloc_id();
        Command::market(id, side, qty, Timestamp(self.next_seq))
    }

    fn find_passive(&mut self, fair: u64, prefer: Side) -> Option<(Side, u64)> {
        let style = self.style();
        let start = self.inside_distance(style.levels);
        let min_price = style.min_price;
        let max_price = style.max_price;
        let levels = style.levels;
        for side in [prefer, prefer.opposite()] {
            for step in 0..levels {
                let distance = 1 + (start - 1 + step) % levels;
                if let Some(price) = ranged_price(fair, side, distance, min_price, max_price) {
                    return Some((side, price));
                }
            }
        }
        None
    }

    fn inside_distance(&mut self, levels: u64) -> u64 {
        let left = self.rng.below(levels);
        let right = self.rng.below(levels);
        1 + left.min(right)
    }

    fn passive_side(&mut self, index: usize) -> Side {
        let (bids, asks) = self.counts(index);
        if bids < asks {
            Side::Buy
        } else if asks < bids {
            Side::Sell
        } else if self.rng.below(2) == 0 {
            Side::Buy
        } else {
            Side::Sell
        }
    }

    fn taker_side(&mut self, index: usize) -> Side {
        let (bids, asks) = self.counts(index);
        let roll = self.rng.below(100);
        if asks > bids && roll < 65 {
            Side::Buy
        } else if bids > asks && roll < 65 {
            Side::Sell
        } else if roll % 2 == 0 {
            Side::Buy
        } else {
            Side::Sell
        }
    }

    fn counts(&self, index: usize) -> (usize, usize) {
        let mut bids = 0usize;
        let mut asks = 0usize;
        for order in &self.markets[index].live {
            match order.side {
                Side::Buy => bids += 1,
                Side::Sell => asks += 1,
            }
        }
        (bids, asks)
    }

    fn pick_qty(&mut self, take: bool) -> u64 {
        let table = if take {
            self.style().take_qty
        } else {
            self.style().qty
        };
        table[self.rng.below_usize(table.len())]
    }

    fn alloc_id(&mut self) -> OrderId {
        let id = OrderId(self.next_id);
        self.next_id = self.next_id.checked_add(1).expect("order id exhausted");
        id
    }
}

impl MarketSim {
    fn new(id: MarketId) -> Self {
        Self {
            id,
            live: Vec::new(),
        }
    }
}

impl Iterator for Simulator {
    type Item = Event;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        Some(self.next_event())
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = usize::try_from(self.remaining).unwrap_or(usize::MAX);
        (n, Some(n))
    }
}

fn pick_cancel(live: &[Tracked], fair: u64, now: u64, needle: u64) -> Option<usize> {
    if live.is_empty() {
        return None;
    }
    let total = live.iter().fold(0u64, |sum, order| {
        sum.saturating_add(cancel_weight(order, fair, now))
    });
    let mut needle = if total == 0 { 0 } else { needle % total };
    for (index, order) in live.iter().enumerate() {
        let weight = cancel_weight(order, fair, now);
        if needle < weight {
            return Some(index);
        }
        needle -= weight;
    }
    Some(live.len() - 1)
}

fn cancel_weight(order: &Tracked, fair: u64, now: u64) -> u64 {
    let age = now.saturating_sub(order.born).min(20_000) + 1;
    let distance = order.price.abs_diff(fair);
    age.saturating_add(distance.saturating_mul(8)).max(1)
}

fn ranged_price(
    fair: u64,
    side: Side,
    distance: u64,
    min_price: u64,
    max_price: u64,
) -> Option<u64> {
    let price = match side {
        Side::Buy => fair.checked_sub(distance)?,
        Side::Sell => fair.checked_add(distance)?,
    };
    (min_price..=max_price).contains(&price).then_some(price)
}

fn no_fair(yes: u64, shock: i64) -> u64 {
    let yes = i64::try_from(yes).expect("yes price fits in i64");
    let value = 100 - yes + shock;
    u64::try_from(value.clamp(1, 99)).expect("clamped NO price is non-negative")
}

fn clamp_fair(value: i64, min: u64, max: u64) -> u64 {
    let min = i64::try_from(min).expect("fair minimum fits in i64");
    let max = i64::try_from(max).expect("fair maximum fits in i64");
    u64::try_from(value.clamp(min, max)).expect("clamped fair is non-negative")
}

fn prediction_anchor(seed: u64) -> u64 {
    20 + (mix_seed(seed) % 61)
}

fn mix_seed(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
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

    fn below_usize(&mut self, exclusive_end: usize) -> usize {
        debug_assert!(exclusive_end > 0);
        let end = u64::try_from(exclusive_end).expect("length fits in u64");
        usize::try_from(self.below(end)).expect("value is below the length")
    }

    fn noise(&mut self, span: u64) -> i64 {
        if span == 0 {
            return 0;
        }
        let width = span * 2 + 1;
        self.below(width) as i64 - span as i64
    }
}

impl fmt::Debug for Rng {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Rng")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prediction_shocks_open_a_complement_gap_and_stay_bounded() {
        let mut sim = Simulator::new(SimConfig::new(Preset::PredictionMarket, 7, 4_000));
        let mut shocks = 0u64;
        let mut gaps = 0u64;
        for _ in 0..4_000 {
            assert_eq!(sim.shock == 0, sim.shock_left == 0);
            if sim.shock != 0 {
                shocks += 1;
                assert!((1..=4).contains(&sim.shock.unsigned_abs()));
            }
            let event = sim.next().unwrap();
            let complement = 100 - sim.latent;
            if sim.shock == 0 {
                assert_eq!(sim.fair_for(1), complement);
            } else if sim.fair_for(1) != complement {
                gaps += 1;
            }
            if let Command::Limit { price, .. } = event.command {
                assert!(
                    (1..=99).contains(&price.ticks()),
                    "price {} left 1..=99",
                    price.ticks()
                );
            }
            assert!(event.market.as_str() == "YES" || event.market.as_str() == "NO");
        }
        assert!(
            shocks > 0,
            "a 4000-event stream produced no complement shock"
        );
        assert!(gaps > 0, "shocks never moved NO off 100 - YES");
        assert!(sim.next().is_none());
    }

    #[test]
    fn equity_fair_stays_centered_on_the_anchor() {
        let mut sim = Simulator::new(SimConfig::new(Preset::Equity, 3, 3_000));
        for _ in 0..3_000 {
            let event = sim.next().unwrap();
            assert!((6_000..=14_000).contains(&sim.latent));
            assert_eq!(event.market.as_str(), "DEFAULT");
            assert_eq!(sim.shock, 0);
        }
    }
}
