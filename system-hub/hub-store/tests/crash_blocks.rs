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

//! The span handoff under `SIGKILL` (RFC 0010 Testing plan, *Block files and the span
//! handoff*): a child process appends across many spans with a 20 ms sweep, so spans are
//! handed off while it runs, and is killed at random moments (inside a handoff's file write,
//! between its rename and its commit, or anywhere else). After each kill the store reopens and:
//! - every point appended before the last completed durable commit is read back, exactly once,
//!   as an unbroken prefix, and the minute and hour buckets equal a recomputation from it;
//! - no span is both in `chunks` and in a block file, every block file has its row and every
//!   row its file, and no temporary is left.
//!
//! One series is silent for the first ten minutes of each raw span, past the previous span's
//! end and grace, so its chunk of that span is sealed by the sweep that makes the span due:
//! handing a span off before the commit holding those seals would lose its points. At the end, a reopen past every span's grace must hand off every
//! closed span of every tier, including those waiting when a kill hit. A `SIGKILL` keeps the
//! page cache, so nothing here can show a missing `fsync`: that is review's.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hub_store::codec::{Bucket, RawPoint};
use hub_store::name::{Generation, MetricName, SystemKey};
use hub_store::rollup::Accumulator;
use hub_store::series::SeriesKey;
use hub_store::store::{
    Abort, Answer, CatalogTable, Commit, Limit, Order, Query, Samples, Store, StoreOptions,
    SystemClock,
};
use hub_store::tier::{RollupTier, Tier};
use hub_store::value::ValueKind;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

const T0: u64 = 1_800_057_600;
/// Seconds between a series' points: a raw span every ~116 iterations, an hour span every ~2,800.
const STEP: u64 = 31;
const SERIES: [&str; 3] = ["cpu", "memory", "load1"];
/// Silent for the first ten minutes of each raw span (longer than a span's grace).
const QUIET: &str = "quiet";
const HOUR: u64 = 3_600;
const DAY: u64 = 86_400;

/// Whether iteration `i` appends to the quiet series.
fn quiet_at(i: u64) -> bool {
    (T0 + STEP * i) % HOUR >= 600
}
const CHILD: &str = "HUB_STORE_CRASH_BLOCKS_CHILD";
const PROGRESS: CatalogTable = CatalogTable::new("catalog.crash_progress");
const CHUNKS: TableDefinition<(u8, u64, u32, u16), &[u8]> = TableDefinition::new("chunks");
const BLOCKS: TableDefinition<(u8, u64), &[u8]> = TableDefinition::new("blocks");

struct StepClock(Arc<AtomicU64>);

impl SystemClock for StepClock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn kind(name: &str) -> ValueKind {
    if name == "load1" {
        ValueKind::Load
    } else {
        ValueKind::Percent
    }
}

fn key(name: &str) -> SeriesKey {
    SeriesKey {
        system: SystemKey::try_from(&b"crash"[..]).expect("valid"),
        generation: Generation::new(1),
        metric: MetricName::try_from(name).expect("valid"),
    }
}

fn value(i: u64) -> i64 {
    ((i * 7_919) % 10_000) as i64
}

fn open(dir: &Path, clock: Arc<AtomicU64>, sweep: Duration) -> Store<()> {
    let options = StoreOptions {
        commit_interval: Duration::from_millis(20),
        tail_rotation: Duration::from_millis(500),
        sweep_interval: sweep,
        ..StoreOptions::with_clock(Arc::new(StepClock(clock)))
    };
    Store::open(dir, options).expect("opens")
}

fn query(store: &Store<()>, name: &str, tier: Tier, from: u64, order: Order) -> Samples {
    let q = Query {
        from,
        until: u64::MAX,
        tier,
        order,
        limit: Limit::new(Limit::MAX),
    };
    store
        .query(&key(name), &q)
        .expect("query")
        .map_or(Samples::Raw(Vec::new()), |s| s.samples)
}

/// Every raw point of a series, a query's limit at a time.
fn raw(store: &Store<()>, name: &str) -> Vec<RawPoint> {
    let mut all: Vec<RawPoint> = Vec::new();
    loop {
        let from = all.last().map_or(0, |p| p.ts + 1);
        let Samples::Raw(page) = query(store, name, Tier::Raw, from, Order::Earliest) else {
            panic!("raw query answered buckets");
        };
        if page.is_empty() {
            return all;
        }
        all.extend(page);
    }
}

/// Every closed bucket of a series in a rollup tier, a query's limit at a time.
fn rollups(store: &Store<()>, name: &str, tier: RollupTier) -> Vec<Bucket> {
    let mut all: Vec<Bucket> = Vec::new();
    loop {
        let from = all.last().map_or(0, |b| (b.index + 1) * tier.bucket_secs());
        let Samples::Rollup(page) = query(store, name, tier.tier(), from, Order::Earliest) else {
            panic!("rollup query answered points");
        };
        if page.is_empty() {
            return all;
        }
        all.extend(page);
    }
}

/// The child's workload: from the iteration after the last stored point, one point per series
/// per iteration at hub time `T0 + STEP × i`, and every 25 iterations a durable transaction
/// whose iteration it prints.
#[test]
fn crash_blocks_child() {
    let Ok(dir) = std::env::var(CHILD) else {
        return;
    };
    let clock = Arc::new(AtomicU64::new(T0));
    let store = open(
        Path::new(&dir),
        Arc::clone(&clock),
        Duration::from_millis(20),
    );
    let start = match query(&store, SERIES[0], Tier::Raw, 0, Order::Latest) {
        Samples::Raw(points) => points.last().map_or(0, |p| (p.ts - T0) / STEP + 1),
        Samples::Rollup(_) => panic!("raw query answered buckets"),
    };
    for i in start.. {
        clock.store(T0 + STEP * i, Ordering::SeqCst);
        let quiet = quiet_at(i).then_some(QUIET);
        let points: Vec<_> = SERIES
            .iter()
            .copied()
            .chain(quiet)
            .map(|n| (MetricName::try_from(n).expect("valid"), kind(n), value(i)))
            .collect();
        let report = store
            .append(&key(SERIES[0]).system, Generation::new(1), &points)
            .expect("append");
        assert!(report.rejected.is_empty(), "{report:?}");
        if i % 25 == 0 {
            store
                .transact(Commit::Durable, move |txn| {
                    txn.insert(PROGRESS, b"i", &i.to_be_bytes());
                    Ok::<_, Abort<()>>(((), Answer::AfterCommit))
                })
                .expect("commits");
            println!("durable {i}");
        }
    }
}

/// Runs the child until a random extra time has passed, kills it, and answers the last
/// iteration it reported durable.
fn run_and_kill(dir: &Path, extra: Duration) -> Option<u64> {
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "crash_blocks_child",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env(CHILD, dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawns");
    let stdout = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .filter_map(|l| l.strip_prefix("durable ").and_then(|n| n.parse().ok()))
            .last()
    });
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(300) + extra {
        std::thread::sleep(Duration::from_millis(5));
    }
    child.kill().expect("SIGKILL");
    child.wait().expect("reaped");
    reader.join().expect("reader")
}

fn recomputed(points: &[RawPoint], tier: RollupTier) -> Vec<Bucket> {
    let mut out: Vec<Bucket> = Vec::new();
    let mut acc: Option<Accumulator> = None;
    for p in points {
        match acc.as_mut() {
            Some(a) if a.holds(tier, p.ts) => a.add(p.value),
            _ => {
                out.extend(acc.map(|a| a.close()));
                acc = Some(Accumulator::open(tier, p.ts, p.value));
            }
        }
    }
    // The last bucket too: the sweep closes it once its series goes quiet.
    out.extend(acc.map(|a| a.close()));
    out
}

/// The series' points and buckets, against what the workload appended.
fn check_series(store: &Store<()>, round: u64, durable: u64) {
    for name in SERIES {
        let points = raw(store, name);
        let expected: Vec<RawPoint> = (0..points.len() as u64)
            .map(|i| RawPoint {
                ts: T0 + STEP * i,
                value: value(i),
            })
            .collect();
        assert_eq!(
            points, expected,
            "round {round}, {name}: an unbroken prefix, each once"
        );
        assert!(
            points.len() as u64 > durable,
            "round {round}, {name}: {} points, durable up to {durable}",
            points.len()
        );
        check_rollups(store, name, &points, round);
    }
    // The quiet series shares every append with the others: the same iterations, filtered.
    let iterations = raw(store, SERIES[0]).len() as u64;
    let points = raw(store, QUIET);
    let expected: Vec<RawPoint> = (0..iterations)
        .filter(|&i| quiet_at(i))
        .map(|i| RawPoint {
            ts: T0 + STEP * i,
            value: value(i),
        })
        .collect();
    assert_eq!(
        points, expected,
        "round {round}, {QUIET}: every point, each once"
    );
    check_rollups(store, QUIET, &points, round);
}

/// A series' stored buckets equal a recomputation from its points, all closed ones stored.
fn check_rollups(store: &Store<()>, name: &str, points: &[RawPoint], round: u64) {
    for tier in [RollupTier::Minute, RollupTier::Hour] {
        let stored = rollups(store, name, tier);
        let expected = recomputed(points, tier);
        assert_eq!(
            stored[..],
            expected[..stored.len()],
            "round {round}, {name}, {tier:?}: buckets match raw"
        );
        assert!(
            stored.len() + 2 >= expected.len(),
            "round {round}, {name}, {tier:?}: closed buckets are stored: {} of {}, last \
             stored {:?}, last point {:?}",
            stored.len(),
            expected.len(),
            stored.last().map(|b| b.index),
            points.last().map(|p| p.ts)
        );
    }
}

/// Files and rows of a closed store agree, no span is on both sides, no temporary is left.
/// Answers the rows.
fn check_files(dir: &Path, round: u64) -> usize {
    let mut files: BTreeSet<(u8, u64)> = BTreeSet::new();
    for tier in Tier::ALL {
        for entry in std::fs::read_dir(dir.join("blocks").join(tier.name())).expect("tier dir") {
            let name = entry
                .expect("entry")
                .file_name()
                .into_string()
                .expect("utf-8");
            assert!(
                !name.ends_with(".tmp"),
                "round {round}: a temporary is left: {name}"
            );
            let span = name.strip_suffix(".blk").and_then(|s| s.parse().ok());
            let span = span.unwrap_or_else(|| panic!("round {round}: an odd file: {name}"));
            files.insert((tier.code(), span));
        }
    }
    let db = Database::open(dir.join("hub.redb")).expect("redb opens");
    let read = db.begin_read().expect("read");
    let rows: BTreeSet<(u8, u64)> = read
        .open_table(BLOCKS)
        .expect("blocks")
        .iter()
        .expect("iter")
        .map(|e| e.expect("entry").0.value())
        .collect();
    assert_eq!(
        files, rows,
        "round {round}: every file has its row and every row its file"
    );
    for (tier, span) in &rows {
        let held = read
            .open_table(CHUNKS)
            .expect("chunks")
            .range((*tier, *span, 0u32, 0u16)..=(*tier, *span, u32::MAX, u16::MAX))
            .expect("range")
            .next()
            .is_some();
        assert!(
            !held,
            "round {round}: span {tier}/{span} is both in chunks and in a file"
        );
    }
    rows.len()
}

#[test]
fn a_kill_during_handoffs_loses_no_durable_point_and_leaves_files_and_rows_in_agreement() {
    if std::env::var(CHILD).is_ok() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let mut durable_max = None;
    let mut rows = 0;
    for round in 0..8u64 {
        let extra = Duration::from_millis((round * 389) % 900);
        if let Some(i) = run_and_kill(dir.path(), extra) {
            durable_max = durable_max.max(Some(i));
        }
        let durable = durable_max.expect("the child made durable progress");
        // A quiet sweep: the checks read what the kill left, nothing handed off meanwhile.
        let store = open(
            dir.path(),
            Arc::new(AtomicU64::new(T0)),
            Duration::from_secs(3_600),
        );
        check_series(&store, round, durable);
        store.close().expect("closes");
        drop(store);
        rows = check_files(dir.path(), round);
    }
    assert!(
        rows >= 10,
        "the workload handed spans off: {rows} block files"
    );
    every_closed_span_is_handed_off_after_a_reopen(dir.path());
}

/// Past every span's grace, a reopened store hands off every span of every tier, those left
/// waiting by a kill included, and `chunks` ends empty.
fn every_closed_span_is_handed_off_after_a_reopen(dir: &Path) {
    let clock = Arc::new(AtomicU64::new(T0));
    let store = open(dir, Arc::clone(&clock), Duration::from_secs(3_600));
    let points = raw(&store, SERIES[0]);
    store.close().expect("closes");
    drop(store);
    let last = points.last().expect("points").ts;
    let mut expected = [0u64; 3];
    for tier in Tier::ALL {
        let spans: BTreeSet<u64> = points.iter().map(|p| tier.span_of(p.ts).get()).collect();
        expected[usize::from(tier.code())] = spans.len() as u64;
    }
    clock.store(last + 2 * DAY, Ordering::SeqCst);
    let store = open(dir, clock, Duration::from_millis(20));
    let started = Instant::now();
    loop {
        let stats = store.stats().expect("stats");
        let counts = Tier::ALL.map(|t| stats.block_files(t));
        if counts == expected {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "every closed span handed off: {counts:?} files of {expected:?} spans"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    store.close().expect("closes");
    drop(store);
    // Straight after the live handoffs, with no open in between to clean up after them: every
    // write removed its own temporary.
    check_files(dir, u64::MAX);
    let db = Database::open(dir.join("hub.redb")).expect("redb opens");
    let read = db.begin_read().expect("read");
    let left = read
        .open_table(CHUNKS)
        .expect("chunks")
        .iter()
        .expect("iter")
        .count();
    assert_eq!(left, 0, "no chunk left in redb once every span is closed");
}
