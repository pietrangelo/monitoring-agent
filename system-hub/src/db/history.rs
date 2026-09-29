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
use crate::models::MetricPoint;

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

/// What `Database::store_round` did with a round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundStored {
    Stored,
    Duplicate,
    TooSoon,
    /// The system's row is gone: nothing was decided or written.
    SystemGone,
}

impl Database {
    pub fn insert_metric(
        &self,
        system_id: &str,
        metric: &str,
        value: f32,
        timestamp: u64,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO metrics (system_id, metric, value, timestamp) VALUES (?1,?2,?3,?4)",
            rusqlite::params![system_id, metric, value, timestamp],
        )?;

        let retention: u64 = conn
            .query_row(
                "SELECT retention_secs FROM metric_retention WHERE system_id=?1 AND metric=?2",
                rusqlite::params![system_id, metric],
                |row| row.get(0),
            )
            .unwrap_or(86400);

        let cutoff = timestamp.saturating_sub(retention);
        conn.execute(
            "DELETE FROM metrics WHERE system_id=?1 AND metric=?2 AND timestamp < ?3",
            rusqlite::params![system_id, metric, cutoff],
        )?;
        Ok(())
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
            let mut insert = tx.prepare_cached(
                "INSERT INTO metrics (system_id, metric, value, timestamp) VALUES (?1,?2,?3,?4)",
            )?;
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
mod tests {
    use super::*;
    use crate::db::tests::{sample_system, temp_db};

    #[test]
    fn insert_metric_and_get_metrics_round_trip() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "web-01")).unwrap();
        db.insert_metric("id-1", "cpu", 10.0, 100).unwrap();
        db.insert_metric("id-1", "cpu", 20.0, 200).unwrap();
        db.insert_metric("id-1", "cpu", 30.0, 300).unwrap();

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
            db.insert_metric("id-1", "cpu", i as f32, 100 + i).unwrap();
        }
        let points = db.get_metrics("id-1", "cpu", 3, None).unwrap();
        assert_eq!(points.len(), 3);
    }

    #[test]
    fn get_metrics_with_since_filters_and_stays_ascending() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "web-01")).unwrap();
        db.insert_metric("id-1", "cpu", 1.0, 100).unwrap();
        db.insert_metric("id-1", "cpu", 2.0, 200).unwrap();
        db.insert_metric("id-1", "cpu", 3.0, 300).unwrap();

        let points = db.get_metrics("id-1", "cpu", 100, Some(150)).unwrap();
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].timestamp, 200);
        assert_eq!(points[1].timestamp, 300);
    }

    #[test]
    fn get_metrics_different_metric_names_are_isolated() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "web-01")).unwrap();
        db.insert_metric("id-1", "cpu", 1.0, 100).unwrap();
        db.insert_metric("id-1", "memory", 2.0, 100).unwrap();

        let cpu_points = db.get_metrics("id-1", "cpu", 100, None).unwrap();
        assert_eq!(cpu_points.len(), 1);
        assert_eq!(cpu_points[0].value, 1.0);
    }

    #[test]
    fn insert_metric_applies_default_retention_cutoff() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "web-01")).unwrap();
        // Default retention is 86400s. First point at ts=0.
        db.insert_metric("id-1", "cpu", 1.0, 0).unwrap();
        // Second point far enough ahead that the cutoff (ts - 86400) prunes the first.
        db.insert_metric("id-1", "cpu", 2.0, 100_000).unwrap();

        let points = db.get_metrics("id-1", "cpu", 100, None).unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].timestamp, 100_000);
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
}
