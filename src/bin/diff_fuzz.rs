//! Command-line differential fuzzer.
//!
//! ```text
//! cargo run --release --bin diff_fuzz -- --orders 2000000 --seed 42 --profile mixed
//! ```

#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::str::FromStr;
use std::time::Instant;

use mini_exchange::fuzz::{run, FuzzConfig, FuzzReport, Profile};

fn main() -> ExitCode {
    match Args::parse(std::env::args().skip(1)) {
        Ok(Cli::Help) => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        Ok(Cli::Run(args)) => run_fuzz(&args),
        Err(message) => {
            eprintln!("{message}");
            eprintln!("Run with --help for usage.");
            ExitCode::from(2)
        }
    }
}

fn run_fuzz(args: &Args) -> ExitCode {
    let config = FuzzConfig {
        orders: args.orders,
        seed: args.seed,
        profile: args.profile,
        check_every: args.check_every,
        check_determinism: args.determinism,
    };
    let started = Instant::now();
    match run(&config) {
        Ok(report) => {
            let elapsed = started.elapsed();
            print_report(&report, args.determinism, elapsed.as_secs_f64());
            ExitCode::SUCCESS
        }
        Err(err) => {
            let elapsed = started.elapsed();
            eprintln!("{err}");
            println!();
            println!("mini-exchange differential fuzz");
            println!("  profile:          {}", args.profile);
            println!("  seed:             {}", args.seed);
            println!("  orders processed: {}", grouped(&args.orders.to_string()));
            println!("  elapsed:          {:.3}s", elapsed.as_secs_f64());
            println!(
                "  determinism:      {}",
                if args.determinism { "fail" } else { "off" }
            );
            println!("  status:           FAIL");
            ExitCode::from(1)
        }
    }
}

fn print_report(report: &FuzzReport, determinism: bool, secs: f64) {
    let passes = if determinism { 2u64 } else { 1 };
    let throughput = if secs > 0.0 {
        (report.orders as f64) * (passes as f64) / secs
    } else {
        0.0
    };
    let rate = throughput.round().max(0.0) as u64;
    println!("mini-exchange differential fuzz");
    println!("  profile:          {}", report.profile);
    println!("  seed:             {}", report.seed);
    println!(
        "  orders processed: {}",
        grouped(&report.orders.to_string())
    );
    println!(
        "  trades matched:   {} ({})",
        grouped(&report.trades.to_string()),
        shares(report.shares_filled)
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
        "  snapshot checks:  {}",
        grouped(&report.snapshot_checks.to_string())
    );
    println!("  trade digest:     {:016x}", report.trade_digest);
    println!("  passes:           {passes}");
    println!("  elapsed:          {secs:.3}s");
    println!("  orders/sec:       {}", grouped(&rate.to_string()));
    println!(
        "  determinism:      {}",
        if determinism { "pass" } else { "off" }
    );
    println!("  status:           PASS");
}

fn shares(qty: u128) -> String {
    format!("{} shares", grouped(&qty.to_string()))
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

enum Cli {
    Help,
    Run(Args),
}

struct Args {
    orders: u64,
    seed: u64,
    profile: Profile,
    check_every: u64,
    determinism: bool,
}

impl Args {
    fn parse(args: impl Iterator<Item = String>) -> Result<Cli, String> {
        let mut orders = 100_000u64;
        let mut seed = 1u64;
        let mut profile = Profile::Mixed;
        let mut check_every = 1_024u64;
        let mut determinism = true;
        let mut raw = args;
        while let Some(arg) = raw.next() {
            if arg == "-h" || arg == "--help" {
                return Ok(Cli::Help);
            } else if arg == "--determinism" {
                determinism = true;
            } else if arg == "--no-determinism" {
                determinism = false;
            } else if arg == "--orders" {
                orders = number("--orders", raw.next())?;
            } else if arg == "--seed" {
                seed = number("--seed", raw.next())?;
            } else if arg == "--check-every" {
                check_every = number("--check-every", raw.next())?;
            } else if arg == "--profile" {
                let value = raw
                    .next()
                    .ok_or_else(|| "missing value for --profile".to_string())?;
                profile = parse_profile(&value)?;
            } else if let Some(value) = arg.strip_prefix("--orders=") {
                orders = parse_number("--orders", value)?;
            } else if let Some(value) = arg.strip_prefix("--seed=") {
                seed = parse_number("--seed", value)?;
            } else if let Some(value) = arg.strip_prefix("--check-every=") {
                check_every = parse_number("--check-every", value)?;
            } else if let Some(value) = arg.strip_prefix("--profile=") {
                profile = parse_profile(value)?;
            } else {
                return Err(format!("unknown argument {arg}"));
            }
        }
        Ok(Cli::Run(Args {
            orders,
            seed,
            profile,
            check_every,
            determinism,
        }))
    }
}

fn number(flag: &str, value: Option<String>) -> Result<u64, String> {
    let value = value.ok_or_else(|| format!("missing value for {flag}"))?;
    parse_number(flag, &value)
}

fn parse_number(flag: &str, value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("invalid number for {flag}: {value}"))
}

fn parse_profile(value: &str) -> Result<Profile, String> {
    Profile::from_str(value).map_err(|err| err.to_string())
}

const HELP: &str = "\
Differential fuzzer for mini-exchange.

Feeds one deterministic command stream to ReferenceEngine and FastEngine.
Every command result must match. Book snapshots and quantity invariants are
checked on a cadence, and the seed is replayed to confirm the trade digest.

Usage:
  diff_fuzz [--orders N] [--seed N] [--profile NAME] [--check-every N]
            [--determinism | --no-determinism]

Options:
  --orders N         Commands in the stream (default 100000)
  --seed N           Generator seed (default 1)
  --profile NAME     balanced, tight-spread, wide-sweep, adversarial, or mixed
                     (default mixed)
  --check-every N    Compare books and check invariants every N commands.
                     0 checks only after the last command (default 1024)
  --determinism      Replay the seed and require the same book and trade digest
                     (default)
  --no-determinism   Run a single pass
  -h, --help         Show this help
";
