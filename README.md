# mini-exchange

Deterministic miniature stock exchange: an order-book matching engine, a seeded order-flow simulator, an exact replayer, and a backtester.

Prices and quantities are integer ticks. Engines do not read the clock and do not use randomness. The same command sequence always produces the same trades and the same book.

## Roadmap

1. **Core types and reference engine** — the `MatchingEngine` trait and an obviously-correct price-time `ReferenceEngine`.
2. **Fast, cache-friendly engine** — `FastEngine` behind the same trait.
3. **Differential fuzzing and invariant checks** — deterministic workloads compared on `ReferenceEngine` and `FastEngine`, plus a `diff_fuzz` binary.
4. Deterministic order-stream simulator and replayer.
5. **Backtester and trading strategy API** — integer-tick portfolio accounting, `ComplementArbStrategy`, and a top-of-book market maker.
6. Benchmarks (orders/sec, latency percentiles) and a full guide.

## Phase 1

- Domain types in `src/types.rs`: tick prices, quantities, order ids, logical timestamps, sides, limit / market / cancel commands, trades, command results, and owned book snapshots.
- `MatchingEngine` in `src/engine.rs`. Book queries (`best_bid`, `best_ask`, `depth`, `resting_order`, `book`) are defined so two engines can be compared with ordinary equality. `depth` is the canonical view; the other queries are derived from it.
- `ReferenceEngine` in `src/reference.rs`. Prices live in a `BTreeMap` and each level is a FIFO `VecDeque`. Clarity is the goal.

Matching rules:

- Bids match highest price first, asks lowest price first. The same price is FIFO by submission order, not by the caller-supplied timestamp.
- A trade prints at the resting (maker) order's price.
- A limit order trades against crossing liquidity, then rests any remainder.
- A market order trades until it is filled or the opposite book is empty. The remainder does not rest.
- Cancel removes a resting order. Unknown, already filled, and already canceled ids are reported and do not panic.
- Zero quantity is rejected and does not consume the order id. An accepted id cannot be reused.

## Phase 2

`FastEngine` in `src/fast.rs` implements the same `MatchingEngine` contract as `ReferenceEngine`.

- Orders live in a slab. Each price level is an intrusive doubly-linked FIFO of slab indices, so a cancel rewrites links and a quantity and does not allocate.
- An order-id map records a live slab slot, or that the id already filled or left the book. Lookup and cancel are one hash probe. The hasher has fixed keys, so the engine still does not read the OS random source.
- Each side tracks its best level and links levels from best to worst. Emptying the inside price promotes the next level without a scan. A `BTreeMap` finds a price and its neighbors when a level is created.
- Slabs reuse slots after fills and cancels. Steady-state matching does not allocate resting-order storage. The engine is safe Rust: links are indices.

`depth`, `best_bid`, `best_ask`, `resting_order`, and `book` follow the same snapshot rules, so a `FastEngine` book compares equal to a `ReferenceEngine` book.

## Phase 3

`src/fuzz.rs` generates a command stream and runs it through both engines.

- The generator is a pure function of a profile and a seed. It does not read the operating system's random source.
- Profiles: `balanced` (limit, market, and cancel mix around a moving mid), `tight-spread` (long FIFO queues with head, middle, and tail cancels), `wide-sweep` (level creation, eviction, multi-level sweeps, empty-book markets), `adversarial` (duplicate ids, zero quantities, stale cancels, price and quantity extremes), and `mixed` (the four in rotation).
- Every command must return the same `CommandResult`, including trades in order and reject reasons.
- Every N commands, and always at the end, `fast.book()` must equal `reference.book()`. The book must be uncrossed, strictly price-ordered, free of empty levels and zero-quantity orders, and conserve quantity: submitted limit size equals fills plus cancels plus resting size, and a market fill never exceeds its submitted size.
- The same seed is replayed. The final book and the trade-stream digest must match. A mismatch names the seed, profile, command index, command, and both outputs.

```bash
cargo run --release --bin diff_fuzz -- --orders 2000000 --seed 42 --profile mixed
cargo run --release --bin diff_fuzz -- --orders 500000 --seed 7 --profile tight-spread
```

`orders/sec` counts every pass. The default run replays the seed, so that rate includes the determinism check.

## Phase 4

`src/event_log.rs`, `src/sim.rs`, and `src/replay.rs` record an order stream, synthesize one, and play it back.

The log is a small CSV file. Comments start with `#`. The header is required, and `seq` is a strictly increasing logical timestamp:

```text
# mini-exchange event log v1
seq,market,cmd,id,side,price,qty
1,DEFAULT,limit,1,buy,10000,25
2,YES,market,2,sell,,3
3,NO,cancel,1,,,
```

- `market` is a symbol. An empty field means `DEFAULT`, which is what a single-book stream uses. `YES` and `NO` name the two legs of a binary contract.
- `limit` needs a side, an integer tick price, and a quantity. `market` needs a side and a quantity, and leaves the price empty. `cancel` carries only the order id.
- `EventReader` and `EventWriter` stream through `BufRead` and `Write`. A bad line returns an error that includes its line number. The reader does not collect the file.

The simulator is a pure function of a preset and a seed. It does not read the operating system's random source. A latent fair value mean-reverts. Liquidity providers post layered bids and asks around it, takers send crossing limits and market orders, and cancels prefer orders that are old or far from the fair value.

- `equity` is one book centered on 10_000 ticks.
- `prediction-market` is a YES/NO pair. Every price is inside `1..=99`. NO tracks `100 - YES` except during short shocks that open a complement gap.

`replay::run` feeds any `MatchingEngine` from a slice, a `Vec`, or an iterator. `replay::run_reader` streams a file. `ReplayReport` counts commands, limits, market orders, cancels, and rejects, and records trades, share volume, VWAP, the last trade, sampled top-of-book statistics, and the final canonical book of each market. Its digest is FNV-1a over the trade tape and those books, so two runs match when the digests are equal. `replay::verify` replays the same stream through `FastEngine` and `ReferenceEngine` and requires identical results and digests.

```bash
cargo run --release --bin replay -- generate --preset equity --orders 1000000 --seed 1 --out equity.log
cargo run --release --bin replay -- generate --preset prediction-market --orders 1000000 --seed 1 --out pred.log
cargo run --release --bin replay -- run --input equity.log --engine fast
cargo run --release --bin replay -- run --input pred.log --engine reference --verify
```

`orders/sec` on a verified run counts both engines. `--sample-every 0` samples only the final book.

## Phase 5

`src/strategy.rs`, `src/portfolio.rs`, and `src/backtest.rs` replay any event stream while a strategy trades against it.

A `Strategy` sees each market event after the engine applies it: the logical sequence, the market, the command result, the top of book on every active market, and a read-only portfolio snapshot. It returns zero or more `Limit`, `Market`, or `Cancel` actions. Strategy order ids start at `1 << 62`, so they stay out of the simulator's id space and out of an ordinary recorded log. A log that already uses one of those ids is rejected.

The backtester drives `FastEngine` or `ReferenceEngine`. Cash, fees, cost basis, and PnL are integer ticks. A fill updates a FIFO lot queue. Realized PnL is the closing cashflow minus the cost of the lots it consumes. Open inventory is marked to the mid (or to the touch). A long YES share together with a long NO share, or a short pair, is marked at the 100-tick settlement value; only the unmatched remainder uses the book. Equity is cash plus that inventory value. The run tracks peak equity and the worst drawdown in ticks and in basis points.

`ComplementArbStrategy` is the binary-contract strategy. It buys both legs when `ask(YES) + ask(NO) + fees <= 100 - min_edge`, and it can sell the pair when the bids pay `100 + min_edge` after fees. Size is the minimum of the two top-of-book quantities, `max_order_qty`, and the room under `max_position`. Orders are immediate-or-cancel limits at the touch, so a partial clip does not rest. If the legs are already uneven, the next decision buys the smaller leg only when that hedge still locks a non-negative edge, and it will not open a new pair while the imbalance is above `max_unhedged`.

`TopOfBookMarketMaker` is the single-book baseline. It joins the bid and the ask, cancels a quote that has left the touch, and stops quoting a side that would push inventory through `max_position`.

The report's digest is FNV-1a over the fee schedule, every strategy fill, the equity path, and the final portfolio and books. `--verify` requires `FastEngine` and `ReferenceEngine` to produce the same report and the same digest. One tick is one cent in the dollar column.

```bash
cargo run --release --bin backtest -- --preset prediction-market --orders 100000 --seed 1 --strategy complement-arb --min-edge 1 --max-qty 20 --verify
cargo run --release --bin backtest -- --preset equity --orders 100000 --seed 1 --strategy market-maker --engine fast
cargo run --release --bin backtest -- --input pred.log --strategy complement-arb --fee-ticks 1 --verify
```

## Tests

The behavioral suite runs against both engines. Extra cases cover FIFO link edits (head, middle, and tail), level eviction, multi-level sweeps, slab reuse, and fixed-seed streams compared side by side. `tests/replay.rs` covers the event-log round trip, line-numbered parse errors, replay determinism, `FastEngine` / `ReferenceEngine` equivalence on both presets, prediction-market tick bounds, and a streamed log. `tests/backtest.rs` covers exact PnL and drawdown, fees, partial fills, YES/NO settlement, complement-arb size limits, determinism, and engine equivalence.

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
