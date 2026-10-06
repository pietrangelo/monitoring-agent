// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Pietrangelo Masala
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! Block files in the running store (RFC 0010 §5, §6): a closed span's chunks move out of
//! `chunks` into an immutable file and its `blocks` row in one commit, queries read them from
//! there, and the open reconciles the files with the rows. Handoffs run on the sweep interval
//! only: the open itself hands nothing off. Rewrites (rule (b)) arrive with retention.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hub_store::block::format::{ChunkRef, HEADER_LEN, encode_block};
use hub_store::codec::{Bucket, RawPoint};
use hub_store::name::{Generation, MetricName, SystemKey};
use hub_store::series::{SeriesId, SeriesKey};
use hub_store::store::{
    CorruptChunk, FailCause, HandoffFailure, Limit, OpenError, Order, Query, Samples, Store,
    StoreError, StoreOptions, SystemClock,
};
use hub_store::tier::{SpanStart, Tier};
use hub_store::value::ValueKind;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

/// A day boundary: on every tier's grid.
const T0: u64 = 1_800_057_600;
const HOUR: u64 = 3_600;
const DAY: u64 = 86_400;
const CHUNKS: TableDefinition<(u8, u64, u32, u16), &[u8]> = TableDefinition::new("chunks");
const BLOCKS: TableDefinition<(u8, u64), &[u8]> = TableDefinition::new("blocks");

#[derive(Clone)]
struct TestClock(Arc<AtomicU64>);

impl SystemClock for TestClock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl TestClock {
    fn at(t: u64) -> TestClock {
        TestClock(Arc::new(AtomicU64::new(t)))
    }
    fn set(&self, t: u64) {
        self.0.store(t, Ordering::SeqCst);
    }
}

fn open_with(dir: &Path, clock: &TestClock, sweep: Duration) -> Result<Store<()>, OpenError> {
    let options = StoreOptions {
        commit_interval: Duration::from_millis(20),
        sweep_interval: sweep,
        ..StoreOptions::with_clock(Arc::new(clock.clone()))
    };
    Store::open(dir, options)
}

/// A store whose sweep, and so the handoff, runs every 20 ms.
fn open(dir: &Path, clock: &TestClock) -> Store<()> {
    open_with(dir, clock, Duration::from_millis(20)).expect("opens")
}

/// A store that hands nothing off while the test runs.
fn open_quiet(dir: &Path, clock: &TestClock) -> Result<Store<()>, OpenError> {
    open_with(dir, clock, Duration::from_secs(3_600))
}

/// Closes a store and drops it, so its file is free for the next open.
fn shut(store: Store<()>) {
    store.close().expect("closes");
}

fn key(name: &str) -> SeriesKey {
    SeriesKey {
        system: SystemKey::try_from(&b"s"[..]).expect("valid"),
        generation: Generation::new(1),
        metric: MetricName::try_from(name).expect("valid"),
    }
}

fn append_one(store: &Store<()>, clock: &TestClock, name: &str, t: u64, value: i64) {
    clock.set(t);
    let report = store
        .append(
            &key(name).system,
            Generation::new(1),
            &[(
                MetricName::try_from(name).expect("valid"),
                ValueKind::Percent,
                value,
            )],
        )
        .expect("append");
    assert!(report.rejected.is_empty(), "{report:?}");
}

/// Appends one point to `a` and then `b` (so `a` takes series id 0) at hub time `t`.
fn append_at(store: &Store<()>, clock: &TestClock, t: u64, value: i64) {
    for name in ["a", "b"] {
        append_one(store, clock, name, t, value);
    }
}

fn query_range(
    store: &Store<()>,
    name: &str,
    tier: Tier,
    from: u64,
    until: u64,
) -> Result<Samples, StoreError> {
    let q = Query {
        from,
        until,
        tier,
        order: Order::Earliest,
        limit: Limit::new(10_000),
    };
    store
        .query(&key(name), &q)
        .map(|s| s.map_or(Samples::Raw(Vec::new()), |s| s.samples))
}

fn query(store: &Store<()>, name: &str, tier: Tier) -> Result<Samples, StoreError> {
    query_range(store, name, tier, 0, u64::MAX)
}

fn raw(store: &Store<()>, name: &str) -> Vec<RawPoint> {
    match query(store, name, Tier::Raw).expect("query") {
        Samples::Raw(points) => points,
        Samples::Rollup(_) => panic!("raw query answered buckets"),
    }
}

fn buckets(store: &Store<()>, name: &str, tier: Tier) -> Vec<Bucket> {
    match query(store, name, tier).expect("query") {
        Samples::Rollup(buckets) => buckets,
        Samples::Raw(_) => panic!("rollup query answered points"),
    }
}

fn block_path(dir: &Path, tier: Tier, span: u64) -> PathBuf {
    dir.join("blocks")
        .join(tier.name())
        .join(format!("{span}.blk"))
}

/// Writes a file, creating its directory first (a fixture: whether the store made it is
/// `the_open_creates_a_private_directory_per_tier`'s question).
fn plant(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    std::fs::write(path, bytes).expect("write");
}

fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let started = Instant::now();
    while !cond() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "waited for {what}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits until the tier's block files, by committed row, number `n`.
fn wait_for_rows(store: &Store<()>, tier: Tier, n: u64) {
    wait_for(&format!("{n} {} rows", tier.name()), || {
        store.stats().expect("stats").block_files(tier) == n
    });
}

/// Ten points a minute apart in the raw and minute span at T0, then one in the next span.
fn fill_first_span(store: &Store<()>, clock: &TestClock) -> Vec<RawPoint> {
    let mut expected = Vec::new();
    for i in 0..10u64 {
        append_at(store, clock, T0 + i * 60, (i * 100) as i64);
        expected.push(RawPoint {
            ts: T0 + i * 60,
            value: (i * 100) as i64,
        });
    }
    append_at(store, clock, T0 + HOUR + 200, 5_000);
    expected.push(RawPoint {
        ts: T0 + HOUR + 200,
        value: 5_000,
    });
    expected
}

/// (tier code, span start) pairs.
type Spans = Vec<(u8, u64)>;

/// The spans with chunks in `chunks`, and the spans with a `blocks` row, of a closed store.
fn redb_spans(dir: &Path) -> (Spans, Spans) {
    let db = Database::open(dir.join("hub.redb")).expect("redb opens");
    let read = db.begin_read().expect("read");
    let mut chunks: Vec<(u8, u64)> = read
        .open_table(CHUNKS)
        .expect("chunks")
        .iter()
        .expect("iter")
        .map(|e| {
            let (k, _) = e.expect("entry");
            let (tier, span, _, _) = k.value();
            (tier, span)
        })
        .collect();
    chunks.dedup();
    let rows = read
        .open_table(BLOCKS)
        .expect("blocks")
        .iter()
        .expect("iter")
        .map(|e| e.expect("entry").0.value())
        .collect();
    (chunks, rows)
}

/// A file of span `span` holding the chunks redb holds of `held_span` (a closed store's).
fn file_from_redb(dir: &Path, tier: Tier, held_span: u64, span: u64) -> Vec<u8> {
    let db = Database::open(dir.join("hub.redb")).expect("redb opens");
    let read = db.begin_read().expect("read");
    let table = read.open_table(CHUNKS).expect("chunks");
    let held: Vec<(u32, u16, Vec<u8>)> = table
        .range((tier.code(), held_span, 0u32, 0u16)..=(tier.code(), held_span, u32::MAX, u16::MAX))
        .expect("range")
        .map(|e| {
            let (k, v) = e.expect("entry");
            let (_, _, id, seq) = k.value();
            (id, seq, v.value().to_vec())
        })
        .collect();
    assert!(!held.is_empty(), "redb holds the span");
    let refs: Vec<ChunkRef<'_>> = held
        .iter()
        .map(|(id, seq, bytes)| ChunkRef {
            id: SeriesId::from_raw(*id),
            seq: *seq,
            bytes,
        })
        .collect();
    let span = SpanStart::new(tier, span).expect("grid");
    encode_block(tier, span, &refs).expect("ordered")
}

/// A store whose raw and minute spans at T0 were handed off, closed.
fn handed_off() -> (tempfile::TempDir, TestClock, Vec<RawPoint>, Vec<Bucket>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0);
    let store = open(dir.path(), &clock);
    let expected = fill_first_span(&store, &clock);
    let minutes = buckets(&store, "a", Tier::Minute);
    wait_for_rows(&store, Tier::Raw, 1);
    wait_for_rows(&store, Tier::Minute, 1);
    shut(store);
    (dir, clock, expected, minutes)
}

fn corrupt_raw_t0() -> Result<Samples, StoreError> {
    Err(StoreError::Corrupt(CorruptChunk {
        tier: Tier::Raw,
        span: T0,
    }))
}

#[test]
fn the_open_creates_a_private_directory_per_tier() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = open(dir.path(), &TestClock::at(T0));
    for path in [dir.path().join("blocks")]
        .into_iter()
        .chain(Tier::ALL.map(|t| dir.path().join("blocks").join(t.name())))
    {
        let meta = std::fs::metadata(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
        assert!(meta.is_dir(), "{path:?}");
        assert_eq!(meta.permissions().mode() & 0o777, 0o700, "{path:?}");
    }
    shut(store);
}

#[test]
fn a_closed_span_moves_to_a_block_file_and_reads_back_whole() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0);
    let store = open(dir.path(), &clock);
    let expected = fill_first_span(&store, &clock);
    let minutes_before = buckets(&store, "a", Tier::Minute);
    assert_eq!(minutes_before.len(), 10, "ten closed minute buckets");
    wait_for_rows(&store, Tier::Raw, 1);
    wait_for_rows(&store, Tier::Minute, 1);
    assert!(block_path(dir.path(), Tier::Raw, T0).exists());
    assert!(block_path(dir.path(), Tier::Minute, T0).exists());
    // Read while running, once the rows have committed: from the files, every point once.
    assert_eq!(raw(&store, "a"), expected);
    assert_eq!(buckets(&store, "a", Tier::Minute), minutes_before);
    assert_eq!(store.stats().expect("stats").block_files(Tier::Hour), 0);
    assert!(
        !block_path(dir.path(), Tier::Hour, T0).exists(),
        "the hour span at T0 is still open"
    );
    shut(store);

    let (chunks, rows) = redb_spans(dir.path());
    for span in [(Tier::Raw.code(), T0), (Tier::Minute.code(), T0)] {
        assert!(rows.contains(&span), "a row for {span:?}: {rows:?}");
        assert!(
            !chunks.contains(&span),
            "no chunk of {span:?} left: {chunks:?}"
        );
    }
    assert!(!rows.contains(&(Tier::Hour.code(), T0)), "{rows:?}");

    let store = open(dir.path(), &clock);
    assert_eq!(
        store.stats().expect("stats").block_files(Tier::Raw),
        1,
        "the open counts the rows"
    );
    assert_eq!(raw(&store, "a"), expected, "read back after a reopen");
    assert_eq!(raw(&store, "b"), expected);
    assert_eq!(buckets(&store, "a", Tier::Minute), minutes_before);
    shut(store);
}

#[test]
fn an_hour_span_moves_once_the_day_and_its_grace_have_passed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0);
    let store = open(dir.path(), &clock);
    for i in 0..3u64 {
        append_at(&store, &clock, T0 + i * HOUR, 1_000 + i as i64);
    }
    append_at(&store, &clock, T0 + DAY + HOUR, 2_000);
    let hours = buckets(&store, "a", Tier::Hour);
    assert_eq!(hours.len(), 3, "{hours:?}");
    // The day's grace is one hour bucket and one sweep: not closed yet.
    wait_for_rows(&store, Tier::Raw, 3);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(store.stats().expect("stats").block_files(Tier::Hour), 0);
    // Exactly at the end of the grace (one hour bucket plus one sweep): still not closed.
    append_at(&store, &clock, T0 + DAY + HOUR + 60, 2_050);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(store.stats().expect("stats").block_files(Tier::Hour), 0);
    append_at(&store, &clock, T0 + DAY + HOUR + 61, 2_100);
    wait_for_rows(&store, Tier::Hour, 1);
    assert!(block_path(dir.path(), Tier::Hour, T0).exists());
    assert_eq!(buckets(&store, "a", Tier::Hour)[..3], hours[..]);
    shut(store);
    let store = open(dir.path(), &clock);
    assert_eq!(buckets(&store, "a", Tier::Hour)[..3], hours[..]);
    shut(store);
}

#[test]
fn a_flipped_chunk_byte_fails_only_its_series_span() {
    let (dir, clock, expected, _) = handed_off();
    let path = block_path(dir.path(), Tier::Raw, T0);
    let mut bytes = std::fs::read(&path).expect("read");
    // The data region starts with series 0's (`a`'s) first chunk.
    bytes[HEADER_LEN] ^= 0x01;
    std::fs::write(&path, &bytes).expect("write");
    let store = open_quiet(dir.path(), &clock).expect("opens");
    assert_eq!(query(&store, "a", Tier::Raw), corrupt_raw_t0());
    assert_eq!(
        raw(&store, "b"),
        expected,
        "another series of the file answers"
    );
    assert_eq!(
        query_range(&store, "a", Tier::Raw, T0 + HOUR, u64::MAX),
        Ok(Samples::Raw(vec![RawPoint {
            ts: T0 + HOUR + 200,
            value: 5_000
        }])),
        "a range clear of the damaged span answers"
    );
    assert_eq!(
        buckets(&store, "a", Tier::Minute).len(),
        10,
        "another tier answers"
    );
    shut(store);
}

#[test]
fn a_damaged_index_block_fails_its_span_at_query_time() {
    let (dir, clock, _, minutes) = handed_off();
    let path = block_path(dir.path(), Tier::Raw, T0);
    let mut bytes = std::fs::read(&path).expect("read");
    // The header's index offset: after magic (6), format, tier, span (8) and count (4).
    let at = u64::from_be_bytes(bytes[20..28].try_into().expect("8 bytes")) as usize;
    bytes[at + 1] ^= 0x01;
    std::fs::write(&path, &bytes).expect("write");
    let store = open_quiet(dir.path(), &clock).expect("the summary still checks");
    assert_eq!(query(&store, "a", Tier::Raw), corrupt_raw_t0());
    assert_eq!(buckets(&store, "a", Tier::Minute), minutes);
    shut(store);
}

#[test]
fn a_damaged_or_missing_file_opens_with_its_span_unreadable() {
    type Damage = fn(&Path);
    let cases: [(&str, Damage); 4] = [
        ("summary checksum", |p| {
            let mut bytes = std::fs::read(p).expect("read");
            let at = bytes.len() - 7; // inside the trailer's CRC
            bytes[at] ^= 0xFF;
            std::fs::write(p, &bytes).expect("write");
        }),
        (
            "a well-formed file of another span under this one's name",
            |p| {
                let other = SpanStart::new(Tier::Raw, T0 + HOUR).expect("grid");
                let chunk = [ChunkRef {
                    id: SeriesId::from_raw(0),
                    seq: 0,
                    bytes: b"x",
                }];
                std::fs::write(
                    p,
                    encode_block(Tier::Raw, other, &chunk).expect("one chunk"),
                )
                .expect("write");
            },
        ),
        ("truncated", |p| {
            let bytes = std::fs::read(p).expect("read");
            std::fs::write(p, &bytes[..bytes.len() / 2]).expect("write");
        }),
        ("missing", |p| std::fs::remove_file(p).expect("remove")),
    ];
    for (name, damage) in cases {
        let (dir, clock, _, minutes) = handed_off();
        damage(&block_path(dir.path(), Tier::Raw, T0));
        let store = open_quiet(dir.path(), &clock)
            .unwrap_or_else(|e| panic!("{name}: one damaged file never stops the open: {e}"));
        for series in ["a", "b"] {
            assert_eq!(
                query(&store, series, Tier::Raw),
                corrupt_raw_t0(),
                "{name}: {series}"
            );
        }
        assert_eq!(
            buckets(&store, "a", Tier::Minute),
            minutes,
            "{name}: other tiers answer"
        );
        shut(store);
    }
}

#[test]
fn a_leftover_temporary_is_deleted_at_open() {
    let (dir, clock, expected, _) = handed_off();
    let blocks = dir.path().join("blocks");
    let temporaries = [
        blocks.join("raw").join(format!("{}.blk.tmp", T0 + HOUR)),
        blocks.join("minute").join(format!("{}.r1.blk.tmp", T0)),
    ];
    for tmp in &temporaries {
        std::fs::write(tmp, b"half a file").expect("write");
    }
    let foreign = blocks.join("raw").join("notes.txt");
    std::fs::write(&foreign, b"not the store's").expect("write");
    let store = open_quiet(dir.path(), &clock).expect("opens");
    for tmp in &temporaries {
        assert!(!tmp.exists(), "{tmp:?} is gone");
    }
    assert!(foreign.exists(), "a foreign file is left alone");
    assert_eq!(raw(&store, "a"), expected);
    shut(store);
}

#[test]
fn a_row_less_file_no_rule_explains_refuses_the_open_and_is_kept() {
    let (dir, clock, _, _) = handed_off();
    let stray = block_path(dir.path(), Tier::Raw, T0 - HOUR);
    let span = SpanStart::new(Tier::Raw, T0 - HOUR).expect("grid");
    let chunk = [ChunkRef {
        id: SeriesId::from_raw(0),
        seq: 0,
        bytes: b"x",
    }];
    std::fs::write(
        &stray,
        encode_block(Tier::Raw, span, &chunk).expect("one chunk"),
    )
    .expect("write");
    match open_quiet(dir.path(), &clock) {
        Err(OpenError::UnexplainedBlock(named)) => {
            assert!(named.contains(&format!("{}.blk", T0 - HOUR)), "{named}");
        }
        Err(other) => panic!("refused for another reason: {other}"),
        Ok(_) => panic!("opened with a file no rule explains"),
    }
    assert!(stray.exists(), "nothing is deleted");
    assert!(block_path(dir.path(), Tier::Raw, T0).exists());
}

/// A handoff that crashed after its rename and before its commit: the file and the span's
/// chunks both exist, the span is closed and every tail has it sealed.
#[test]
fn a_row_less_copy_of_a_span_still_in_redb_is_deleted_at_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0);
    let store = open_quiet(dir.path(), &clock).expect("opens");
    let expected = fill_first_span(&store, &clock);
    shut(store);
    let path = block_path(dir.path(), Tier::Raw, T0);
    plant(&path, &file_from_redb(dir.path(), Tier::Raw, T0, T0));
    let store = open_quiet(dir.path(), &clock).expect("opens");
    assert!(!path.exists(), "the redundant copy is deleted");
    assert_eq!(raw(&store, "a"), expected, "the span is read from redb");
    shut(store);
}

/// Rule (a)'s two other conditions: a copy is redundant only once its span is closed by the
/// persisted clock and no tail holds it open; otherwise the open refuses.
#[test]
fn a_row_less_copy_of_a_span_not_yet_closed_or_held_open_refuses_the_open() {
    type Fill = fn(&Store<()>, &TestClock);
    let within_grace: Fill = |store, clock| {
        for i in 0..10u64 {
            append_at(store, clock, T0 + i * 60, 100);
        }
        // In the next span, but within the grace: the span at T0 isn't closed.
        append_at(store, clock, T0 + HOUR + 100, 100);
    };
    // (name, fill, the system clock at the reopen: `None` leaves it where the fill did).
    let cases: [(&str, Fill, Option<u64>); 3] = [
        ("not closed by the persisted clock", within_grace, None),
        (
            "closed by the live clock only: the persisted one decides",
            within_grace,
            Some(T0 + DAY),
        ),
        (
            "a tail holds it open",
            |store, clock| {
                for i in 0..10u64 {
                    append_at(store, clock, T0 + i * 60, 100);
                }
                // `a` moves on and seals its chunk of T0; `b` stays quiet, open in T0.
                append_one(store, clock, "a", T0 + HOUR + 200, 100);
            },
            None,
        ),
    ];
    for (name, fill, reopen_at) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock = TestClock::at(T0);
        let store = open_quiet(dir.path(), &clock).expect("opens");
        fill(&store, &clock);
        shut(store);
        if let Some(t) = reopen_at {
            clock.set(t);
        }
        let path = block_path(dir.path(), Tier::Raw, T0);
        plant(&path, &file_from_redb(dir.path(), Tier::Raw, T0, T0));
        match open_quiet(dir.path(), &clock) {
            Err(OpenError::UnexplainedBlock(named)) => {
                assert!(named.contains(&format!("{T0}.blk")), "{name}: {named}");
            }
            Err(other) => panic!("{name}: refused for another reason: {other}"),
            Ok(_) => panic!("{name}: opened, so the copy was taken as redundant"),
        }
        assert!(path.exists(), "{name}: nothing is deleted");
    }
}

#[test]
fn a_handoff_never_replaces_an_existing_file_and_retries_once_it_is_gone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0);
    let store = open(dir.path(), &clock);
    let path = block_path(dir.path(), Tier::Raw, T0);
    plant(&path, b"someone else's");
    let expected = fill_first_span(&store, &clock);
    wait_for_rows(&store, Tier::Minute, 1);
    wait_for("a failed raw handoff", || {
        store.stats().expect("stats").handoff_failures() >= 1
    });
    assert_eq!(std::fs::read(&path).expect("read"), b"someone else's");
    assert_eq!(store.stats().expect("stats").block_files(Tier::Raw), 0);
    assert_eq!(
        raw(&store, "a"),
        expected,
        "the span is still served from redb"
    );
    std::fs::remove_file(&path).expect("remove");
    wait_for_rows(&store, Tier::Raw, 1);
    assert!(path.exists(), "the retry wrote the span's file");
    assert_eq!(raw(&store, "a"), expected);
    shut(store);
    let (chunks, rows) = redb_spans(dir.path());
    assert!(rows.contains(&(Tier::Raw.code(), T0)), "{rows:?}");
    assert!(!chunks.contains(&(Tier::Raw.code(), T0)), "{chunks:?}");
}

#[test]
fn a_fourth_span_waiting_on_a_failing_handoff_fails_the_store_stop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0);
    let store = open(dir.path(), &clock);
    // A directory where each raw span's temporary would go: its writes fail.
    for i in 0..5u64 {
        let tmp = dir
            .path()
            .join("blocks")
            .join("raw")
            .join(format!("{}.blk.tmp", T0 + i * HOUR));
        std::fs::create_dir_all(tmp).expect("dir");
    }
    for i in 0..4u64 {
        append_at(&store, &clock, T0 + i * HOUR, 100);
    }
    // Three raw spans closed and failing: the store keeps running.
    append_at(&store, &clock, T0 + 3 * HOUR + 200, 100);
    wait_for_rows(&store, Tier::Minute, 3);
    wait_for("three failed raw spans", || {
        store.stats().expect("stats").handoff_failures() >= 3
    });
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        query(&store, "a", Tier::Raw).is_ok(),
        "three spans waiting are within the bound"
    );
    // A fourth closes.
    append_at(&store, &clock, T0 + 4 * HOUR + 200, 100);
    wait_for("the fail-stop", || {
        query(&store, "a", Tier::Raw) == Err(StoreError::Failed)
    });
}

/// RFC 0010 §6: a failed handoff is counted with its cause, per tier.
#[test]
fn a_failed_handoff_reports_its_cause_for_its_tier() {
    // (name, the failing tier, the tier that hands off, what is planted, the cause)
    type Case = (&'static str, Tier, Tier, fn(&Path), HandoffFailure);
    let cases: [Case; 3] = [
        (
            "a raw file of the span's name exists",
            Tier::Raw,
            Tier::Minute,
            |dir| plant(&block_path(dir, Tier::Raw, T0), b"someone else's"),
            HandoffFailure::Exists,
        ),
        (
            "the raw temporary can't be created",
            Tier::Raw,
            Tier::Minute,
            |dir| std::fs::create_dir_all(temporary(dir, Tier::Raw, T0)).expect("dir"),
            HandoffFailure::Write(std::io::ErrorKind::AlreadyExists),
        ),
        (
            "a minute file of the span's name exists",
            Tier::Minute,
            Tier::Raw,
            |dir| plant(&block_path(dir, Tier::Minute, T0), b"someone else's"),
            HandoffFailure::Exists,
        ),
    ];
    for (name, failing, landing, planted, cause) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock = TestClock::at(T0);
        let store = open(dir.path(), &clock);
        assert_eq!(
            store.stats().expect("stats").last_handoff_failure(failing),
            None,
            "{name}: nothing failed yet"
        );
        planted(dir.path());
        fill_first_span(&store, &clock);
        wait_for_rows(&store, landing, 1);
        wait_for(&format!("{name}: a failure"), || {
            store.stats().expect("stats").handoff_failures() >= 1
        });
        let stats = store.stats().expect("stats");
        assert_eq!(stats.last_handoff_failure(failing), Some(cause), "{name}");
        assert_eq!(stats.last_handoff_failure(landing), None, "{name}");
        assert_eq!(
            store.fail_cause(),
            None,
            "{name}: one failure doesn't stop the store"
        );
        shut(store);
    }
}

/// Where a span's temporary goes while its file is written.
fn temporary(dir: &Path, tier: Tier, span: u64) -> PathBuf {
    dir.join("blocks")
        .join(tier.name())
        .join(format!("{span}.blk.tmp"))
}

/// A tier's last failure is the latest one, not the first.
#[test]
fn a_tiers_last_handoff_failure_is_its_latest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0);
    let store = open(dir.path(), &clock);
    let tmp = temporary(dir.path(), Tier::Raw, T0);
    std::fs::create_dir_all(&tmp).expect("dir");
    fill_first_span(&store, &clock);
    wait_for("the first failure", || {
        store
            .stats()
            .expect("stats")
            .last_handoff_failure(Tier::Raw)
            == Some(HandoffFailure::Write(std::io::ErrorKind::AlreadyExists))
    });
    plant(&block_path(dir.path(), Tier::Raw, T0), b"someone else's");
    std::fs::remove_dir(&tmp).expect("rmdir");
    wait_for("the second, different failure", || {
        store
            .stats()
            .expect("stats")
            .last_handoff_failure(Tier::Raw)
            == Some(HandoffFailure::Exists)
    });
    shut(store);
}

/// A panic on the writer is no decision of the store's: it fails stop with no cause.
#[test]
fn a_writer_panic_fails_the_store_with_no_cause() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = open(dir.path(), &TestClock::at(T0));
    let answer = store.transact(
        hub_store::store::Commit::Batched,
        |_| -> Result<((), hub_store::store::Answer), hub_store::store::Abort<()>> {
            panic!("injected");
        },
    );
    assert!(answer.is_err(), "the panic fails the store");
    assert_eq!(query(&store, "a", Tier::Raw), Err(StoreError::Failed));
    assert_eq!(store.fail_cause(), None);
}

/// RFC 0010 §6: the backlog's fail-stop names the tier and the block-write failure.
#[test]
fn the_backlog_fail_stop_names_its_tier_and_cause() {
    // (name, what is planted for each span, the tier it fails, the last cause)
    type Case = (&'static str, fn(&Path, u64), Tier, HandoffFailure);
    let cases: [Case; 3] = [
        (
            "raw temporaries that can't be created",
            |dir, span| std::fs::create_dir_all(temporary(dir, Tier::Raw, span)).expect("dir"),
            Tier::Raw,
            HandoffFailure::Write(std::io::ErrorKind::AlreadyExists),
        ),
        (
            "raw files of the spans' names exist",
            |dir, span| plant(&block_path(dir, Tier::Raw, span), b"someone else's"),
            Tier::Raw,
            HandoffFailure::Exists,
        ),
        (
            "minute files of the spans' names exist",
            |dir, span| plant(&block_path(dir, Tier::Minute, span), b"someone else's"),
            Tier::Minute,
            HandoffFailure::Exists,
        ),
    ];
    for (name, planted, tier, last) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock = TestClock::at(T0);
        let store = open(dir.path(), &clock);
        for i in 0..5u64 {
            planted(dir.path(), T0 + i * HOUR);
        }
        for i in 0..5u64 {
            append_at(&store, &clock, T0 + i * HOUR, 100);
        }
        append_at(&store, &clock, T0 + 4 * HOUR + 200, 100);
        wait_for(&format!("{name}: the fail-stop"), || {
            query(&store, "a", Tier::Raw) == Err(StoreError::Failed)
        });
        assert_eq!(
            store.fail_cause(),
            Some(FailCause::HandoffBacklog { tier, last }),
            "{name}"
        );
    }
}
