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

//! The store over a temporary redb file (RFC 0010 §6, §9; RFC 0011 §1): appends and queries,
//! the group commit and its classes, the commit hook, catalog transactions, reopening.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hub_store::codec::{Bucket, RawPoint};
use hub_store::name::{Generation, MetricName, SystemKey};
use hub_store::series::SeriesKey;
use hub_store::store::{
    Abort, Answer, CatalogTable, Commit, Limit, Order, Query, Rejected, Samples, Store, StoreError,
    StoreOptions, SystemClock, TransactError,
};
use hub_store::tier::Tier;
use hub_store::value::ValueKind;

const T0: u64 = 1_800_057_600;
const SYSTEMS: CatalogTable = CatalogTable::new("catalog.test_systems");

/// A system clock the test moves by hand.
#[derive(Clone)]
struct TestClock(Arc<AtomicU64>);

impl TestClock {
    fn at(t: u64) -> TestClock {
        TestClock(Arc::new(AtomicU64::new(t)))
    }
    fn set(&self, t: u64) {
        self.0.store(t, Ordering::SeqCst);
    }
}

impl SystemClock for TestClock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn options(clock: &TestClock, commit_interval: Duration) -> StoreOptions {
    StoreOptions {
        commit_interval,
        ..StoreOptions::with_clock(Arc::new(clock.clone()))
    }
}

type TestStore = Store<String>;

fn open(dir: &std::path::Path, clock: &TestClock) -> TestStore {
    Store::open(dir, options(clock, Duration::from_millis(40))).expect("opens")
}

fn system(id: &str) -> SystemKey {
    SystemKey::try_from(id.as_bytes()).expect("valid")
}

fn metric(name: &str) -> MetricName {
    MetricName::try_from(name).expect("valid")
}

fn series(id: &str, generation: u64, name: &str) -> SeriesKey {
    SeriesKey {
        system: system(id),
        generation: Generation::new(generation),
        metric: metric(name),
    }
}

fn raw(store: &TestStore, key: &SeriesKey) -> Vec<RawPoint> {
    let q = Query {
        from: 0,
        until: u64::MAX,
        tier: Tier::Raw,
        order: Order::Earliest,
        limit: Limit::new(10_000),
    };
    match store.query(key, &q).expect("query").map(|s| s.samples) {
        Some(Samples::Raw(points)) => points,
        Some(Samples::Rollup(_)) => panic!("raw query answered buckets"),
        None => Vec::new(),
    }
}

fn buckets(store: &TestStore, key: &SeriesKey, tier: Tier) -> Vec<Bucket> {
    let q = Query {
        from: 0,
        until: u64::MAX,
        tier,
        order: Order::Earliest,
        limit: Limit::new(10_000),
    };
    match store.query(key, &q).expect("query").map(|s| s.samples) {
        Some(Samples::Rollup(b)) => b,
        Some(Samples::Raw(_)) => panic!("rollup query answered points"),
        None => Vec::new(),
    }
}

fn append_one(
    store: &TestStore,
    id: &str,
    name: &str,
    kind: ValueKind,
    value: i64,
) -> Vec<(MetricName, Rejected)> {
    let report = store
        .append(
            &system(id),
            Generation::new(1),
            &[(metric(name), kind, value)],
        )
        .expect("append");
    report.rejected
}

#[test]
fn appended_points_are_stamped_at_hub_time_and_read_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0 + 10);
    let store = open(dir.path(), &clock);
    let report = store
        .append(
            &system("web01"),
            Generation::new(1),
            &[
                (metric("cpu"), ValueKind::Percent, 1_234),
                (metric("load1"), ValueKind::Load, 150),
            ],
        )
        .expect("append");
    assert_eq!(
        (report.at, report.accepted, report.rejected.len()),
        (T0 + 10, 2, 0)
    );
    clock.set(T0 + 12);
    append_one(&store, "web01", "cpu", ValueKind::Percent, 2_000);
    assert_eq!(
        raw(&store, &series("web01", 1, "cpu")),
        vec![
            RawPoint {
                ts: T0 + 10,
                value: 1_234
            },
            RawPoint {
                ts: T0 + 12,
                value: 2_000
            }
        ]
    );
    assert_eq!(
        raw(&store, &series("web01", 1, "load1")),
        vec![RawPoint {
            ts: T0 + 10,
            value: 150
        }]
    );
    assert_eq!(
        raw(&store, &series("web01", 2, "cpu")),
        vec![],
        "another generation is another series"
    );
    assert_eq!(raw(&store, &series("web02", 1, "cpu")), vec![]);
}

#[test]
fn refusals_are_reported_per_point_and_the_rest_is_stored() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0);
    let store = open(dir.path(), &clock);
    append_one(&store, "s", "cpu", ValueKind::Percent, 10);
    let report = store
        .append(
            &system("s"),
            Generation::new(1),
            &[
                (metric("cpu"), ValueKind::Percent, 20),
                (metric("memory"), ValueKind::Percent, 100_001),
                (metric("load1"), ValueKind::Load, 5),
            ],
        )
        .expect("append");
    assert_eq!(report.accepted, 1);
    assert_eq!(
        report.rejected,
        vec![
            (metric("cpu"), Rejected::NotAfterLast),
            (metric("memory"), Rejected::OutOfDomain)
        ]
    );
    clock.set(T0 + 1);
    assert_eq!(
        append_one(&store, "s", "cpu", ValueKind::Count, 3),
        vec![(metric("cpu"), Rejected::KindMismatch)]
    );
    assert_eq!(
        raw(&store, &series("s", 1, "cpu")),
        vec![RawPoint { ts: T0, value: 10 }]
    );
    assert_eq!(
        raw(&store, &series("s", 1, "load1")),
        vec![RawPoint { ts: T0, value: 5 }]
    );
    assert_eq!(raw(&store, &series("s", 1, "memory")), vec![]);
}

#[test]
fn hub_time_holds_when_the_system_clock_steps_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0 + 100);
    let store = open(dir.path(), &clock);
    append_one(&store, "s", "cpu", ValueKind::Percent, 1);
    clock.set(T0 + 50);
    let report = store
        .append(
            &system("s"),
            Generation::new(1),
            &[(metric("cpu"), ValueKind::Percent, 2)],
        )
        .expect("append");
    assert_eq!(report.at, T0 + 100, "held");
    assert_eq!(
        report.rejected,
        vec![(metric("cpu"), Rejected::NotAfterLast)]
    );
    clock.set(T0 + 101);
    assert_eq!(
        append_one(&store, "s", "cpu", ValueKind::Percent, 3),
        vec![]
    );
}

#[test]
fn queries_answer_rollups_and_respect_range_order_and_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0);
    let store = open(dir.path(), &clock);
    for i in 0..600u64 {
        clock.set(T0 + 2 * i);
        append_one(&store, "s", "cpu", ValueKind::Percent, (i % 100) as i64);
    }
    let key = series("s", 1, "cpu");
    let points = raw(&store, &key);
    assert_eq!(points.len(), 600, "every point once, sealed or not");
    assert!(points.windows(2).all(|w| w[0].ts < w[1].ts));
    let minute = buckets(&store, &key, Tier::Minute);
    assert_eq!(
        minute.len(),
        19,
        "20 minutes of points; the last bucket is still open"
    );
    assert_eq!(
        minute[0],
        Bucket {
            index: T0 / 60,
            avg: 15,
            min: 0,
            max: 29,
            count: 30
        }
    );
    let latest = Query {
        from: 0,
        until: u64::MAX,
        tier: Tier::Raw,
        order: Order::Latest,
        limit: Limit::new(3),
    };
    let Some(Samples::Raw(last3)) = store
        .query(&key, &latest)
        .expect("query")
        .map(|s| s.samples)
    else {
        panic!("raw points");
    };
    assert_eq!(
        last3.iter().map(|p| p.ts).collect::<Vec<_>>(),
        vec![T0 + 1_194, T0 + 1_196, T0 + 1_198],
        "ascending"
    );
    let window = Query {
        from: T0 + 100,
        until: T0 + 104,
        tier: Tier::Raw,
        order: Order::Earliest,
        limit: Limit::new(10),
    };
    let Some(Samples::Raw(w)) = store
        .query(&key, &window)
        .expect("query")
        .map(|s| s.samples)
    else {
        panic!("raw points");
    };
    assert_eq!(
        w.iter().map(|p| p.ts).collect::<Vec<_>>(),
        vec![T0 + 100, T0 + 102, T0 + 104],
        "inclusive bounds"
    );
    let none = Query {
        limit: Limit::new(0),
        ..window
    };
    assert_eq!(
        store.query(&key, &none).expect("query").map(|s| s.samples),
        Some(Samples::Raw(vec![]))
    );
}

#[test]
fn a_limit_is_clamped_to_ten_thousand() {
    assert_eq!(Limit::new(10_000).get(), 10_000);
    assert_eq!(Limit::new(10_001).get(), 10_000);
    assert_eq!(Limit::new(u16::MAX).get(), 10_000);
    assert_eq!(Limit::new(0).get(), 0);
}

#[test]
fn points_survive_a_clean_close_and_reopen_as_they_were() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0);
    let key = series("s", 1, "memory");
    let before = {
        let store = open(dir.path(), &clock);
        for i in 0..700u64 {
            clock.set(T0 + 3 * i);
            append_one(
                &store,
                "s",
                "memory",
                ValueKind::Percent,
                (i * 13 % 9_000) as i64,
            );
        }
        let before = (raw(&store, &key), buckets(&store, &key, Tier::Minute));
        store.close().expect("closes");
        assert_eq!(
            store.append(&system("s"), Generation::new(1), &[]).err(),
            Some(StoreError::Closed)
        );
        let q = Query {
            from: 0,
            until: u64::MAX,
            tier: Tier::Raw,
            order: Order::Earliest,
            limit: Limit::new(1),
        };
        assert_eq!(store.query(&key, &q).err(), Some(StoreError::Closed));
        assert_eq!(store.close(), Ok(()), "close is idempotent");
        before
    };
    let store = open(dir.path(), &clock);
    assert_eq!(raw(&store, &key), before.0);
    assert_eq!(buckets(&store, &key, Tier::Minute), before.1);
    clock.set(T0);
    let report = store
        .append(
            &system("s"),
            Generation::new(1),
            &[(metric("memory"), ValueKind::Percent, 1)],
        )
        .expect("append");
    assert_eq!(
        report.at,
        T0 + 3 * 699,
        "hub time resumed from its committed value"
    );
    assert_eq!(
        report.rejected,
        vec![(metric("memory"), Rejected::NotAfterLast)]
    );
    clock.set(T0 + 3 * 700);
    assert_eq!(
        append_one(&store, "s", "memory", ValueKind::Percent, 1),
        vec![],
        "the series goes on"
    );
}

#[test]
fn a_batched_transaction_returns_after_the_next_commit_and_a_durable_one_at_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0);
    let store: TestStore =
        Store::open(dir.path(), options(&clock, Duration::from_millis(400))).expect("opens");
    // Let the first commit pass so the next one is a whole interval away.
    std::thread::sleep(Duration::from_millis(450));
    let started = Instant::now();
    store
        .transact(Commit::Durable, |txn| {
            txn.insert(SYSTEMS, b"a", b"1");
            Ok::<_, Abort<()>>(((), Answer::AfterCommit))
        })
        .expect("commits");
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "durable: {:?}",
        started.elapsed()
    );
    std::thread::sleep(Duration::from_millis(50));
    let started = Instant::now();
    store
        .transact(Commit::Batched, |txn| {
            txn.insert(SYSTEMS, b"b", b"2");
            Ok::<_, Abort<()>>(((), Answer::AfterCommit))
        })
        .expect("commits");
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(150),
        "batched rides the next group commit: {waited:?}"
    );
    let read = store
        .read_catalog(|r| Ok((r.get(SYSTEMS, b"a")?, r.get(SYSTEMS, b"b")?)))
        .expect("reads");
    assert_eq!(
        read,
        (Some(b"1".to_vec()), Some(b"2".to_vec())),
        "read your writes"
    );
}

#[test]
fn transactions_in_one_group_commit_compose_and_the_hook_sees_commits_in_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0);
    let store: Arc<TestStore> = Arc::new(
        Store::open(dir.path(), options(&clock, Duration::from_millis(200))).expect("opens"),
    );
    let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let hook_seen = Arc::clone(&seen);
    store.on_commit(Box::new(move |changes: &[String]| {
        hook_seen.lock().expect("lock").push(changes.to_vec())
    }));
    let threads: Vec<_> = (0..4)
        .map(|i| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                store
                    .transact(Commit::Batched, move |txn| {
                        let count = txn
                            .get(SYSTEMS, b"count")
                            .map_or(0, |v| u64::from_be_bytes(v.try_into().expect("8 bytes")));
                        txn.insert(SYSTEMS, b"count", &(count + 1).to_be_bytes());
                        txn.notify(format!("t{i}"));
                        Ok::<_, Abort<()>>((count, Answer::AfterCommit))
                    })
                    .expect("commits")
            })
        })
        .collect();
    let mut counts: Vec<u64> = threads
        .into_iter()
        .map(|t| t.join().expect("joins"))
        .collect();
    counts.sort_unstable();
    assert_eq!(
        counts,
        vec![0, 1, 2, 3],
        "each transaction read the one before it, committed or not"
    );
    let total = store
        .read_catalog(|r| r.get(SYSTEMS, b"count"))
        .expect("reads");
    assert_eq!(total, Some(4u64.to_be_bytes().to_vec()));
    let seen = seen.lock().expect("lock").concat();
    let mut names = seen.clone();
    names.sort();
    assert_eq!(
        names,
        vec!["t0", "t1", "t2", "t3"],
        "every change reached the hook once"
    );
}

#[test]
fn an_abort_before_any_write_is_answered_at_once_and_writes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0);
    let store: TestStore =
        Store::open(dir.path(), options(&clock, Duration::from_millis(500))).expect("opens");
    std::thread::sleep(Duration::from_millis(550));
    let started = Instant::now();
    let answer = store.transact(Commit::Batched, |txn| {
        if txn.get(SYSTEMS, b"x").is_none() {
            return Err(Abort("absent"));
        }
        Ok(((), Answer::AfterCommit))
    });
    assert!(matches!(
        answer,
        Err(TransactError::Aborted(Abort("absent")))
    ));
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "answered at once"
    );
    assert_eq!(
        store.read_catalog(|r| r.get(SYSTEMS, b"x")).expect("reads"),
        None
    );
}

#[test]
fn a_write_free_transaction_is_answered_at_once_when_nothing_is_uncommitted_beneath_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0);
    let store: TestStore =
        Store::open(dir.path(), options(&clock, Duration::from_millis(500))).expect("opens");
    std::thread::sleep(Duration::from_millis(550));
    let started = Instant::now();
    let read = store
        .transact(Commit::Batched, |txn| {
            Ok::<_, Abort<()>>((txn.get(SYSTEMS, b"x"), Answer::AfterCommit))
        })
        .expect("answers");
    assert_eq!(read, None);
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "no uncommitted write beneath it"
    );
}

#[test]
fn an_identical_write_writes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0);
    let store: TestStore =
        Store::open(dir.path(), options(&clock, Duration::from_millis(500))).expect("opens");
    store
        .transact(Commit::Durable, |txn| {
            txn.insert(SYSTEMS, b"k", b"v");
            Ok::<_, Abort<()>>(((), Answer::AfterCommit))
        })
        .expect("commits");
    std::thread::sleep(Duration::from_millis(550));
    let started = Instant::now();
    let wrote = store
        .transact(Commit::Durable, |txn| {
            txn.insert(SYSTEMS, b"k", b"v");
            Ok::<_, Abort<()>>((txn.wrote(), Answer::AtOnce))
        })
        .expect("answers");
    assert!(!wrote, "the same value is no write");
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "a write-free transaction forces no commit"
    );
}

#[test]
fn catalog_ranges_and_removals_read_back_in_key_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0);
    let store = open(dir.path(), &clock);
    store
        .transact(Commit::Durable, |txn| {
            for key in [&b"a/1"[..], b"a/2", b"a/3", b"b/1"] {
                txn.insert(SYSTEMS, key, key);
            }
            assert!(txn.remove(SYSTEMS, b"a/2"));
            assert!(!txn.remove(SYSTEMS, b"a/9"));
            Ok::<_, Abort<()>>(((), Answer::AfterCommit))
        })
        .expect("commits");
    let keys = store
        .read_catalog(|r| {
            Ok(r.prefix(SYSTEMS, b"a/", 10)?
                .into_iter()
                .map(|(k, _)| k)
                .collect::<Vec<_>>())
        })
        .expect("reads");
    assert_eq!(keys, vec![b"a/1".to_vec(), b"a/3".to_vec()]);
    let first = store
        .read_catalog(|r| r.prefix(SYSTEMS, b"", 1))
        .expect("reads");
    assert_eq!(
        first,
        vec![(b"a/1".to_vec(), b"a/1".to_vec())],
        "the limit bounds the scan"
    );
}

#[test]
fn a_catalog_table_name_must_carry_the_catalog_prefix() {
    let refused = std::panic::catch_unwind(|| CatalogTable::new("series"));
    assert!(
        refused.is_err(),
        "a store table's name can't be a catalog table"
    );
}

#[test]
fn a_transaction_stamps_with_hub_time_and_its_time_is_committed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0 + 500);
    {
        let store = open(dir.path(), &clock);
        let now = store
            .transact(Commit::Durable, |txn| {
                txn.insert(SYSTEMS, b"t", &txn.now().to_be_bytes());
                Ok::<_, Abort<()>>((txn.now(), Answer::AfterCommit))
            })
            .expect("commits");
        assert_eq!(now, T0 + 500);
        store.close().expect("closes");
    }
    clock.set(T0);
    let store = open(dir.path(), &clock);
    let report = store
        .append(
            &system("s"),
            Generation::new(1),
            &[(metric("cpu"), ValueKind::Percent, 1)],
        )
        .expect("append");
    assert_eq!(
        report.at,
        T0 + 500,
        "a reopened clock never issues before what it committed"
    );
}

#[test]
fn a_panic_on_the_writer_thread_fails_the_store_and_every_later_call() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0);
    {
        let store = open(dir.path(), &clock);
        append_one(&store, "s", "cpu", ValueKind::Percent, 1);
        store
            .transact(Commit::Durable, |txn| {
                txn.insert(SYSTEMS, b"kept", b"1");
                Ok::<_, Abort<()>>(((), Answer::AfterCommit))
            })
            .expect("commits");
        let answer = store.transact(Commit::Batched, |txn| -> Result<((), Answer), Abort<()>> {
            txn.insert(SYSTEMS, b"lost", b"1");
            panic!("injected");
        });
        assert_eq!(answer, Err(TransactError::Store(StoreError::Failed)));
        assert_eq!(
            store.append(&system("s"), Generation::new(1), &[]).err(),
            Some(StoreError::Failed)
        );
        let q = Query {
            from: 0,
            until: u64::MAX,
            tier: Tier::Raw,
            order: Order::Earliest,
            limit: Limit::new(1),
        };
        assert_eq!(
            store.query(&series("s", 1, "cpu"), &q).err(),
            Some(StoreError::Failed)
        );
        assert_eq!(
            store.read_catalog(|r| r.get(SYSTEMS, b"kept")).err(),
            Some(StoreError::Failed)
        );
    }
    let store = open(dir.path(), &clock);
    let read = store
        .read_catalog(|r| Ok((r.get(SYSTEMS, b"kept")?, r.get(SYSTEMS, b"lost")?)))
        .expect("reads");
    assert_eq!(
        read,
        (Some(b"1".to_vec()), None),
        "the committed write survives, the failed interval's doesn't"
    );
    assert_eq!(
        raw(&store, &series("s", 1, "cpu")),
        vec![RawPoint { ts: T0, value: 1 }]
    );
}

#[test]
fn an_abort_after_staging_points_fails_the_store_and_nothing_of_it_reopens() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0);
    {
        let store: TestStore =
            Store::open(dir.path(), options(&clock, Duration::from_secs(30))).expect("opens");
        let answer = store.transact(Commit::Batched, |txn| {
            let report = txn.append(
                &system("mail"),
                Generation::new(1),
                &[(metric("cpu"), ValueKind::Percent, 7)],
            );
            assert_eq!(report.accepted, 1);
            assert!(txn.wrote(), "a staged point is a write");
            Err::<((), Answer), _>(Abort("too late"))
        });
        assert_eq!(answer, Err(TransactError::Store(StoreError::Failed)));
        assert_eq!(
            store.append(&system("s"), Generation::new(1), &[]).err(),
            Some(StoreError::Failed)
        );
    }
    let store = open(dir.path(), &clock);
    assert_eq!(
        raw(&store, &series("mail", 1, "cpu")),
        vec![],
        "no series, no point"
    );
    let report = store
        .append(
            &system("other"),
            Generation::new(1),
            &[(metric("cpu"), ValueKind::Percent, 1)],
        )
        .expect("append");
    assert_eq!(report.accepted, 1, "the store opens and takes points");
}

#[test]
fn staged_points_commit_with_their_transaction_and_answer_after_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::at(T0 + 9);
    let store = open(dir.path(), &clock);
    let report = store
        .transact(Commit::Batched, |txn| {
            txn.insert(SYSTEMS, b"receipt", b"r1");
            let report = txn.append(
                &system("mail"),
                Generation::new(1),
                &[(metric("cpu"), ValueKind::Percent, 42)],
            );
            Ok::<_, Abort<()>>((report, Answer::AfterCommit))
        })
        .expect("commits");
    assert_eq!((report.at, report.accepted), (T0 + 9, 1));
    assert_eq!(
        raw(&store, &series("mail", 1, "cpu")),
        vec![RawPoint {
            ts: T0 + 9,
            value: 42
        }]
    );
}
