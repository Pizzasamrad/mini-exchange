# mini-exchange

A deterministic price-time matching engine in safe Rust, checked against a reference oracle, with streaming replay and a prediction-market backtester.

Prices and quantities are integer ticks. The engines do not read a clock and do not draw random numbers, so the same command sequence always produces the same trades, the same book, and the same 64-bit digest. `#![forbid(unsafe_code)]` is crate-wide.

## Architecture

Order flow is data. A seeded simulator or a recorded event log produces commands. A `MatchingEngine` turns those commands into trades and a book. The replayer fingerprints that result. The backtester sits on the same path and lets a strategy trade against the stream.

```text
  Simulator (seeded) ----+
                         |
  Event log (CSV) -------+--> Replayer --> MatchingEngine --> trades + book
                                            |            |         |
                                            | FastEngine |         +--> FNV-1a digest
                                            | Reference  |
                                            +------------+
                                                   |
                                                   v
                                             Backtester
                                              |        |
                                         Strategy   Portfolio
                                              |
                              ComplementArbStrategy, TopOfBookMarketMaker
```

`FastEngine` and `ReferenceEngine` implement the same `MatchingEngine` trait. The differential fuzzer sends one command stream to both and requires equal results. Replay and the backtester can each run on either engine, and `--verify` requires their digests to match.

## Engineering highlights

**Integer ticks.** `Price` and `Qty` are `u64` newtypes. Cash, fees, cost basis, PnL, and drawdown are integer ticks. A fill prints at the maker's price. The match and the account stay in integer arithmetic, so two replays of the same commands print the same PnL.

**FastEngine layout.** Resting orders live in a slab. Each price level is an intrusive doubly-linked FIFO of slab indices, so a cancel rewrites two links and a quantity. An order-id map with a fixed-key hasher makes lookup, duplicate detection, and cancel one hash probe, and the map stays deterministic. Each side keeps its best level and links levels from best to worst, so emptying the inside price promotes the next level without a scan. A `BTreeMap` resolves a price when a level is created. The match loop follows the level links. Slots are reused after fills and cancels. Links are indices, so the engine stays in safe Rust.

**Correctness and determinism.** `ReferenceEngine` is the oracle: a `BTreeMap` of prices and a `VecDeque` per level, plus a full-book invariant walk on every command. The fuzzer checks three things. Command results match, including trades in order and reject reasons. The book stays uncrossed, strictly price-ordered, free of empty levels and zero-quantity orders. Quantity is conserved: limit size submitted equals fills plus cancels plus resting size, and a market fill never exceeds its submitted size. The same seed replayed must produce the same book and the same trade digest. Replay and backtest digests are 64-bit FNV-1a over the tape and the final state. Two runs match when the digests match. On this machine, `diff_fuzz --orders 2000000 --seed 42 --profile mixed` completed two passes with status `PASS` and determinism `pass`: 541,692 trades, trade digest `34f9773068903cde`, zero divergences.

**Backtester.** A `Strategy` sees each market event after the engine applies it and may return limit, market, or cancel actions. Strategy order ids start at `1 << 62`, above the simulator's id space. Fills update a FIFO lot queue. Realized PnL is the closing cashflow minus the cost of the lots it consumes. A long YES share together with a long NO share is marked at the 100-tick settlement value; only the unmatched remainder uses the book. The run tracks peak equity and the worst drawdown in ticks and in basis points.

`ComplementArbStrategy` buys both legs of a binary contract when `ask(YES) + ask(NO) + fees` is at most `100 - min_edge`, and it can sell the pair when the bids pay `100 + min_edge` after fees. Size is the minimum of the two top-of-book quantities, `max_order_qty`, and the room under `max_position`. Orders are immediate-or-cancel at the touch. If the legs are already uneven, the next decision buys the smaller leg only when that hedge still locks a non-negative edge, and it will not open a new pair while the imbalance is above `max_unhedged`.

`TopOfBookMarketMaker` is the single-book baseline. It joins the bid and the ask, cancels a quote that has left the touch, and stops quoting a side that would push inventory through `max_position`.

## Benchmarks

Measured with the default workload size on this machine. The process is a KVM guest: 4 vCPUs, `Intel(R) Xeon(R) Processor` (family 6, model 207, 1 thread per core), 16,003 MiB RAM, Linux 6.12.113+ x86_64. The compiler is `rustc 1.100.0 (0ef1bc386 2026-11-09)`.

```bash
cargo +stable run --release --bin bench
```

That command is `--ops 200000 --seed 1` and a release build. The figure below is the program's stdout, unchanged.

```text
mini-exchange benchmark
  os/arch:    linux/x86_64
  cpu:        Intel(R) Xeon(R) Processor
  cores:      4
  memory:     16,003 MiB
  rustc:      rustc 1.100.0 (0ef1bc386 2026-11-09)
  profile:    release
  ops:        200,000
  seed:       1
  warmup:     1 full pass, excluded from the numbers below
  inputs:     generated from the seed before the clock starts
  throughput: one timed pass, no per-operation clock read
  latency:    a separate pass, nanoseconds per operation
  fast:       release execute does not walk the book
  reference:  execute scans the book for ids and invariant checks

engine workloads
  workload     engine        ops/sec      elapsed     p50 ns     p95 ns     p99 ns    p99.9 ns      max ns   trades            digest
  add-only     fast        8,307,285    0.024075s         86        135      1,056       2,976   1,566,662        0  9efc92c061799306
  add-only     reference         304  658.371919s  2,693,262  5,789,725  6,893,272  10,849,177  21,916,413        0  9efc92c061799306
  add-cancel   fast       29,494,184    0.006780s         59         76        125         313     392,500        0  bf25e0580b5c0f10
  add-cancel   reference      46,898    4.264556s     21,579     23,954     28,593      36,187     637,945        0  bf25e0580b5c0f10
  match-heavy  fast       10,278,048    0.019458s         75        225        323         489     734,576  150,000  256fe9e2d2d8f815
  match-heavy  reference   5,692,482    0.035134s        176        464        664         922   1,339,448  150,000  256fe9e2d2d8f815
  mixed        fast       18,765,800    0.010657s         65        162        316       1,366     384,682   54,164  fadcdf2fead5b5de
  mixed        reference     685,056    0.291946s        953      4,010      5,588       6,620     662,079   54,164  fadcdf2fead5b5de

prediction-market complement arb
  strategy:   complement-arb (min edge 1 tick, max position 200, max qty 20, fee 0)
  cash:       1,000,000 ticks
  events:     200,000
  orders/sec: market events in one end-to-end backtest
  latency:    one market event, including strategy and accounting

  engine     orders/sec    elapsed  p50 ns  p95 ns  p99 ns  p99.9 ns   max ns
  fast        2,823,506  0.070833s     326     789   1,409     2,067  527,308
  reference   1,336,476  0.149647s     710   1,458   2,667     3,553  724,717

  strategy orders:  18,924
  strategy actions: 18,924
  strategy fills:   19,398 (113,772 shares)
  paired shares:    28,499
  net pnl:          191,553 ticks
  realized pnl:     191,204 ticks
  unrealized pnl:   349 ticks
  fees paid:        0 ticks
  max drawdown:     240 ticks (2 bps)
  final equity:     1,191,553 ticks
  leg imbalance:    0
  avg edge:         3 ticks
  win rate:         100.00% (10000 bps)
  digest:           6094a4311e2da5c8
  engine parity:    pass
```

Digests match on every workload, including the complement-arb report.

Read the rows against the book they build. `match-heavy` keeps only a few resting orders, so it is the closest comparison of the match loop: 10,278,048 ops/sec on `FastEngine` and 5,692,482 ops/sec on `ReferenceEngine`, with p50 latency of 75 ns and 176 ns. `pred-market-arb` is the full strategy session: 2,823,506 and 1,336,476 market events per second. `add-cancel` holds about 512 live orders and cancels at the head, middle, and tail: 29,494,184 ops/sec against 46,898. `add-only` leaves all 200,000 orders on the book. `FastEngine` sustains 8,307,285 inserts per second. `ReferenceEngine` takes 658.37s (304 ops/sec) because each command scans every resting order. Those latency percentiles describe a run that gets slower as the book grows. `add-cancel` is the steady-state cancel measurement.

`ReferenceEngine` is the oracle. The release build walks the book after every command. `FastEngine` skips that walk when debug assertions are off. The large gaps above combine that walk, the linear id scan, and the matching work.

## Quickstart

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Differential fuzz, 2,000,000 mixed commands, replayed once to confirm the digest:

```bash
cargo +stable run --release --bin diff_fuzz -- --orders 2000000 --seed 42 --profile mixed
```

```text
mini-exchange differential fuzz
  profile:          mixed
  seed:             42
  orders processed: 2,000,000
  trades matched:   541,692 (1,698,242 shares)
  cancels:          334,173
  rejects:          694,754
  snapshot checks:  1,954
  trade digest:     34f9773068903cde
  passes:           2
  elapsed:          8.114s
  orders/sec:       492,982
  determinism:      pass
  status:           PASS
```

`orders/sec` counts both passes. `status: PASS` means every command result matched and the replay digest matched.

Generate a log, replay it, and check both engines:

```bash
cargo run --release --bin replay -- generate --preset equity --orders 1000000 --seed 1 --out equity.log
cargo run --release --bin replay -- generate --preset prediction-market --orders 1000000 --seed 1 --out pred.log
cargo run --release --bin replay -- run --input equity.log --engine fast
cargo run --release --bin replay -- run --input pred.log --engine reference --verify
```

Backtest the complement strategy on the prediction-market preset:

```bash
cargo +stable run --release --bin backtest -- --preset prediction-market --orders 100000 --seed 1 --strategy complement-arb --min-edge 1 --max-qty 20 --verify
```

Sample output from that command on the machine above:

```text
mini-exchange backtest
  strategy:         complement-arb
  engine:           fast
  source:           prediction-market seed 1 orders 100000
  orders replayed:  100,000
  strategy actions: 9,608
  strategy orders:  9,608
  strategy fills:   9,850 (58,326 shares)
  paired arbs:      14,633
  round trips:      29,060 shares closed
  fees paid:        0 ticks ($0.00)
  realized pnl:     97,528 ticks ($975.28)
  unrealized pnl:   452 ticks ($4.52)
  net pnl:          97,980 ticks ($979.80)
  final equity:     1,097,980 ticks ($10,979.80)
  peak equity:      1,098,054 ticks ($10,980.54)
  max drawdown:     185 ticks ($1.85), 1 bps (0.01%)
  win rate:         100.00% (14,633/14,633 pairs)
  avg edge:         3 ticks
  leg imbalance:    0
  elapsed:          0.136s
  orders/sec:       1,473,073
  digest:           2335762ce45f2a5a
  verify:           pass
  status:           PASS
  book NO           net +103, open cost 5,644 ticks ($56.44), avg 54
  book YES          net +103, open cost 4,204 ticks ($42.04), avg 40
```

`--verify` replays the stream through both engines, so `orders/sec` counts both passes. `verify: pass` means the reports and the digest matched.

Benchmark both engines. The default is 200,000 operations per workload, seed 1:

```bash
cargo +stable run --release --bin bench
cargo +stable run --release --bin bench -- --ops 200000 --seed 1 --json
```

## CLI reference

### `diff_fuzz`

Compares `ReferenceEngine` and `FastEngine` on one generated stream.

```text
diff_fuzz [--orders N] [--seed N] [--profile NAME] [--check-every N]
          [--determinism | --no-determinism]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--orders` | 100000 | Commands in the stream |
| `--seed` | 1 | Generator seed |
| `--profile` | mixed | `balanced`, `tight-spread`, `wide-sweep`, `adversarial`, or `mixed` |
| `--check-every` | 1024 | Snapshot and invariant cadence. `0` checks only at the end |
| `--no-determinism` | off | Skip the second pass that replays the seed |

`orders/sec` on a verified run counts both passes.

### `replay`

`generate` writes an event log. `run` replays it.

```text
replay generate --preset NAME [--orders N] [--seed N] --out FILE
replay run --input FILE [--engine fast|reference] [--verify] [--sample-every N]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--preset` | | `equity` or `prediction-market` |
| `--orders` | 1000000 on generate | Events to synthesize |
| `--seed` | 1 | Generator seed |
| `--engine` | fast | `fast` or `reference` |
| `--verify` | off | Require both engines to agree |
| `--sample-every` | 64 | Top-of-book sample cadence. `0` samples only the final book |

The log is CSV. `seq` is a strictly increasing logical timestamp. An empty market column means `DEFAULT`. `YES` and `NO` are the two legs of a binary contract.

```text
# mini-exchange event log v1
seq,market,cmd,id,side,price,qty
1,DEFAULT,limit,1,buy,10000,25
2,YES,market,2,sell,,3
3,NO,cancel,1,,,
```

### `backtest`

Replays a preset or a log through a strategy.

```text
backtest (--preset NAME | --input FILE) --strategy NAME
         [--orders N] [--seed N] [--engine fast|reference]
         [--min-edge N] [--max-qty N] [--fee-ticks N] [--cash N] [--verify]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--preset` | | `equity` or `prediction-market`. Mutually exclusive with `--input` |
| `--input` | | Event log to replay |
| `--strategy` | required | `complement-arb` or `market-maker` |
| `--orders` | 100000 | Events when using `--preset` |
| `--seed` | 1 | Generator seed |
| `--engine` | fast | Engine that produces the printed report |
| `--min-edge` | 1 | Minimum locked edge, in ticks, for complement arb |
| `--max-qty` | 20 | Clip size. Position cap is `max(max-qty * 10, max-qty)` |
| `--fee-ticks` | 0 | Taker and maker fee, ticks per share |
| `--cash` | 1000000 | Starting cash, in ticks |
| `--verify` | off | Require `FastEngine` and `ReferenceEngine` to match |

One tick is one cent in the dollar column of the report.

### `bench`

Throughput and latency for five workloads. Streams are built from `--seed` before the clock starts. A warmup pass is discarded. The throughput pass does not read the clock per operation. The latency pass times each operation on its own.

```text
bench [--ops N] [--seed N] [--markdown | --json]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--ops` | 200000 | Commands, or market events, per workload |
| `--seed` | 1 | Generator seed |
| `--markdown` | text table | Markdown tables |
| `--json` | text table | One JSON object |

| Workload | What it measures |
| --- | --- |
| `add-only` | Passive limits across 256 bid levels and 256 ask levels. The book never crosses, and it grows to `--ops` resting orders. |
| `add-cancel` | Passive limits paired with cancels. After a short fill the book stays near `min(ops/4, 512)` orders. Cancels hit the head, middle, or tail of a level. |
| `match-heavy` | Three resting orders swept by a crossing limit or a market order, on both sides, repeating. |
| `mixed` | `fuzz` profile `mixed`: balanced, tight-spread, wide-sweep, and adversarial phases of 500 commands. |
| `pred-market-arb` | End-to-end `ComplementArbStrategy` on the prediction-market preset. Orders/sec counts market events. Latency is one event, including the strategy and the portfolio update. Summary metrics come from that backtest. |

`ReferenceEngine::execute` scans the book to find an id and walks every resting order to check invariants. `FastEngine::execute` does not walk the book in a release build. The gap on `add-only` is mostly that scan, which grows with the book. `add-cancel` is the steady-state comparison.

## Matching rules

Bids match highest price first. Asks match lowest price first. The same price is FIFO by submission order, not by the caller-supplied timestamp. A limit order trades against crossing liquidity, then rests any remainder. A market order trades until it is filled or the opposite book is empty, and the remainder does not rest. Cancel of an unknown, filled, or already-canceled id is reported and does not panic. Zero quantity is rejected and does not consume the order id. An accepted id cannot be reused. After every command the book is uncrossed.

## Project structure

```text
src/
  lib.rs            crate root
  types.rs          ticks, ids, commands, trades, book snapshots
  engine.rs         MatchingEngine
  reference.rs      BTreeMap + VecDeque oracle
  fast.rs           slab, intrusive FIFO levels, fixed-key id map
  fuzz.rs           deterministic workloads and differential checks
  sim.rs            equity and prediction-market simulators
  event_log.rs      streaming CSV log
  replay.rs         replay, book stats, FNV-1a digest
  strategy.rs       Strategy, ComplementArbStrategy, TopOfBookMarketMaker
  portfolio.rs      integer-tick FIFO lots, PnL, drawdown
  backtest.rs       strategy session and report
  bench.rs          workload generation, percentiles, measurements
  bin/
    diff_fuzz.rs
    replay.rs
    backtest.rs
    bench.rs
tests/
  matching.rs       both engines, including FIFO link edits and sweeps
  differential.rs   profiles, invariants, determinism
  replay.rs         log round trip, both presets, streamed replay
  backtest.rs       PnL, fees, complement arb, engine equivalence
```

## Development

CI runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` on stable. The same three commands are the local bar. Release numbers in this file come from `cargo +stable run --release --bin bench` with the default `--ops 200000` and `--seed 1`.
