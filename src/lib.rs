//! Deterministic order-book matching for a miniature exchange.
//!
//! Prices and quantities are integer ticks. A [`MatchingEngine`] does not read
//! a clock and does not draw random numbers, so the same command sequence
//! always produces the same trades and the same book.
//!
//! [`ReferenceEngine`] is the straightforward price-time implementation.
//! [`FastEngine`] follows the same contract with a slab layout built for
//! throughput. Later milestones differentially check the two and add a
//! backtester.

#![forbid(unsafe_code)]

pub mod engine;
pub mod fast;
pub mod reference;
pub mod types;

pub use engine::MatchingEngine;
pub use fast::FastEngine;
pub use reference::ReferenceEngine;
pub use types::{
    Book, CancelRejectReason, Command, CommandResult, Level, OrderId, Price, Qty, Quote,
    RejectReason, RestingOrder, Side, Timestamp, Trade,
};
