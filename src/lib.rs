//! Deterministic order-book matching for a miniature exchange.
//!
//! Prices and quantities are integer ticks. A [`MatchingEngine`] does not read
//! a clock and does not draw random numbers, so the same command sequence
//! always produces the same trades and the same book.
//!
//! Phase 1 is the shared [`MatchingEngine`] contract and [`ReferenceEngine`],
//! a straightforward price-time implementation. Later milestones add a fast
//! engine behind the same trait, differential checks against this reference,
//! and a backtester.

pub mod engine;
pub mod reference;
pub mod types;

pub use engine::MatchingEngine;
pub use reference::ReferenceEngine;
pub use types::{
    Book, CancelRejectReason, Command, CommandResult, Level, OrderId, Price, Qty, Quote,
    RejectReason, RestingOrder, Side, Timestamp, Trade,
};
