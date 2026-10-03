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

//! Storing one mail report (RFC 0017 §6): registration, the receipt, every sample's points,
//! the newest report's status, the alert records and the receipts' prune, in one transaction
//! under the connection mutex, so a failure leaves nothing and a duplicate stores nothing.

use rusqlite::{OptionalExtension, Transaction};

use super::history::{store_unexpired_points, write_status};
use super::{Database, INSERT_SYSTEM, insert_alert_record, system_params};
use crate::mail_intake::receipt::Recency;
use crate::mail_intake::report::MailInterval;
use crate::models::{AlertRecord, SystemId, SystemInfo};
use crate::registry::{MAIL_URL, StatusUpdate};
use crate::snapshot::{Snapshot, SnapshotTime};

/// One accepted report's receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailReceipt {
    pub run: String,
    pub seq: u64,
    pub created_at: SnapshotTime,
    pub interval_secs: u64,
    pub received_at: SnapshotTime,
}

/// What to write for one report, as the domain decided it.
pub struct MailWrite<'a> {
    pub system_id: &'a SystemId,
    /// The row registered when the id is new (its id is `system_id`); its `url` is the mail
    /// sentinel.
    pub system: &'a SystemInfo,
    pub receipt: MailReceipt,
    /// After the snapshot rule, oldest first.
    pub snapshots: Vec<(Snapshot, SnapshotTime)>,
    /// Written only when the report is its system's newest.
    pub status: StatusUpdate,
    pub alerts: Vec<AlertRecord>,
    /// The hub's clock: points older than their series' retention at `now` are left out.
    pub now: SnapshotTime,
    /// Receipts created before this are pruned, each system's newest current one aside.
    pub prune_before: SnapshotTime,
}

/// What `store_mail_report` did.
#[derive(Debug, PartialEq)]
pub enum MailStored<R> {
    /// Committed. `expired` points were left out as past their retention; `newest` is what
    /// `on_newest` returned for a `Newest` report.
    Stored {
        recency: Recency,
        expired: usize,
        newest: Option<R>,
    },
    /// Its `(run, seq)` was already accepted (or retired): nothing written.
    Duplicate,
    /// The id belongs to a pushed or polled system: nothing written.
    TransportMismatch,
}

/// One mail system as the overdue sweep reads it: its newest current receipt, if any.
#[derive(Debug, Clone, PartialEq)]
pub struct MailPresenceRow {
    pub id: String,
    pub status: crate::models::SystemStatus,
    pub enabled: bool,
    /// The newest current receipt's creation time and interval.
    pub newest: Option<(SnapshotTime, MailInterval)>,
}

impl Database {
    /// Every mail system's status, `enabled` and newest current receipt. Rows that don't map
    /// are skipped.
    pub fn mail_presence(&self) -> Result<Vec<MailPresenceRow>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.status, s.enabled, r.created_at, r.interval_secs
             FROM systems s LEFT JOIN mail_receipts r ON r.rowid = (
                 SELECT rowid FROM mail_receipts
                 WHERE system_id = s.id AND retired = 0
                 ORDER BY created_at DESC LIMIT 1)
             WHERE s.url = ?1",
        )?;
        let rows = stmt.query_map([MAIL_URL], |row| {
            Ok(presence_row(
                row.get(0),
                row.get(1),
                row.get(2),
                row.get(3),
                row.get(4),
            ))
        })?;
        Ok(rows.filter_map(|row| row.ok().flatten()).collect())
    }

    /// Marks a mail system offline as overdue, keeping its last seen, only if it isn't offline
    /// yet and its newest current receipt is still `newest`: a report stored since the sweep
    /// read the row wins. Returns whether the row changed.
    pub fn mark_mail_overdue(
        &self,
        system_id: &str,
        newest: Option<SnapshotTime>,
        last_error: &str,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE systems SET status = 'offline', last_error = ?3
             WHERE id = ?1 AND status != 'offline'
               AND (SELECT MAX(created_at) FROM mail_receipts
                    WHERE system_id = ?1 AND retired = 0) IS ?2",
            rusqlite::params![system_id, newest.map(SnapshotTime::seconds), last_error],
        )?;
        Ok(changed == 1)
    }

    /// Stores one report in one transaction. `decide` gets the system's newest current receipt
    /// time, read before this report's receipt is inserted; `on_newest` gets the newest
    /// sample of a `Newest` report after the commit, still under the mutex, and must not call
    /// back into `Database` or panic.
    pub fn store_mail_report<R>(
        &self,
        write: MailWrite,
        decide: impl FnOnce(Option<SnapshotTime>) -> Recency,
        on_newest: impl FnOnce(Snapshot, SnapshotTime) -> R,
    ) -> Result<MailStored<R>, rusqlite::Error> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let system_id = write.system_id;
        match known_url(&tx, write.system)? {
            Some(true) => {}
            Some(false) => return Ok(MailStored::TransportMismatch),
            None => insert_system(&tx, write.system)?,
        }
        let recency = decide(newest_current_receipt(&tx, system_id)?);
        if !insert_receipt(&tx, system_id, &write.receipt)? {
            return Ok(MailStored::Duplicate);
        }
        let (expired, newest) = store_samples(&tx, system_id, write.snapshots, write.now)?;
        if recency == Recency::Newest {
            write_status(&tx, system_id, &write.status)?;
        }
        for alert in &write.alerts {
            insert_alert_record(&tx, alert)?;
        }
        prune_receipts(&tx, system_id, write.prune_before)?;
        tx.commit()?;
        let newest = match recency {
            Recency::Newest => newest.map(|(snapshot, time)| on_newest(snapshot, time)),
            Recency::Backfill => None,
        };
        Ok(MailStored::Stored {
            recency,
            expired,
            newest,
        })
    }
}

/// One row, or `None` when its id, status or `enabled` doesn't map.
fn presence_row(
    id: rusqlite::Result<String>,
    status: rusqlite::Result<String>,
    enabled: rusqlite::Result<bool>,
    created_at: rusqlite::Result<Option<u64>>,
    interval_secs: rusqlite::Result<Option<u64>>,
) -> Option<MailPresenceRow> {
    let created_at = created_at
        .ok()
        .flatten()
        .and_then(|secs| SnapshotTime::try_from(secs).ok());
    let interval =
        (interval_secs.ok().flatten()).and_then(|secs| MailInterval::try_from(secs).ok());
    Some(MailPresenceRow {
        id: id.ok()?,
        status: super::parse_status(&status.ok()?),
        enabled: enabled.ok()?,
        newest: created_at.zip(interval),
    })
}

fn known_url(tx: &Transaction, system: &SystemInfo) -> Result<Option<bool>, rusqlite::Error> {
    tx.query_row(
        "SELECT url IS ?2 FROM systems WHERE id = ?1",
        [&system.id, &system.url],
        |row| row.get(0),
    )
    .optional()
}

fn insert_system(tx: &Transaction, system: &SystemInfo) -> Result<(), rusqlite::Error> {
    tx.execute(INSERT_SYSTEM, system_params(system))?;
    Ok(())
}

/// The system's newest current receipt's creation time.
fn newest_current_receipt(
    tx: &Transaction,
    system_id: &SystemId,
) -> Result<Option<SnapshotTime>, rusqlite::Error> {
    let newest: Option<i64> = tx.query_row(
        "SELECT MAX(created_at) FROM mail_receipts WHERE system_id = ?1 AND retired = 0",
        [system_id.as_str()],
        |row| row.get(0),
    )?;
    Ok(newest.and_then(|secs| SnapshotTime::try_from(u64::try_from(secs).ok()?).ok()))
}

/// Inserts the receipt; `false` when its `(run, seq)` was already there, retired or not.
fn insert_receipt(
    tx: &Transaction,
    system_id: &SystemId,
    receipt: &MailReceipt,
) -> Result<bool, rusqlite::Error> {
    let inserted = tx.execute(
        "INSERT INTO mail_receipts
            (system_id, run, seq, created_at, interval_secs, received_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT DO NOTHING",
        rusqlite::params![
            system_id.as_str(),
            receipt.run,
            receipt.seq,
            receipt.created_at.seconds(),
            receipt.interval_secs,
            receipt.received_at.seconds(),
        ],
    )?;
    Ok(inserted == 1)
}

/// Stores every sample's points; returns how many were past their retention, and the newest
/// sample.
fn store_samples(
    tx: &Transaction,
    system_id: &SystemId,
    snapshots: Vec<(Snapshot, SnapshotTime)>,
    now: SnapshotTime,
) -> Result<(usize, Option<(Snapshot, SnapshotTime)>), rusqlite::Error> {
    let mut expired = 0;
    let mut newest = None;
    for (snapshot, time) in snapshots {
        expired += store_unexpired_points(tx, system_id, &snapshot, time, Some(now))?;
        newest = Some((snapshot, time));
    }
    Ok((expired, newest))
}

/// Deletes the receipts created before `before`: retired ones all, current ones except the
/// system's newest.
fn prune_receipts(
    tx: &Transaction,
    system_id: &SystemId,
    before: SnapshotTime,
) -> Result<(), rusqlite::Error> {
    tx.execute(
        "DELETE FROM mail_receipts WHERE system_id = ?1 AND created_at < ?2
           AND (retired = 1 OR created_at < (SELECT MAX(created_at) FROM mail_receipts
                                            WHERE system_id = ?1 AND retired = 0))",
        rusqlite::params![system_id.as_str(), before.seconds()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{sample_system, temp_db};
    use super::*;
    use crate::models::SystemStatus;
    use crate::registry::LastSeen;
    use crate::snapshot::{ReportedSnapshot, snapshot_rule};

    const NOW: u64 = 1_700_000_000;

    fn at(seconds: u64) -> SnapshotTime {
        SnapshotTime::try_from(seconds).unwrap()
    }

    fn mail_row(id: &str) -> SystemInfo {
        SystemInfo {
            url: "mail://".into(),
            token: String::new(),
            ..sample_system(id, id)
        }
    }

    fn snapshot(cpu: f32) -> Snapshot {
        snapshot_rule(ReportedSnapshot {
            cpu: Some(cpu),
            ..ReportedSnapshot::default()
        })
        .0
    }

    fn alert(id: &str) -> AlertRecord {
        AlertRecord {
            id: id.into(),
            system_id: "mailed".into(),
            system_name: "mailed".into(),
            severity: "warning".into(),
            message: "CPU high".into(),
            current_value: 95.0,
            fired_at: "2023-11-14T22:18:20Z".into(),
            stored_at: "2023-11-14T22:20:00Z".into(),
            acknowledged: false,
        }
    }

    /// A report `seq` created at `created_at`, with one sample per cpu value, a minute apart
    /// and ending at `created_at`.
    fn write<'a>(row: &'a SystemInfo, seq: u64, created_at: u64, cpus: &[f32]) -> MailWrite<'a> {
        let first = created_at - 60 * (cpus.len() as u64 - 1);
        MailWrite {
            system_id: Box::leak(Box::new(SystemId::try_from(row.id.clone()).unwrap())),
            system: row,
            receipt: MailReceipt {
                run: "run-a".into(),
                seq,
                created_at: at(created_at),
                interval_secs: 300,
                received_at: at(NOW),
            },
            snapshots: (cpus.iter().enumerate())
                .map(|(i, cpu)| (snapshot(*cpu), at(first + 60 * i as u64)))
                .collect(),
            status: StatusUpdate::after_snapshot(LastSeen::ReportedAt(at(created_at))),
            alerts: vec![alert(&format!("mailed_run-a-{seq}"))],
            now: at(NOW),
            prune_before: at(NOW - 7 * 86_400),
        }
    }

    /// The newest-first `decide` the intake passes: the pure rule.
    fn decide(created_at: u64) -> impl FnOnce(Option<SnapshotTime>) -> Recency {
        move |previous| crate::mail_intake::receipt::recency(at(created_at), previous)
    }

    fn cpu_points(db: &Database) -> Vec<(i64, f32)> {
        let conn = db.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT timestamp, value FROM metrics WHERE metric = 'cpu' ORDER BY timestamp")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn receipts(db: &Database) -> Vec<(u64, bool)> {
        let conn = db.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT seq, retired FROM mail_receipts ORDER BY seq")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn status(db: &Database, id: &str) -> Option<(SystemStatus, String)> {
        db.get_system(id)
            .unwrap()
            .map(|sys| (sys.status, sys.last_seen))
    }

    /// RFC 0017 §6: a new id is registered as a mail system, and a newest report stores every
    /// sample, its alert records and its receipt, and marks the system online.
    #[test]
    fn a_first_report_registers_its_system_and_stores_everything() {
        let (db, _dir) = temp_db();
        let row = mail_row("mailed");

        let stored = db.store_mail_report(write(&row, 1, NOW, &[1.0, 2.0]), decide(NOW), |_, t| t);

        assert_eq!(
            stored.unwrap(),
            MailStored::Stored {
                recency: Recency::Newest,
                expired: 0,
                newest: Some(at(NOW))
            }
        );
        assert_eq!(db.get_system("mailed").unwrap().unwrap().url, "mail://");
        assert_eq!(
            cpu_points(&db),
            [((NOW - 60) as i64, 1.0), (NOW as i64, 2.0)]
        );
        assert_eq!(
            status(&db, "mailed"),
            Some((SystemStatus::Online, crate::clock::unix_to_iso8601(NOW)))
        );
        assert_eq!(db.get_alerts(Some("mailed"), None, 10).unwrap().len(), 1);
        assert_eq!(receipts(&db), [(1, false)]);
    }

    /// RFC 0017 §6: mail is delivered at least once: the same report again stores nothing.
    #[test]
    fn a_duplicate_report_stores_nothing() {
        let (db, _dir) = temp_db();
        let row = mail_row("mailed");
        db.store_mail_report(write(&row, 1, NOW, &[1.0]), decide(NOW), |_, _| ())
            .unwrap();

        let again = db.store_mail_report(write(&row, 1, NOW, &[1.0]), decide(NOW), |_, _| ());

        assert_eq!(again.unwrap(), MailStored::Duplicate);
        assert_eq!(cpu_points(&db).len(), 1, "each point once");
    }

    /// RFC 0017 §6: a failure partway leaves no point and no receipt, so the same message
    /// later is stored whole, once.
    #[test]
    fn a_failure_partway_leaves_nothing_and_a_retry_stores_once() {
        let (db, _dir) = temp_db();
        let row = mail_row("mailed");
        db.conn
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_second BEFORE INSERT ON metrics WHEN NEW.value = 2.0
                 BEGIN SELECT RAISE(ABORT, 'refused'); END;",
            )
            .unwrap();

        let failed = db.store_mail_report(write(&row, 1, NOW, &[1.0, 2.0]), decide(NOW), |_, _| ());

        assert!(failed.is_err(), "the store failed");
        assert_eq!(cpu_points(&db), [], "no point kept");
        assert_eq!(receipts(&db), [], "no receipt kept");
        assert!(
            db.get_system("mailed").unwrap().is_none(),
            "no registration kept"
        );
        db.conn
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_second;")
            .unwrap();
        db.store_mail_report(write(&row, 1, NOW, &[1.0, 2.0]), decide(NOW), |_, _| ())
            .unwrap();
        assert_eq!(cpu_points(&db).len(), 2, "each point once");
    }

    /// RFC 0017 §6: a report older than the newest accepted one adds its points and alerts,
    /// but never rewinds the status or last seen, and `on_newest` isn't called.
    #[test]
    fn a_backfill_report_adds_points_but_leaves_the_status() {
        let (db, _dir) = temp_db();
        let row = mail_row("mailed");
        db.store_mail_report(write(&row, 2, NOW, &[2.0]), decide(NOW), |_, _| ())
            .unwrap();
        db.update_system_status("mailed", &SystemStatus::Offline, "kept", None)
            .unwrap();

        let late = NOW - 300;
        let stored = db.store_mail_report(write(&row, 1, late, &[1.0]), decide(late), |_, t| t);

        assert_eq!(
            stored.unwrap(),
            MailStored::Stored {
                recency: Recency::Backfill,
                expired: 0,
                newest: None
            }
        );
        assert_eq!(cpu_points(&db).len(), 2);
        assert_eq!(
            status(&db, "mailed"),
            Some((SystemStatus::Offline, "kept".into()))
        );
    }

    /// RFC 0017 §6: a mail report never writes into a pushed or polled system.
    #[test]
    fn a_report_for_a_pushed_or_polled_id_is_a_transport_mismatch() {
        for url in ["push://", "http://10.0.0.1:9090"] {
            let (db, _dir) = temp_db();
            let mut other = sample_system("mailed", "mailed");
            other.url = url.into();
            db.insert_system(&other).unwrap();

            let stored = db.store_mail_report(
                write(&mail_row("mailed"), 1, NOW, &[1.0]),
                decide(NOW),
                |_, _| (),
            );

            assert_eq!(stored.unwrap(), MailStored::TransportMismatch, "{url}");
            assert_eq!(cpu_points(&db), [], "{url}");
            assert_eq!(receipts(&db), [], "{url}");
        }
    }

    /// RFC 0017 §6: a point older than its series' retention at the hub's `now` is left out
    /// and counted; the report's other points are kept.
    #[test]
    fn a_point_past_its_retention_is_left_out_and_counted() {
        let (db, _dir) = temp_db();
        let row = mail_row("mailed");
        let created = NOW - 86_400 + 30;
        // Samples at created − 60 (expired: past the default 24 h) and created (kept).
        let stored = db.store_mail_report(
            write(&row, 1, created, &[1.0, 2.0]),
            decide(created),
            |_, _| (),
        );

        assert_eq!(
            stored.unwrap(),
            MailStored::Stored {
                recency: Recency::Newest,
                expired: 1,
                newest: Some(())
            }
        );
        assert_eq!(cpu_points(&db), [(created as i64, 2.0)]);
    }

    /// RFC 0017 §6: receipts past the window are pruned, except the system's newest current
    /// one, which `mail_status` reads.
    #[test]
    fn the_prune_keeps_the_newest_current_receipt() {
        let (db, _dir) = temp_db();
        let row = mail_row("mailed");
        let old = NOW - 8 * 86_400;
        for seq in [1, 2] {
            let mut setup = write(&row, seq, old + seq, &[1.0]);
            setup.prune_before = at(0);
            db.store_mail_report(setup, decide(old + seq), |_, _| ())
                .unwrap();
        }
        assert_eq!(receipts(&db), [(1, false), (2, false)]);

        let late = NOW - 9 * 86_400;
        db.store_mail_report(write(&row, 3, late, &[1.0]), decide(late), |_, _| ())
            .unwrap();

        assert_eq!(
            receipts(&db),
            [(2, false)],
            "only the newest current receipt is kept"
        );
    }

    /// RFC 0017 §6: deleting a system retires its receipts in one transaction: a replay of a
    /// report accepted before is a duplicate, whatever its clock; a new report registers the
    /// system again as its first.
    #[test]
    fn deleting_a_system_retires_its_receipts() {
        let (db, _dir) = temp_db();
        let row = mail_row("mailed");
        db.store_mail_report(write(&row, 1, NOW, &[1.0]), decide(NOW), |_, _| ())
            .unwrap();

        db.delete_system("mailed").unwrap();

        assert_eq!(receipts(&db), [(1, true)]);
        let replay = db.store_mail_report(write(&row, 1, NOW, &[1.0]), decide(NOW), |_, _| ());
        assert_eq!(replay.unwrap(), MailStored::Duplicate);
        assert!(
            db.get_system("mailed").unwrap().is_none(),
            "not registered again"
        );
        let earlier = NOW - 600;
        let fresh =
            db.store_mail_report(write(&row, 2, earlier, &[1.0]), decide(earlier), |_, _| ());
        assert_eq!(
            fresh.unwrap(),
            MailStored::Stored {
                recency: Recency::Newest,
                expired: 0,
                newest: Some(())
            },
            "the retired receipt isn't its newest"
        );
    }

    /// RFC 0017 §6: `delete_system` is one transaction: a failure partway deletes nothing.
    #[test]
    fn a_delete_that_fails_partway_deletes_nothing() {
        let (db, _dir) = temp_db();
        let row = mail_row("mailed");
        db.store_mail_report(write(&row, 1, NOW, &[1.0]), decide(NOW), |_, _| ())
            .unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER refuse BEFORE DELETE ON systems
                 BEGIN SELECT RAISE(ABORT, 'refused'); END;",
            )
            .unwrap();

        assert!(db.delete_system("mailed").is_err());

        assert_eq!(cpu_points(&db).len(), 1, "metrics kept");
        assert_eq!(receipts(&db), [(1, false)], "receipts not retired");
    }
}
