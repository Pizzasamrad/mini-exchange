//! Replay an event stream through a strategy and print the PnL report.
//!
//! ```text
//! cargo run --release --bin backtest -- --preset prediction-market --orders 100000 --seed 1 --strategy complement-arb --verify
//! cargo run --release --bin backtest -- --preset equity --orders 100000 --seed 1 --strategy market-maker
//! cargo run --release --bin backtest -- --input pred.log --strategy complement-arb --fee-ticks 1 --min-edge 1
//! ```

#![forbid(unsafe_code)]

use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;
use std::time::Instant;

use mini_exchange::backtest::{self, BacktestConfig, BacktestError, BacktestReport};
use mini_exchange::event_log::{EventReader, ReadError};
use mini_exchange::sim::{generate, Preset, SimConfig};
use mini_exchange::strategy::{
    Action, ActionUpdate, ComplementArbConfig, ComplementArbStrategy, MarketMakerConfig,
    MarketUpdate, Strategy, TopOfBookMarketMaker,
};
use mini_exchange::{FastEngine, ReferenceEngine};

fn main() -> ExitCode {
    match Cli::parse(std::env::args().skip(1)) {
        Ok(Cli::Help) => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        Ok(Cli::Run(args)) => run(&args),
        Err(message) => {
            eprintln!("{message}");
            eprintln!("Run with --help for usage.");
            ExitCode::from(2)
        }
    }
}

fn run(args: &RunArgs) -> ExitCode {
    let started = Instant::now();
    match execute(args) {
        Ok(report) => {
            print_report(&report, args, started.elapsed().as_secs_f64());
            ExitCode::SUCCESS
        }
        Err(err) => {
            let secs = started.elapsed().as_secs_f64();
            eprintln!("{err}");
            println!();
            println!("mini-exchange backtest");
            println!("  strategy:         {}", args.strategy);
            println!("  engine:           {}", args.engine);
            println!("  source:           {}", source(args));
            println!("  elapsed:          {secs:.3}s");
            println!(
                "  verify:           {}",
                if args.verify { "fail" } else { "off" }
            );
            println!("  status:           FAIL");
            ExitCode::from(1)
        }
    }
}

fn execute(args: &RunArgs) -> Result<BacktestReport, BacktestError> {
    let config = config(args);
    if args.verify {
        let fast = run_once(args, EngineName::Fast, config)?;
        let reference = run_once(args, EngineName::Reference, config)?;
        if fast != reference {
            return Err(BacktestError::Mismatch {
                detail: format!(
                    "digest {:016x} != {:016x}, net pnl {} != {}, fills {} != {}",
                    fast.digest,
                    reference.digest,
                    fast.net_pnl,
                    reference.net_pnl,
                    fast.strategy_fills,
                    reference.strategy_fills
                ),
            });
        }
        Ok(match args.engine {
            EngineName::Fast => fast,
            EngineName::Reference => reference,
        })
    } else {
        run_once(args, args.engine, config)
    }
}

fn run_once(
    args: &RunArgs,
    engine: EngineName,
    config: BacktestConfig,
) -> Result<BacktestReport, BacktestError> {
    let strategy = build_strategy(args);
    match (&args.input, args.preset) {
        (Some(path), None) => {
            let reader = open_reader(path)?;
            match engine {
                EngineName::Fast => {
                    backtest::run_reader(reader, |_| FastEngine::new(), strategy, config)
                }
                EngineName::Reference => {
                    backtest::run_reader(reader, |_| ReferenceEngine::new(), strategy, config)
                }
            }
        }
        (None, Some(preset)) => {
            let events = generate(SimConfig::new(preset, args.seed, args.orders));
            match engine {
                EngineName::Fast => backtest::run(events, |_| FastEngine::new(), strategy, config),
                EngineName::Reference => {
                    backtest::run(events, |_| ReferenceEngine::new(), strategy, config)
                }
            }
        }
        _ => Err(BacktestError::Accounting(
            "pass either --input or --preset".to_string(),
        )),
    }
}

fn open_reader(path: &PathBuf) -> Result<EventReader<BufReader<File>>, BacktestError> {
    let file = File::open(path).map_err(|err| BacktestError::Read(ReadError::Io(err)))?;
    Ok(EventReader::new(BufReader::with_capacity(128 * 1024, file)))
}

fn config(args: &RunArgs) -> BacktestConfig {
    BacktestConfig {
        initial_cash: args.cash,
        taker_fee: args.fee_ticks,
        maker_fee: args.fee_ticks,
        ..BacktestConfig::default()
    }
}

fn build_strategy(args: &RunArgs) -> AnyStrategy {
    let room = args.max_qty.saturating_mul(10).max(args.max_qty);
    match args.strategy {
        StrategyName::ComplementArb => {
            AnyStrategy::Arb(ComplementArbStrategy::new(ComplementArbConfig {
                min_edge_ticks: args.min_edge,
                max_position: room,
                max_order_qty: args.max_qty,
                max_unhedged: 0,
                allow_unwind: true,
                ..ComplementArbConfig::default()
            }))
        }
        StrategyName::MarketMaker => {
            AnyStrategy::Maker(TopOfBookMarketMaker::new(MarketMakerConfig {
                max_position: room,
                order_qty: args.max_qty,
                min_spread: 1,
                market: None,
            }))
        }
    }
}

enum AnyStrategy {
    Arb(ComplementArbStrategy),
    Maker(TopOfBookMarketMaker),
}

impl Strategy for AnyStrategy {
    fn on_market(&mut self, update: &MarketUpdate<'_>) -> Vec<Action> {
        match self {
            Self::Arb(strategy) => strategy.on_market(update),
            Self::Maker(strategy) => strategy.on_market(update),
        }
    }

    fn on_action(&mut self, update: &ActionUpdate<'_>) -> Vec<Action> {
        match self {
            Self::Arb(strategy) => strategy.on_action(update),
            Self::Maker(strategy) => strategy.on_action(update),
        }
    }
}

fn print_report(report: &BacktestReport, args: &RunArgs, secs: f64) {
    let passes = if args.verify { 2 } else { 1 };
    let rate = per_second(report.events.saturating_mul(passes), secs);
    println!("mini-exchange backtest");
    println!("  strategy:         {}", args.strategy);
    println!("  engine:           {}", args.engine);
    println!("  source:           {}", source(args));
    println!(
        "  orders replayed:  {}",
        grouped(&report.events.to_string())
    );
    println!(
        "  strategy actions: {}",
        grouped(&report.strategy_actions.to_string())
    );
    println!(
        "  strategy orders:  {}",
        grouped(&report.strategy_orders.to_string())
    );
    println!(
        "  strategy fills:   {} ({})",
        grouped(&report.strategy_fills.to_string()),
        shares(report.strategy_volume)
    );
    println!(
        "  paired arbs:      {}",
        grouped(&report.paired_qty.to_string())
    );
    println!(
        "  round trips:      {} shares closed",
        grouped(&report.closed_qty.to_string())
    );
    println!("  fees paid:        {}", money(report.fees_paid));
    println!("  realized pnl:     {}", money(report.realized_pnl));
    println!("  unrealized pnl:   {}", money(report.unrealized_pnl));
    println!("  net pnl:          {}", money(report.net_pnl));
    println!("  final equity:     {}", money(report.final_equity));
    println!("  peak equity:      {}", money(report.peak_equity));
    println!("  max drawdown:     {}", drawdown(report));
    println!("  win rate:         {}", win_rate(report));
    println!("  avg edge:         {}", edge(report));
    println!("  leg imbalance:    {}", report.max_leg_imbalance);
    println!("  elapsed:          {secs:.3}s");
    println!("  orders/sec:       {}", grouped(&rate.to_string()));
    println!("  digest:           {:016x}", report.digest);
    println!(
        "  verify:           {}",
        if args.verify { "pass" } else { "off" }
    );
    println!("  status:           PASS");
    for pos in &report.positions {
        if pos.net != 0 {
            let avg = match pos.avg_cost_ticks {
                Some(ticks) => ticks.to_string(),
                None => "-".to_string(),
            };
            println!(
                "  book {:<12} net {:+}, open cost {}, avg {avg}",
                pos.market,
                pos.net,
                money(pos.open_cashflow)
            );
        }
    }
}

fn source(args: &RunArgs) -> String {
    match (&args.input, args.preset) {
        (Some(path), _) => path.display().to_string(),
        (None, Some(preset)) => format!("{preset} seed {} orders {}", args.seed, args.orders),
        (None, None) => "-".to_string(),
    }
}

fn drawdown(report: &BacktestReport) -> String {
    let bps = report.max_drawdown_bps;
    let whole = bps / 100;
    let frac = bps % 100;
    format!(
        "{}, {bps} bps ({whole}.{frac:02}%)",
        money(report.max_drawdown_ticks)
    )
}

fn win_rate(report: &BacktestReport) -> String {
    let Some(bps) = report.win_rate_bps else {
        return "-".to_string();
    };
    let whole = bps / 100;
    let frac = bps % 100;
    if report.paired_qty > 0 {
        let total = report
            .pair_wins
            .saturating_add(report.pair_losses)
            .saturating_add(report.pair_scratches);
        format!(
            "{whole}.{frac:02}% ({}/{} pairs)",
            grouped(&report.pair_wins.to_string()),
            grouped(&total.to_string())
        )
    } else {
        let total = report
            .close_wins
            .saturating_add(report.close_losses)
            .saturating_add(report.close_scratches);
        format!(
            "{whole}.{frac:02}% ({}/{} closes)",
            grouped(&report.close_wins.to_string()),
            grouped(&total.to_string())
        )
    }
}

fn edge(report: &BacktestReport) -> String {
    match report.avg_edge_ticks {
        Some(ticks) => format!("{ticks} ticks"),
        None => "-".to_string(),
    }
}

fn money(ticks: i128) -> String {
    let negative = ticks < 0;
    let abs = ticks.unsigned_abs();
    let dollars = abs / 100;
    let cents = abs % 100;
    let sign = if negative { "-" } else { "" };
    format!(
        "{sign}{} ticks ({sign}${}.{:02})",
        grouped(&abs.to_string()),
        grouped(&dollars.to_string()),
        cents
    )
}

fn shares(qty: u128) -> String {
    format!("{} shares", grouped(&qty.to_string()))
}

fn per_second(count: u64, secs: f64) -> u64 {
    if secs > 0.0 {
        ((count as f64) / secs).round().max(0.0) as u64
    } else {
        0
    }
}

fn grouped(digits: &str) -> String {
    let mut out = String::new();
    for (index, ch) in digits.chars().rev().enumerate() {
        if index > 0 && index.is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

struct RunArgs {
    input: Option<PathBuf>,
    preset: Option<Preset>,
    orders: u64,
    seed: u64,
    strategy: StrategyName,
    engine: EngineName,
    min_edge: u64,
    max_qty: u64,
    fee_ticks: u64,
    cash: i128,
    verify: bool,
}

enum Cli {
    Help,
    Run(RunArgs),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EngineName {
    Fast,
    Reference,
}

impl std::fmt::Display for EngineName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fast => f.write_str("fast"),
            Self::Reference => f.write_str("reference"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StrategyName {
    ComplementArb,
    MarketMaker,
}

impl std::fmt::Display for StrategyName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ComplementArb => f.write_str("complement-arb"),
            Self::MarketMaker => f.write_str("market-maker"),
        }
    }
}

impl Cli {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let args: Vec<String> = args.collect();
        if args.is_empty() || args.iter().any(|arg| arg == "-h" || arg == "--help") {
            return Ok(Self::Help);
        }
        Ok(Self::Run(parse_run(&args)?))
    }
}

fn parse_run(args: &[String]) -> Result<RunArgs, String> {
    let mut input = None;
    let mut preset = None;
    let mut orders = 100_000u64;
    let mut seed = 1u64;
    let mut strategy = None;
    let mut engine = EngineName::Fast;
    let mut min_edge = 1u64;
    let mut max_qty = 20u64;
    let mut fee_ticks = 0u64;
    let mut cash = 1_000_000i128;
    let mut verify = false;
    let mut raw = args.iter().cloned();
    while let Some(arg) = raw.next() {
        if arg == "--input" {
            input = Some(PathBuf::from(require(&mut raw, "--input")?));
        } else if let Some(value) = arg.strip_prefix("--input=") {
            input = Some(PathBuf::from(value));
        } else if arg == "--preset" {
            preset = Some(parse_preset(&require(&mut raw, "--preset")?)?);
        } else if let Some(value) = arg.strip_prefix("--preset=") {
            preset = Some(parse_preset(value)?);
        } else if arg == "--orders" {
            orders = number("--orders", &require(&mut raw, "--orders")?)?;
        } else if let Some(value) = arg.strip_prefix("--orders=") {
            orders = number("--orders", value)?;
        } else if arg == "--seed" {
            seed = number("--seed", &require(&mut raw, "--seed")?)?;
        } else if let Some(value) = arg.strip_prefix("--seed=") {
            seed = number("--seed", value)?;
        } else if arg == "--strategy" {
            strategy = Some(parse_strategy(&require(&mut raw, "--strategy")?)?);
        } else if let Some(value) = arg.strip_prefix("--strategy=") {
            strategy = Some(parse_strategy(value)?);
        } else if arg == "--engine" {
            engine = parse_engine(&require(&mut raw, "--engine")?)?;
        } else if let Some(value) = arg.strip_prefix("--engine=") {
            engine = parse_engine(value)?;
        } else if arg == "--min-edge" {
            min_edge = number("--min-edge", &require(&mut raw, "--min-edge")?)?;
        } else if let Some(value) = arg.strip_prefix("--min-edge=") {
            min_edge = number("--min-edge", value)?;
        } else if arg == "--max-qty" {
            max_qty = number("--max-qty", &require(&mut raw, "--max-qty")?)?;
        } else if let Some(value) = arg.strip_prefix("--max-qty=") {
            max_qty = number("--max-qty", value)?;
        } else if arg == "--fee-ticks" {
            fee_ticks = number("--fee-ticks", &require(&mut raw, "--fee-ticks")?)?;
        } else if let Some(value) = arg.strip_prefix("--fee-ticks=") {
            fee_ticks = number("--fee-ticks", value)?;
        } else if arg == "--cash" {
            cash = signed("--cash", &require(&mut raw, "--cash")?)?;
        } else if let Some(value) = arg.strip_prefix("--cash=") {
            cash = signed("--cash", value)?;
        } else if arg == "--verify" {
            verify = true;
        } else {
            return Err(format!("unknown argument {arg}"));
        }
    }
    if input.is_some() && preset.is_some() {
        return Err("pass either --input or --preset, not both".to_string());
    }
    if input.is_none() && preset.is_none() {
        return Err("missing --input or --preset".to_string());
    }
    if max_qty == 0 {
        return Err("--max-qty must be at least 1".to_string());
    }
    let strategy = strategy.unwrap_or(match preset {
        Some(Preset::PredictionMarket) => StrategyName::ComplementArb,
        _ => StrategyName::MarketMaker,
    });
    Ok(RunArgs {
        input,
        preset,
        orders,
        seed,
        strategy,
        engine,
        min_edge,
        max_qty,
        fee_ticks,
        cash,
        verify,
    })
}

fn require(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("missing value for {flag}"))
}

fn number(flag: &str, value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("invalid number for {flag}: {value}"))
}

fn signed(flag: &str, value: &str) -> Result<i128, String> {
    value
        .parse::<i128>()
        .map_err(|_| format!("invalid number for {flag}: {value}"))
}

fn parse_preset(value: &str) -> Result<Preset, String> {
    Preset::from_str(value).map_err(|err| err.to_string())
}

fn parse_engine(value: &str) -> Result<EngineName, String> {
    match value {
        "fast" => Ok(EngineName::Fast),
        "reference" | "ref" => Ok(EngineName::Reference),
        other => Err(format!(
            "unknown engine {other} (expected fast or reference)"
        )),
    }
}

fn parse_strategy(value: &str) -> Result<StrategyName, String> {
    match value {
        "complement-arb" | "complement_arb" | "arb" => Ok(StrategyName::ComplementArb),
        "market-maker" | "market_maker" | "mm" => Ok(StrategyName::MarketMaker),
        other => Err(format!(
            "unknown strategy {other} (expected complement-arb or market-maker)"
        )),
    }
}

const HELP: &str = "\
Deterministic backtester for mini-exchange.

Usage:
  backtest --input PATH [options]
  backtest --preset equity|prediction-market [--orders N] [--seed N] [options]

The stream is replayed through one matching engine. After each event the
strategy may submit limit, market, or cancel actions. Strategy order ids start
at 1<<62, so they do not collide with simulator ids. Cash, fees, and PnL are
integer ticks. One tick is one cent in the dollar column.

complement-arb buys YES and NO when the asks sum to 100 - min edge or less
after fees, and sells the pair when the bids pay 100 + min edge or more.
market-maker joins the touch on a single book and is the baseline for equity.

--verify replays the same stream through FastEngine and ReferenceEngine and
requires identical reports and digests. A verified run counts both passes in
orders/sec.

Options:
  --input PATH       Recorded CSV event log
  --preset NAME      equity or prediction-market, generated from --seed
  --orders N         Events to generate (default 100000)
  --seed N           Generator seed (default 1)
  --strategy NAME    complement-arb or market-maker
                     (default: complement-arb for prediction-market, otherwise
                     market-maker)
  --engine NAME      fast (default) or reference
  --min-edge TICKS   Minimum complement edge (default 1)
  --max-qty N        Max order size. Position cap is 10x this (default 20)
  --fee-ticks N      Taker and maker fee, in ticks per share (default 0)
  --cash TICKS       Starting cash (default 1000000)
  --verify           Require FastEngine and ReferenceEngine to agree
  -h, --help         Show this help
";
