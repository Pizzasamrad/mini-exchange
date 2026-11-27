//! Differential runs of [`mini_exchange::fuzz`] across every workload profile.
//!
//! These tests stay in the thousands-to-tens-of-thousands of commands so
//! `cargo test` finishes quickly. The `diff_fuzz` binary is the multi-million
//! command run.

use std::collections::BTreeSet;

use mini_exchange::fuzz::{check_stream, generate, run, DiffOptions, FuzzConfig, Profile};
use mini_exchange::{
    CancelRejectReason, Command, CommandResult, MatchingEngine, OrderId, Price, Qty,
    ReferenceEngine, RejectReason, Side, Timestamp,
};

const SEEDS: [u64; 6] = [1, 7, 42, 99, 1_000, 0x00C0_FFEE];

fn assert_fuzz(
    profile: Profile,
    seed: u64,
    orders: u64,
    check_every: u64,
    determinism: bool,
) -> mini_exchange::fuzz::FuzzReport {
    let config = FuzzConfig {
        orders,
        seed,
        profile,
        check_every,
        check_determinism: determinism,
    };
    let report = run(&config).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(report.orders, orders);
    assert_eq!(report.profile, profile);
    assert_eq!(report.seed, seed);
    assert_eq!(report.snapshot_checks, expected_checks(orders, check_every));
    report
}

fn expected_checks(orders: u64, check_every: u64) -> u64 {
    if orders == 0 || check_every == 0 {
        1
    } else {
        orders.div_ceil(check_every)
    }
}

#[test]
fn balanced_profile_matches_across_seeds() {
    for seed in SEEDS {
        let report = assert_fuzz(Profile::Balanced, seed, 2_500, 32, false);
        assert!(report.trades > 0, "seed {seed} produced no trades");
        assert!(report.cancels > 0, "seed {seed} produced no cancels");
    }
}

#[test]
fn tight_spread_profile_matches_across_seeds() {
    for seed in SEEDS {
        let report = assert_fuzz(Profile::TightSpread, seed, 2_500, 32, false);
        assert!(report.trades > 0, "seed {seed} produced no trades");
        assert!(report.cancels > 0, "seed {seed} produced no cancels");
    }
}

#[test]
fn wide_sweep_profile_matches_across_seeds() {
    for seed in SEEDS {
        let report = assert_fuzz(Profile::WideSweep, seed, 2_500, 32, false);
        assert!(report.trades > 0, "seed {seed} produced no trades");
        assert!(report.cancels > 0, "seed {seed} produced no cancels");
    }
}

#[test]
fn adversarial_profile_matches_across_seeds() {
    for seed in SEEDS {
        let report = assert_fuzz(Profile::Adversarial, seed, 2_500, 16, false);
        assert!(report.trades > 0, "seed {seed} produced no trades");
        assert!(report.rejects > 0, "seed {seed} produced no rejects");
    }
}

#[test]
fn mixed_profile_matches_across_seeds() {
    for seed in SEEDS {
        let report = assert_fuzz(Profile::Mixed, seed, 3_000, 40, false);
        assert!(report.trades > 0, "seed {seed} produced no trades");
        assert!(report.cancels > 0, "seed {seed} produced no cancels");
        assert!(report.rejects > 0, "seed {seed} produced no rejects");
    }
}

#[test]
fn replay_is_deterministic_for_every_profile() {
    for profile in [
        Profile::Balanced,
        Profile::TightSpread,
        Profile::WideSweep,
        Profile::Adversarial,
        Profile::Mixed,
    ] {
        for seed in [1_u64, 42] {
            let report = assert_fuzz(profile, seed, 1_200, 48, true);
            assert_ne!(
                report.trade_digest, 0,
                "{profile} seed {seed} left the trade digest at zero"
            );
        }
    }
}

#[test]
fn long_mixed_stream_stays_in_agreement() {
    let report = assert_fuzz(Profile::Mixed, 42, 20_000, 64, true);
    assert!(report.trades > 100);
    assert!(report.snapshot_checks > 100);
}

#[test]
fn different_seeds_do_not_share_a_trade_digest() {
    let first = assert_fuzz(Profile::Mixed, 1, 800, 100, false);
    let second = assert_fuzz(Profile::Mixed, 2, 800, 100, false);
    assert_ne!(first.trade_digest, second.trade_digest);
    assert_ne!(first.result_digest, second.result_digest);
}

#[test]
fn every_command_checks_the_book_and_a_zero_cadence_checks_once() {
    let each = assert_fuzz(Profile::Balanced, 7, 400, 1, false);
    assert_eq!(each.snapshot_checks, 400);
    let once = assert_fuzz(Profile::TightSpread, 3, 350, 0, false);
    assert_eq!(once.snapshot_checks, 1);
}

#[test]
fn empty_stream_checks_the_empty_book() {
    let report = assert_fuzz(Profile::Balanced, 1, 0, 10, true);
    assert_eq!(report.trades, 0);
    assert_eq!(report.snapshot_checks, 1);
}

#[test]
fn hand_written_stream_matches_and_conserves() {
    let commands = vec![
        Command::limit(OrderId(1), Side::Buy, Price(10), Qty(4), Timestamp(1)),
        Command::limit(OrderId(2), Side::Buy, Price(10), Qty(4), Timestamp(2)),
        Command::limit(OrderId(3), Side::Buy, Price(9), Qty(3), Timestamp(3)),
        Command::limit(OrderId(4), Side::Sell, Price(12), Qty(5), Timestamp(4)),
        Command::cancel(OrderId(2)),
        Command::limit(OrderId(5), Side::Sell, Price(10), Qty(6), Timestamp(5)),
        Command::market(OrderId(6), Side::Buy, Qty(10), Timestamp(6)),
        Command::cancel(OrderId(1)),
        Command::cancel(OrderId(0)),
        Command::limit(OrderId(5), Side::Buy, Price(1), Qty(1), Timestamp(9)),
        Command::market(OrderId(7), Side::Sell, Qty(0), Timestamp(10)),
    ];
    let report = check_stream(&commands, &DiffOptions::new(Profile::Balanced, 11))
        .unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(report.orders, commands.len() as u64);
    assert!(report.trades >= 2);
    assert!(report.cancels >= 1);
    assert!(report.rejects >= 2);
}

#[test]
fn adversarial_stream_hits_every_edge() {
    let commands = generate(Profile::Adversarial, 1, 80);
    assert!(
        commands
            .iter()
            .any(|command| matches!(command, Command::Limit { qty, .. } if qty.is_zero())),
        "missing a zero-quantity limit"
    );
    assert!(
        commands
            .iter()
            .any(|command| matches!(command, Command::Market { qty, .. } if qty.is_zero())),
        "missing a zero-quantity market"
    );
    assert!(has_limit_price(&commands, 0), "missing price 0");
    assert!(has_limit_price(&commands, 1), "missing price 1");
    assert!(
        has_limit_price(&commands, 3),
        "missing the reserved huge-order price"
    );
    assert!(
        has_limit_price(&commands, u64::MAX - 1),
        "missing price u64::MAX - 1"
    );
    assert!(
        has_limit_price(&commands, u64::MAX),
        "missing price u64::MAX"
    );
    assert!(
        commands
            .iter()
            .any(|command| matches!(command, Command::Limit { qty, .. } if qty.get() == u64::MAX)),
        "missing a maximum quantity"
    );
    assert!(
        commands
            .iter()
            .any(|command| matches!(command, Command::Cancel { id } if id.get() == 0)),
        "missing a cancel of order 0"
    );
    assert!(
        commands
            .iter()
            .any(|command| matches!(command, Command::Cancel { id } if id.get() == u64::MAX)),
        "missing a cancel of u64::MAX"
    );
    assert!(
        has_duplicate_submit(&commands),
        "missing a repeated non-zero order id"
    );

    let mut engine = ReferenceEngine::new();
    let mut accepted = BTreeSet::new();
    let mut saw_zero = false;
    let mut saw_zero_on_consumed_id = false;
    let mut saw_duplicate = false;
    let mut saw_missing = false;
    let mut saw_filled = false;
    let mut saw_canceled = false;
    for command in &commands {
        let result = engine.execute(*command);
        match (command, &result) {
            (
                Command::Limit { id, qty, .. } | Command::Market { id, qty, .. },
                CommandResult::Rejected { reason, .. },
            ) => {
                if qty.is_zero() {
                    saw_zero = true;
                    if accepted.contains(id) {
                        assert_eq!(*reason, RejectReason::ZeroQuantity);
                        saw_zero_on_consumed_id = true;
                    }
                } else if *reason == RejectReason::DuplicateOrderId {
                    saw_duplicate = true;
                }
            }
            (_, CommandResult::CancelRejected { reason, .. }) => match reason {
                CancelRejectReason::NotFound => saw_missing = true,
                CancelRejectReason::AlreadyFilled => saw_filled = true,
                CancelRejectReason::AlreadyCanceled => saw_canceled = true,
            },
            (_, CommandResult::Accepted { order_id, .. }) => {
                accepted.insert(*order_id);
            }
            _ => {}
        }
    }
    assert!(saw_zero, "zero quantity was not rejected");
    assert!(
        saw_zero_on_consumed_id,
        "zero quantity lost to the duplicate-id check"
    );
    assert!(saw_duplicate, "duplicate id was not rejected");
    assert!(saw_missing, "unknown cancel was not rejected");
    assert!(saw_filled, "cancel of a filled id was not rejected");
    assert!(saw_canceled, "cancel of a canceled id was not rejected");

    assert_fuzz(Profile::Adversarial, 1, 1_500, 10, true);
}

#[test]
fn tight_spread_cancels_the_head_middle_and_tail_of_a_price() {
    let commands = generate(Profile::TightSpread, 7, 200);
    assert!(
        has_fifo_cancel_burst(&commands),
        "tight-spread did not cancel the head, tail, and middle of a queue"
    );
    let report = assert_fuzz(Profile::TightSpread, 7, 600, 1, false);
    assert!(report.cancels > 10);
}

#[test]
fn wide_sweep_builds_levels_then_fires_an_empty_market() {
    let commands = generate(Profile::WideSweep, 9, 250);
    assert!(
        has_decreasing_bid_run(&commands, 8),
        "wide-sweep did not create a decreasing bid ladder"
    );
    assert!(
        has_empty_book_market_pair(&commands, Side::Buy),
        "wide-sweep did not follow a clearing buy with an empty-book buy"
    );
    assert!(
        has_empty_book_market_pair(&commands, Side::Sell),
        "wide-sweep did not follow a clearing sell with an empty-book sell"
    );
    assert_fuzz(Profile::WideSweep, 9, 800, 20, false);
}

#[test]
fn mixed_profile_visits_every_phase() {
    let commands = generate(Profile::Mixed, 5, 2_500);
    assert!(
        commands
            .iter()
            .any(|command| matches!(command, Command::Cancel { id } if id.get() == 0)),
        "mixed stream never reached the adversarial cancels"
    );
    assert!(
        has_fifo_cancel_burst(&commands),
        "mixed stream never reached the tight-spread queue"
    );
    assert!(
        has_decreasing_bid_run(&commands, 8),
        "mixed stream never reached the wide ladder"
    );
    assert!(
        has_empty_book_market_pair(&commands, Side::Buy),
        "mixed stream never reached the empty-book market"
    );
    assert_fuzz(Profile::Mixed, 5, 2_500, 25, false);
}

fn has_limit_price(commands: &[Command], price: u64) -> bool {
    commands.iter().any(|command| {
        matches!(command, Command::Limit { price: level, qty, .. }
            if level.ticks() == price && !qty.is_zero())
    })
}

fn has_duplicate_submit(commands: &[Command]) -> bool {
    let mut seen = BTreeSet::new();
    for command in commands {
        if let Command::Limit { id, qty, .. } | Command::Market { id, qty, .. } = command {
            if qty.is_zero() {
                continue;
            }
            if !seen.insert(*id) {
                return true;
            }
        }
    }
    false
}

fn has_fifo_cancel_burst(commands: &[Command]) -> bool {
    for index in 0..commands.len() {
        let Some(streak) = same_price_limits(&commands[index..]) else {
            continue;
        };
        if streak.len() < 5 {
            continue;
        }
        let head = streak[0];
        let tail = streak[streak.len() - 1];
        let middle = streak[streak.len() / 2];
        let next = index + streak.len();
        if commands.get(next) == Some(&Command::cancel(head))
            && commands.get(next + 1) == Some(&Command::cancel(tail))
            && commands.get(next + 2) == Some(&Command::cancel(middle))
        {
            return true;
        }
    }
    false
}

fn same_price_limits(commands: &[Command]) -> Option<Vec<OrderId>> {
    let Command::Limit {
        id,
        side,
        price,
        qty,
        ..
    } = commands.first().copied()?
    else {
        return None;
    };
    if qty.is_zero() {
        return None;
    }
    let mut ids = vec![id];
    for command in &commands[1..] {
        match command {
            Command::Limit {
                id,
                side: order_side,
                price: order_price,
                qty,
                ..
            } if *order_side == side && *order_price == price && !qty.is_zero() => {
                ids.push(*id);
            }
            _ => break,
        }
    }
    Some(ids)
}

fn has_decreasing_bid_run(commands: &[Command], need: usize) -> bool {
    let mut previous: Option<u64> = None;
    let mut run = 0usize;
    for command in commands {
        match command {
            Command::Limit {
                side: Side::Buy,
                price,
                qty,
                ..
            } if !qty.is_zero() => {
                if previous.is_some_and(|previous| price.ticks() < previous) {
                    run += 1;
                } else {
                    run = 1;
                }
                if run >= need {
                    return true;
                }
                previous = Some(price.ticks());
            }
            _ => {
                previous = None;
                run = 0;
            }
        }
    }
    false
}

fn has_empty_book_market_pair(commands: &[Command], side: Side) -> bool {
    commands.windows(2).any(|pair| {
        matches!(pair[0], Command::Market { side: order_side, qty, .. }
            if order_side == side && qty.get() >= 10_000)
            && matches!(pair[1], Command::Market { side: order_side, qty, .. }
                if order_side == side && qty.get() == 1)
    })
}
