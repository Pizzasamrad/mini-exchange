//! Event log, synthetic flow, and exact replay.
//!
//! Parser cases use small fixtures. Engine comparisons stay in the low
//! thousands of commands so `cargo test` stays quick. The streaming test
//! writes a larger log and replays it without collecting the events.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufReader, BufWriter, Cursor};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use mini_exchange::event_log::{Event, EventReader, EventWriter, MarketId, ReadError, HEADER};
use mini_exchange::replay::{
    run, run_reader, verify, BookStats, ReplayOptions, ReplayReport, VerifiedReplay,
};
use mini_exchange::sim::{generate, Preset, SimConfig};
use mini_exchange::{Command, FastEngine, OrderId, Price, Qty, ReferenceEngine, Side, Timestamp};

const SEEDS: [u64; 3] = [1, 7, 42];

#[test]
fn writer_emits_the_documented_format_and_reads_back() {
    let events = vec![
        Event::limit(
            1,
            MarketId::default_market(),
            OrderId(7),
            Side::Buy,
            Price(10_000),
            Qty(25),
        ),
        Event::market(2, MarketId::yes(), OrderId(8), Side::Sell, Qty(3)),
        Event::cancel(3, MarketId::no(), OrderId(7)),
    ];
    let mut buf = Vec::new();
    {
        let mut writer = EventWriter::create(&mut buf, ["example"]).unwrap();
        assert_eq!(writer.write_events(events.clone()).unwrap(), 3);
        writer.flush().unwrap();
    }
    let text = String::from_utf8(buf).unwrap();
    assert_eq!(
        text,
        "\
# mini-exchange event log v1
# example
seq,market,cmd,id,side,price,qty
1,DEFAULT,limit,7,buy,10000,25
2,YES,market,8,sell,,3
3,NO,cancel,7,,,
"
    );
    let parsed = read_all(&text).unwrap();
    assert_eq!(parsed, events);
}

#[test]
fn generated_streams_round_trip_for_both_presets() {
    for preset in [Preset::Equity, Preset::PredictionMarket] {
        for seed in SEEDS {
            let events: Vec<Event> = generate(SimConfig::new(preset, seed, 400)).collect();
            let mut buf = Vec::new();
            let mut writer = EventWriter::create(&mut buf, std::iter::empty::<&str>()).unwrap();
            writer.write_events(events.clone()).unwrap();
            writer.flush().unwrap();
            let parsed = read_all(&String::from_utf8(buf).unwrap()).unwrap();
            assert_eq!(parsed, events, "{preset} seed {seed}");
        }
    }
}

#[test]
fn comments_blank_lines_crlf_and_field_whitespace_are_accepted() {
    let text = "\n# comment\r\n\r\nseq,market,cmd,id,side,price,qty\r\n\n# mid\n1, DEFAULT , limit , 1 , buy , 10 , 4 \n\n2,,market,2,sell,,3\n";
    let events = read_all(text).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].seq, 1);
    assert_eq!(events[0].market, MarketId::default_market());
    assert_eq!(
        events[0].command,
        Command::limit(OrderId(1), Side::Buy, Price(10), Qty(4), Timestamp(1))
    );
    assert_eq!(events[1].market, MarketId::default_market());
    assert_eq!(
        events[1].command,
        Command::market(OrderId(2), Side::Sell, Qty(3), Timestamp(2))
    );
    let _ = HEADER;
}

#[test]
fn malformed_lines_name_the_line_and_the_problem() {
    assert_parse_err("", 1, "missing header");
    assert_parse_err("not,a,header,row,at,all,here\n", 1, "expected header");
    assert_parse_err(
        "seq,market,cmd,id,side,price,qty\n1,DEFAULT,stop,1,buy,10,1\n",
        2,
        "unknown command `stop`",
    );
    assert_parse_err(
        "seq,market,cmd,id,side,price,qty\n1,DEFAULT,Limit,1,buy,10,1\n",
        2,
        "unknown command `Limit`",
    );
    assert_parse_err(
        "seq,market,cmd,id,side,price,qty\n1,DEFAULT,limit,1,buy,,1\n",
        2,
        "missing price",
    );
    assert_parse_err(
        "seq,market,cmd,id,side,price,qty\n1,DEFAULT,limit,1,,10,1\n",
        2,
        "missing side",
    );
    assert_parse_err(
        "seq,market,cmd,id,side,price,qty\n1,DEFAULT,limit,1,bid,10,1\n",
        2,
        "unknown side `bid`",
    );
    assert_parse_err(
        "seq,market,cmd,id,side,price,qty\n1,DEFAULT,market,1,buy,10,1\n",
        2,
        "market order price must be empty",
    );
    assert_parse_err(
        "seq,market,cmd,id,side,price,qty\n1,DEFAULT,cancel,1,buy,,\n",
        2,
        "cancel must leave side, price, and quantity empty",
    );
    assert_parse_err(
        "seq,market,cmd,id,side,price,qty\n1,DEFAULT,limit,x,buy,10,1\n",
        2,
        "invalid order id `x`",
    );
    assert_parse_err(
        "seq,market,cmd,id,side,price,qty\n1,YES/NO,limit,1,buy,10,1\n",
        2,
        "invalid market id",
    );
    assert_parse_err(
        "seq,market,cmd,id,side,price,qty\n1,DEFAULT,limit,1,buy,10,1,extra\n",
        2,
        "expected 7 fields, found 8",
    );
    assert_parse_err(
        "seq,market,cmd,id,side,price,qty\n1,DEFAULT,limit,1,buy,10\n",
        2,
        "expected 7 fields, found 6",
    );
    assert_parse_err(
        "\
seq,market,cmd,id,side,price,qty
2,DEFAULT,limit,1,buy,10,1
1,DEFAULT,limit,2,sell,11,1
",
        3,
        "sequence 1 is not greater than previous sequence 2",
    );
    assert_parse_err(
        "\
seq,market,cmd,id,side,price,qty
5,DEFAULT,limit,1,buy,10,1
5,DEFAULT,limit,2,sell,11,1
",
        3,
        "sequence 5 is not greater than previous sequence 5",
    );

    let mut reader = EventReader::new(Cursor::new("nope\n"));
    let err = reader.next().unwrap().unwrap_err();
    assert!(err.to_string().contains("line 1"), "{err}");
    assert!(reader.next().is_none());
}

#[test]
fn writer_rejects_a_timestamp_that_disagrees_with_seq() {
    let mut event = Event::limit(
        4,
        MarketId::default_market(),
        OrderId(1),
        Side::Buy,
        Price(10),
        Qty(1),
    );
    event.command = Command::limit(OrderId(1), Side::Buy, Price(10), Qty(1), Timestamp(9));
    let mut buf = Vec::new();
    let mut writer = EventWriter::create(&mut buf, std::iter::empty::<&str>()).unwrap();
    let err = writer.write_event(&event).unwrap_err();
    assert!(err.to_string().contains("timestamp"), "{err}");
}

#[test]
fn hand_written_tape_reports_vwap_spread_and_an_empty_book() {
    let events = vec![
        Event::limit(
            1,
            MarketId::default_market(),
            OrderId(1),
            Side::Buy,
            Price(100),
            Qty(10),
        ),
        Event::limit(
            2,
            MarketId::default_market(),
            OrderId(2),
            Side::Sell,
            Price(105),
            Qty(10),
        ),
        Event::limit(
            3,
            MarketId::default_market(),
            OrderId(3),
            Side::Buy,
            Price(105),
            Qty(4),
        ),
        Event::cancel(4, MarketId::default_market(), OrderId(1)),
        Event::market(5, MarketId::default_market(), OrderId(4), Side::Buy, Qty(6)),
        Event::cancel(6, MarketId::default_market(), OrderId(99)),
    ];
    let options = ReplayOptions::new(1);
    let verified = verify(events.clone(), options).unwrap();
    assert_eq!(verified.fast, verified.reference);
    let report = verified.fast;
    assert_eq!(report.commands, 6);
    assert_eq!(report.limits, 3);
    assert_eq!(report.markets, 1);
    assert_eq!(report.cancels, 2);
    assert_eq!(report.rejects, 1);
    assert_eq!(
        report.limits + report.markets + report.cancels,
        report.commands
    );
    assert_eq!(report.trades, 2);
    assert_eq!(report.volume, 10);
    assert_eq!(report.notional, 1_050);
    assert_eq!(report.vwap_ticks(), Some(105));
    assert_eq!(report.last_price, Some(Price(105)));
    assert_book_stats(
        &report.top_of_book,
        BookStats {
            samples: 7,
            two_sided: 2,
            spread_sum: 10,
            spread_min: Some(5),
            spread_max: Some(5),
            mid_sum: 410,
        },
    );
    assert_eq!(report.top_of_book.average_spread_ticks(), Some(5));
    assert_eq!(report.top_of_book.average_mid_ticks(), Some(102));
    let book = report.book().unwrap();
    assert!(book.bids.is_empty());
    assert!(book.asks.is_empty());
    assert!(!book.is_crossed());
    assert_eq!(report.snapshots.len(), 1);
    assert_eq!(report.snapshots[0].top_of_book, report.top_of_book);

    let final_only = run(&events, |_| ReferenceEngine::new(), ReplayOptions::new(0));
    assert_eq!(final_only.digest, report.digest);
    assert_eq!(final_only.book(), report.book());
    assert_eq!(final_only.top_of_book.samples, 1);
    assert_eq!(final_only.top_of_book.two_sided, 0);
    assert_eq!(final_only.vwap_ticks(), Some(105));
}

#[test]
fn zero_quantity_is_rejected_and_does_not_rest() {
    let events = [Event::limit(
        1,
        MarketId::default_market(),
        OrderId(1),
        Side::Buy,
        Price(10),
        Qty(0),
    )];
    let report = run(&events, |_| ReferenceEngine::new(), ReplayOptions::new(0));
    assert_eq!(report.rejects, 1);
    assert_eq!(report.trades, 0);
    assert!(report.book().unwrap().bids.is_empty());
    assert_ne!(report.digest, 0);
}

#[test]
fn slice_vec_and_iterator_replay_to_the_same_report() {
    let events: Vec<Event> = generate(SimConfig::new(Preset::Equity, 11, 300)).collect();
    let options = ReplayOptions::default();
    let from_vec = run(events.clone(), |_| FastEngine::new(), options);
    let from_slice = run(events.as_slice(), |_| FastEngine::new(), options);
    let from_iter = run(events.iter(), |_| ReferenceEngine::new(), options);
    assert_eq!(from_vec, from_slice);
    assert_eq!(from_slice, from_iter);
}

#[test]
fn replaying_a_file_twice_is_identical() {
    let path = temp_log("determinism");
    let config = SimConfig::new(Preset::Equity, 42, 2_000);
    write_log(&path, config);
    let first = stream_fast(&path, ReplayOptions::default());
    let second = stream_fast(&path, ReplayOptions::default());
    assert_eq!(first, second);
    assert_eq!(first.commands, 2_000);
    assert!(first.trades > 0);
    assert!(first.cancels > 0);
    let reference = stream_reference(&path, ReplayOptions::default());
    assert_eq!(first.digest, reference.digest);
    assert_eq!(first.book(), reference.book());
    assert_eq!(first, reference);
}

#[test]
fn fast_and_reference_agree_on_both_presets() {
    for preset in [Preset::Equity, Preset::PredictionMarket] {
        for seed in SEEDS {
            let events: Vec<Event> = generate(SimConfig::new(preset, seed, 2_500)).collect();
            let verified =
                verify(events.as_slice(), ReplayOptions::new(32)).unwrap_or_else(|err| {
                    panic!("{preset} seed {seed}: {err}");
                });
            assert_preset_report(preset, &verified, 2_500);
        }
    }
}

#[test]
fn different_seeds_do_not_share_a_digest() {
    let options = ReplayOptions::new(16);
    let first = run(
        generate(SimConfig::new(Preset::Equity, 1, 800)),
        |_| FastEngine::new(),
        options,
    );
    let second = run(
        generate(SimConfig::new(Preset::Equity, 2, 800)),
        |_| FastEngine::new(),
        options,
    );
    assert_ne!(first.digest, second.digest);
    let same = run(
        generate(SimConfig::new(Preset::Equity, 1, 800)),
        |_| ReferenceEngine::new(),
        options,
    );
    assert_eq!(first.digest, same.digest);
}

#[test]
fn synthetic_streams_obey_ids_cancels_and_price_bounds() {
    for seed in SEEDS {
        assert_stream(Preset::Equity, seed, 2_000);
        assert_stream(Preset::PredictionMarket, seed, 2_000);
    }
}

#[test]
fn the_same_seed_writes_the_same_bytes() {
    let config = SimConfig::new(Preset::PredictionMarket, 9, 250);
    let mut first = Vec::new();
    let mut second = Vec::new();
    write_buf(&mut first, config);
    write_buf(&mut second, config);
    assert_eq!(first, second);
    let mut other = Vec::new();
    write_buf(
        &mut other,
        SimConfig::new(Preset::PredictionMarket, 10, 250),
    );
    assert_ne!(first, other);
}

#[test]
fn a_reader_error_reports_the_line_without_replaying() {
    let text = "seq,market,cmd,id,side,price,qty\n1,DEFAULT,nope,1,buy,10,1\n";
    let err = run_reader(
        EventReader::new(Cursor::new(text)),
        |_| FastEngine::new(),
        ReplayOptions::default(),
    )
    .unwrap_err();
    let ReadError::Parse(parse) = err else {
        panic!("expected a parse error, got {err}");
    };
    assert_eq!(parse.line(), 2);
    assert!(parse.message().contains("unknown command"));
}

#[test]
fn streams_a_large_log_end_to_end() {
    for (preset, orders) in [
        (Preset::Equity, 30_000u64),
        (Preset::PredictionMarket, 15_000),
    ] {
        let path = temp_log(preset.name());
        write_log(&path, SimConfig::new(preset, 1234, orders));
        let bytes = std::fs::metadata(&path).unwrap().len();
        assert!(bytes > 100_000, "{preset} log was only {bytes} bytes");
        let verified = stream_verify(&path, ReplayOptions::new(128));
        assert_preset_report(preset, &verified, orders);
        let again = stream_fast(&path, ReplayOptions::new(128));
        assert_eq!(again, verified.fast);
    }
}

fn assert_preset_report(preset: Preset, verified: &VerifiedReplay, orders: u64) {
    assert_eq!(verified.fast, verified.reference);
    let report = &verified.fast;
    assert_eq!(report.commands, orders);
    assert_eq!(
        report.limits + report.markets + report.cancels,
        report.commands
    );
    assert!(report.trades > 0, "{preset} produced no trades");
    assert!(report.cancels > 0, "{preset} produced no cancels");
    assert!(report.volume > 0);
    assert_eq!(report.vwap_ticks(), vwap(report.notional, report.volume));
    assert_ne!(report.digest, 0);
    if report.top_of_book.two_sided > 0 {
        assert!(report.top_of_book.spread_min.unwrap() >= 1);
    }
    match preset {
        Preset::Equity => {
            assert_eq!(report.snapshots.len(), 1);
            assert_eq!(report.snapshots[0].market.as_str(), "DEFAULT");
            assert!(report.book().is_some());
        }
        Preset::PredictionMarket => {
            assert_eq!(report.snapshots.len(), 2);
            assert!(report.book().is_none());
            assert!(report.snapshot(&MarketId::yes()).is_some());
            assert!(report.snapshot(&MarketId::no()).is_some());
        }
    }
    let mut samples = 0u64;
    for snap in &report.snapshots {
        assert!(!snap.book.is_crossed(), "{}", snap.market);
        samples += snap.top_of_book.samples;
        assert_eq!(snap.limits + snap.markets + snap.cancels, snap.commands);
    }
    assert_eq!(samples, report.top_of_book.samples);
}

fn assert_stream(preset: Preset, seed: u64, orders: u64) {
    let events: Vec<Event> = generate(SimConfig::new(preset, seed, orders)).collect();
    assert_eq!(events.len() as u64, orders);
    let mut issued = BTreeSet::new();
    let mut prev = 0u64;
    let mut saw_yes = false;
    let mut saw_no = false;
    let mut limit_sum = 0u128;
    let mut limit_n = 0u128;
    let mut cancels = 0u64;
    for event in &events {
        assert!(event.seq > prev, "seq {} then {}", prev, event.seq);
        prev = event.seq;
        match preset {
            Preset::Equity => assert_eq!(event.market.as_str(), "DEFAULT"),
            Preset::PredictionMarket => {
                assert!(event.market.as_str() == "YES" || event.market.as_str() == "NO");
                if event.market.as_str() == "YES" {
                    saw_yes = true;
                } else {
                    saw_no = true;
                }
            }
        }
        match event.command {
            Command::Limit {
                id, price, qty, ts, ..
            } => {
                assert!(issued.insert(id), "duplicate id {id}");
                assert!(!qty.is_zero());
                assert_eq!(ts.get(), event.seq);
                let px = price.ticks();
                match preset {
                    Preset::Equity => {
                        assert!((5_900..=14_200).contains(&px), "equity price {px}");
                        limit_sum += u128::from(px);
                        limit_n += 1;
                    }
                    Preset::PredictionMarket => {
                        assert!((1..=99).contains(&px), "prediction price {px}");
                    }
                }
            }
            Command::Market { id, qty, ts, .. } => {
                assert!(issued.insert(id), "duplicate id {id}");
                assert!(!qty.is_zero());
                assert_eq!(ts.get(), event.seq);
            }
            Command::Cancel { id } => {
                assert!(issued.contains(&id), "cancel of unknown id {id}");
                cancels += 1;
            }
        }
    }
    let max = issued.iter().next_back().unwrap();
    assert_eq!(issued.iter().next().unwrap().get(), 1);
    assert_eq!(issued.len() as u64, max.get());
    assert!(cancels > 0, "{preset} seed {seed} produced no cancels");
    if preset == Preset::PredictionMarket {
        assert!(saw_yes && saw_no, "seed {seed} missed a leg");
    } else {
        let mean = limit_sum / limit_n;
        assert!(
            (9_000..=11_000).contains(&mean),
            "equity mean {mean} is not centered on 10_000"
        );
    }
}

fn assert_book_stats(actual: &BookStats, expected: BookStats) {
    assert_eq!(actual, &expected);
}

fn vwap(notional: u128, volume: u128) -> Option<u64> {
    if volume == 0 {
        None
    } else {
        u64::try_from(notional / volume).ok()
    }
}

fn read_all(text: &str) -> Result<Vec<Event>, ReadError> {
    EventReader::new(Cursor::new(text)).collect()
}

fn assert_parse_err(text: &str, line: u64, needle: &str) {
    let err = read_all(text).expect_err(text);
    let ReadError::Parse(parse) = err else {
        panic!("expected a parse error for {text}");
    };
    assert_eq!(parse.line(), line, "{parse}");
    assert!(
        parse.message().contains(needle),
        "line {line} message {:?} does not contain {needle}",
        parse.message()
    );
}

fn write_log(path: &Path, config: SimConfig) {
    let file = File::create(path).unwrap();
    let comment = format!(
        "preset={} seed={} orders={}",
        config.preset, config.seed, config.orders
    );
    let mut writer = EventWriter::create(BufWriter::new(file), [comment.as_str()]).unwrap();
    let written = writer.write_events(generate(config)).unwrap();
    assert_eq!(written, config.orders);
    writer.flush().unwrap();
}

fn write_buf(buf: &mut Vec<u8>, config: SimConfig) {
    let mut writer = EventWriter::create(&mut *buf, std::iter::empty::<&str>()).unwrap();
    writer.write_events(generate(config)).unwrap();
    writer.flush().unwrap();
}

fn stream_fast(path: &Path, options: ReplayOptions) -> ReplayReport {
    let file = File::open(path).unwrap();
    run_reader(
        EventReader::new(BufReader::new(file)),
        |_| FastEngine::new(),
        options,
    )
    .unwrap()
}

fn stream_reference(path: &Path, options: ReplayOptions) -> ReplayReport {
    let file = File::open(path).unwrap();
    run_reader(
        EventReader::new(BufReader::new(file)),
        |_| ReferenceEngine::new(),
        options,
    )
    .unwrap()
}

fn stream_verify(path: &Path, options: ReplayOptions) -> VerifiedReplay {
    let file = File::open(path).unwrap();
    mini_exchange::verify_reader(EventReader::new(BufReader::new(file)), options).unwrap()
}

struct TempPath(PathBuf);

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl AsRef<Path> for TempPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl std::ops::Deref for TempPath {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

fn temp_log(label: &str) -> TempPath {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "mini-exchange-{label}-{}-{n}.log",
        std::process::id()
    ));
    TempPath(path)
}
