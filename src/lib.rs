//! Deterministic order-book matching for a miniature exchange.
//!
//! Prices and quantities are integer ticks. A [`MatchingEngine`] does not read
//! a clock and does not draw random numbers, so the same command sequence
//! always produces the same trades and the same book.
//!
//! [`ReferenceEngine`] is the straightforward price-time implementation.
//! [`FastEngine`] follows the same contract with a slab layout built for
//! throughput. [`fuzz`] runs the same deterministic command stream through
//! both and checks book invariants. [`sim`] draws a seeded order stream,
//! [`event_log`] records it, and [`replay`] plays that log back through either
//! engine. [`backtest`] replays a stream while a [`strategy`] trades, and
//! accounts for the result in integer ticks.

#![forbid(unsafe_code)]

pub mod backtest;
pub mod engine;
pub mod event_log;
pub mod fast;
pub mod fuzz;
pub mod portfolio;
pub mod reference;
pub mod replay;
pub mod sim;
pub mod strategy;
pub mod types;

pub use backtest::{BacktestConfig, BacktestError, BacktestReport, TopSnapshot, VerifiedBacktest};
pub use engine::MatchingEngine;
pub use event_log::{Event, EventReader, EventWriter, MarketId, ParseError, ReadError};
pub use fast::FastEngine;
pub use portfolio::{
    AccountSnapshot, ArbStats, DrawdownTracker, MarkMethod, Portfolio, PortfolioError,
    PositionSnapshot, TopOfBook, COMPLEMENT_PAYOUT,
};
pub use reference::ReferenceEngine;
pub use replay::{
    run, run_reader, verify, verify_reader, BookStats, MarketSnapshot, ReplayOptions, ReplayReport,
    Replayer, StreamError, VerifiedReplay, VerifyError,
};
pub use sim::{generate, Preset, SimConfig, Simulator};
pub use strategy::{
    Action, ActionUpdate, ComplementArbConfig, ComplementArbStrategy, LiveOrder, MarketMakerConfig,
    MarketUpdate, Strategy, StrategyIds, TopOfBookMarketMaker,
};
pub use types::{
    Book, CancelRejectReason, Command, CommandResult, Level, OrderId, Price, Qty, Quote,
    RejectReason, RestingOrder, Side, Timestamp, Trade,
};
