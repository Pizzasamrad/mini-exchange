//! Throughput and latency of `FastEngine` and `ReferenceEngine`.
//!
//! ```text
//! cargo run --release --bin bench -- --ops 200000 --seed 1
//! cargo run --release --bin bench -- --markdown
//! cargo run --release --bin bench -- --json
//! ```

#![forbid(unsafe_code)]

use std::process::{Command, ExitCode};

use mini_exchange::backtest::BacktestReport;
use mini_exchange::bench::{
    bench_commands, bench_complement_arb, generate_commands, generate_prediction_market, ArbBench,
    EngineBench, Percentiles, Workload,
};
use mini_exchange::{FastEngine, ReferenceEngine};

fn main() -> ExitCode {
    match Cli::parse(std::env::args().skip(1)) {
        Ok(Cli::Help) => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        Ok(Cli::Run(args)) => match execute(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("{message}");
                ExitCode::from(1)
            }
        },
        Err(message) => {
            eprintln!("{message}");
            eprintln!("Run with --help for usage.");
            ExitCode::from(2)
        }
    }
}

fn execute(args: &Args) -> Result<(), String> {
    let ops = usize::try_from(args.ops).map_err(|_| "--ops does not fit in usize".to_string())?;
    if ops == 0 {
        return Err("--ops must be at least 1".to_string());
    }
    let prepared = prepare(args.seed, ops, args.ops);
    let env = Environment::collect(args);
    let mut rows = Vec::with_capacity(Workload::ALL.len() * 2);
    let mut failures = Vec::new();
    for (workload, commands) in &prepared.commands {
        let fast = bench_commands(commands, FastEngine::new);
        let reference = bench_commands(commands, ReferenceEngine::new);
        if fast.digest != reference.digest || fast.trades != reference.trades {
            failures.push(format!(
                "{workload}: fast digest {:016x} ({} trades) != reference {:016x} ({} trades)",
                fast.digest, fast.trades, reference.digest, reference.trades
            ));
        }
        rows.push(engine_row(workload.name(), "fast", &fast));
        rows.push(engine_row(workload.name(), "reference", &reference));
    }
    let fast_arb = bench_complement_arb(prepared.events.as_slice(), |_| FastEngine::new())
        .map_err(|err| err.to_string())?;
    let reference_arb =
        bench_complement_arb(prepared.events.as_slice(), |_| ReferenceEngine::new())
            .map_err(|err| err.to_string())?;
    if fast_arb.report != reference_arb.report {
        failures.push(format!(
            "pred-market-arb: fast digest {:016x} != reference {:016x}",
            fast_arb.report.digest, reference_arb.report.digest
        ));
    }
    let report = Report {
        env,
        rows,
        fast_arb,
        reference_arb,
    };
    match args.format {
        Format::Text => print_text(&report),
        Format::Markdown => print_markdown(&report),
        Format::Json => print_json(&report),
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

struct Prepared {
    commands: Vec<(Workload, Vec<mini_exchange::Command>)>,
    events: Vec<mini_exchange::Event>,
}

fn prepare(seed: u64, ops: usize, events: u64) -> Prepared {
    let commands = Workload::ALL
        .into_iter()
        .map(|workload| (workload, generate_commands(workload, seed, ops)))
        .collect();
    let events = generate_prediction_market(seed, events);
    Prepared { commands, events }
}

struct Report {
    env: Environment,
    rows: Vec<Row>,
    fast_arb: ArbBench,
    reference_arb: ArbBench,
}

struct Row {
    workload: &'static str,
    engine: &'static str,
    ops_per_sec: u64,
    elapsed_ns: u128,
    latency: Percentiles,
    trades: u64,
    digest: u64,
}

fn engine_row(workload: &'static str, engine: &'static str, bench: &EngineBench) -> Row {
    Row {
        workload,
        engine,
        ops_per_sec: bench.ops_per_sec,
        elapsed_ns: bench.elapsed_ns,
        latency: bench.latency,
        trades: bench.trades,
        digest: bench.digest,
    }
}

struct Environment {
    os: &'static str,
    arch: &'static str,
    cpu: String,
    cores: usize,
    memory_mib: Option<u64>,
    rustc: String,
    profile: &'static str,
    ops: u64,
    seed: u64,
}

impl Environment {
    fn collect(args: &Args) -> Self {
        Self {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            cpu: cpu_model(),
            cores: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            memory_mib: memory_mib(),
            rustc: rustc_version(),
            profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            ops: args.ops,
            seed: args.seed,
        }
    }
}

fn print_text(report: &Report) {
    print_env_text(&report.env);
    println!();
    println!("engine workloads");
    print_aligned(&report.rows);
    println!();
    print_arb_text(&report.fast_arb, &report.reference_arb);
}

fn print_env_text(env: &Environment) {
    println!("mini-exchange benchmark");
    println!("  os/arch:    {}/{}", env.os, env.arch);
    println!("  cpu:        {}", env.cpu);
    println!("  cores:      {}", env.cores);
    println!("  memory:     {}", memory_label(env.memory_mib));
    println!("  rustc:      {}", env.rustc);
    println!("  profile:    {}", env.profile);
    println!("  ops:        {}", grouped_u64(env.ops));
    println!("  seed:       {}", env.seed);
    println!("  warmup:     1 full pass, excluded from the numbers below");
    println!("  inputs:     generated from the seed before the clock starts");
    println!("  throughput: one timed pass, no per-operation clock read");
    println!("  latency:    a separate pass, nanoseconds per operation");
    println!("  fast:       release execute does not walk the book");
    println!("  reference:  execute scans the book for ids and invariant checks");
}

fn print_arb_text(fast: &ArbBench, reference: &ArbBench) {
    println!("prediction-market complement arb");
    println!("  strategy:   complement-arb (min edge {ARB_EDGE} tick, max position {ARB_POS}, max qty {ARB_QTY}, fee 0)");
    println!("  cash:       {} ticks", grouped_u64(1_000_000));
    println!("  events:     {}", grouped_u64(fast.events));
    println!("  orders/sec: market events in one end-to-end backtest");
    println!("  latency:    one market event, including strategy and accounting");
    println!();
    let rows = [arb_row("fast", fast), arb_row("reference", reference)];
    print_arb_table(&rows);
    println!();
    let summary = &fast.report;
    println!(
        "  strategy orders:  {}",
        grouped_u64(summary.strategy_orders)
    );
    println!(
        "  strategy actions: {}",
        grouped_u64(summary.strategy_actions)
    );
    println!(
        "  strategy fills:   {} ({})",
        grouped_u64(summary.strategy_fills),
        shares(summary.strategy_volume)
    );
    println!("  paired shares:    {}", grouped_u64(summary.paired_qty));
    println!(
        "  net pnl:          {} ticks",
        grouped_i128(summary.net_pnl)
    );
    println!(
        "  realized pnl:     {} ticks",
        grouped_i128(summary.realized_pnl)
    );
    println!(
        "  unrealized pnl:   {} ticks",
        grouped_i128(summary.unrealized_pnl)
    );
    println!(
        "  fees paid:        {} ticks",
        grouped_i128(summary.fees_paid)
    );
    println!(
        "  max drawdown:     {} ticks ({} bps)",
        grouped_i128(summary.max_drawdown_ticks),
        summary.max_drawdown_bps
    );
    println!(
        "  final equity:     {} ticks",
        grouped_i128(summary.final_equity)
    );
    println!("  leg imbalance:    {}", summary.max_leg_imbalance);
    println!("  avg edge:         {}", edge(summary));
    println!("  win rate:         {}", win_rate(summary));
    println!("  digest:           {:016x}", summary.digest);
    println!(
        "  engine parity:    {}",
        if fast.report == reference.report {
            "pass"
        } else {
            "fail"
        }
    );
}

const ARB_EDGE: u64 = mini_exchange::bench::ARB_MIN_EDGE_TICKS;
const ARB_POS: u64 = mini_exchange::bench::ARB_MAX_POSITION;
const ARB_QTY: u64 = mini_exchange::bench::ARB_MAX_ORDER_QTY;

fn arb_row(engine: &'static str, bench: &ArbBench) -> Row {
    Row {
        workload: "pred-market-arb",
        engine,
        ops_per_sec: bench.orders_per_sec,
        elapsed_ns: bench.elapsed_ns,
        latency: bench.latency,
        trades: bench.report.strategy_fills,
        digest: bench.report.digest,
    }
}

fn print_aligned(rows: &[Row]) {
    let header = [
        "workload", "engine", "ops/sec", "elapsed", "p50 ns", "p95 ns", "p99 ns", "p99.9 ns",
        "max ns", "trades", "digest",
    ];
    let body: Vec<Vec<String>> = rows.iter().map(engine_cells).collect();
    print_cells(&header, &body, 2);
}

fn print_arb_table(rows: &[Row]) {
    let header = [
        "engine",
        "orders/sec",
        "elapsed",
        "p50 ns",
        "p95 ns",
        "p99 ns",
        "p99.9 ns",
        "max ns",
    ];
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            vec![
                row.engine.to_string(),
                grouped_u64(row.ops_per_sec),
                format_secs(row.elapsed_ns),
                grouped_u64(row.latency.p50),
                grouped_u64(row.latency.p95),
                grouped_u64(row.latency.p99),
                grouped_u64(row.latency.p99_9),
                grouped_u64(row.latency.max),
            ]
        })
        .collect();
    print_cells(&header, &body, 1);
}

fn engine_cells(row: &Row) -> Vec<String> {
    vec![
        row.workload.to_string(),
        row.engine.to_string(),
        grouped_u64(row.ops_per_sec),
        format_secs(row.elapsed_ns),
        grouped_u64(row.latency.p50),
        grouped_u64(row.latency.p95),
        grouped_u64(row.latency.p99),
        grouped_u64(row.latency.p99_9),
        grouped_u64(row.latency.max),
        grouped_u64(row.trades),
        format!("{digest:016x}", digest = row.digest),
    ]
}

fn print_cells(header: &[&str], body: &[Vec<String>], text_columns: usize) {
    let mut widths: Vec<usize> = header.iter().map(|cell| cell.len()).collect();
    for row in body {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }
    let header_cells: Vec<String> = header.iter().map(|cell| (*cell).to_string()).collect();
    print_cell_row(&header_cells, &widths, text_columns);
    for row in body {
        print_cell_row(row, &widths, text_columns);
    }
}

fn print_cell_row(cells: &[String], widths: &[usize], text_columns: usize) {
    let mut line = String::from("  ");
    for (index, (cell, width)) in cells.iter().zip(widths).enumerate() {
        if index > 0 {
            line.push_str("  ");
        }
        let text = if index < text_columns {
            format!("{cell:<width$}")
        } else {
            format!("{cell:>width$}")
        };
        line.push_str(&text);
    }
    println!("{line}");
}

fn print_markdown(report: &Report) {
    let env = &report.env;
    println!("# mini-exchange benchmark");
    println!();
    println!("- os/arch: `{}/{}`", env.os, env.arch);
    println!("- cpu: {}", env.cpu);
    println!("- cores: {}", env.cores);
    println!("- memory: {}", memory_label(env.memory_mib));
    println!("- rustc: `{}`", env.rustc);
    println!("- profile: `{}`", env.profile);
    println!("- ops: {}", grouped_u64(env.ops));
    println!("- seed: {}", env.seed);
    println!("- warmup: 1 full pass, excluded");
    println!("- inputs: generated from the seed before the clock starts");
    println!("- throughput: one timed pass, no per-operation clock read");
    println!("- latency: a separate pass, nanoseconds per operation");
    println!("- fast: release execute does not walk the book");
    println!("- reference: execute scans the book for ids and invariant checks");
    println!();
    println!("## Engine workloads");
    println!();
    println!(
        "| workload | engine | ops/sec | elapsed | p50 ns | p95 ns | p99 ns | p99.9 ns | max ns | trades | digest |"
    );
    println!("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |");
    for row in &report.rows {
        let cells = engine_cells(row);
        println!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | `{}` |",
            cells[0],
            cells[1],
            cells[2],
            cells[3],
            cells[4],
            cells[5],
            cells[6],
            cells[7],
            cells[8],
            cells[9],
            cells[10]
        );
    }
    println!();
    println!("## Prediction-market complement arb");
    println!();
    println!(
        "Strategy `complement-arb`, min edge {ARB_EDGE} tick, max position {ARB_POS}, max qty {ARB_QTY}, fee 0, cash 1,000,000 ticks. Orders/sec counts market events. Latency is one market event, including the strategy and accounting."
    );
    println!();
    println!("| engine | orders/sec | elapsed | p50 ns | p95 ns | p99 ns | p99.9 ns | max ns |");
    println!("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    for (engine, bench) in [
        ("fast", &report.fast_arb),
        ("reference", &report.reference_arb),
    ] {
        println!(
            "| {engine} | {} | {} | {} | {} | {} | {} | {} |",
            grouped_u64(bench.orders_per_sec),
            format_secs(bench.elapsed_ns),
            grouped_u64(bench.latency.p50),
            grouped_u64(bench.latency.p95),
            grouped_u64(bench.latency.p99),
            grouped_u64(bench.latency.p99_9),
            grouped_u64(bench.latency.max)
        );
    }
    println!();
    print_arb_summary_lines(
        &report.fast_arb.report,
        report.fast_arb.report == report.reference_arb.report,
    );
}

fn print_arb_summary_lines(summary: &BacktestReport, parity: bool) {
    println!(
        "- strategy orders: {}",
        grouped_u64(summary.strategy_orders)
    );
    println!(
        "- strategy actions: {}",
        grouped_u64(summary.strategy_actions)
    );
    println!(
        "- strategy fills: {} ({})",
        grouped_u64(summary.strategy_fills),
        shares(summary.strategy_volume)
    );
    println!("- paired shares: {}", grouped_u64(summary.paired_qty));
    println!("- net pnl: {} ticks", grouped_i128(summary.net_pnl));
    println!(
        "- realized pnl: {} ticks",
        grouped_i128(summary.realized_pnl)
    );
    println!(
        "- unrealized pnl: {} ticks",
        grouped_i128(summary.unrealized_pnl)
    );
    println!("- fees paid: {} ticks", grouped_i128(summary.fees_paid));
    println!(
        "- max drawdown: {} ticks ({} bps)",
        grouped_i128(summary.max_drawdown_ticks),
        summary.max_drawdown_bps
    );
    println!(
        "- final equity: {} ticks",
        grouped_i128(summary.final_equity)
    );
    println!("- leg imbalance: {}", summary.max_leg_imbalance);
    println!("- avg edge: {}", edge(summary));
    println!("- win rate: {}", win_rate(summary));
    println!("- digest: `{:016x}`", summary.digest);
    println!("- engine parity: {}", if parity { "pass" } else { "fail" });
}

fn print_json(report: &Report) {
    let env = &report.env;
    println!("{{");
    println!("  \"os\": \"{}\",", json_escape(env.os));
    println!("  \"arch\": \"{}\",", json_escape(env.arch));
    println!("  \"cpu\": \"{}\",", json_escape(&env.cpu));
    println!("  \"cores\": {},", env.cores);
    match env.memory_mib {
        Some(mib) => println!("  \"memory_mib\": {mib},"),
        None => println!("  \"memory_mib\": null,"),
    }
    println!("  \"rustc\": \"{}\",", json_escape(&env.rustc));
    println!("  \"profile\": \"{}\",", env.profile);
    println!("  \"ops\": {},", env.ops);
    println!("  \"seed\": {},", env.seed);
    println!("  \"warmup_passes\": 1,");
    println!("  \"engines\": [");
    for (index, row) in report.rows.iter().enumerate() {
        let comma = if index + 1 == report.rows.len() {
            ""
        } else {
            ","
        };
        print_json_row(row, report.env.ops, comma);
    }
    println!("  ],");
    println!("  \"pred_market_arb\": {{");
    print_json_arb("fast", &report.fast_arb, ",");
    print_json_arb("reference", &report.reference_arb, "");
    let parity = report.fast_arb.report == report.reference_arb.report;
    println!(
        "    \"engine_parity\": {}",
        if parity { "true" } else { "false" }
    );
    println!("  }}");
    println!("}}");
}

fn print_json_row(row: &Row, ops: u64, comma: &str) {
    println!("    {{");
    println!("      \"workload\": \"{}\",", row.workload);
    println!("      \"engine\": \"{}\",", row.engine);
    println!("      \"ops\": {ops},");
    println!("      \"ops_per_sec\": {},", row.ops_per_sec);
    println!("      \"elapsed_ns\": {},", row.elapsed_ns);
    print_json_latency(&row.latency);
    println!("      \"trades\": {},", row.trades);
    println!("      \"digest\": \"{:016x}\"", row.digest);
    println!("    }}{comma}");
}

fn print_json_arb(name: &str, bench: &ArbBench, comma: &str) {
    let summary = &bench.report;
    println!("    \"{name}\": {{");
    println!("      \"events\": {},", bench.events);
    println!("      \"orders_per_sec\": {},", bench.orders_per_sec);
    println!("      \"elapsed_ns\": {},", bench.elapsed_ns);
    print_json_latency(&bench.latency);
    println!("      \"strategy_orders\": {},", summary.strategy_orders);
    println!("      \"strategy_actions\": {},", summary.strategy_actions);
    println!("      \"strategy_fills\": {},", summary.strategy_fills);
    println!("      \"strategy_volume\": {},", summary.strategy_volume);
    println!("      \"paired_qty\": {},", summary.paired_qty);
    println!("      \"net_pnl_ticks\": {},", summary.net_pnl);
    println!("      \"realized_pnl_ticks\": {},", summary.realized_pnl);
    println!(
        "      \"unrealized_pnl_ticks\": {},",
        summary.unrealized_pnl
    );
    println!("      \"fees_paid_ticks\": {},", summary.fees_paid);
    println!(
        "      \"max_drawdown_ticks\": {},",
        summary.max_drawdown_ticks
    );
    println!("      \"max_drawdown_bps\": {},", summary.max_drawdown_bps);
    println!("      \"final_equity_ticks\": {},", summary.final_equity);
    println!(
        "      \"max_leg_imbalance\": {},",
        summary.max_leg_imbalance
    );
    println!("      \"digest\": \"{:016x}\"", summary.digest);
    println!("    }}{comma}");
}

fn print_json_latency(latency: &Percentiles) {
    println!("      \"latency_ns\": {{");
    println!("        \"p50\": {},", latency.p50);
    println!("        \"p95\": {},", latency.p95);
    println!("        \"p99\": {},", latency.p99);
    println!("        \"p99_9\": {},", latency.p99_9);
    println!("        \"max\": {}", latency.max);
    println!("      }},");
}

fn edge(report: &BacktestReport) -> String {
    match report.avg_edge_ticks {
        Some(ticks) => format!("{ticks} ticks"),
        None => "-".to_string(),
    }
}

fn win_rate(report: &BacktestReport) -> String {
    let Some(bps) = report.win_rate_bps else {
        return "-".to_string();
    };
    let whole = bps / 100;
    let frac = bps % 100;
    format!("{whole}.{frac:02}% ({bps} bps)")
}

fn shares(qty: u128) -> String {
    format!("{} shares", grouped_u128(qty))
}

fn memory_label(mib: Option<u64>) -> String {
    match mib {
        Some(value) => format!("{} MiB", grouped_u64(value)),
        None => "unknown".to_string(),
    }
}

fn format_secs(elapsed_ns: u128) -> String {
    let micros = elapsed_ns / 1_000;
    let secs = micros / 1_000_000;
    let frac = micros % 1_000_000;
    format!("{secs}.{frac:06}s")
}

fn grouped_i128(value: i128) -> String {
    let negative = value < 0;
    let digits = grouped_u128(value.unsigned_abs());
    if negative {
        format!("-{digits}")
    } else {
        digits
    }
}

fn grouped_u64(value: u64) -> String {
    grouped_u128(u128::from(value))
}

fn grouped_u128(value: u128) -> String {
    grouped(&value.to_string())
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

fn json_escape(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out
}

fn cpu_model() -> String {
    let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") else {
        return "unknown".to_string();
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key == "model name" || key == "Model" || key == "Hardware" {
            return value.trim().to_string();
        }
    }
    "unknown".to_string()
}

fn memory_mib() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("MemTotal:") else {
            continue;
        };
        let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
        return Some(kb / 1024);
    }
    None
}

fn rustc_version() -> String {
    Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

enum Cli {
    Help,
    Run(Args),
}

struct Args {
    ops: u64,
    seed: u64,
    format: Format,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Text,
    Markdown,
    Json,
}

impl Cli {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut ops = 200_000_u64;
        let mut seed = 1_u64;
        let mut format = Format::Text;
        let mut raw = args;
        while let Some(arg) = raw.next() {
            if arg == "-h" || arg == "--help" {
                return Ok(Self::Help);
            } else if arg == "--markdown" {
                format = merge_format(format, Format::Markdown)?;
            } else if arg == "--json" {
                format = merge_format(format, Format::Json)?;
            } else if arg == "--ops" {
                ops = number("--ops", raw.next())?;
            } else if let Some(value) = arg.strip_prefix("--ops=") {
                ops = parse_number("--ops", value)?;
            } else if arg == "--seed" {
                seed = number("--seed", raw.next())?;
            } else if let Some(value) = arg.strip_prefix("--seed=") {
                seed = parse_number("--seed", value)?;
            } else {
                return Err(format!("unknown argument {arg}"));
            }
        }
        Ok(Self::Run(Args { ops, seed, format }))
    }
}

fn merge_format(current: Format, next: Format) -> Result<Format, String> {
    if current != Format::Text && current != next {
        return Err("pass only one of --markdown or --json".to_string());
    }
    Ok(next)
}

fn number(flag: &str, value: Option<String>) -> Result<u64, String> {
    let value = value.ok_or_else(|| format!("missing value for {flag}"))?;
    parse_number(flag, &value)
}

fn parse_number(flag: &str, value: &str) -> Result<u64, String> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("invalid number for {flag}: {value}"));
    }
    value
        .parse::<u64>()
        .map_err(|_| format!("invalid number for {flag}: {value}"))
}

const HELP: &str = "\
Reproducible benchmarks for mini-exchange.

Measures throughput and per-operation latency of FastEngine and ReferenceEngine
on deterministic workloads, then runs a prediction-market complement-arb
backtest. Streams are generated from --seed before the clock starts. A warmup
pass is discarded. Throughput and latency are separate passes, so the ops/sec
figure does not include a clock read per operation.

Usage:
  bench [--ops N] [--seed N] [--markdown | --json]

Options:
  --ops N       Commands or market events per workload (default 200000)
  --seed N      Generator seed (default 1)
  --markdown    Print Markdown tables
  --json        Print one JSON object
  -h, --help    Show this help

Workloads:
  add-only          passive limit inserts across price levels
  add-cancel        limit inserts paired with head, middle, and tail cancels
  match-heavy       crossing limits and market orders that take resting liquidity
  mixed             fuzz profile mixed (balanced, tight-spread, wide-sweep, adversarial)
  pred-market-arb   prediction-market stream through ComplementArbStrategy

Latency for the engine workloads is one MatchingEngine::execute call.
Latency for pred-market-arb is one market event, including the strategy
and the portfolio update. Orders/sec for that workload counts market events.
";
