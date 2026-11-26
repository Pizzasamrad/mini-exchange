//! Generate a deterministic order stream, or replay one through a matching engine.
//!
//! ```text
//! cargo run --release --bin replay -- generate --preset equity --orders 1000000 --seed 1 --out equity.log
//! cargo run --release --bin replay -- run --input equity.log --engine fast --verify
//! ```

#![forbid(unsafe_code)]

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;
use std::time::Instant;

use mini_exchange::event_log::EventWriter;
use mini_exchange::replay::{
    run_reader, verify_reader, BookStats, MarketSnapshot, ReplayOptions, ReplayReport, StreamError,
};
use mini_exchange::sim::{generate, Preset, SimConfig};
use mini_exchange::{FastEngine, ReferenceEngine};

fn main() -> ExitCode {
    match Cli::parse(std::env::args().skip(1)) {
        Ok(Cli::Help) => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        Ok(Cli::Generate(args)) => generate_log(&args),
        Ok(Cli::Run(args)) => replay_log(&args),
        Err(message) => {
            eprintln!("{message}");
            eprintln!("Run with --help for usage.");
            ExitCode::from(2)
        }
    }
}

fn generate_log(args: &GenArgs) -> ExitCode {
    let started = Instant::now();
    let comment = format!(
        "preset={} seed={} orders={}",
        args.preset, args.seed, args.orders
    );
    let file = match File::create(&args.out) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("could not create {}: {err}", args.out.display());
            return ExitCode::from(1);
        }
    };
    let mut writer = match EventWriter::create(BufWriter::new(file), [comment.as_str()]) {
        Ok(writer) => writer,
        Err(err) => {
            eprintln!("could not write {}: {err}", args.out.display());
            return ExitCode::from(1);
        }
    };
    let config = SimConfig::new(args.preset, args.seed, args.orders);
    let written = match writer.write_events(generate(config)) {
        Ok(written) => written,
        Err(err) => {
            eprintln!("could not write {}: {err}", args.out.display());
            return ExitCode::from(1);
        }
    };
    if let Err(err) = writer.flush() {
        eprintln!("could not flush {}: {err}", args.out.display());
        return ExitCode::from(1);
    }
    let secs = started.elapsed().as_secs_f64();
    print_generate(args, written, secs);
    ExitCode::SUCCESS
}

fn replay_log(args: &RunArgs) -> ExitCode {
    let started = Instant::now();
    match execute_run(args) {
        Ok(report) => {
            let secs = started.elapsed().as_secs_f64();
            print_report(&report, args, secs);
            ExitCode::SUCCESS
        }
        Err(err) => {
            let secs = started.elapsed().as_secs_f64();
            eprintln!("{err}");
            println!();
            println!("mini-exchange replay");
            println!("  engine:           {}", args.engine);
            println!("  input:            {}", args.input.display());
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

fn execute_run(args: &RunArgs) -> Result<ReplayReport, StreamError> {
    let file = File::open(&args.input).map_err(StreamError::from)?;
    let reader =
        mini_exchange::event_log::EventReader::new(BufReader::with_capacity(128 * 1024, file));
    let options = ReplayOptions::new(args.sample_every);
    if args.verify {
        let verified = verify_reader(reader, options)?;
        Ok(match args.engine {
            EngineName::Fast => verified.fast,
            EngineName::Reference => verified.reference,
        })
    } else {
        match args.engine {
            EngineName::Fast => {
                run_reader(reader, |_| FastEngine::new(), options).map_err(StreamError::from)
            }
            EngineName::Reference => {
                run_reader(reader, |_| ReferenceEngine::new(), options).map_err(StreamError::from)
            }
        }
    }
}

fn print_generate(args: &GenArgs, written: u64, secs: f64) {
    let rate = per_second(written, secs);
    println!("mini-exchange generate");
    println!("  preset:           {}", args.preset);
    println!("  seed:             {}", args.seed);
    println!("  orders:           {}", grouped(&written.to_string()));
    println!("  output:           {}", args.out.display());
    println!("  elapsed:          {secs:.3}s");
    println!("  orders/sec:       {}", grouped(&rate.to_string()));
}

fn print_report(report: &ReplayReport, args: &RunArgs, secs: f64) {
    let passes = if args.verify { 2u64 } else { 1 };
    let rate = per_second(report.commands.saturating_mul(passes), secs);
    println!("mini-exchange replay");
    println!("  engine:           {}", args.engine);
    println!("  input:            {}", args.input.display());
    println!(
        "  orders processed: {}",
        grouped(&report.commands.to_string())
    );
    println!(
        "  limits:           {}",
        grouped(&report.limits.to_string())
    );
    println!(
        "  market orders:    {}",
        grouped(&report.markets.to_string())
    );
    println!(
        "  cancels:          {}",
        grouped(&report.cancels.to_string())
    );
    println!(
        "  rejects:          {}",
        grouped(&report.rejects.to_string())
    );
    println!(
        "  trades matched:   {} ({})",
        grouped(&report.trades.to_string()),
        shares(report.volume)
    );
    println!(
        "  vwap:             {}",
        format_vwap(report.notional, report.volume)
    );
    println!("  last trade:       {}", format_price(report.last_price));
    println!(
        "  top of book:      {}",
        format_book_stats(&report.top_of_book)
    );
    println!("  digest:           {:016x}", report.digest);
    println!("  passes:           {passes}");
    println!("  elapsed:          {secs:.3}s");
    println!("  orders/sec:       {}", grouped(&rate.to_string()));
    println!(
        "  verify:           {}",
        if args.verify { "pass" } else { "off" }
    );
    println!("  status:           PASS");
    for snap in &report.snapshots {
        println!("  book {:<12} {}", snap.market, format_snapshot(snap));
    }
}

fn format_snapshot(snap: &MarketSnapshot) -> String {
    format!(
        "{} / {}, {}, trades {}, vwap {}, last {}, avg spread {}",
        count_label(snap.book.bids.len(), "bid", "bids"),
        count_label(snap.book.asks.len(), "ask", "asks"),
        format_touch(&snap.book),
        grouped(&snap.trades.to_string()),
        format_vwap(snap.notional, snap.volume),
        format_price(snap.last_price),
        format_ratio(
            snap.top_of_book.spread_sum,
            u128::from(snap.top_of_book.two_sided),
            2
        ),
    )
}

fn count_label(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

fn format_touch(book: &mini_exchange::Book) -> String {
    match (book.bids.first(), book.asks.first()) {
        (Some(bid), Some(ask)) => format!(
            "bid {} x {} / ask {} x {}",
            bid.price, bid.total_qty, ask.price, ask.total_qty
        ),
        (Some(bid), None) => format!("bid {} x {} / ask -", bid.price, bid.total_qty),
        (None, Some(ask)) => format!("bid - / ask {} x {}", ask.price, ask.total_qty),
        (None, None) => "empty".to_string(),
    }
}

fn format_book_stats(stats: &BookStats) -> String {
    let spread = match (stats.spread_min, stats.spread_max) {
        (Some(min), Some(max)) => format!("{min}..={max}"),
        _ => "-".to_string(),
    };
    format!(
        "{} samples, {} two-sided, avg spread {}, avg mid {}, spread {spread}",
        grouped(&stats.samples.to_string()),
        grouped(&stats.two_sided.to_string()),
        format_ratio(stats.spread_sum, u128::from(stats.two_sided), 2),
        format_ratio(
            stats.mid_sum,
            u128::from(stats.two_sided).saturating_mul(2),
            2
        ),
    )
}

fn format_vwap(notional: u128, volume: u128) -> String {
    format_ratio(notional, volume, 4)
}

fn format_price(price: Option<mini_exchange::Price>) -> String {
    match price {
        Some(price) => price.to_string(),
        None => "-".to_string(),
    }
}

fn format_ratio(num: u128, den: u128, places: usize) -> String {
    if den == 0 {
        return "-".to_string();
    }
    let mut scale = 1u128;
    for _ in 0..places {
        scale = scale.saturating_mul(10);
    }
    let whole = num / den;
    let rem = num % den;
    let frac = rem.saturating_mul(scale) / den;
    format!("{whole}.{frac:0width$}", width = places)
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
        if index > 0 && index % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

enum Cli {
    Help,
    Generate(GenArgs),
    Run(RunArgs),
}

struct GenArgs {
    preset: Preset,
    orders: u64,
    seed: u64,
    out: PathBuf,
}

struct RunArgs {
    input: PathBuf,
    engine: EngineName,
    verify: bool,
    sample_every: u64,
}

impl Cli {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let args: Vec<String> = args.collect();
        if args.is_empty() || args.iter().any(|arg| arg == "-h" || arg == "--help") {
            return Ok(Self::Help);
        }
        match args[0].as_str() {
            "generate" => Ok(Self::Generate(parse_generate(&args[1..])?)),
            "run" => Ok(Self::Run(parse_run(&args[1..])?)),
            other => Err(format!("unknown command {other}")),
        }
    }
}

fn parse_generate(args: &[String]) -> Result<GenArgs, String> {
    let mut preset = Preset::Equity;
    let mut orders = 100_000u64;
    let mut seed = 1u64;
    let mut out: Option<PathBuf> = None;
    let mut raw = args.iter().cloned();
    while let Some(arg) = raw.next() {
        if arg == "--preset" {
            preset = parse_preset(&require(&mut raw, "--preset")?)?;
        } else if let Some(value) = arg.strip_prefix("--preset=") {
            preset = parse_preset(value)?;
        } else if arg == "--orders" {
            orders = number("--orders", &require(&mut raw, "--orders")?)?;
        } else if let Some(value) = arg.strip_prefix("--orders=") {
            orders = number("--orders", value)?;
        } else if arg == "--seed" {
            seed = number("--seed", &require(&mut raw, "--seed")?)?;
        } else if let Some(value) = arg.strip_prefix("--seed=") {
            seed = number("--seed", value)?;
        } else if arg == "--out" {
            out = Some(PathBuf::from(require(&mut raw, "--out")?));
        } else if let Some(value) = arg.strip_prefix("--out=") {
            out = Some(PathBuf::from(value));
        } else {
            return Err(format!("unknown argument {arg}"));
        }
    }
    let out = out.ok_or_else(|| "missing --out".to_string())?;
    Ok(GenArgs {
        preset,
        orders,
        seed,
        out,
    })
}

fn parse_run(args: &[String]) -> Result<RunArgs, String> {
    let mut input: Option<PathBuf> = None;
    let mut engine = EngineName::Fast;
    let mut verify = false;
    let mut sample_every = 64u64;
    let mut raw = args.iter().cloned();
    while let Some(arg) = raw.next() {
        if arg == "--input" {
            input = Some(PathBuf::from(require(&mut raw, "--input")?));
        } else if let Some(value) = arg.strip_prefix("--input=") {
            input = Some(PathBuf::from(value));
        } else if arg == "--engine" {
            engine = parse_engine(&require(&mut raw, "--engine")?)?;
        } else if let Some(value) = arg.strip_prefix("--engine=") {
            engine = parse_engine(value)?;
        } else if arg == "--verify" {
            verify = true;
        } else if arg == "--sample-every" {
            sample_every = number("--sample-every", &require(&mut raw, "--sample-every")?)?;
        } else if let Some(value) = arg.strip_prefix("--sample-every=") {
            sample_every = number("--sample-every", value)?;
        } else {
            return Err(format!("unknown argument {arg}"));
        }
    }
    let input = input.ok_or_else(|| "missing --input".to_string())?;
    Ok(RunArgs {
        input,
        engine,
        verify,
        sample_every,
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

fn parse_preset(value: &str) -> Result<Preset, String> {
    Preset::from_str(value).map_err(|err| err.to_string())
}

fn parse_engine(value: &str) -> Result<EngineName, String> {
    match value {
        "fast" => Ok(EngineName::Fast),
        "reference" | "ref" => Ok(EngineName::Reference),
        _ => Err(format!(
            "unknown engine {value} (expected fast or reference)"
        )),
    }
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

const HELP: &str = "\
Deterministic order-stream generator and replayer for mini-exchange.

Usage:
  replay generate --out PATH [--preset equity|prediction-market] [--orders N] [--seed N]
  replay run --input PATH [--engine fast|reference] [--verify] [--sample-every N]

generate writes a seeded synthetic log. equity is one book centered on 10_000
ticks. prediction-market writes a YES/NO pair bounded to 1..=99, with short
complement gaps. The generator does not read the operating system's random source.

run replays a log through one matching engine. --verify replays it through
FastEngine and ReferenceEngine in one pass and requires identical results,
books, and digests.

Options:
  --preset NAME      equity (default) or prediction-market
  --orders N         Events to generate (default 100000)
  --seed N           Generator seed (default 1)
  --out PATH         Output log
  --input PATH       Log to replay
  --engine NAME      fast (default) or reference
  --verify           Require FastEngine and ReferenceEngine to agree
  --sample-every N   Sample the inside quote every N commands.
                     0 samples only the final book (default 64)
  -h, --help         Show this help
";
