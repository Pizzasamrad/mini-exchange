//! Interactive session for the browser demo.
//!
//! The tape is the deterministic simulator, applied to [`FastEngine`](mini_exchange::FastEngine).
//! In the prediction-market preset, [`ComplementArbStrategy`](mini_exchange::ComplementArbStrategy)
//! trades after each event with the same accounting rules as [`backtest`](mini_exchange::backtest).
//! The digest is the replay FNV-1a of the trades and the final books, so a pure
//! simulator tape matches [`replay::run`](mini_exchange::replay::run).

use std::collections::{BTreeMap, VecDeque};

use mini_exchange::backtest::BacktestConfig;
use mini_exchange::event_log::MarketId;
use mini_exchange::portfolio::{abs_diff_pos, DrawdownTracker, MarkMethod, Portfolio, TopOfBook};
use mini_exchange::replay::{self, ReplayOptions};
use mini_exchange::sim::{generate, Preset, SimConfig};
use mini_exchange::strategy::{
    Action, ActionUpdate, ComplementArbStrategy, LiveOrder, MarketUpdate, Strategy, StrategyIds,
};
use mini_exchange::types::{
    Book, CancelRejectReason, Command, CommandResult, Level, OrderId, Price, Qty, RejectReason,
    Side, Timestamp, Trade,
};
use mini_exchange::{FastEngine, MatchingEngine};

/// Simulator events in one browser session. The id map keeps every accept, so
/// this stays well under what a tab can hold.
const STREAM_EVENTS: u64 = 500_000;
/// Prefix used to show that one seed has one digest, on both engines.
const CHECKPOINT_EVENTS: u64 = 256;
/// Manual ids sit above the simulator and below [`StrategyIds::BASE`].
const MANUAL_BASE: u64 = 1_000_000_000;
/// Timestamps on hand-entered orders. They do not affect price-time priority.
const MANUAL_TS: u64 = 10_000_000;
const DEPTH: usize = 12;
const ORDERS_PER_LEVEL: usize = 4;
const MAX_PRINTS: usize = 240;

const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x100_0000_01B3;

/// Fast/reference fingerprint of the first [`CHECKPOINT_EVENTS`] simulator events.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Checkpoint {
    events: u64,
    fast_digest: u64,
    reference_digest: u64,
    error: Option<String>,
}

impl Checkpoint {
    fn engines_agree(&self) -> bool {
        self.error.is_none() && self.fast_digest == self.reference_digest
    }
}

/// One print from the command that just ran.
#[derive(Debug, Clone)]
struct Print {
    market: String,
    price: u64,
    qty: u64,
    side: &'static str,
    ts: u64,
    source: &'static str,
}

/// A hand-entered order, so the page can list what is still resting.
#[derive(Debug, Clone)]
struct TrackedUser {
    id: OrderId,
    market: MarketId,
    side: Side,
    price: u64,
    original: u64,
}

/// Result of one manual command, before the arb strategy reacts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandView {
    pub kind: &'static str,
    pub id: u64,
    pub status: &'static str,
    pub filled: u64,
    pub resting: u64,
    pub canceled: u64,
    pub reason: Option<&'static str>,
}

/// Live book, optional complement arb, and the running replay digest.
pub struct DemoSession {
    preset: Preset,
    seed: u64,
    sim: mini_exchange::sim::Simulator,
    engines: BTreeMap<MarketId, FastEngine>,
    books: BTreeMap<MarketId, TopOfBook>,
    strategy: Option<ComplementArbStrategy>,
    portfolio: Portfolio,
    ids: StrategyIds,
    drawdown: DrawdownTracker,
    config: BacktestConfig,
    live: Vec<LiveOrder>,
    trade_hash: u64,
    commands: u64,
    sim_events: u64,
    manual_orders: u64,
    trades: u64,
    rejects: u64,
    volume: u128,
    strategy_actions: u64,
    strategy_orders: u64,
    strategy_cancels: u64,
    strategy_fills: u64,
    strategy_volume: u128,
    max_imbalance: u64,
    manual_next: u64,
    user_orders: Vec<TrackedUser>,
    prints: VecDeque<Print>,
    step_commands: u64,
    done: bool,
    halt: Option<String>,
    checkpoint: Checkpoint,
    scripted_digest: u64,
}

fn uses_arb(preset: Preset) -> bool {
    matches!(preset, Preset::PredictionMarket)
}

impl DemoSession {
    /// Session at event 0. The checkpoint replays the seed through both engines.
    pub fn new(preset: Preset, seed: u64) -> Self {
        let checkpoint = seed_checkpoint(preset, seed, CHECKPOINT_EVENTS);
        let scripted = {
            let mut probe = Self::open(preset, seed, uses_arb(preset));
            probe.advance(u32::try_from(CHECKPOINT_EVENTS).unwrap_or(u32::MAX));
            probe.digest()
        };
        let mut session = Self::open(preset, seed, uses_arb(preset));
        session.checkpoint = checkpoint;
        session.scripted_digest = scripted;
        session
    }

    fn open(preset: Preset, seed: u64, arb: bool) -> Self {
        let config = BacktestConfig::default();
        Self {
            preset,
            seed,
            sim: generate(SimConfig::new(preset, seed, STREAM_EVENTS)),
            engines: BTreeMap::new(),
            books: BTreeMap::new(),
            strategy: arb.then(ComplementArbStrategy::default),
            portfolio: Portfolio::new(config.initial_cash),
            ids: StrategyIds::new(),
            drawdown: DrawdownTracker::new(config.initial_cash),
            config,
            live: Vec::new(),
            trade_hash: FNV_OFFSET,
            commands: 0,
            sim_events: 0,
            manual_orders: 0,
            trades: 0,
            rejects: 0,
            volume: 0,
            strategy_actions: 0,
            strategy_orders: 0,
            strategy_cancels: 0,
            strategy_fills: 0,
            strategy_volume: 0,
            max_imbalance: 0,
            manual_next: MANUAL_BASE,
            user_orders: Vec::new(),
            prints: VecDeque::new(),
            step_commands: 0,
            done: false,
            halt: None,
            checkpoint: Checkpoint {
                events: CHECKPOINT_EVENTS,
                fast_digest: 0,
                reference_digest: 0,
                error: Some("checkpoint not computed".to_string()),
            },
            scripted_digest: 0,
        }
    }

    /// Apply up to `n` simulator events and the strategy actions they cause.
    pub fn advance(&mut self, n: u32) {
        let before = self.commands;
        self.prints.clear();
        if self.halt.is_none() {
            for _ in 0..n {
                if self.done {
                    break;
                }
                let Some(event) = self.sim.next() else {
                    self.done = true;
                    break;
                };
                if let Err(err) = self.apply_sim(event) {
                    self.halt = Some(err);
                    break;
                }
            }
        }
        self.step_commands = self.commands.saturating_sub(before);
    }

    /// Limit order from the visitor. Remainder rests. The arb may react after it.
    pub fn submit_limit(
        &mut self,
        market: &str,
        side: &str,
        price: u64,
        qty: u64,
    ) -> Result<CommandView, String> {
        self.prints.clear();
        self.step_commands = 0;
        let before = self.commands;
        if let Some(err) = &self.halt {
            return Err(err.clone());
        }
        let market = parse_market(market)?;
        let side = parse_side(side)?;
        let id = self.alloc_manual()?;
        let seq = MANUAL_TS.saturating_add(self.manual_orders);
        let command = Command::limit(id, side, Price(price), Qty(qty), Timestamp(seq));
        let result = self.execute(&market, command, "manual");
        self.manual_orders = self.manual_orders.saturating_add(1);
        if let CommandResult::Accepted { .. } = result {
            self.user_orders.push(TrackedUser {
                id,
                market: market.clone(),
                side,
                price,
                original: qty,
            });
        }
        let view = view_of("limit", &result);
        if let Err(err) = self.react(seq, &market, &result) {
            self.step_commands = self.commands.saturating_sub(before);
            self.halt = Some(err.clone());
            return Err(err);
        }
        self.step_commands = self.commands.saturating_sub(before);
        Ok(view)
    }

    /// Cancel a resting id on `market`.
    pub fn submit_cancel(&mut self, market: &str, id: u64) -> Result<CommandView, String> {
        self.prints.clear();
        self.step_commands = 0;
        let before = self.commands;
        if let Some(err) = &self.halt {
            return Err(err.clone());
        }
        let market = parse_market(market)?;
        let seq = MANUAL_TS.saturating_add(self.manual_orders);
        let result = self.execute(&market, Command::cancel(OrderId(id)), "manual");
        self.manual_orders = self.manual_orders.saturating_add(1);
        let view = view_of("cancel", &result);
        if let Err(err) = self.react(seq, &market, &result) {
            self.step_commands = self.commands.saturating_sub(before);
            self.halt = Some(err.clone());
            return Err(err);
        }
        self.step_commands = self.commands.saturating_sub(before);
        Ok(view)
    }

    /// JSON snapshot of the books, the tape batch, and the digest.
    pub fn snapshot_json(&self, command: Option<&CommandView>, notice: Option<&str>) -> String {
        let listed = self.market_list();
        let taken: Vec<(MarketId, Book)> = self
            .engines
            .iter()
            .map(|(market, engine)| (market.clone(), engine.book()))
            .collect();
        let digest = finish_hash(self.trade_hash, &taken);
        let matches_script = self.halt.is_none()
            && self.manual_orders == 0
            && self.sim_events == CHECKPOINT_EVENTS
            && digest == self.scripted_digest
            && self.scripted_digest != 0;

        let mut obj = Obj::new();
        obj.string("preset", self.preset.name());
        obj.string("seed", &self.seed.to_string());
        obj.u64("events", self.sim_events);
        obj.u64("streamEvents", STREAM_EVENTS);
        obj.bool("done", self.done);
        obj.u64("commands", self.commands);
        obj.u64("stepCommands", self.step_commands);
        obj.u64("trades", self.trades);
        obj.u128("volume", self.volume);
        obj.u64("rejects", self.rejects);
        obj.u64("manualOrders", self.manual_orders);
        obj.string("digest", &hex(digest));
        obj.u64("scriptedEvents", CHECKPOINT_EVENTS);
        obj.string("scriptedDigest", &hex(self.scripted_digest));
        obj.bool("digestMatchesScript", matches_script);
        obj.raw_key("checkpoint");
        {
            let mut cp = Obj::new();
            cp.u64("events", self.checkpoint.events);
            cp.string("fast", &hex(self.checkpoint.fast_digest));
            cp.string("reference", &hex(self.checkpoint.reference_digest));
            cp.bool("agree", self.checkpoint.engines_agree());
            match &self.checkpoint.error {
                Some(err) => cp.string("error", err),
                None => cp.null("error"),
            }
            obj.embed(&cp.finish());
        }
        match self.halt.as_deref().or(notice) {
            Some(err) => obj.string("error", err),
            None => obj.null("error"),
        }
        obj.raw_key("markets");
        {
            let mut arr = Arr::new();
            for market in &listed {
                let book = taken
                    .iter()
                    .find(|(id, _)| id == market)
                    .map(|(_, book)| book);
                let top = self.books.get(market);
                arr.item_raw(&market_json(market, book, top));
            }
            obj.embed(&arr.finish());
        }
        obj.raw_key("tradesBatch");
        {
            let mut arr = Arr::new();
            for print in &self.prints {
                let mut row = Obj::new();
                row.string("market", &print.market);
                row.u64("price", print.price);
                row.u64("qty", print.qty);
                row.string("side", print.side);
                row.u64("ts", print.ts);
                row.string("source", print.source);
                arr.item_raw(&row.finish());
            }
            obj.embed(&arr.finish());
        }
        obj.raw_key("working");
        {
            let mut arr = Arr::new();
            for order in &self.user_orders {
                let Some(resting) = self
                    .engines
                    .get(&order.market)
                    .and_then(|engine| engine.resting_order(order.id))
                else {
                    continue;
                };
                let mut row = Obj::new();
                row.string("id", &order.id.get().to_string());
                row.string("market", order.market.as_str());
                row.string("side", side_name(order.side));
                row.u64("price", order.price);
                row.u64("remaining", resting.remaining_qty.get());
                row.u64("original", order.original);
                arr.item_raw(&row.finish());
            }
            obj.embed(&arr.finish());
        }
        obj.raw_key("arb");
        if self.strategy.is_some() {
            obj.embed(&self.arb_json());
        } else {
            obj.embed("null");
        }
        obj.raw_key("lastCommand");
        match command {
            Some(view) => obj.embed(&command_json(view)),
            None => obj.embed("null"),
        }
        obj.finish()
    }

    /// Replay digest of the commands applied so far, including the current books.
    pub fn digest(&self) -> u64 {
        let taken: Vec<(MarketId, Book)> = self
            .engines
            .iter()
            .map(|(market, engine)| (market.clone(), engine.book()))
            .collect();
        finish_hash(self.trade_hash, &taken)
    }

    fn arb_json(&self) -> String {
        let account = self.portfolio.account(&self.books, self.config.mark);
        let arb = self.portfolio.arb_stats();
        let yes = self.portfolio.net(&MarketId::yes());
        let no = self.portfolio.net(&MarketId::no());
        let mut obj = Obj::new();
        obj.bool("enabled", true);
        match account {
            Ok(account) => {
                obj.i128("netPnl", account.net_pnl);
                obj.i128("realizedPnl", account.realized_pnl);
                obj.i128("unrealizedPnl", account.unrealized_pnl);
                obj.i128("equity", account.equity);
                obj.i128("cash", account.cash);
                obj.i128("feesPaid", account.fees_paid);
                obj.i128("inventory", account.inventory_value);
            }
            Err(_) => {
                obj.null("netPnl");
                obj.null("realizedPnl");
                obj.null("unrealizedPnl");
                obj.null("equity");
                obj.null("cash");
                obj.null("feesPaid");
                obj.null("inventory");
            }
        }
        obj.i128("drawdownTicks", self.drawdown.max_drawdown_ticks());
        obj.u64("drawdownBps", self.drawdown.max_drawdown_bps());
        obj.i128("peakEquity", self.drawdown.peak());
        obj.u64("pairedQty", arb.qty);
        match arb.avg_edge_ticks() {
            Some(edge) => obj.i128("avgEdgeTicks", edge),
            None => obj.null("avgEdgeTicks"),
        }
        match arb.win_rate_bps() {
            Some(bps) => obj.u64("winRateBps", bps),
            None => obj.null("winRateBps"),
        }
        obj.i64("yesNet", yes);
        obj.i64("noNet", no);
        obj.u64("imbalance", abs_diff_pos(yes, no));
        obj.u64("maxImbalance", self.max_imbalance);
        obj.u64("actions", self.strategy_actions);
        obj.u64("orders", self.strategy_orders);
        obj.u64("cancels", self.strategy_cancels);
        obj.u64("fills", self.strategy_fills);
        obj.u128("strategyVolume", self.strategy_volume);
        obj.finish()
    }

    fn market_list(&self) -> Vec<MarketId> {
        match self.preset {
            Preset::Equity => vec![MarketId::default_market()],
            Preset::PredictionMarket => vec![MarketId::yes(), MarketId::no()],
        }
    }

    fn apply_sim(&mut self, event: mini_exchange::event_log::Event) -> Result<(), String> {
        if StrategyIds::contains(event.command.order_id()) {
            return Err(format!(
                "simulator id {} collided with the strategy id space",
                event.command.order_id().get()
            ));
        }
        let market = event.market.clone();
        let seq = event.seq;
        let result = self.execute(&market, event.command, "sim");
        self.react(seq, &market, &result)?;
        self.sim_events = self.sim_events.saturating_add(1);
        self.note_imbalance();
        Ok(())
    }

    fn react(&mut self, seq: u64, market: &MarketId, result: &CommandResult) -> Result<(), String> {
        if self.strategy.is_none() {
            self.refresh(market, last_price(result));
            return Ok(());
        }
        self.account_result(market, result, false)?;
        self.refresh(market, last_price(result));
        self.observe()?;
        let actions = self.pull_actions(seq, market, result)?;
        self.drain(seq, actions)
    }

    fn execute(
        &mut self,
        market: &MarketId,
        command: Command,
        source: &'static str,
    ) -> CommandResult {
        let result = self.engine_mut(market).execute(command);
        self.commands = self.commands.saturating_add(1);
        match &result {
            CommandResult::Accepted { trades, .. } => {
                for trade in trades {
                    observe_trade(&mut self.trade_hash, market, trade);
                    self.trades = self.trades.saturating_add(1);
                    self.volume = self.volume.saturating_add(u128::from(trade.qty.get()));
                    self.push_print(Print {
                        market: market.as_str().to_string(),
                        price: trade.price.ticks(),
                        qty: trade.qty.get(),
                        side: side_name(trade.taker_side),
                        ts: trade.ts.get(),
                        source,
                    });
                }
            }
            CommandResult::Rejected { .. } | CommandResult::CancelRejected { .. } => {
                self.rejects = self.rejects.saturating_add(1);
            }
            CommandResult::Canceled { .. } => {}
        }
        result
    }

    fn push_print(&mut self, print: Print) {
        if self.prints.len() == MAX_PRINTS {
            self.prints.pop_front();
        }
        self.prints.push_back(print);
    }

    fn drain(&mut self, seq: u64, actions: Vec<Action>) -> Result<(), String> {
        let mut queue: VecDeque<Action> = actions.into();
        let limit = self.config.max_actions_per_event;
        let mut budget = limit;
        while let Some(action) = queue.pop_front() {
            if budget == 0 {
                return Err(format!("strategy exceeded {limit} actions on event {seq}"));
            }
            budget -= 1;
            self.strategy_actions = self.strategy_actions.saturating_add(1);
            let result = self.apply_action(seq, &action)?;
            self.observe()?;
            let more = self.pull_followups(seq, &action, &result)?;
            queue.extend(more);
        }
        Ok(())
    }

    fn apply_action(&mut self, seq: u64, action: &Action) -> Result<CommandResult, String> {
        match action {
            Action::Limit {
                market,
                side,
                price,
                qty,
                ioc,
            } => self.submit_strategy_limit(seq, market, *side, *price, *qty, *ioc),
            Action::Market { market, side, qty } => {
                self.submit_strategy_market(seq, market, *side, *qty)
            }
            Action::Cancel { market, id } => self.submit_strategy_cancel(market, *id),
        }
    }

    fn submit_strategy_limit(
        &mut self,
        seq: u64,
        market: &MarketId,
        side: Side,
        price: Price,
        qty: Qty,
        ioc: bool,
    ) -> Result<CommandResult, String> {
        self.strategy_orders = self.strategy_orders.saturating_add(1);
        let id = self.ids.alloc();
        let command = Command::limit(id, side, price, qty, Timestamp(seq));
        let result = self.execute(market, command, "strategy");
        self.account_result(market, &result, true)?;
        let reported = match result {
            CommandResult::Accepted {
                resting_qty,
                order_id,
                filled_qty,
                canceled_qty,
                trades,
            } => {
                if resting_qty.is_zero() {
                    accepted(order_id, trades, filled_qty, Qty::ZERO, canceled_qty)
                } else if ioc {
                    self.cancel_remainder(market, order_id)?;
                    let canceled = canceled_qty
                        .get()
                        .checked_add(resting_qty.get())
                        .ok_or_else(|| "canceled quantity overflowed".to_string())?;
                    accepted(order_id, trades, filled_qty, Qty::ZERO, Qty(canceled))
                } else {
                    self.live.push(LiveOrder {
                        id: order_id,
                        market: market.clone(),
                        side,
                        price,
                        remaining: resting_qty.get(),
                    });
                    accepted(order_id, trades, filled_qty, resting_qty, canceled_qty)
                }
            }
            other => other,
        };
        self.refresh(market, last_price(&reported));
        Ok(reported)
    }

    fn submit_strategy_market(
        &mut self,
        seq: u64,
        market: &MarketId,
        side: Side,
        qty: Qty,
    ) -> Result<CommandResult, String> {
        self.strategy_orders = self.strategy_orders.saturating_add(1);
        let id = self.ids.alloc();
        let command = Command::market(id, side, qty, Timestamp(seq));
        let result = self.execute(market, command, "strategy");
        self.account_result(market, &result, true)?;
        self.refresh(market, last_price(&result));
        Ok(result)
    }

    fn submit_strategy_cancel(
        &mut self,
        market: &MarketId,
        id: OrderId,
    ) -> Result<CommandResult, String> {
        if !StrategyIds::contains(id) {
            return Err(format!("strategy tried to cancel foreign id {}", id.get()));
        }
        self.strategy_cancels = self.strategy_cancels.saturating_add(1);
        let result = self.execute(market, Command::cancel(id), "strategy");
        if matches!(result, CommandResult::Canceled { .. }) {
            self.live.retain(|order| order.id != id);
        }
        self.refresh(market, None);
        Ok(result)
    }

    fn cancel_remainder(&mut self, market: &MarketId, id: OrderId) -> Result<(), String> {
        let result = self.execute(market, Command::cancel(id), "strategy");
        match result {
            CommandResult::Canceled { .. } => Ok(()),
            other => Err(format!(
                "immediate-or-cancel remainder for {} did not cancel: {other:?}",
                id.get()
            )),
        }
    }

    fn account_result(
        &mut self,
        market: &MarketId,
        result: &CommandResult,
        taker_ours: bool,
    ) -> Result<(), String> {
        let CommandResult::Accepted { trades, .. } = result else {
            return Ok(());
        };
        for trade in trades {
            let maker_ours = StrategyIds::contains(trade.maker_id);
            let taker_is_ours = taker_ours || StrategyIds::contains(trade.taker_id);
            if taker_is_ours {
                self.apply_fill(market, trade.taker_side, trade, self.config.taker_fee)?;
            }
            if maker_ours {
                self.apply_fill(
                    market,
                    trade.taker_side.opposite(),
                    trade,
                    self.config.maker_fee,
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
    ) -> Result<(), String> {
        self.portfolio
            .apply_fill(market, side, trade.price, trade.qty, fee)
            .map_err(|_| "portfolio arithmetic overflowed".to_string())?;
        self.strategy_fills = self
            .strategy_fills
            .checked_add(1)
            .ok_or_else(|| "fill count overflowed".to_string())?;
        self.strategy_volume = self
            .strategy_volume
            .checked_add(u128::from(trade.qty.get()))
            .ok_or_else(|| "strategy volume overflowed".to_string())?;
        Ok(())
    }

    fn reduce_live(&mut self, id: OrderId, qty: u64) {
        if let Some(order) = self.live.iter_mut().find(|order| order.id == id) {
            order.remaining = order.remaining.saturating_sub(qty);
        }
        self.live.retain(|order| order.remaining > 0);
    }

    fn note_imbalance(&mut self) {
        if self.strategy.is_none() {
            return;
        }
        let gap = abs_diff_pos(
            self.portfolio.net(&MarketId::yes()),
            self.portfolio.net(&MarketId::no()),
        );
        if gap > self.max_imbalance {
            self.max_imbalance = gap;
        }
    }

    fn observe(&mut self) -> Result<(), String> {
        let account = self
            .portfolio
            .account(&self.books, MarkMethod::Mid)
            .map_err(|_| "portfolio arithmetic overflowed".to_string())?;
        self.drawdown.observe(account.equity);
        Ok(())
    }

    fn pull_actions(
        &mut self,
        seq: u64,
        market: &MarketId,
        result: &CommandResult,
    ) -> Result<Vec<Action>, String> {
        let Some(strategy) = self.strategy.as_mut() else {
            return Ok(Vec::new());
        };
        let account = self
            .portfolio
            .account(&self.books, self.config.mark)
            .map_err(|_| "portfolio arithmetic overflowed".to_string())?;
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
        Ok(strategy.on_market(&update))
    }

    fn pull_followups(
        &mut self,
        seq: u64,
        action: &Action,
        result: &CommandResult,
    ) -> Result<Vec<Action>, String> {
        let Some(strategy) = self.strategy.as_mut() else {
            return Ok(Vec::new());
        };
        let account = self
            .portfolio
            .account(&self.books, self.config.mark)
            .map_err(|_| "portfolio arithmetic overflowed".to_string())?;
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
        Ok(strategy.on_action(&update))
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

    fn engine_mut(&mut self, market: &MarketId) -> &mut FastEngine {
        if !self.engines.contains_key(market) {
            self.engines.insert(market.clone(), FastEngine::new());
        }
        self.engines
            .get_mut(market)
            .expect("market engine exists after insert")
    }

    fn alloc_manual(&mut self) -> Result<OrderId, String> {
        let id = OrderId(self.manual_next);
        self.manual_next = self
            .manual_next
            .checked_add(1)
            .ok_or_else(|| "manual order id space exhausted".to_string())?;
        Ok(id)
    }

    #[cfg(test)]
    fn best_ask_qty(&self, market: &str) -> Option<(u64, u64)> {
        let id = MarketId::parse(market).ok()?;
        self.engines
            .get(&id)
            .and_then(MatchingEngine::best_ask)
            .map(|quote| (quote.price.ticks(), quote.qty.get()))
    }

    #[cfg(test)]
    fn best_bid_qty(&self, market: &str) -> Option<(u64, u64)> {
        let id = MarketId::parse(market).ok()?;
        self.engines
            .get(&id)
            .and_then(MatchingEngine::best_bid)
            .map(|quote| (quote.price.ticks(), quote.qty.get()))
    }
}

fn seed_checkpoint(preset: Preset, seed: u64, events: u64) -> Checkpoint {
    let stream: Vec<_> = generate(SimConfig::new(preset, seed, events)).collect();
    match replay::verify(stream.iter(), ReplayOptions::new(0)) {
        Ok(verified) => Checkpoint {
            events,
            fast_digest: verified.fast.digest,
            reference_digest: verified.reference.digest,
            error: None,
        },
        Err(err) => Checkpoint {
            events,
            fast_digest: 0,
            reference_digest: 0,
            error: Some(truncate(&err.to_string(), 400)),
        },
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        text.to_string()
    } else {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &text[..end])
    }
}

fn last_price(result: &CommandResult) -> Option<Price> {
    match result {
        CommandResult::Accepted { trades, .. } => trades.last().map(|trade| trade.price),
        _ => None,
    }
}

fn accepted(
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

fn view_of(kind: &'static str, result: &CommandResult) -> CommandView {
    match result {
        CommandResult::Accepted {
            order_id,
            filled_qty,
            resting_qty,
            canceled_qty,
            ..
        } => CommandView {
            kind,
            id: order_id.get(),
            status: "accepted",
            filled: filled_qty.get(),
            resting: resting_qty.get(),
            canceled: canceled_qty.get(),
            reason: None,
        },
        CommandResult::Rejected { order_id, reason } => CommandView {
            kind,
            id: order_id.get(),
            status: "rejected",
            filled: 0,
            resting: 0,
            canceled: 0,
            reason: Some(reject_name(*reason)),
        },
        CommandResult::Canceled {
            order_id,
            canceled_qty,
        } => CommandView {
            kind,
            id: order_id.get(),
            status: "canceled",
            filled: 0,
            resting: 0,
            canceled: canceled_qty.get(),
            reason: None,
        },
        CommandResult::CancelRejected { order_id, reason } => CommandView {
            kind,
            id: order_id.get(),
            status: "cancel_rejected",
            filled: 0,
            resting: 0,
            canceled: 0,
            reason: Some(cancel_name(*reason)),
        },
    }
}

fn reject_name(reason: RejectReason) -> &'static str {
    match reason {
        RejectReason::ZeroQuantity => "zero_quantity",
        RejectReason::DuplicateOrderId => "duplicate_order_id",
    }
}

fn cancel_name(reason: CancelRejectReason) -> &'static str {
    match reason {
        CancelRejectReason::NotFound => "not_found",
        CancelRejectReason::AlreadyFilled => "already_filled",
        CancelRejectReason::AlreadyCanceled => "already_canceled",
    }
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

fn parse_side(text: &str) -> Result<Side, String> {
    match text.trim().to_ascii_lowercase().as_str() {
        "buy" | "bid" => Ok(Side::Buy),
        "sell" | "ask" => Ok(Side::Sell),
        _ => Err("side must be buy or sell".to_string()),
    }
}

fn parse_market(text: &str) -> Result<MarketId, String> {
    MarketId::parse(text.trim()).map_err(|err| err.to_string())
}

fn hex(value: u64) -> String {
    format!("{value:016x}")
}

fn mix(hash: &mut u64, value: u64) {
    *hash = (*hash ^ value).wrapping_mul(FNV_PRIME);
}

fn mix_str(hash: &mut u64, text: &str) {
    mix(hash, u64::try_from(text.len()).unwrap_or(u64::MAX));
    for byte in text.bytes() {
        mix(hash, u64::from(byte));
    }
}

fn side_code(side: Side) -> u64 {
    match side {
        Side::Buy => 1,
        Side::Sell => 2,
    }
}

fn observe_trade(hash: &mut u64, market: &MarketId, trade: &Trade) {
    mix(hash, 1);
    mix_str(hash, market.as_str());
    mix(hash, trade.maker_id.get());
    mix(hash, trade.taker_id.get());
    mix(hash, trade.price.ticks());
    mix(hash, trade.qty.get());
    mix(hash, side_code(trade.taker_side));
    mix(hash, trade.ts.get());
}

fn observe_book(hash: &mut u64, market: &MarketId, book: &Book) {
    mix(hash, 2);
    mix_str(hash, market.as_str());
    observe_levels(hash, &book.bids);
    observe_levels(hash, &book.asks);
}

fn observe_levels(hash: &mut u64, levels: &[Level]) {
    mix(hash, u64::try_from(levels.len()).unwrap_or(u64::MAX));
    for level in levels {
        mix(hash, level.price.ticks());
        mix(hash, level.total_qty.get());
        mix(hash, u64::try_from(level.orders.len()).unwrap_or(u64::MAX));
        for order in &level.orders {
            mix(hash, order.id.get());
            mix(hash, side_code(order.side));
            mix(hash, order.price.ticks());
            mix(hash, order.remaining_qty.get());
            mix(hash, order.original_qty.get());
            mix(hash, order.ts.get());
        }
    }
}

fn finish_hash(trade_hash: u64, books: &[(MarketId, Book)]) -> u64 {
    let mut hash = trade_hash;
    for (market, book) in books {
        observe_book(&mut hash, market, book);
    }
    mix(&mut hash, u64::MAX);
    hash
}

fn market_json(market: &MarketId, book: Option<&Book>, top: Option<&TopOfBook>) -> String {
    let mut obj = Obj::new();
    obj.string("symbol", market.as_str());
    let bid = top.and_then(|book| book.bid);
    let ask = top.and_then(|book| book.ask);
    let last = top.and_then(|book| book.last);
    match bid {
        Some(quote) => {
            obj.u64("bestBid", quote.price.ticks());
            obj.u64("bestBidQty", quote.qty.get());
        }
        None => {
            obj.null("bestBid");
            obj.null("bestBidQty");
        }
    }
    match ask {
        Some(quote) => {
            obj.u64("bestAsk", quote.price.ticks());
            obj.u64("bestAskQty", quote.qty.get());
        }
        None => {
            obj.null("bestAsk");
            obj.null("bestAskQty");
        }
    }
    match (bid, ask) {
        (Some(bid), Some(ask)) => {
            obj.u64(
                "spread",
                ask.price.ticks().saturating_sub(bid.price.ticks()),
            );
        }
        _ => obj.null("spread"),
    }
    match last {
        Some(price) => obj.u64("last", price.ticks()),
        None => obj.null("last"),
    }
    let asks = book.map(|book| book.asks.as_slice());
    let bids = book.map(|book| book.bids.as_slice());
    obj.raw_key("asks");
    obj.embed(&levels_json(asks, true));
    obj.raw_key("bids");
    obj.embed(&levels_json(bids, false));
    obj.finish()
}

fn levels_json(levels: Option<&[Level]>, reverse_best: bool) -> String {
    let mut arr = Arr::new();
    let Some(levels) = levels else {
        return arr.finish();
    };
    let mut visible: Vec<&Level> = levels.iter().take(DEPTH).collect();
    if reverse_best {
        visible.reverse();
    }
    for level in visible {
        let mut row = Obj::new();
        row.u64("price", level.price.ticks());
        row.u64("qty", level.total_qty.get());
        row.u64(
            "orderCount",
            u64::try_from(level.orders.len()).unwrap_or(u64::MAX),
        );
        row.raw_key("orders");
        {
            let mut orders = Arr::new();
            for order in level.orders.iter().take(ORDERS_PER_LEVEL) {
                let mut item = Obj::new();
                item.string("id", &order.id.get().to_string());
                item.u64("qty", order.remaining_qty.get());
                orders.item_raw(&item.finish());
            }
            row.embed(&orders.finish());
        }
        arr.item_raw(&row.finish());
    }
    arr.finish()
}

fn command_json(view: &CommandView) -> String {
    let mut obj = Obj::new();
    obj.string("kind", view.kind);
    obj.string("id", &view.id.to_string());
    obj.string("status", view.status);
    obj.u64("filled", view.filled);
    obj.u64("resting", view.resting);
    obj.u64("canceled", view.canceled);
    match view.reason {
        Some(reason) => obj.string("reason", reason),
        None => obj.null("reason"),
    }
    obj.finish()
}

struct Obj {
    buf: String,
    sep: bool,
}

impl Obj {
    fn new() -> Self {
        Self {
            buf: String::from('{'),
            sep: false,
        }
    }

    fn raw_key(&mut self, key: &str) {
        self.comma();
        push_string(&mut self.buf, key);
        self.buf.push(':');
    }

    fn embed(&mut self, raw: &str) {
        self.buf.push_str(raw);
    }

    fn string(&mut self, key: &str, value: &str) {
        self.raw_key(key);
        push_string(&mut self.buf, value);
    }

    fn u64(&mut self, key: &str, value: u64) {
        self.raw_key(key);
        self.buf.push_str(&value.to_string());
    }

    fn u128(&mut self, key: &str, value: u128) {
        self.raw_key(key);
        push_int(&mut self.buf, &value.to_string());
    }

    fn i64(&mut self, key: &str, value: i64) {
        self.raw_key(key);
        self.buf.push_str(&value.to_string());
    }

    fn i128(&mut self, key: &str, value: i128) {
        self.raw_key(key);
        push_int(&mut self.buf, &value.to_string());
    }

    fn bool(&mut self, key: &str, value: bool) {
        self.raw_key(key);
        self.buf.push_str(if value { "true" } else { "false" });
    }

    fn null(&mut self, key: &str) {
        self.raw_key(key);
        self.buf.push_str("null");
    }

    fn comma(&mut self) {
        if self.sep {
            self.buf.push(',');
        }
        self.sep = true;
    }

    fn finish(mut self) -> String {
        self.buf.push('}');
        self.buf
    }
}

struct Arr {
    buf: String,
    sep: bool,
}

impl Arr {
    fn new() -> Self {
        Self {
            buf: String::from('['),
            sep: false,
        }
    }

    fn item_raw(&mut self, raw: &str) {
        if self.sep {
            self.buf.push(',');
        }
        self.sep = true;
        self.buf.push_str(raw);
    }

    fn finish(mut self) -> String {
        self.buf.push(']');
        self.buf
    }
}

fn push_string(buf: &mut String, text: &str) {
    buf.push('"');
    for ch in text.chars() {
        match ch {
            '"' => buf.push_str("\\\""),
            '\\' => buf.push_str("\\\\"),
            '\n' => buf.push_str("\\n"),
            '\r' => buf.push_str("\\r"),
            '\t' => buf.push_str("\\t"),
            ch if ch.is_control() => buf.push_str(&format!("\\u{:04x}", u32::from(ch))),
            ch => buf.push(ch),
        }
    }
    buf.push('"');
}

/// Numbers past the JS safe integer range travel as strings.
fn push_int(buf: &mut String, digits: &str) {
    let magnitude = digits.trim_start_matches('-');
    if magnitude.len() < 16 {
        buf.push_str(digits);
        return;
    }
    // 2^53 - 1 is 16 digits. Compare the digit string when lengths match.
    if magnitude.len() == 16 && magnitude <= "9007199254740991" && !digits.starts_with('-') {
        buf.push_str(digits);
        return;
    }
    if digits.starts_with('-') && magnitude.len() == 16 && magnitude <= "9007199254740991" {
        buf.push_str(digits);
        return;
    }
    push_string(buf, digits);
}

#[cfg(test)]
mod tests {
    use super::*;
    use mini_exchange::backtest;
    use mini_exchange::strategy::ComplementArbStrategy;

    fn equity(seed: u64) -> DemoSession {
        DemoSession::open(Preset::Equity, seed, false)
    }

    #[test]
    fn equity_digest_matches_replay() {
        let mut demo = equity(7);
        demo.advance(1_000);
        let events: Vec<_> = generate(SimConfig::new(Preset::Equity, 7, 1_000)).collect();
        let report = replay::run(events, |_| FastEngine::new(), ReplayOptions::new(0));
        assert_eq!(demo.digest(), report.digest);
        assert_eq!(demo.trades, report.trades);
        assert_eq!(demo.sim_events, report.commands);
    }

    #[test]
    fn prediction_without_strategy_matches_replay() {
        let mut demo = DemoSession::open(Preset::PredictionMarket, 4, false);
        demo.advance(600);
        let events: Vec<_> = generate(SimConfig::new(Preset::PredictionMarket, 4, 600)).collect();
        let report = replay::run(events, |_| FastEngine::new(), ReplayOptions::new(0));
        assert_eq!(demo.digest(), report.digest);
        assert_eq!(demo.trades, report.trades);
    }

    #[test]
    fn prediction_arb_matches_backtest() {
        let n = 1_500u64;
        let events: Vec<_> = generate(SimConfig::new(Preset::PredictionMarket, 1, n)).collect();
        let report = backtest::run(
            events,
            |_| FastEngine::new(),
            ComplementArbStrategy::default(),
            BacktestConfig::default(),
        )
        .expect("backtest");
        let mut demo = DemoSession::open(Preset::PredictionMarket, 1, true);
        demo.advance(u32::try_from(n).unwrap());
        let json: serde_json::Value =
            serde_json::from_str(&demo.snapshot_json(None, None)).expect("json");
        let arb = &json["arb"];
        assert_eq!(arb["netPnl"].as_i64().unwrap() as i128, report.net_pnl);
        assert_eq!(
            arb["realizedPnl"].as_i64().unwrap() as i128,
            report.realized_pnl
        );
        assert_eq!(
            arb["unrealizedPnl"].as_i64().unwrap() as i128,
            report.unrealized_pnl
        );
        assert_eq!(arb["equity"].as_i64().unwrap() as i128, report.final_equity);
        assert_eq!(arb["pairedQty"].as_u64().unwrap(), report.paired_qty);
        assert_eq!(
            arb["drawdownTicks"].as_i64().unwrap() as i128,
            report.max_drawdown_ticks
        );
        assert_eq!(
            arb["drawdownBps"].as_u64().unwrap(),
            report.max_drawdown_bps
        );
        assert_eq!(arb["fills"].as_u64().unwrap(), report.strategy_fills);
        assert_eq!(arb["orders"].as_u64().unwrap(), report.strategy_orders);
        assert_eq!(arb["actions"].as_u64().unwrap(), report.strategy_actions);
        assert_eq!(
            arb["maxImbalance"].as_u64().unwrap(),
            report.max_leg_imbalance
        );
        assert!(report.paired_qty > 0, "expected the arb to lock a pair");
    }

    #[test]
    fn two_sessions_share_a_digest() {
        let mut left = DemoSession::open(Preset::PredictionMarket, 9, true);
        let mut right = DemoSession::open(Preset::PredictionMarket, 9, true);
        left.advance(400);
        right.advance(400);
        assert_eq!(left.digest(), right.digest());
        assert_eq!(left.trades, right.trades);
    }

    #[test]
    fn checkpoint_agrees_and_scripted_run_matches() {
        let demo = DemoSession::new(Preset::Equity, 3);
        assert!(demo.checkpoint.engines_agree());
        let events: Vec<_> =
            generate(SimConfig::new(Preset::Equity, 3, CHECKPOINT_EVENTS)).collect();
        let report = replay::run(events, |_| FastEngine::new(), ReplayOptions::new(0));
        assert_eq!(demo.checkpoint.fast_digest, report.digest);
        assert_eq!(demo.scripted_digest, report.digest);

        let mut live = DemoSession::new(Preset::Equity, 3);
        live.advance(u32::try_from(CHECKPOINT_EVENTS).unwrap());
        assert_eq!(live.digest(), demo.scripted_digest);
        let json: serde_json::Value =
            serde_json::from_str(&live.snapshot_json(None, None)).unwrap();
        assert_eq!(json["digestMatchesScript"], true);
        assert_eq!(json["checkpoint"]["agree"], true);
    }

    #[test]
    fn manual_cross_and_cancel_are_deterministic() {
        let mut left = equity(11);
        let mut right = equity(11);
        for _ in 0..40 {
            left.advance(25);
            right.advance(25);
            if left.best_ask_qty("DEFAULT").is_some() && left.best_bid_qty("DEFAULT").is_some() {
                break;
            }
        }
        let (ask, qty) = left.best_ask_qty("DEFAULT").expect("ask");
        let crossed = left
            .submit_limit("DEFAULT", "buy", ask, qty.max(1))
            .expect("cross");
        let again = right
            .submit_limit("DEFAULT", "buy", ask, qty.max(1))
            .expect("cross");
        assert_eq!(crossed.status, "accepted");
        assert!(crossed.filled > 0);
        assert_eq!(crossed, again);
        assert_eq!(left.digest(), right.digest());

        let mut plain = equity(11);
        plain.advance(u32::try_from(left.sim_events).unwrap());
        assert_ne!(plain.digest(), left.digest());

        let (bid, _) = left.best_bid_qty("DEFAULT").expect("bid");
        let rested = left
            .submit_limit("DEFAULT", "buy", bid.saturating_sub(1).max(1), 4)
            .expect("rest");
        assert_eq!(rested.status, "accepted");
        assert!(rested.resting > 0);
        let canceled = left.submit_cancel("DEFAULT", rested.id).expect("cancel");
        assert_eq!(canceled.status, "canceled");
        assert!(canceled.canceled > 0);

        let rejected = left
            .submit_limit("DEFAULT", "buy", 10_000, 0)
            .expect("zero qty");
        assert_eq!(rejected.status, "rejected");
        assert_eq!(rejected.reason, Some("zero_quantity"));
    }

    #[test]
    fn snapshot_json_is_an_object() {
        let demo = DemoSession::new(Preset::PredictionMarket, 2);
        let json: serde_json::Value =
            serde_json::from_str(&demo.snapshot_json(None, None)).unwrap();
        assert_eq!(json["preset"], "prediction-market");
        assert_eq!(json["events"], 0);
        assert_eq!(json["markets"].as_array().unwrap().len(), 2);
        assert!(json["checkpoint"]["agree"].as_bool().unwrap());
        assert!(json["arb"].is_object());
    }
}
