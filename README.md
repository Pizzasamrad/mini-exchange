# mini-exchange

Deterministic miniature stock exchange: an order-book matching engine and, in later milestones, a backtesting simulator.

Prices and quantities are integer ticks. Engines do not read the clock and do not use randomness. The same command sequence always produces the same trades and the same book.

## Roadmap

1. **Core types and reference engine** — the `MatchingEngine` trait and an obviously-correct price-time `ReferenceEngine`.
2. **Fast, cache-friendly engine** — `FastEngine` behind the same trait.
3. Differential fuzzing (fast vs. reference) and an invariant checker.
4. Deterministic order-stream simulator and replayer.
5. Backtester and trading strategy API, including an arbitrage strategy.
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

## Tests

The behavioral suite runs against both engines. Extra cases cover FIFO link edits (head, middle, and tail), level eviction, multi-level sweeps, slab reuse, and fixed-seed streams compared side by side.

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
