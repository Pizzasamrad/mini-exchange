//! Portfolio accounting, complement arbitrage, and deterministic backtests.
//!
//! Hand-crafted sequences pin exact cash, PnL, and drawdown. Synthetic
//! prediction-market streams check that the arbitrage strategy makes money
//! without exceeding its size caps, and that FastEngine and ReferenceEngine
//! produce the same report.

use std::collections::BTreeMap;

use mini_exchange::backtest::{self, BacktestConfig, BacktestError, BacktestReport};
use mini_exchange::event_log::{Event, MarketId};
use mini_exchange::portfolio::{
    DrawdownTracker, MarkMethod, Portfolio, TopOfBook, COMPLEMENT_PAYOUT,
};
use mini_exchange::sim::{generate, Preset, SimConfig};
use mini_exchange::strategy::{
    Action, ComplementArbConfig, ComplementArbStrategy, MarketMakerConfig, MarketUpdate, Strategy,
    StrategyIds, TopOfBookMarketMaker,
};
use mini_exchange::{CommandResult, FastEngine, OrderId, Price, Qty, Quote, RejectReason, Side};

fn cash(ticks: i128) -> BacktestConfig {
    BacktestConfig::new(ticks)
}

fn fee_config(ticks: i128, fee: u64) -> BacktestConfig {
    BacktestConfig::new(ticks).with_fee(fee)
}

fn limit(seq: u64, market: MarketId, id: u64, side: Side, price: u64, qty: u64) -> Event {
    Event::limit(seq, market, OrderId(id), side, Price(price), Qty(qty))
}

fn cancel(seq: u64, market: MarketId, id: u64) -> Event {
    Event::cancel(seq, market, OrderId(id))
}

fn quote(price: u64, qty: u64) -> Quote {
    Quote {
        price: Price(price),
        qty: Qty(qty),
    }
}

fn top(bid: Option<(u64, u64)>, ask: Option<(u64, u64)>) -> TopOfBook {
    TopOfBook {
        bid: bid.map(|(price, qty)| quote(price, qty)),
        ask: ask.map(|(price, qty)| quote(price, qty)),
        last: None,
    }
}

fn fill(portfolio: &mut Portfolio, market: &MarketId, side: Side, price: u64, qty: u64, fee: u64) {
    portfolio
        .apply_fill(market, side, Price(price), Qty(qty), fee)
        .unwrap();
}

fn assert_consistent(report: &BacktestReport) {
    assert_eq!(
        report.net_pnl,
        report.realized_pnl + report.unrealized_pnl,
        "net pnl"
    );
    assert_eq!(
        report.final_equity,
        report.initial_cash + report.net_pnl,
        "equity"
    );
    assert_eq!(
        report.final_equity,
        report.final_cash + report.inventory_value,
        "cash plus inventory"
    );
    assert!(report.peak_equity >= report.final_equity);
    assert!(report.max_drawdown_ticks >= report.peak_equity - report.final_equity);
    let open: i128 = report.positions.iter().map(|pos| pos.open_cashflow).sum();
    assert_eq!(report.unrealized_pnl, report.inventory_value - open);
}

fn drive<S: Strategy>(events: Vec<Event>, strategy: S, config: BacktestConfig) -> BacktestReport {
    let report = backtest::run(events, |_| FastEngine::new(), strategy, config).unwrap();
    assert_consistent(&report);
    report
}

struct Script {
    steps: Vec<Vec<Action>>,
    calls: usize,
}

impl Script {
    fn new(steps: Vec<Vec<Action>>) -> Self {
        Self { steps, calls: 0 }
    }
}

impl Strategy for Script {
    fn on_market(&mut self, _update: &MarketUpdate<'_>) -> Vec<Action> {
        let index = self.calls;
        self.calls += 1;
        if index < self.steps.len() {
            self.steps[index].clone()
        } else {
            Vec::new()
        }
    }
}

fn ioc(market: MarketId, side: Side, price: u64, qty: u64) -> Action {
    Action::ioc_limit(market, side, Price(price), Qty(qty))
}

fn book_of<'a>(report: &'a BacktestReport, name: &str) -> &'a mini_exchange::TopSnapshot {
    report
        .books
        .iter()
        .find(|book| book.market.as_str() == name)
        .unwrap_or_else(|| panic!("missing book {name}"))
}

fn position_of<'a>(report: &'a BacktestReport, name: &str) -> &'a mini_exchange::PositionSnapshot {
    report
        .positions
        .iter()
        .find(|pos| pos.market.as_str() == name)
        .unwrap_or_else(|| panic!("missing position {name}"))
}

#[test]
fn partial_close_fees_and_a_flip_use_exact_ticks() {
    let market = MarketId::default_market();
    let mut portfolio = Portfolio::new(0);
    fill(&mut portfolio, &market, Side::Buy, 100, 10, 0);
    let effect = portfolio
        .apply_fill(&market, Side::Sell, Price(110), Qty(4), 0)
        .unwrap();
    assert_eq!(effect.realized_pnl, 40);
    assert_eq!(effect.closed_qty, 4);
    assert_eq!(effect.net, 6);
    assert_eq!(portfolio.open_cashflow(&market), 600);
    assert_eq!(portfolio.avg_cost_ticks(&market), Some(100));
    assert_eq!(portfolio.cash(), -1_000 + 440);

    let rest = portfolio
        .apply_fill(&market, Side::Sell, Price(90), Qty(6), 0)
        .unwrap();
    assert_eq!(rest.realized_pnl, -60);
    assert_eq!(portfolio.realized_pnl(), -20);
    assert_eq!(portfolio.net(&market), 0);
    assert_eq!(portfolio.open_cashflow(&market), 0);
    assert_eq!(portfolio.cash(), -20);

    let mut portfolio = Portfolio::new(0);
    fill(&mut portfolio, &market, Side::Buy, 100, 10, 1);
    assert_eq!(portfolio.fees_paid(), 10);
    assert_eq!(portfolio.open_cashflow(&market), 1_010);
    let sold = portfolio
        .apply_fill(&market, Side::Sell, Price(110), Qty(10), 1)
        .unwrap();
    assert_eq!(sold.realized_pnl, 80);
    assert_eq!(portfolio.fees_paid(), 20);
    assert_eq!(portfolio.cash(), 80);
    assert_eq!(portfolio.net(&market), 0);

    let mut portfolio = Portfolio::new(0);
    fill(&mut portfolio, &market, Side::Buy, 10, 5, 0);
    let flip = portfolio
        .apply_fill(&market, Side::Sell, Price(12), Qty(8), 0)
        .unwrap();
    assert_eq!(flip.realized_pnl, 10);
    assert_eq!(flip.closed_qty, 5);
    assert_eq!(flip.opened_qty, 3);
    assert_eq!(flip.net, -3);
    assert_eq!(portfolio.open_cashflow(&market), -36);
    assert_eq!(portfolio.cash(), -50 + 96);
}

#[test]
fn yes_no_pairs_settle_at_100_and_the_fifo_tail_is_marked() {
    let yes = MarketId::yes();
    let no = MarketId::no();
    let mut portfolio = Portfolio::new(0);
    fill(&mut portfolio, &yes, Side::Buy, 40, 5, 0);
    fill(&mut portfolio, &no, Side::Buy, 50, 5, 0);
    let flat_books = BTreeMap::new();
    let paired = portfolio.account(&flat_books, MarkMethod::Mid).unwrap();
    assert_eq!(paired.inventory_value, i128::from(COMPLEMENT_PAYOUT) * 5);
    assert_eq!(paired.unrealized_pnl, 50);
    assert_eq!(paired.equity, 50);
    assert_eq!(portfolio.arb_stats().qty, 5);
    assert_eq!(portfolio.arb_stats().edge_sum, 50);
    assert_eq!(portfolio.arb_stats().wins, 5);
    assert_eq!(portfolio.arb_stats().avg_edge_ticks(), Some(10));

    fill(&mut portfolio, &yes, Side::Buy, 60, 3, 0);
    let at_cost = portfolio.account(&flat_books, MarkMethod::Mid).unwrap();
    assert_eq!(
        at_cost.unrealized_pnl, 50,
        "unquoted residual stays at cost"
    );

    let mut books = BTreeMap::new();
    books.insert(yes.clone(), top(Some((30, 1)), None));
    let marked = portfolio.account(&books, MarkMethod::Mid).unwrap();
    // Five pairs are worth 500. The newest three YES shares were 60 and are
    // now marked at 30, so the residual loses 90 and total unrealized is -40.
    assert_eq!(marked.inventory_value, 500 + 90);
    assert_eq!(marked.unrealized_pnl, 50 - 90);
    assert_eq!(marked.equity, -40);
    assert_eq!(portfolio.arb_stats().qty, 5);

    let mut portfolio = Portfolio::new(0);
    fill(&mut portfolio, &yes, Side::Buy, 40, 4, 0);
    fill(&mut portfolio, &no, Side::Buy, 50, 4, 0);
    assert_eq!(portfolio.arb_stats().qty, 4);
    assert_eq!(portfolio.arb_stats().edge_sum, 40);
    fill(&mut portfolio, &yes, Side::Sell, 60, 1, 0);
    fill(&mut portfolio, &no, Side::Sell, 60, 1, 0);
    assert_eq!(
        portfolio.arb_stats().qty,
        4,
        "a partial close is not a new pair"
    );
    assert_eq!(portfolio.arb_stats().edge_sum, 40);
    fill(&mut portfolio, &yes, Side::Buy, 30, 1, 0);
    fill(&mut portfolio, &no, Side::Buy, 60, 1, 0);
    assert_eq!(portfolio.arb_stats().qty, 5);
    assert_eq!(portfolio.arb_stats().edge_sum, 50);
}

#[test]
fn touch_and_mid_marks_differ_and_an_empty_book_uses_the_last_trade() {
    let market = MarketId::default_market();
    let mut portfolio = Portfolio::new(1_000);
    fill(&mut portfolio, &market, Side::Buy, 100, 10, 0);
    let mut books = BTreeMap::new();
    books.insert(market.clone(), top(Some((90, 4)), Some((110, 4))));
    let mid = portfolio.account(&books, MarkMethod::Mid).unwrap();
    let touch = portfolio.account(&books, MarkMethod::Touch).unwrap();
    assert_eq!(mid.unrealized_pnl, 0);
    assert_eq!(touch.unrealized_pnl, -100);

    books.insert(
        market.clone(),
        TopOfBook {
            bid: None,
            ask: None,
            last: Some(Price(80)),
        },
    );
    let last = portfolio.account(&books, MarkMethod::Mid).unwrap();
    assert_eq!(last.unrealized_pnl, -200);

    books.insert(market.clone(), TopOfBook::default());
    let cost = portfolio.account(&books, MarkMethod::Mid).unwrap();
    assert_eq!(cost.unrealized_pnl, 0);
    assert_eq!(cost.equity, 1_000);
}

#[test]
fn drawdown_tracks_the_worst_peak_to_trough_and_its_basis_points() {
    let mut tracker = DrawdownTracker::new(10_000);
    tracker.observe(9_700);
    tracker.observe(10_250);
    tracker.observe(10_200);
    assert_eq!(tracker.peak(), 10_250);
    assert_eq!(tracker.max_drawdown_ticks(), 300);
    assert_eq!(tracker.max_drawdown_bps(), 300);
}

#[test]
fn a_losing_stretch_then_a_recovery_keeps_the_max_drawdown() {
    let market = MarketId::default_market();
    let mut steps = vec![Vec::new(); 9];
    steps[1] = vec![ioc(market.clone(), Side::Buy, 100, 10)];
    steps[8] = vec![ioc(market.clone(), Side::Sell, 120, 10)];
    let events = vec![
        limit(1, market.clone(), 1, Side::Buy, 90, 10),
        limit(2, market.clone(), 2, Side::Sell, 100, 10),
        cancel(3, market.clone(), 1),
        limit(4, market.clone(), 3, Side::Buy, 70, 10),
        limit(5, market.clone(), 4, Side::Sell, 80, 10),
        cancel(6, market.clone(), 4),
        cancel(7, market.clone(), 3),
        limit(8, market.clone(), 5, Side::Buy, 120, 10),
        limit(9, market.clone(), 6, Side::Sell, 130, 10),
    ];
    let report = drive(events, Script::new(steps), cash(10_000));
    assert_eq!(report.realized_pnl, 200);
    assert_eq!(report.unrealized_pnl, 0);
    assert_eq!(report.net_pnl, 200);
    assert_eq!(report.final_cash, 10_200);
    assert_eq!(report.final_equity, 10_200);
    assert_eq!(report.peak_equity, 10_250);
    assert_eq!(report.max_drawdown_ticks, 300);
    assert_eq!(report.max_drawdown_bps, 300);
    assert_eq!(report.strategy_fills, 2);
    assert_eq!(report.closed_qty, 10);
    assert_eq!(report.close_wins, 10);
    assert_eq!(position_of(&report, "DEFAULT").net, 0);
}

#[test]
fn fees_and_a_partial_ioc_fill_are_exact_on_the_engine() {
    let market = MarketId::default_market();
    let buy_then_sell = vec![
        vec![ioc(market.clone(), Side::Buy, 50, 10)],
        vec![ioc(market.clone(), Side::Sell, 60, 10)],
    ];
    let round_trip = vec![
        limit(1, market.clone(), 1, Side::Sell, 50, 10),
        limit(2, market.clone(), 2, Side::Buy, 60, 10),
    ];
    let report = drive(
        round_trip,
        Script::new(buy_then_sell),
        fee_config(10_000, 1),
    );
    assert_eq!(report.fees_paid, 20);
    assert_eq!(report.realized_pnl, 80);
    assert_eq!(report.unrealized_pnl, 0);
    assert_eq!(report.net_pnl, 80);
    assert_eq!(report.final_equity, 10_080);
    assert_eq!(report.peak_equity, 10_090);
    assert_eq!(report.max_drawdown_ticks, 10);
    assert_eq!(report.max_drawdown_bps, 10);

    let partial = vec![vec![
        ioc(market.clone(), Side::Buy, 40, 4),
        ioc(market.clone(), Side::Buy, 40, 10),
    ]];
    let report = drive(
        vec![limit(1, market.clone(), 1, Side::Sell, 40, 10)],
        Script::new(partial),
        cash(10_000),
    );
    assert_eq!(report.strategy_orders, 2);
    assert_eq!(report.strategy_fills, 2);
    assert_eq!(report.strategy_volume, 10);
    assert_eq!(report.strategy_rejects, 0);
    assert_eq!(position_of(&report, "DEFAULT").net, 10);
    assert_eq!(position_of(&report, "DEFAULT").open_cashflow, 400);
    assert!(book_of(&report, "DEFAULT").ask.is_none());
    assert!(book_of(&report, "DEFAULT").bid.is_none());
    assert_eq!(report.unrealized_pnl, 0);
    assert_eq!(report.final_cash, 9_600);
}

#[test]
fn complement_arb_locks_the_settlement_edge_and_respects_size() {
    let yes = MarketId::yes();
    let no = MarketId::no();
    let events = vec![
        limit(1, yes.clone(), 1, Side::Sell, 40, 10),
        limit(2, no.clone(), 2, Side::Sell, 55, 10),
    ];
    let strategy = ComplementArbStrategy::new(ComplementArbConfig::new(1, 10, 10));
    let report = drive(events, strategy, cash(10_000));
    assert_eq!(report.paired_qty, 10);
    assert_eq!(report.pair_edge_sum, 50);
    assert_eq!(report.avg_edge_ticks, Some(5));
    assert_eq!(report.win_rate_bps, Some(10_000));
    assert_eq!(report.net_pnl, 50);
    assert_eq!(report.realized_pnl, 0);
    assert_eq!(report.unrealized_pnl, 50);
    assert_eq!(report.max_leg_imbalance, 0);
    assert_eq!(position_of(&report, "YES").net, 10);
    assert_eq!(position_of(&report, "NO").net, 10);
    assert_eq!(report.final_equity, 10_050);

    let capped = vec![
        limit(1, yes.clone(), 1, Side::Sell, 40, 10),
        limit(2, no.clone(), 2, Side::Sell, 55, 10),
        limit(3, yes.clone(), 3, Side::Sell, 40, 10),
        limit(4, no, 4, Side::Sell, 55, 10),
    ];
    let strategy = ComplementArbStrategy::new(ComplementArbConfig::new(1, 4, 10));
    let report = drive(capped, strategy, cash(10_000));
    assert_eq!(report.paired_qty, 4);
    assert_eq!(report.net_pnl, 20);
    assert_eq!(position_of(&report, "YES").max_abs_net, 4);
    assert_eq!(position_of(&report, "NO").max_abs_net, 4);
    assert_eq!(report.max_leg_imbalance, 0);
}

#[test]
fn complement_arb_sizes_to_the_thinner_ask_and_can_sell_the_pair() {
    let yes = MarketId::yes();
    let no = MarketId::no();
    let thin = vec![
        limit(1, yes.clone(), 1, Side::Sell, 40, 3),
        limit(2, no.clone(), 2, Side::Sell, 50, 10),
    ];
    let strategy = ComplementArbStrategy::new(ComplementArbConfig::new(1, 20, 20));
    let report = drive(thin, strategy, cash(5_000));
    assert_eq!(report.paired_qty, 3);
    assert_eq!(report.net_pnl, 30);
    assert_eq!(position_of(&report, "YES").net, 3);
    assert_eq!(position_of(&report, "NO").net, 3);
    assert_eq!(book_of(&report, "NO").ask.unwrap().qty, Qty(7));

    let round_trip = vec![
        limit(1, yes.clone(), 1, Side::Sell, 40, 5),
        limit(2, no.clone(), 2, Side::Sell, 50, 5),
        limit(3, yes.clone(), 3, Side::Buy, 55, 5),
        limit(4, no.clone(), 4, Side::Buy, 55, 5),
    ];
    let strategy = ComplementArbStrategy::new(ComplementArbConfig::new(1, 10, 10));
    let report = drive(round_trip, strategy, cash(5_000));
    assert_eq!(report.paired_qty, 5);
    assert_eq!(report.realized_pnl, 100);
    assert_eq!(report.unrealized_pnl, 0);
    assert_eq!(report.net_pnl, 100);
    assert_eq!(report.closed_qty, 10);
    assert_eq!(position_of(&report, "YES").net, 0);
    assert_eq!(position_of(&report, "NO").net, 0);
}

#[test]
fn min_edge_and_fees_block_a_complement_that_is_no_longer_cheap() {
    let yes = MarketId::yes();
    let no = MarketId::no();
    let events = vec![
        limit(1, yes.clone(), 1, Side::Sell, 40, 4),
        limit(2, no.clone(), 2, Side::Sell, 59, 4),
    ];
    let strategy = ComplementArbStrategy::new(ComplementArbConfig::new(2, 10, 10));
    let report = drive(events.clone(), strategy, cash(1_000));
    assert_eq!(report.strategy_orders, 0);
    assert_eq!(report.paired_qty, 0);
    assert_eq!(report.net_pnl, 0);

    let tight = vec![
        limit(1, yes, 1, Side::Sell, 40, 4),
        limit(2, no, 2, Side::Sell, 58, 4),
    ];
    let strategy = ComplementArbStrategy::new(ComplementArbConfig::new(1, 10, 10));
    let report = drive(tight, strategy, fee_config(1_000, 1));
    assert_eq!(report.strategy_orders, 0);
    assert_eq!(report.net_pnl, 0);
}

#[test]
fn an_unhedged_long_is_hedged_only_when_the_pair_still_pays() {
    let yes = MarketId::yes();
    let no = MarketId::no();
    let mut portfolio = Portfolio::new(10_000);
    fill(&mut portfolio, &yes, Side::Buy, 40, 5, 0);
    let mut books = BTreeMap::new();
    books.insert(yes.clone(), top(None, Some((40, 10))));
    books.insert(no.clone(), top(None, Some((55, 10))));
    let mut strategy = ComplementArbStrategy::new(ComplementArbConfig {
        max_unhedged: 0,
        max_order_qty: 2,
        max_position: 10,
        min_edge_ticks: 1,
        ..ComplementArbConfig::default()
    });
    let actions = decide(&mut strategy, &portfolio, &books, 0);
    assert_eq!(
        actions,
        vec![ioc(no.clone(), Side::Buy, 55, 2)],
        "hedge is capped by max_order_qty and does not add more YES"
    );

    books.insert(no.clone(), top(None, Some((70, 10))));
    let actions = decide(&mut strategy, &portfolio, &books, 0);
    assert!(
        actions.is_empty(),
        "a 70 ask would lock a loss against YES at 40"
    );

    fill(&mut portfolio, &no, Side::Buy, 55, 5, 0);
    let mut books = BTreeMap::new();
    books.insert(yes.clone(), top(Some((60, 8)), Some((40, 8))));
    books.insert(no.clone(), top(Some((50, 8)), Some((55, 8))));
    let mut strategy = ComplementArbStrategy::new(ComplementArbConfig::new(1, 5, 10));
    let actions = decide(&mut strategy, &portfolio, &books, 0);
    assert_eq!(
        actions,
        vec![
            ioc(yes, Side::Sell, 60, 5),
            ioc(MarketId::no(), Side::Sell, 50, 5),
        ]
    );
}

fn decide(
    strategy: &mut ComplementArbStrategy,
    portfolio: &Portfolio,
    books: &BTreeMap<MarketId, TopOfBook>,
    fee: u64,
) -> Vec<Action> {
    let account = portfolio.account(books, MarkMethod::Mid).unwrap();
    let result = CommandResult::Rejected {
        order_id: OrderId(1),
        reason: RejectReason::ZeroQuantity,
    };
    let market = MarketId::yes();
    let update = MarketUpdate {
        seq: 1,
        market: &market,
        result: &result,
        books,
        portfolio,
        account,
        live_orders: &[],
        taker_fee: fee,
        maker_fee: fee,
        mark: MarkMethod::Mid,
    };
    strategy.on_market(&update)
}

#[test]
fn the_same_backtest_twice_matches_including_the_digest() {
    let events = arb_book();
    let config = cash(10_000);
    let first = drive(
        events.clone(),
        ComplementArbStrategy::new(ComplementArbConfig::new(1, 10, 10)),
        config,
    );
    let second = drive(
        events,
        ComplementArbStrategy::new(ComplementArbConfig::new(1, 10, 10)),
        config,
    );
    assert_eq!(first, second);
    assert_eq!(first.digest, second.digest);
}

#[test]
fn fast_and_reference_engines_produce_the_same_backtest() {
    let config = cash(10_000);
    let verified = backtest::verify(
        arb_book(),
        || ComplementArbStrategy::new(ComplementArbConfig::new(1, 10, 10)),
        config,
    )
    .unwrap();
    assert_eq!(verified.fast, verified.reference);
    assert_eq!(verified.fast.digest, verified.reference.digest);
    assert_eq!(verified.fast.net_pnl, 50);

    let market = MarketId::default_market();
    let mut steps = vec![Vec::new(); 2];
    steps[0] = vec![ioc(market.clone(), Side::Buy, 50, 10)];
    steps[1] = vec![ioc(market.clone(), Side::Sell, 60, 10)];
    let events = vec![
        limit(1, market.clone(), 1, Side::Sell, 50, 10),
        limit(2, market, 2, Side::Buy, 60, 10),
    ];
    let verified =
        backtest::verify(events, || Script::new(steps.clone()), fee_config(10_000, 1)).unwrap();
    assert_eq!(verified.fast, verified.reference);
    assert_eq!(verified.fast.net_pnl, 80);
}

fn arb_book() -> Vec<Event> {
    vec![
        limit(1, MarketId::yes(), 1, Side::Sell, 40, 10),
        limit(2, MarketId::no(), 2, Side::Sell, 55, 10),
    ]
}

#[test]
fn strategy_ids_live_in_a_reserved_namespace() {
    let mut ids = StrategyIds::new();
    let first = ids.alloc();
    let second = ids.alloc();
    assert_eq!(first.get(), StrategyIds::BASE);
    assert_eq!(second.get(), StrategyIds::BASE + 1);
    assert!(StrategyIds::contains(first));
    assert!(!StrategyIds::contains(OrderId(1)));

    let event = limit(
        1,
        MarketId::default_market(),
        StrategyIds::BASE,
        Side::Buy,
        10,
        1,
    );
    let err = backtest::run(
        vec![event],
        |_| FastEngine::new(),
        ComplementArbStrategy::default(),
        BacktestConfig::default(),
    )
    .unwrap_err();
    match err {
        BacktestError::OrderIdCollision { id, .. } => assert_eq!(id.get(), StrategyIds::BASE),
        other => panic!("expected a collision, got {other}"),
    }
}

struct ForeignCancel;

impl Strategy for ForeignCancel {
    fn on_market(&mut self, _update: &MarketUpdate<'_>) -> Vec<Action> {
        vec![Action::cancel(MarketId::default_market(), OrderId(1))]
    }
}

#[test]
fn a_strategy_cannot_cancel_an_id_outside_its_namespace() {
    let err = backtest::run(
        vec![limit(1, MarketId::default_market(), 1, Side::Buy, 10, 1)],
        |_| FastEngine::new(),
        ForeignCancel,
        BacktestConfig::default(),
    )
    .unwrap_err();
    assert!(matches!(err, BacktestError::ForeignCancel { id, .. } if id == OrderId(1)));
}

#[test]
fn complement_arb_captures_positive_pnl_and_obeys_caps_on_simulated_flow() {
    let mut paired = 0u64;
    for seed in [1u64, 7, 42, 99] {
        let events: Vec<Event> =
            generate(SimConfig::new(Preset::PredictionMarket, seed, 12_000)).collect();
        let strategy = ComplementArbStrategy::new(ComplementArbConfig {
            min_edge_ticks: 1,
            max_position: 25,
            max_order_qty: 5,
            max_unhedged: 0,
            allow_unwind: true,
            ..ComplementArbConfig::default()
        });
        let report = drive(events, strategy, cash(1_000_000));
        assert!(report.net_pnl >= 0, "seed {seed} lost {}", report.net_pnl);
        assert_eq!(report.max_leg_imbalance, 0, "seed {seed}");
        for pos in &report.positions {
            assert!(
                pos.max_abs_net <= 25,
                "seed {seed} {} reached {}",
                pos.market,
                pos.max_abs_net
            );
        }
        let yes = report
            .positions
            .iter()
            .find(|pos| pos.market == MarketId::yes())
            .map(|pos| pos.net)
            .unwrap_or(0);
        let no = report
            .positions
            .iter()
            .find(|pos| pos.market == MarketId::no())
            .map(|pos| pos.net)
            .unwrap_or(0);
        assert_eq!(yes, no, "seed {seed}");
        paired += report.paired_qty;
        assert!(
            report.paired_qty == 0 || report.net_pnl > 0,
            "seed {seed} paired {} shares for {}",
            report.paired_qty,
            report.net_pnl
        );
    }
    assert!(paired > 0, "no complement pair was captured");
}

#[test]
fn simulated_complement_arb_is_deterministic_and_engine_equivalent() {
    let events: Vec<Event> = generate(SimConfig::new(Preset::PredictionMarket, 7, 1_500)).collect();
    let make = || {
        ComplementArbStrategy::new(ComplementArbConfig {
            min_edge_ticks: 0,
            max_position: 40,
            max_order_qty: 8,
            max_unhedged: 0,
            allow_unwind: true,
            ..ComplementArbConfig::default()
        })
    };
    let config = fee_config(1_000_000, 0);
    let first = drive(events.clone(), make(), config);
    let second = drive(events.clone(), make(), config);
    assert_eq!(first, second);
    let verified = backtest::verify(events, make, config).unwrap();
    assert_eq!(verified.fast, verified.reference);
    assert_eq!(verified.fast.digest, first.digest);
}

#[test]
fn the_market_maker_trades_a_single_book_and_matches_across_engines() {
    let events: Vec<Event> = generate(SimConfig::new(Preset::Equity, 1, 1_500)).collect();
    let make = || TopOfBookMarketMaker::new(MarketMakerConfig::new(50, 10));
    let config = BacktestConfig::default();
    let first = drive(events.clone(), make(), config);
    assert!(first.strategy_orders > 0);
    assert!(first.strategy_fills > 0);
    for pos in &first.positions {
        assert!(pos.max_abs_net <= 50);
    }
    let second = drive(events.clone(), make(), config);
    assert_eq!(first, second);
    let verified = backtest::verify(events, make, config).unwrap();
    assert_eq!(verified.fast, verified.reference);
    assert_eq!(verified.fast.digest, first.digest);
}
