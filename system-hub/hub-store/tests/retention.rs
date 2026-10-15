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

//! The retention pass in the running store (RFC 0010 §5): the guarded retention clock, dead
//! chunks unreadable at once, deleted from `chunks`, and block files whose every chunk is dead
//! retired and unlinked. Here the raw tier keeps one hour (its minimum); the rollups keep their
//! defaults.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hub_store::codec::{Bucket, RawPoint};
use hub_store::name::{Generation, MetricName, SystemKey};
use hub_store::retention::{RetentionPolicy, TierPeriod};
use hub_store::series::SeriesKey;
use hub_store::store::{
    Limit, MonotonicClock, Order, Query, Samples, Store, StoreError, StoreOptions, StoreStats,
    SystemClock,
};
use hub_store::tier::Tier;
use hub_store::value::ValueKind;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

/// A day boundary: on every tier's grid.
const T0: u64 = 1_800_057_600;
const HOUR: u64 = 3_600;
const CHUNKS: TableDefinition<(u8, u64, u32, u16), &[u8]> = TableDefinition::new("chunks");
const BLOCKS: TableDefinition<(u8, u64), &[u8]> = TableDefinition::new("blocks");
const BLOCK_GENERATIONS: TableDefinition<(u64, u8, u64), u64> =
    TableDefinition::new("block_generations");
const PENDING_UNLINKS: TableDefinition<(u8, u64, u32), ()> =
    TableDefinition::new("pending_unlinks");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

/// The system clock and the monotonic clock, which move together unless a test parts them.
#[derive(Clone)]
struct Clocks {
    system: Arc<AtomicU64>,
    monotonic: Arc<AtomicU64>,
}

impl SystemClock for Clocks {
    fn now(&self) -> u64 {
        self.system.load(Ordering::SeqCst)
    }
}

impl MonotonicClock for Clocks {
    fn elapsed(&self) -> Duration {
        Duration::from_secs(self.monotonic.load(Ordering::SeqCst))
    }
}

impl Clocks {
    fn at(t: u64) -> Clocks {
        Clocks {
            system: Arc::new(AtomicU64::new(t)),
            monotonic: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Both clocks to `t`: the monotonic clock moves by as much as the system clock.
    fn set(&self, t: u64) {
        let before = self.system.swap(t, Ordering::SeqCst);
        self.monotonic
            .fetch_add(t.saturating_sub(before), Ordering::SeqCst);
    }

    /// The system clock alone: a step the monotonic clock didn't see.
    fn step_system(&self, t: u64) {
        self.system.store(t, Ordering::SeqCst);
    }

    fn advance_monotonic(&self, secs: u64) {
        self.monotonic.fetch_add(secs, Ordering::SeqCst);
    }
}

fn raw_hour() -> RetentionPolicy {
    let hour = TierPeriod::new(Tier::Raw, Duration::from_secs(HOUR)).expect("bounds");
    RetentionPolicy::default().with(hour)
}

/// A store whose commits, retention passes and (unless `quiet`) sweeps run every 20 ms.
fn open_store(dir: &Path, clocks: &Clocks, quiet: bool) -> Store<()> {
    let sweep = match quiet {
        true => Duration::from_secs(3_600),
        false => Duration::from_millis(20),
    };
    let options = StoreOptions {
        commit_interval: Duration::from_millis(20),
        sweep_interval: sweep,
        retention_interval: Duration::from_millis(20),
        retention: raw_hour(),
        monotonic: Arc::new(clocks.clone()),
        ..StoreOptions::with_clock(Arc::new(clocks.clone()))
    };
    Store::open(dir, options).expect("opens")
}

fn open(dir: &Path, clocks: &Clocks) -> Store<()> {
    open_store(dir, clocks, false)
}

/// A store that hands nothing off: closed spans stay in `chunks`.
fn open_quiet(dir: &Path, clocks: &Clocks) -> Store<()> {
    open_store(dir, clocks, true)
}

fn shut(store: Store<()>) {
    store.close().expect("closes");
}

fn system() -> SystemKey {
    SystemKey::try_from(&b"s"[..]).expect("valid")
}

fn key(name: &str, generation: u64) -> SeriesKey {
    SeriesKey {
        system: system(),
        generation: Generation::new(generation),
        metric: MetricName::try_from(name).expect("valid"),
    }
}

fn append(store: &Store<()>, clocks: &Clocks, generation: u64, name: &str, t: u64, value: i64) {
    clocks.set(t);
    let point = (
        MetricName::try_from(name).expect("valid"),
        ValueKind::Percent,
        value,
    );
    let report = store
        .append(&system(), Generation::new(generation), &[point])
        .expect("append");
    assert!(report.rejected.is_empty(), "{report:?}");
}

/// One point of `a` (generation 1) at `t`.
fn append_at(store: &Store<()>, clocks: &Clocks, t: u64, value: i64) {
    append(store, clocks, 1, "a", t, value);
}

fn query(store: &Store<()>, name: &str, tier: Tier) -> Result<Samples, StoreError> {
    let q = Query {
        from: 0,
        until: u64::MAX,
        tier,
        order: Order::Earliest,
        limit: Limit::new(10_000),
    };
    store
        .query(&key(name, 1), &q)
        .map(|s| s.map_or(Samples::Raw(Vec::new()), |s| s.samples))
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

fn stats(store: &Store<()>) -> StoreStats {
    store.stats().expect("stats")
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

/// Waits for `n` more passes to finish, so the next one starts after this call.
fn wait_for_passes(store: &Store<()>, n: u64) {
    let target = stats(store).retention_passes() + n;
    wait_for(&format!("{n} more passes"), || {
        stats(store).retention_passes() >= target
    });
}

/// Ten points of `a` a minute apart in the spans at T0, then one in the next raw span.
fn fill_first_span(store: &Store<()>, clocks: &Clocks) {
    for i in 0..10u64 {
        append_at(store, clocks, T0 + i * 60, (i * 100) as i64);
    }
    append_at(store, clocks, T0 + HOUR + 200, 5_000);
}

/// The last point `fill_first_span` appends, the one in the raw span after T0.
fn later_point() -> RawPoint {
    RawPoint {
        ts: T0 + HOUR + 200,
        value: 5_000,
    }
}

/// (tier code, span start) pairs.
type Spans = Vec<(u8, u64)>;

/// The spans with chunks in `chunks`, and the `blocks` rows, of a closed store.
fn redb_spans(dir: &Path) -> (Spans, Spans) {
    let db = Database::open(dir.join("hub.redb")).expect("redb opens");
    let read = db.begin_read().expect("read");
    let mut chunks: Vec<(u8, u64)> = read
        .open_table(CHUNKS)
        .expect("chunks")
        .iter()
        .expect("iter")
        .map(|e| {
            let (tier, span, _, _) = e.expect("entry").0.value();
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

/// The `block_generations` entries and the `pending_unlinks` keys of a closed store.
type Generations = Vec<((u64, u8, u64), u64)>;
fn redb_files(dir: &Path) -> (Generations, Vec<(u8, u64, u32)>) {
    let db = Database::open(dir.join("hub.redb")).expect("redb opens");
    let read = db.begin_read().expect("read");
    let generations = read
        .open_table(BLOCK_GENERATIONS)
        .expect("block_generations")
        .iter()
        .expect("iter")
        .map(|e| {
            let (k, v) = e.expect("entry");
            (k.value(), v.value())
        })
        .collect();
    let pending = read
        .open_table(PENDING_UNLINKS)
        .expect("pending_unlinks")
        .iter()
        .expect("iter")
        .map(|e| e.expect("entry").0.value())
        .collect();
    (generations, pending)
}

/// The bytes each generation's chunks of a span take in `chunks`, of a closed store.
fn chunk_bytes(dir: &Path, tier: Tier, span: u64, of: &[(u32, u64)]) -> Vec<(u64, u64)> {
    let db = Database::open(dir.join("hub.redb")).expect("redb opens");
    let read = db.begin_read().expect("read");
    let table = read.open_table(CHUNKS).expect("chunks");
    let mut bytes: Vec<(u64, u64)> = Vec::new();
    for e in table
        .range((tier.code(), span, 0u32, 0u16)..=(tier.code(), span, u32::MAX, u16::MAX))
        .expect("range")
    {
        let (k, v) = e.expect("entry");
        let id = k.value().2;
        let (_, generation) = of.iter().find(|(i, _)| *i == id).expect("a known series");
        match bytes.iter_mut().find(|(g, _)| g == generation) {
            Some((_, b)) => *b += v.value().len() as u64,
            None => bytes.push((*generation, v.value().len() as u64)),
        }
    }
    bytes.sort_unstable();
    bytes
}

/// RFC 0010 §2: each pass moves the retention clock by at most twice the monotonic time since
/// the last one, never past hub time or the system clock; the first pass after an open doesn't
/// move it, and it is persisted.
#[test]
fn the_retention_clock_moves_at_most_twice_monotonic_time_and_survives_a_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clocks = Clocks::at(T0);
    let store = open_quiet(dir.path(), &clocks);
    wait_for_passes(&store, 2);
    assert_eq!(
        stats(&store).retention_clock(),
        T0,
        "created at the open's clock"
    );
    // The system clock jumps ten days; the monotonic clock saw one hour.
    clocks.step_system(T0 + 240 * HOUR);
    clocks.advance_monotonic(HOUR);
    wait_for("the clock to move", || stats(&store).retention_clock() > T0);
    wait_for_passes(&store, 2);
    assert_eq!(
        stats(&store).retention_clock(),
        T0 + 2 * HOUR,
        "twice the hour"
    );
    // The monotonic clock catches up: hub time bounds it.
    clocks.advance_monotonic(1_000 * HOUR);
    wait_for("the clock to reach hub time", || {
        stats(&store).retention_clock() == T0 + 240 * HOUR
    });
    // The system clock is set back: hub time holds, and the retention clock stops at the
    // system clock.
    clocks.step_system(T0 + 100 * HOUR);
    clocks.advance_monotonic(HOUR);
    wait_for("the clock to stop at the system clock", || {
        stats(&store).retention_clock() == T0 + 100 * HOUR
    });
    shut(store);

    clocks.step_system(T0 + 300 * HOUR);
    let store = open_quiet(dir.path(), &clocks);
    assert_eq!(
        stats(&store).retention_clock(),
        T0 + 100 * HOUR,
        "persisted"
    );
    wait_for_passes(&store, 2);
    assert_eq!(
        stats(&store).retention_clock(),
        T0 + 100 * HOUR,
        "the first pass after the open measures nothing, and no monotonic time passed since"
    );
    clocks.advance_monotonic(HOUR);
    wait_for_passes(&store, 2);
    assert_eq!(stats(&store).retention_clock(), T0 + 102 * HOUR);
    shut(store);
}

/// RFC 0010 §5: a query never returns a dead chunk, wherever it is. Here `q`'s points are
/// still unsealed in the head (nothing sweeps, and `q` gets no later point), and its raw span
/// past its period is unreadable at once.
#[test]
fn a_query_never_returns_points_of_a_span_past_its_period() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clocks = Clocks::at(T0);
    let store = open_quiet(dir.path(), &clocks);
    for i in 0..10u64 {
        append(&store, &clocks, 1, "q", T0 + i * 60, (i * 100) as i64);
    }
    assert_eq!(raw(&store, "q").len(), 10, "every point while young");
    let minutes = buckets(&store, "q", Tier::Minute);
    // The system clock jumps five hours, which the monotonic clock didn't see: hub time
    // follows at once, the retention clock doesn't, and the points still read. Two passes
    // first: the second begins after the appends, so it measures all their monotonic time.
    wait_for_passes(&store, 2);
    clocks.step_system(T0 + 5 * HOUR);
    let point = (
        MetricName::try_from("a").expect("valid"),
        ValueKind::Percent,
        6_000,
    );
    let report = store
        .append(&system(), Generation::new(1), &[point])
        .expect("append");
    assert_eq!(
        report.at,
        T0 + 5 * HOUR,
        "hub time follows the system clock"
    );
    wait_for_passes(&store, 2);
    assert!(
        stats(&store).retention_clock() <= T0 + 9 * 60,
        "the retention clock held at the last point's time: {}",
        stats(&store).retention_clock()
    );
    assert_eq!(
        raw(&store, "q").len(),
        10,
        "read by the retention clock, not hub time"
    );
    // The raw span at T0 ends at T0 + 1 h: past its hour once the retention clock passes
    // T0 + 2 h, which five monotonic hours allow.
    clocks.advance_monotonic(5 * HOUR);
    wait_for("the clock to reach hub time", || {
        stats(&store).retention_clock() == T0 + 5 * HOUR
    });
    assert_eq!(raw(&store, "q"), Vec::new());
    assert_eq!(
        buckets(&store, "q", Tier::Minute),
        minutes,
        "the minute tier keeps its fourteen days"
    );
    assert_eq!(
        raw(&store, "a"),
        vec![RawPoint {
            ts: T0 + 5 * HOUR,
            value: 6_000
        }],
        "a young point reads"
    );
    shut(store);
}

/// RFC 0010 §5 pass step 3: dead chunks of a span not yet handed off are deleted from
/// `chunks`; the live ones stay.
#[test]
fn dead_chunks_still_in_redb_are_deleted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clocks = Clocks::at(T0);
    let store = open_quiet(dir.path(), &clocks);
    fill_first_span(&store, &clocks);
    append_at(&store, &clocks, T0 + 2 * HOUR + 30 * 60, 6_000);
    wait_for("the clock to reach hub time", || {
        stats(&store).retention_clock() == T0 + 2 * HOUR + 30 * 60
    });
    wait_for_passes(&store, 2);
    shut(store);
    let (chunks, rows) = redb_spans(dir.path());
    assert!(rows.is_empty(), "nothing was handed off: {rows:?}");
    assert!(
        !chunks.contains(&(Tier::Raw.code(), T0)),
        "the raw span at T0 is gone: {chunks:?}"
    );
    assert!(
        chunks.contains(&(Tier::Raw.code(), T0 + HOUR)),
        "the next raw span is live: {chunks:?}"
    );
    assert!(
        chunks.contains(&(Tier::Minute.code(), T0)),
        "the minute span at T0 is live: {chunks:?}"
    );
}

/// RFC 0010 §5: each `blocks` row comes with one `block_generations` entry per generation in
/// its file, holding that generation's chunk bytes.
#[test]
fn a_handoff_records_each_generations_bytes_in_its_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clocks = Clocks::at(T0);
    let store = open_quiet(dir.path(), &clocks);
    for i in 0..10u64 {
        append(&store, &clocks, 1, "a", T0 + i * 60, 100);
        append(&store, &clocks, 2, "a", T0 + i * 60 + 1, 200 + i as i64);
        append(&store, &clocks, 2, "b", T0 + i * 60 + 2, 300);
    }
    // A point in the next span seals the raw chunks; one in a later minute closes the first
    // bucket of the next span, which seals the minute chunks of the span at T0.
    for t in [T0 + HOUR + 200, T0 + HOUR + 300] {
        for (generation, name) in [(1, "a"), (2, "a"), (2, "b")] {
            append(&store, &clocks, generation, name, t, 400);
        }
    }
    shut(store);
    // Series ids follow the first appends: 0 is (1, a), 1 is (2, a), 2 is (2, b).
    let ids = [(0u32, 1u64), (1, 2), (2, 2)];
    let raw_bytes = chunk_bytes(dir.path(), Tier::Raw, T0, &ids);
    let minute_bytes = chunk_bytes(dir.path(), Tier::Minute, T0, &ids);
    assert_eq!(raw_bytes.len(), 2, "both generations sealed raw chunks");
    assert_eq!(
        minute_bytes.len(),
        2,
        "both generations sealed minute chunks"
    );

    let store = open(dir.path(), &clocks);
    wait_for("both files", || {
        let s = stats(&store);
        s.block_files(Tier::Raw) == 1 && s.block_files(Tier::Minute) == 1
    });
    shut(store);
    let (generations, pending) = redb_files(dir.path());
    let mut expected: Generations = raw_bytes
        .iter()
        .map(|&(g, b)| ((g, Tier::Raw.code(), T0), b))
        .chain(
            minute_bytes
                .iter()
                .map(|&(g, b)| ((g, Tier::Minute.code(), T0), b)),
        )
        .collect();
    expected.sort_unstable();
    assert_eq!(generations, expected);
    assert!(pending.is_empty(), "{pending:?}");
}

/// RFC 0010 §5 pass step 4: a block file whose every chunk is dead is retired: its row and its
/// `block_generations` entries go in one commit, then the file is unlinked and its
/// `pending_unlinks` entry cleared. Other tiers' files stay.
#[test]
fn a_block_file_past_its_period_is_retired_and_unlinked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clocks = Clocks::at(T0);
    let store = open(dir.path(), &clocks);
    fill_first_span(&store, &clocks);
    wait_for("the spans at T0 handed off", || {
        let s = stats(&store);
        s.block_files(Tier::Raw) == 1 && s.block_files(Tier::Minute) == 1
    });
    let minutes = buckets(&store, "a", Tier::Minute);
    assert!(block_path(dir.path(), Tier::Raw, T0).exists());
    // Past the raw span at T0 by a minute, before the span at T0 + 1 h closes.
    append_at(&store, &clocks, T0 + 2 * HOUR + 60, 6_000);
    wait_for("the raw file retired", || {
        stats(&store).block_files(Tier::Raw) == 0
    });
    wait_for("the raw file unlinked", || {
        !block_path(dir.path(), Tier::Raw, T0).exists()
    });
    assert!(block_path(dir.path(), Tier::Minute, T0).exists());
    assert_eq!(stats(&store).block_files(Tier::Minute), 1);
    assert_eq!(
        raw(&store, "a")[0],
        later_point(),
        "the span at T0 reads empty"
    );
    assert_eq!(
        buckets(&store, "a", Tier::Minute)[..minutes.len()],
        minutes[..]
    );
    wait_for_passes(&store, 2);
    shut(store);

    let (_, rows) = redb_spans(dir.path());
    assert!(!rows.contains(&(Tier::Raw.code(), T0)), "{rows:?}");
    assert!(rows.contains(&(Tier::Minute.code(), T0)), "{rows:?}");
    let (generations, pending) = redb_files(dir.path());
    assert!(
        generations
            .iter()
            .all(|((_, tier, _), _)| *tier != Tier::Raw.code()),
        "the raw file's entries went with its row: {generations:?}"
    );
    assert!(
        generations
            .iter()
            .any(|((_, tier, span), _)| (*tier, *span) == (Tier::Minute.code(), T0)),
        "the minute file's stay: {generations:?}"
    );
    assert!(pending.is_empty(), "the unlink was recorded: {pending:?}");
    let store = open(dir.path(), &clocks);
    assert_eq!(stats(&store).block_files(Tier::Raw), 0, "a clean reopen");
    assert_eq!(raw(&store, "a")[0], later_point());
    shut(store);
}

/// RFC 0010 §5, §6: a span whose chunks the pass deleted has nothing left to hand off, so its
/// failing handoff no longer counts toward the tier's backlog of three.
#[test]
fn a_failing_span_whose_chunks_expired_no_longer_counts_toward_the_backlog() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clocks = Clocks::at(T0);
    let store = open(dir.path(), &clocks);
    // A directory where each raw span's temporary would go: its writes fail.
    for i in 0..5u64 {
        let tmp = dir
            .path()
            .join("blocks")
            .join("raw")
            .join(format!("{}.blk.tmp", T0 + i * HOUR));
        std::fs::create_dir_all(tmp).expect("dir");
    }
    for i in 0..3u64 {
        append_at(&store, &clocks, T0 + i * HOUR, 100);
    }
    append_at(&store, &clocks, T0 + 3 * HOUR + 200, 100);
    wait_for("three failed raw spans", || {
        stats(&store).handoff_failures() >= 3
    });
    // The three expire: the span at T0 + 2 h ends at T0 + 3 h, an hour before this.
    append_at(&store, &clocks, T0 + 4 * HOUR + 60, 100);
    wait_for("the clock", || {
        stats(&store).retention_clock() == T0 + 4 * HOUR + 60
    });
    wait_for_passes(&store, 2);
    let before = stats(&store).handoff_failures();
    // Two more spans close and fail: with the three forgotten, two wait.
    append_at(&store, &clocks, T0 + 5 * HOUR + 200, 100);
    wait_for("two more failures", || {
        stats(&store).handoff_failures() >= before + 2
    });
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        query(&store, "a", Tier::Raw).is_ok(),
        "the store still runs: {:?}",
        store.fail_cause()
    );
    shut(store);
}

/// RFC 0010 §5: a query never reads a dead span's file, since retention may be unlinking it.
/// Here the raw file at T0 is unreadable and still has its row (the store commits nothing
/// while the test runs, so no pass step retires it): a query that opened it would answer
/// `Corrupt`; one that skips it answers the live points.
#[test]
fn a_query_never_opens_the_file_of_a_dead_span() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clocks = Clocks::at(T0);
    let store = open(dir.path(), &clocks);
    fill_first_span(&store, &clocks);
    wait_for("the raw span at T0 handed off", || {
        stats(&store).block_files(Tier::Raw) == 1
    });
    shut(store);
    std::fs::write(block_path(dir.path(), Tier::Raw, T0), b"damaged").expect("damage");
    // The retention clock a minute past the span's hour, as a pass would have persisted it.
    let past = T0 + 2 * HOUR + 60;
    {
        let db = Database::open(dir.path().join("hub.redb")).expect("redb opens");
        let txn = db.begin_write().expect("write");
        txn.open_table(META)
            .expect("meta")
            .insert("retention_clock", past.to_be_bytes().as_slice())
            .expect("insert");
        txn.commit().expect("commit");
    }
    clocks.set(past);
    let options = StoreOptions {
        commit_interval: Duration::from_secs(3_600),
        sweep_interval: Duration::from_secs(3_600),
        retention_interval: Duration::from_secs(3_600),
        retention: raw_hour(),
        monotonic: Arc::new(clocks.clone()),
        ..StoreOptions::with_clock(Arc::new(clocks.clone()))
    };
    let store: Store<()> = Store::open(dir.path(), options).expect("opens");
    assert_eq!(
        stats(&store).block_files(Tier::Raw),
        1,
        "the row is still there"
    );
    assert_eq!(
        query(&store, "a", Tier::Raw),
        Ok(Samples::Raw(vec![later_point()]))
    );
    drop(store);
}
