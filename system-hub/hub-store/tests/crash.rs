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

//! Our invariants on top of redb (RFC 0010 Testing plan, *Durability of our invariants*): a
//! child process runs a workload (appends across spans, seals, tail rotation, log truncation)
//! and is killed with `SIGKILL` at random moments; after each kill the store reopens and:
//! - every point appended before the last completed durable commit is read back, exactly once,
//!   and the points read back are an unbroken prefix of what was appended;
//! - the minute and hour buckets equal a recomputation from the raw points.

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

const T0: u64 = 1_800_057_600;
/// Seconds between a series' points: a span of raw points every ~514 iterations.
const STEP: u64 = 7;
const SERIES: [&str; 4] = ["cpu", "memory", "load1", "disk:/"];
const CHILD: &str = "HUB_STORE_CRASH_CHILD";
const PROGRESS: CatalogTable = CatalogTable::new("catalog.crash_progress");

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

fn open(dir: &Path, clock: Arc<AtomicU64>) -> Store<()> {
    let options = StoreOptions {
        commit_interval: Duration::from_millis(20),
        tail_rotation: Duration::from_millis(500),
        ..StoreOptions::with_clock(Arc::new(StepClock(clock)))
    };
    Store::open(dir, options).expect("opens")
}

/// Raw points of a series from `from`, at most `limit`, from the given end.
fn raw_page(store: &Store<()>, name: &str, from: u64, order: Order, limit: u16) -> Vec<RawPoint> {
    let q = Query {
        from,
        until: u64::MAX,
        tier: Tier::Raw,
        order,
        limit: Limit::new(limit),
    };
    match store
        .query(&key(name), &q)
        .expect("query")
        .map(|s| s.samples)
    {
        Some(Samples::Raw(points)) => points,
        Some(Samples::Rollup(_)) => panic!("raw query answered buckets"),
        None => Vec::new(),
    }
}

/// Every raw point of a series, a query's limit at a time.
fn raw(store: &Store<()>, name: &str) -> Vec<RawPoint> {
    let mut all: Vec<RawPoint> = Vec::new();
    loop {
        let from = all.last().map_or(0, |p| p.ts + 1);
        let page = raw_page(store, name, from, Order::Earliest, Limit::MAX);
        if page.is_empty() {
            return all;
        }
        all.extend(page);
    }
}

fn rollups(store: &Store<()>, name: &str, tier: Tier) -> Vec<Bucket> {
    let q = Query {
        from: 0,
        until: u64::MAX,
        tier,
        order: Order::Earliest,
        limit: Limit::new(10_000),
    };
    match store
        .query(&key(name), &q)
        .expect("query")
        .map(|s| s.samples)
    {
        Some(Samples::Rollup(buckets)) => buckets,
        Some(Samples::Raw(_)) => panic!("rollup query answered points"),
        None => Vec::new(),
    }
}

/// The child's workload: from the iteration after the last stored point, append one point to
/// each series per iteration at hub time `T0 + STEP × i`, and every 25 iterations commit a
/// durable transaction and print the iteration it covers.
#[test]
fn crash_child() {
    let Ok(dir) = std::env::var(CHILD) else {
        return;
    };
    let clock = Arc::new(AtomicU64::new(T0));
    let store = open(Path::new(&dir), Arc::clone(&clock));
    let start = raw_page(&store, SERIES[0], 0, Order::Latest, 1)
        .last()
        .map_or(0, |p| (p.ts - T0) / STEP + 1);
    for i in start.. {
        clock.store(T0 + STEP * i, Ordering::SeqCst);
        let points: Vec<_> = SERIES
            .iter()
            .map(|n| (MetricName::try_from(*n).expect("valid"), kind(n), value(i)))
            .collect();
        let report = store
            .append(
                &SystemKey::try_from(&b"crash"[..]).expect("valid"),
                Generation::new(1),
                &points,
            )
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

/// Runs the child until it has printed some durable progress and a random extra time has
/// passed, kills it, and answers the last iteration it reported durable.
fn run_and_kill(dir: &Path, extra: Duration) -> Option<u64> {
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "crash_child",
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
        let mut last = None;
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(i) = line
                .strip_prefix("durable ")
                .and_then(|n| n.parse::<u64>().ok())
            {
                last = Some(i);
            }
        }
        last
    });
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(300) + extra {
        std::thread::sleep(Duration::from_millis(10));
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
    out
}

#[test]
fn every_durable_point_survives_a_kill_exactly_once_and_rollups_match_the_raw_points() {
    if std::env::var(CHILD).is_ok() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let mut durable_max = None;
    for round in 0..6u64 {
        let extra = Duration::from_millis((round * 389) % 900);
        if let Some(i) = run_and_kill(dir.path(), extra) {
            durable_max = durable_max.max(Some(i));
        }
        let clock = Arc::new(AtomicU64::new(T0));
        let store = open(dir.path(), clock);
        for name in SERIES {
            let points = raw(&store, name);
            let expected: Vec<RawPoint> = (0..points.len() as u64)
                .map(|i| RawPoint {
                    ts: T0 + STEP * i,
                    value: value(i),
                })
                .collect();
            assert_eq!(
                points, expected,
                "round {round}, {name}: an unbroken prefix, each point once"
            );
            let durable = durable_max.expect("the child made durable progress");
            assert!(
                points.len() as u64 > durable,
                "round {round}, {name}: {} points, durable up to {durable}",
                points.len()
            );
            for tier in [RollupTier::Minute, RollupTier::Hour] {
                let stored = rollups(&store, name, tier.tier());
                let expected = recomputed(&points, tier);
                assert_eq!(
                    stored[..],
                    expected[..stored.len()],
                    "round {round}, {name}, {tier:?}: buckets match raw"
                );
                assert!(
                    stored.len() + 2 >= expected.len(),
                    "round {round}, {name}, {tier:?}: closed buckets are stored"
                );
            }
        }
        store.close().expect("closes");
    }
    assert!(
        durable_max.is_some_and(|i| i >= 1_000),
        "the workload crossed spans: {durable_max:?}"
    );
}
