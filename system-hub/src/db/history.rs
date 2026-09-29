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

//! The fleet history: the `metrics` table, as a child of `db` so it shares the one `conn`
//! mutex. The `systems` and `alerts` tables stay in `db/mod.rs`.

use super::{Database, system_exists};
use crate::applications::{Admission, SourcePace};
use crate::models::{MetricPoint, SystemId};
use crate::registry::{LastSeen, StatusUpdate};
use crate::snapshot::{Snapshot, SnapshotTime, snapshot_retention};
use rusqlite::types::ValueRef;
use rusqlite::{OptionalExtension, Transaction};

/// The most `app:*` points one prune batch deletes, so no batch holds the mutex for long.
/// (`DELETE … LIMIT` needs a compile option the bundled SQLite lacks, hence `rowid IN`.)
const PRUNE_BATCH: usize = 5_000;

/// Deletes up to ?3 of system ?1's `app:*` points older than ?2 whose metric has no usable
/// retention row of its own (a negative `retention_secs`, hand-set, counts as none).
const PRUNE_DEFAULT_RETENTION: &str = "DELETE FROM metrics WHERE rowid IN (
    SELECT rowid FROM metrics
    WHERE system_id = ?1 AND metric >= 'app:' AND metric < 'app;' AND timestamp < ?2
      AND metric NOT IN (
        SELECT metric FROM metric_retention WHERE system_id = ?1 AND retention_secs >= 0)
    LIMIT ?3)";

/// Deletes up to ?3 of system ?1's points of metric ?4 older than ?2.
const PRUNE_ONE_METRIC: &str = "DELETE FROM metrics WHERE rowid IN (
    SELECT rowid FROM metrics
    WHERE system_id = ?1 AND metric = ?4 AND timestamp < ?2
    LIMIT ?3)";

/// The most expired points of its series one snapshot point prunes, oldest first, so a
/// backlog (after a gap, or a far-future timestamp) drains over several snapshots instead of
/// holding the mutex for one (RFC 0007 §2).
const PRUNE_PER_POINT: usize = 16;

const INSERT_POINT: &str =
    "INSERT INTO metrics (system_id, metric, value, timestamp) VALUES (?1,?2,?3,?4)";

const SELECT_RETENTION: &str =
    "SELECT retention_secs FROM metric_retention WHERE system_id = ?1 AND metric = ?2";

/// Deletes up to ?4 of system ?1's oldest points of metric ?2 older than ?3.
const PRUNE_OLDEST: &str = "DELETE FROM metrics WHERE rowid IN (
    SELECT rowid FROM metrics
    WHERE system_id = ?1 AND metric = ?2 AND timestamp < ?3
    ORDER BY timestamp LIMIT ?4)";

/// Writes system ?4's status; a NULL ?2 keeps its `last_seen`.
const WRITE_STATUS: &str = "UPDATE systems
    SET status = ?1, last_seen = COALESCE(?2, last_seen), last_error = ?3 WHERE id = ?4";

/// What `Database::store_round` did with a round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundStored {
    Stored,
    Duplicate,
    TooSoon,
    /// The system's row is gone: nothing was decided or written.
    SystemGone,
}

/// What `Database::store_snapshot` did with a snapshot: `R` is what `on_stored` returned.
#[derive(Debug, Clone, PartialEq)]
pub enum SnapshotStored<R> {
    Stored(R),
    /// The system's row is gone: nothing was written.
    SystemGone,
}

impl Database {
    /// Stores a snapshot and writes the system's status in one transaction, holding the
    /// connection mutex throughout (RFC 0007 §2). In order: the system's row must exist, else
    /// `SystemGone` with nothing written and `on_stored` not called; then each metric point at
    /// `time`, each followed by a capped prune of its series; then `status`; then the commit,
    /// and `on_stored` with the snapshot, still under the mutex. `on_stored` must not call back
    /// into `Database` (the mutex isn't reentrant) and must not panic.
    pub fn store_snapshot<R>(
        &self,
        system_id: &SystemId,
        snapshot: Snapshot,
        time: SnapshotTime,
        status: StatusUpdate,
        on_stored: impl FnOnce(Snapshot) -> R,
    ) -> Result<SnapshotStored<R>, rusqlite::Error> {
        let mut conn = self.conn.lock().unwrap();
        if !system_exists(&conn, system_id.as_str())? {
            return Ok(SnapshotStored::SystemGone);
        }
        let tx = conn.transaction()?;
        store_points(&tx, system_id, &snapshot, time)?;
        write_status(&tx, system_id, &status)?;
        tx.commit()?;
        Ok(SnapshotStored::Stored(on_stored(snapshot)))
    }

    /// Stores one scrape round's points, if `decide` admits it, holding the connection mutex
    /// throughout, so the check and the store are one step for every ingestion path (RFC 0009
    /// §8). In order: the system's row must exist, else `SystemGone` with neither closure
    /// called; then `decide`; on `Accept`, every point in one transaction at `received_at`,
    /// then `on_stored` with the spent pace. The closures run under the mutex, so they must
    /// not call back into `Database` (the mutex isn't reentrant) and must not panic.
    pub fn store_round(
        &self,
        system_id: &str,
        points: impl IntoIterator<Item = (String, f32)>,
        received_at: u64,
        decide: impl FnOnce() -> Admission,
        on_stored: impl FnOnce(SourcePace),
    ) -> Result<RoundStored, rusqlite::Error> {
        let mut conn = self.conn.lock().unwrap();
        if !system_exists(&conn, system_id)? {
            return Ok(RoundStored::SystemGone);
        }
        let pace = match decide() {
            Admission::Accept { pace } => pace,
            Admission::Duplicate => return Ok(RoundStored::Duplicate),
            Admission::TooSoon => return Ok(RoundStored::TooSoon),
        };
        let tx = conn.transaction()?;
        {
            let mut insert = tx.prepare_cached(INSERT_POINT)?;
            for (metric, value) in points {
                insert.execute(rusqlite::params![system_id, metric, value, received_at])?;
            }
        }
        tx.commit()?;
        on_stored(pace);
        Ok(RoundStored::Stored)
    }

    /// Deletes a system's `app:*` points older than their retention: the system's
    /// `metric_retention` row for that metric if it has one, else `default_retention_secs`.
    /// Cutoffs come from `now`, the hub's clock. Deletes in batches, each under its own
    /// hold of the mutex. Returns how many points were deleted.
    pub fn prune_application_points(
        &self,
        system_id: &str,
        now: u64,
        default_retention_secs: u64,
    ) -> Result<usize, rusqlite::Error> {
        let default_cutoff = now.saturating_sub(default_retention_secs);
        let mut deleted =
            self.prune_in_batches(PRUNE_DEFAULT_RETENTION, system_id, None, default_cutoff)?;
        for (metric, retention_secs) in self.application_retention_rows(system_id)? {
            let cutoff = now.saturating_sub(retention_secs);
            deleted += self.prune_in_batches(PRUNE_ONE_METRIC, system_id, Some(&metric), cutoff)?;
        }
        Ok(deleted)
    }

    /// The system's `metric_retention` rows for `app:*` metrics.
    fn application_retention_rows(
        &self,
        system_id: &str,
    ) -> Result<Vec<(String, u64)>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT metric, retention_secs FROM metric_retention
             WHERE system_id = ?1 AND metric >= 'app:' AND metric < 'app;' AND retention_secs >= 0",
        )?;
        let rows = stmt.query_map([system_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect()
    }

    /// Runs one of the prune statements until a batch deletes fewer than `PRUNE_BATCH` rows,
    /// taking the mutex once per batch so other work can interleave.
    fn prune_in_batches(
        &self,
        sql: &str,
        system_id: &str,
        metric: Option<&str>,
        cutoff: u64,
    ) -> Result<usize, rusqlite::Error> {
        let mut deleted = 0;
        loop {
            let batch = {
                let conn = self.conn.lock().unwrap();
                let mut stmt = conn.prepare_cached(sql)?;
                match metric {
                    None => stmt.execute(rusqlite::params![system_id, cutoff, PRUNE_BATCH])?,
                    Some(metric) => {
                        stmt.execute(rusqlite::params![system_id, cutoff, PRUNE_BATCH, metric])?
                    }
                }
            };
            deleted += batch;
            if batch < PRUNE_BATCH {
                return Ok(deleted);
            }
        }
    }

    pub fn get_metrics(
        &self,
        system_id: &str,
        metric: &str,
        limit: usize,
        since_secs: Option<u64>,
    ) -> Result<Vec<MetricPoint>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let to_point = |row: &rusqlite::Row| {
            Ok(MetricPoint {
                timestamp: row.get(0)?,
                value: row.get(1)?,
            })
        };
        // Oldest first from `since`; otherwise the latest `limit`, reversed below.
        let rows: Vec<Result<MetricPoint, rusqlite::Error>> = match since_secs {
            Some(since) => conn
                .prepare(
                    "SELECT timestamp, value FROM metrics
                     WHERE system_id = ?1 AND metric = ?2 AND timestamp >= ?3
                     ORDER BY timestamp ASC LIMIT ?4",
                )?
                .query_map(
                    rusqlite::params![system_id, metric, since, limit as i64],
                    to_point,
                )?
                .collect(),
            None => conn
                .prepare(
                    "SELECT timestamp, value FROM metrics
                     WHERE system_id = ?1 AND metric = ?2
                     ORDER BY timestamp DESC LIMIT ?3",
                )?
                .query_map(rusqlite::params![system_id, metric, limit as i64], to_point)?
                .collect(),
        };

        let mut points = rows.into_iter().collect::<Result<Vec<_>, _>>()?;
        if since_secs.is_none() {
            points.reverse();
        }
        Ok(points)
    }
}

#[cfg(test)]
impl Database {
    /// Inserts one metric point as given, with no prune, for tests that need history in place.
    pub fn plant_point(
        &self,
        system_id: &str,
        metric: &str,
        value: f32,
        timestamp: u64,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            INSERT_POINT,
            rusqlite::params![system_id, metric, value, timestamp],
        )?;
        Ok(())
    }
}

/// Inserts each of the snapshot's metric points at `time`, each followed by the capped prune
/// of its series past the series' retention.
fn store_points(
    tx: &Transaction,
    system_id: &SystemId,
    snapshot: &Snapshot,
    time: SnapshotTime,
) -> Result<(), rusqlite::Error> {
    let id = system_id.as_str();
    let mut insert = tx.prepare_cached(INSERT_POINT)?;
    let mut retention = tx.prepare_cached(SELECT_RETENTION)?;
    let mut prune = tx.prepare_cached(PRUNE_OLDEST)?;
    for (metric, value) in snapshot.metric_points() {
        insert.execute(rusqlite::params![id, metric, value, time.seconds()])?;
        let stored = retention
            .query_row(rusqlite::params![id, metric], |row| {
                Ok(integer(row.get_ref(0)?))
            })
            .optional()?
            .flatten();
        let cutoff = time.cutoff(snapshot_retention(stored));
        prune.execute(rusqlite::params![
            id,
            metric,
            cutoff.seconds(),
            PRUNE_PER_POINT
        ])?;
    }
    Ok(())
}

/// A column's value when it holds an integer; any other type is none, so no stored value can
/// fail the read.
fn integer(value: ValueRef) -> Option<i64> {
    match value {
        ValueRef::Integer(integer) => Some(integer),
        ValueRef::Null | ValueRef::Real(_) | ValueRef::Text(_) | ValueRef::Blob(_) => None,
    }
}

/// Writes the status the registry decided, as it was decided.
fn write_status(
    tx: &Transaction,
    system_id: &SystemId,
    status: &StatusUpdate,
) -> Result<(), rusqlite::Error> {
    let last_seen = match status.last_seen() {
        LastSeen::Uptime(uptime) => Some(uptime.as_str()),
        LastSeen::PolledAt(polled_at) => Some(polled_at.as_str()),
        LastSeen::Unchanged => None,
    };
    tx.prepare_cached(WRITE_STATUS)?.execute(rusqlite::params![
        status.status().to_string(),
        last_seen,
        status.error(),
        system_id.as_str()
    ])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::{sample_system, temp_db};

    #[test]
    fn get_metrics_returns_planted_points_oldest_first() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "web-01")).unwrap();
        db.plant_point("id-1", "cpu", 10.0, 100).unwrap();
        db.plant_point("id-1", "cpu", 20.0, 200).unwrap();
        db.plant_point("id-1", "cpu", 30.0, 300).unwrap();

        let points = db.get_metrics("id-1", "cpu", 100, None).unwrap();
        assert_eq!(points.len(), 3);
        // Without `since`, results come back oldest-first.
        assert_eq!(points[0].timestamp, 100);
        assert_eq!(points[2].timestamp, 300);
    }

    #[test]
    fn get_metrics_respects_limit() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "web-01")).unwrap();
        for i in 0..10 {
            db.plant_point("id-1", "cpu", i as f32, 100 + i).unwrap();
        }
        let points = db.get_metrics("id-1", "cpu", 3, None).unwrap();
        assert_eq!(points.len(), 3);
    }

    #[test]
    fn get_metrics_with_since_filters_and_stays_ascending() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "web-01")).unwrap();
        db.plant_point("id-1", "cpu", 1.0, 100).unwrap();
        db.plant_point("id-1", "cpu", 2.0, 200).unwrap();
        db.plant_point("id-1", "cpu", 3.0, 300).unwrap();

        let points = db.get_metrics("id-1", "cpu", 100, Some(150)).unwrap();
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].timestamp, 200);
        assert_eq!(points[1].timestamp, 300);
    }

    #[test]
    fn get_metrics_different_metric_names_are_isolated() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "web-01")).unwrap();
        db.plant_point("id-1", "cpu", 1.0, 100).unwrap();
        db.plant_point("id-1", "memory", 2.0, 100).unwrap();

        let cpu_points = db.get_metrics("id-1", "cpu", 100, None).unwrap();
        assert_eq!(cpu_points.len(), 1);
        assert_eq!(cpu_points[0].value, 1.0);
    }

    mod rounds {
        use super::*;
        use crate::applications::{RecentRounds, RoundDigester, RoundId, SourcePace, admit};
        use std::cell::Cell;
        use std::time::Instant;

        fn db_with_system() -> (Database, tempfile::TempDir) {
            let (db, dir) = temp_db();
            db.insert_system(&sample_system("id-1", "web-01")).unwrap();
            (db, dir)
        }

        fn points() -> Vec<(String, f32)> {
            vec![
                ("app:orders:heap_used_bytes".into(), 300.0),
                ("app:orders:up".into(), 1.0),
            ]
        }

        fn stored(db: &Database, metric: &str) -> Vec<(u64, f32)> {
            db.get_metrics("id-1", metric, 1000, None)
                .unwrap()
                .into_iter()
                .map(|p| (p.timestamp, p.value))
                .collect()
        }

        fn accept(now: Instant) -> Admission {
            Admission::Accept {
                pace: SourcePace::new(now).take(now).unwrap(),
            }
        }

        #[test]
        fn an_admitted_round_is_stored_at_the_hubs_time_and_reported() {
            let (db, _dir) = db_with_system();
            let now = Instant::now();
            let reported = Cell::new(None);
            let outcome = db.store_round(
                "id-1",
                points(),
                1_790_000_000,
                || accept(now),
                |pace| reported.set(Some(pace)),
            );
            assert_eq!(outcome, Ok(RoundStored::Stored));
            assert_eq!(
                stored(&db, "app:orders:heap_used_bytes"),
                [(1_790_000_000, 300.0)]
            );
            assert_eq!(stored(&db, "app:orders:up"), [(1_790_000_000, 1.0)]);
            assert_eq!(
                reported.get(),
                Some(SourcePace::new(now).take(now).unwrap()),
                "on_stored gets the spent pace"
            );
        }

        #[test]
        fn a_refused_round_stores_nothing_and_isnt_reported() {
            let cases = [
                ("duplicate", Admission::Duplicate, RoundStored::Duplicate),
                ("too soon", Admission::TooSoon, RoundStored::TooSoon),
            ];
            for (case, admission, expected) in cases {
                let (db, _dir) = db_with_system();
                let reported = Cell::new(false);
                let outcome =
                    db.store_round("id-1", points(), 1, || admission, |_| reported.set(true));
                assert_eq!(outcome, Ok(expected), "case: {case}");
                assert!(stored(&db, "app:orders:up").is_empty(), "case: {case}");
                assert!(!reported.get(), "case: {case}");
            }
        }

        #[test]
        fn a_gone_system_decides_nothing_and_writes_nothing() {
            let (db, _dir) = temp_db();
            let decided = Cell::new(false);
            let reported = Cell::new(false);
            let outcome = db.store_round(
                "id-1",
                points(),
                1,
                || {
                    decided.set(true);
                    accept(Instant::now())
                },
                |_| reported.set(true),
            );
            assert_eq!(outcome, Ok(RoundStored::SystemGone));
            assert!(!decided.get(), "decide isn't called");
            assert!(!reported.get(), "on_stored isn't called");
            assert!(stored(&db, "app:orders:up").is_empty());
        }

        #[test]
        fn a_failing_insert_rolls_the_whole_round_back() {
            // Points on either side of the failing one, in name order and in sending order,
            // so no order of inserting them avoids leaving one behind without a transaction.
            let cases = [
                (
                    "sent in name order",
                    ["app:a:up", "app:orders:boom", "app:z:up"],
                ),
                (
                    "sent in reverse order",
                    ["app:z:up", "app:orders:boom", "app:a:up"],
                ),
            ];
            for (case, names) in cases {
                let (db, _dir) = db_with_system();
                // A failure nothing can see coming: SQLite aborts this one insert.
                db.conn
                    .lock()
                    .unwrap()
                    .execute_batch(
                        "CREATE TRIGGER boom BEFORE INSERT ON metrics
                         WHEN NEW.metric = 'app:orders:boom'
                         BEGIN SELECT RAISE(ABORT, 'boom'); END;",
                    )
                    .unwrap();
                let reported = Cell::new(false);
                let points = names.map(|name| (name.to_string(), 1.0)).to_vec();
                let outcome = db.store_round(
                    "id-1",
                    points,
                    1,
                    || accept(Instant::now()),
                    |_| reported.set(true),
                );
                assert!(
                    outcome.is_err(),
                    "case: {case}: the insert fails: {outcome:?}"
                );
                for name in ["app:a:up", "app:z:up"] {
                    assert!(
                        stored(&db, name).is_empty(),
                        "case: {case}: {name} is rolled back"
                    );
                }
                assert!(!reported.get(), "case: {case}: on_stored isn't called");
            }
        }

        #[test]
        fn both_closures_run_under_the_database_mutex() {
            let (db, _dir) = db_with_system();
            let held_in_decide = Cell::new(false);
            let held_in_on_stored = Cell::new(false);
            let outcome = db.store_round(
                "id-1",
                points(),
                1,
                || {
                    held_in_decide.set(db.conn.try_lock().is_err());
                    accept(Instant::now())
                },
                |_| held_in_on_stored.set(db.conn.try_lock().is_err()),
            );
            assert_eq!(outcome, Ok(RoundStored::Stored));
            assert!(held_in_decide.get(), "decide runs under the mutex");
            assert!(held_in_on_stored.get(), "on_stored runs under the mutex");
        }

        #[test]
        fn two_concurrent_stores_of_one_round_store_it_once() {
            let (db, _dir) = db_with_system();
            let db = std::sync::Arc::new(db);
            let recent = std::sync::Arc::new(std::sync::Mutex::new(RecentRounds::default()));
            let digester = RoundDigester::new();
            let round = crate::applications::ScrapeRound::new(
                RoundId::parse("6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b", 1).unwrap(),
                crate::applications::ScrapeInterval::from_secs(15).unwrap(),
                vec![],
            )
            .unwrap();
            let (id, digest) = (round.id(), digester.digest(&round));
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
            let stores: Vec<_> = (0..8)
                .map(|_| {
                    let (db, recent, barrier) = (db.clone(), recent.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        let now = Instant::now();
                        barrier.wait();
                        db.store_round(
                            "id-1",
                            points(),
                            1,
                            || {
                                admit(
                                    &recent.lock().unwrap(),
                                    SourcePace::new(now),
                                    id,
                                    digest,
                                    now,
                                )
                            },
                            |_| recent.lock().unwrap().remember(id, digest),
                        )
                        .unwrap()
                    })
                })
                .collect();
            let outcomes: Vec<RoundStored> =
                stores.into_iter().map(|t| t.join().unwrap()).collect();
            let stored_count = outcomes
                .iter()
                .filter(|o| **o == RoundStored::Stored)
                .count();
            assert_eq!(stored_count, 1, "outcomes: {outcomes:?}");
            assert_eq!(stored(&db, "app:orders:up").len(), 1);
        }

        const DAY: u64 = 86_400;

        fn metric_count(db: &Database, system: &str, metric: &str) -> usize {
            db.get_metrics(system, metric, 1_000_000, None)
                .unwrap()
                .len()
        }

        #[test]
        fn application_points_past_their_retention_are_pruned_and_nothing_else() {
            let (db, _dir) = db_with_system();
            db.insert_system(&sample_system("id-2", "web-02")).unwrap();
            let now = 10 * DAY;
            let old = now - DAY - 1;
            let recent = now - DAY + 1;
            let insert = |system: &str, metric: &str, ts: u64| {
                let conn = db.conn.lock().unwrap();
                conn.execute(
                    "INSERT INTO metrics (system_id, metric, value, timestamp) VALUES (?1,?2,1.0,?3)",
                    rusqlite::params![system, metric, ts],
                )
                .unwrap();
            };
            insert("id-1", "app:orders:up", old);
            insert("id-1", "app:orders:up", recent);
            insert("id-1", "app:gone:up", old); // a series that stopped
            insert("id-1", "app:kept:up", now - 2 * DAY); // under its own 7-day retention row
            insert("id-1", "app:kept:up", now - 8 * DAY);
            insert("id-1", "app:orders:edge", now - DAY); // exactly at the cutoff: kept
            insert("id-1", "app:short:up", now - 2 * 3600); // under its own 1-hour row
            insert("id-1", "cpu", now - 2 * 3600); // a snapshot series with a 1-hour row
            insert("id-1", "cpu", old); // a snapshot series: not this task's
            insert("id-1", "apps", old); // not an `app:` name
            insert("id-2", "app:orders:up", old); // another system
            insert("id-2", "app:kept:up", now - 8 * DAY); // another system's, under id-1's row name
            insert("id-1", "app:neg:up", old); // a negative retention row counts as none
            let rows: [(&str, &str, i64); 5] = [
                ("id-1", "app:kept:up", 7 * DAY as i64),
                ("id-1", "app:short:up", 3600),
                ("id-1", "cpu", 3600),
                ("id-1", "app:neg:up", -5),
                // Another system's row must not shield id-1's `app:gone:up`.
                ("id-2", "app:gone:up", 7 * DAY as i64),
            ];
            for (system, metric, retention) in rows {
                db.conn
                    .lock()
                    .unwrap()
                    .execute(
                        "INSERT INTO metric_retention (system_id, metric, retention_secs) VALUES (?1,?2,?3)",
                        rusqlite::params![system, metric, retention],
                    )
                    .unwrap();
            }

            let pruned = db.prune_application_points("id-1", now, DAY);

            assert_eq!(pruned, Ok(5));
            let cases = [
                ("id-1", "app:orders:up", 1),
                ("id-1", "app:orders:edge", 1),
                ("id-1", "app:short:up", 0),
                ("id-1", "app:gone:up", 0),
                ("id-1", "app:kept:up", 1),
                ("id-1", "cpu", 2),
                ("id-1", "apps", 1),
                ("id-2", "app:orders:up", 1),
                ("id-2", "app:kept:up", 1),
                ("id-1", "app:neg:up", 0),
            ];
            for (system, metric, left) in cases {
                assert_eq!(
                    metric_count(&db, system, metric),
                    left,
                    "case: {system} {metric}"
                );
            }
        }

        #[test]
        fn pruning_goes_on_past_one_batch() {
            let (db, _dir) = db_with_system();
            {
                let mut conn = db.conn.lock().unwrap();
                let tx = conn.transaction().unwrap();
                for ts in 0..12_000u64 {
                    tx.execute(
                        "INSERT INTO metrics (system_id, metric, value, timestamp) VALUES ('id-1','app:orders:up',1.0,?1)",
                        [ts],
                    )
                    .unwrap();
                }
                tx.commit().unwrap();
            }
            assert_eq!(
                db.prune_application_points("id-1", 100 * DAY, DAY),
                Ok(12_000)
            );
            assert_eq!(metric_count(&db, "id-1", "app:orders:up"), 0);
        }
    }

    mod snapshots {
        use super::*;
        use crate::models::{SystemInfo, SystemStatus};
        use crate::registry::{LastSeen, UptimeDisplay};
        use crate::snapshot::{ReportedDisk, ReportedSnapshot, snapshot_rule};
        use std::cell::Cell;

        const TIME: u64 = 1_790_000_000;
        const DAY: u64 = 86_400;

        fn id() -> SystemId {
            SystemId::try_from("id-1".to_string()).unwrap()
        }

        fn at(secs: u64) -> SnapshotTime {
            SnapshotTime::try_from(secs).unwrap()
        }

        /// A system last marked offline by a failed poll, so a stored snapshot visibly
        /// changes every status column.
        fn db_with_offline_system() -> (Database, tempfile::TempDir) {
            let (db, dir) = temp_db();
            db.insert_system(&SystemInfo {
                status: SystemStatus::Offline,
                last_seen: "before".into(),
                last_error: Some("timeout".into()),
                ..sample_system("id-1", "web-01")
            })
            .unwrap();
            (db, dir)
        }

        /// Every scalar, and one disk per mount point given.
        fn snapshot(mounts: &[&str]) -> Snapshot {
            let disks = mounts.iter().enumerate().map(|(i, mount)| ReportedDisk {
                mount_point: Some(mount.to_string()),
                usage_percent: Some(40.0 + i as f32),
            });
            let reported = ReportedSnapshot {
                cpu: Some(10.0),
                memory: Some(20.0),
                swap: Some(30.0),
                load1: Some(1.5),
                load5: Some(2.5),
                disks: disks.collect(),
            };
            snapshot_rule(reported).0
        }

        fn online(uptime: &str) -> StatusUpdate {
            let uptime = UptimeDisplay::try_from(uptime.to_string()).unwrap();
            StatusUpdate::after_snapshot(LastSeen::Uptime(uptime))
        }

        fn stored(db: &Database, metric: &str) -> Vec<(u64, f32)> {
            db.get_metrics("id-1", metric, 100_000, None)
                .unwrap()
                .into_iter()
                .map(|p| (p.timestamp, p.value))
                .collect()
        }

        fn timestamps(db: &Database, metric: &str) -> Vec<u64> {
            stored(db, metric).into_iter().map(|(ts, _)| ts).collect()
        }

        fn plant(db: &Database, metric: &str, timestamps: impl IntoIterator<Item = u64>) {
            let mut conn = db.conn.lock().unwrap();
            let tx = conn.transaction().unwrap();
            for ts in timestamps {
                tx.execute(
                    "INSERT INTO metrics (system_id, metric, value, timestamp) VALUES ('id-1',?1,0.0,?2)",
                    rusqlite::params![metric, ts],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }

        /// (status, last_seen, last_error) as the row holds them.
        fn status_columns(db: &Database) -> (SystemStatus, String, Option<String>) {
            let row = db.get_system("id-1").unwrap().unwrap();
            (row.status, row.last_seen, row.last_error)
        }

        #[test]
        fn a_snapshot_is_stored_at_its_time_with_the_status_given() {
            let uptime = LastSeen::Uptime(UptimeDisplay::try_from("3d 4h 5m".to_string()).unwrap());
            let polled = LastSeen::PolledAt("2026-09-29T10:00:00Z".into());
            // (name, last_seen given, the row's last_seen afterwards)
            let cases = [
                ("push, an uptime", uptime, "3d 4h 5m"),
                ("poll, a poll time", polled, "2026-09-29T10:00:00Z"),
                (
                    "push, a display the rule refused",
                    LastSeen::Unchanged,
                    "before",
                ),
            ];
            for (case, last_seen, shown) in cases {
                let (db, _dir) = db_with_offline_system();
                let kept = snapshot(&["/", "/home"]);
                let status = StatusUpdate::after_snapshot(last_seen);
                let outcome = db.store_snapshot(&id(), kept.clone(), at(TIME), status, |s| s);
                assert_eq!(outcome, Ok(SnapshotStored::Stored(kept)), "case: {case}");
                let expected = [
                    ("cpu", 10.0),
                    ("memory", 20.0),
                    ("swap", 30.0),
                    ("load1", 1.5),
                    ("load5", 2.5),
                    ("disk:/", 40.0),
                    ("disk:/home", 41.0),
                ];
                for (metric, value) in expected {
                    assert_eq!(
                        stored(&db, metric),
                        [(TIME, value)],
                        "case: {case}: {metric}"
                    );
                }
                assert_eq!(
                    status_columns(&db),
                    (SystemStatus::Online, shown.to_string(), None),
                    "case: {case}"
                );
            }
        }

        #[test]
        fn retention_rows_never_fail_a_store_and_fall_back_to_a_day() {
            // (name, the row's retention_secs as SQL, None for no row, the retention it means)
            let cases = [
                ("an hour", Some("3600"), 3_600),
                ("none: everything older than the snapshot", Some("0"), 0),
                ("no row", None, DAY),
                ("a negative row", Some("-1"), DAY),
                ("the most negative row", Some("-9223372036854775808"), DAY),
                ("a row holding text", Some("'abc'"), DAY),
                ("a row holding a real", Some("3600.5"), DAY),
            ];
            for (case, row, retention) in cases {
                let (db, _dir) = db_with_offline_system();
                if let Some(sql) = row {
                    db.conn
                        .lock()
                        .unwrap()
                        .execute_batch(&format!(
                            "INSERT INTO metric_retention (system_id, metric, retention_secs)
                             VALUES ('id-1', 'cpu', {sql})"
                        ))
                        .unwrap();
                }
                let (past, within) = (TIME - retention - 1, TIME - retention);
                plant(&db, "cpu", [past, within]);
                let outcome =
                    db.store_snapshot(&id(), snapshot(&[]), at(TIME), online("1m"), |_| ());
                assert_eq!(outcome, Ok(SnapshotStored::Stored(())), "case: {case}");
                assert_eq!(timestamps(&db, "cpu"), [within, TIME], "case: {case}");
                assert_eq!(
                    timestamps(&db, "memory"),
                    [TIME],
                    "case: {case}: others unpruned"
                );
            }
        }

        #[test]
        fn a_gone_system_gets_nothing_written_and_on_stored_isnt_called() {
            let (db, _dir) = temp_db();
            let called = Cell::new(false);
            let outcome =
                db.store_snapshot(&id(), snapshot(&["/"]), at(TIME), online("1m"), |_| {
                    called.set(true)
                });
            assert_eq!(outcome, Ok(SnapshotStored::SystemGone));
            assert!(!called.get(), "on_stored isn't called");
            assert!(stored(&db, "cpu").is_empty());
            assert!(stored(&db, "disk:/").is_empty());
        }

        #[test]
        fn on_stored_runs_after_the_commit_and_under_the_mutex() {
            let (db, dir) = db_with_offline_system();
            let path = dir.path().join("test.db");
            let seen = Cell::new(None);
            let outcome =
                db.store_snapshot(&id(), snapshot(&["/"]), at(TIME), online("1m"), |_| {
                    let held = db.conn.try_lock().is_err();
                    let other = rusqlite::Connection::open(&path).unwrap();
                    let committed: i64 = other
                        .query_row(
                            "SELECT COUNT(*) FROM metrics WHERE system_id = 'id-1'",
                            [],
                            |row| row.get(0),
                        )
                        .unwrap();
                    let status: String = other
                        .query_row("SELECT status FROM systems WHERE id = 'id-1'", [], |row| {
                            row.get(0)
                        })
                        .unwrap();
                    seen.set(Some((held, committed, status)));
                });
            assert_eq!(outcome, Ok(SnapshotStored::Stored(())));
            let (held, committed, status) = seen.take().expect("on_stored is called");
            assert!(held, "on_stored runs under the mutex");
            assert_eq!(committed, 6, "another connection already sees every point");
            assert_eq!(status, "online", "and the status");
        }

        #[test]
        fn each_point_prunes_at_most_16_of_its_series_oldest_expired_points() {
            let recent = TIME - 10;
            // (name, expired points planted, expired left after one snapshot, then after two)
            let cases: [(&str, u64, std::ops::Range<u64>, std::ops::Range<u64>); 4] = [
                ("a backlog of 100", 100, 16..100, 32..100),
                ("a backlog of 17", 17, 16..17, 17..17),
                ("exactly 16", 16, 16..16, 16..16),
                ("fewer than the cap", 10, 10..10, 10..10),
            ];
            for (case, planted, after_one, after_two) in cases {
                let (db, _dir) = db_with_offline_system();
                // Two series with the same backlog, so the cap is per point, not per snapshot;
                // planted newest first, so rowid order isn't the oldest-first order.
                for metric in ["cpu", "memory"] {
                    plant(&db, metric, [recent].into_iter().chain((0..planted).rev()));
                }
                db.store_snapshot(&id(), snapshot(&[]), at(TIME), online("1m"), |_| ())
                    .unwrap();
                let expected: Vec<u64> = after_one.clone().chain([recent, TIME]).collect();
                for metric in ["cpu", "memory"] {
                    let left = timestamps(&db, metric);
                    assert_eq!(left, expected, "case: {case}: {metric}: one snapshot");
                }
                db.store_snapshot(&id(), snapshot(&[]), at(TIME + 2), online("1m"), |_| ())
                    .unwrap();
                let expected: Vec<u64> = after_two.chain([recent, TIME, TIME + 2]).collect();
                for metric in ["cpu", "memory"] {
                    let left = timestamps(&db, metric);
                    assert_eq!(left, expected, "case: {case}: {metric}: two snapshots");
                }
            }
        }

        #[test]
        fn a_failure_part_way_leaves_nothing_behind() {
            // The failing metric sits between points inserted before it and after it, and an
            // expired point the earlier `cpu` prune deletes must come back too.
            let cases = ["memory", "load5", "disk:/home"];
            for failing in cases {
                let (db, _dir) = db_with_offline_system();
                plant(&db, "cpu", [0]);
                db.conn
                    .lock()
                    .unwrap()
                    .execute_batch(&format!(
                        "CREATE TRIGGER boom BEFORE INSERT ON metrics
                         WHEN NEW.metric = '{failing}' AND NEW.timestamp = {TIME}
                         BEGIN SELECT RAISE(ABORT, 'boom'); END;"
                    ))
                    .unwrap();
                let called = Cell::new(false);
                let outcome = db.store_snapshot(
                    &id(),
                    snapshot(&["/", "/home", "/var"]),
                    at(TIME),
                    online("1m"),
                    |_| called.set(true),
                );
                assert!(
                    outcome.is_err(),
                    "case: {failing}: the insert fails: {outcome:?}"
                );
                assert!(!called.get(), "case: {failing}: on_stored isn't called");
                assert_eq!(
                    timestamps(&db, "cpu"),
                    [0],
                    "case: {failing}: cpu rolled back"
                );
                for metric in [
                    "memory",
                    "swap",
                    "load1",
                    "load5",
                    "disk:/",
                    "disk:/home",
                    "disk:/var",
                ] {
                    assert!(stored(&db, metric).is_empty(), "case: {failing}: {metric}");
                }
                assert_eq!(
                    status_columns(&db),
                    (
                        SystemStatus::Offline,
                        "before".to_string(),
                        Some("timeout".to_string())
                    ),
                    "case: {failing}: the status is unchanged"
                );
            }
        }
    }
}
