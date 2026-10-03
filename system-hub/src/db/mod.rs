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

use crate::registry::{MemoryCapacity, PollInterval};
use rusqlite::Connection;
use std::sync::Mutex;

use crate::models::*;

mod history;
mod mail;
mod sources;
pub use history::{RoundStored, SnapshotStored};
pub use mail::{MailReceipt, MailStored, MailWrite};
pub use sources::SourceRow;

/// What a registration found (RFC 0016 §2): the id was absent and is now inserted, or it was
/// known, and its stored `url` is, or isn't, the one the registration presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registration {
    Inserted,
    Known { same_url: bool },
}

/// Whether the system's row exists, on a connection the caller already holds.
fn system_exists(conn: &Connection, system_id: &str) -> Result<bool, rusqlite::Error> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM systems WHERE id = ?1)",
        [system_id],
        |row| row.get(0),
    )
}

/// Whether `system_id` has a row, and if so whether its `url` is `url`. SQLite compares, so a
/// stored `url` of any type maps (a blob is never equal to text), and no other column is read.
fn known_url(
    conn: &Connection,
    system_id: &str,
    url: &str,
) -> Result<Option<bool>, rusqlite::Error> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT url IS ?2 FROM systems WHERE id = ?1",
        [system_id, url],
        |row| row.get(0),
    )
    .optional()
}

/// Registers a system, never replacing a row.
const INSERT_SYSTEM: &str = "INSERT INTO systems
        (id, name, url, token, status, last_seen, last_error,
         os, hostname, kernel, cpu_model, cpu_cores,
         total_memory_display, total_memory_bytes,
         poll_interval_secs, enabled)
     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
     ON CONFLICT(id) DO NOTHING";

/// A system's columns, in the order the `systems` INSERTs name them.
fn system_params(sys: &SystemInfo) -> impl rusqlite::Params + '_ {
    (
        &sys.id,
        &sys.name,
        &sys.url,
        &sys.token,
        sys.status.to_string(),
        &sys.last_seen,
        &sys.last_error,
        &sys.os,
        &sys.hostname,
        &sys.kernel,
        &sys.cpu_model,
        sys.cpu_cores,
        &sys.total_memory_display,
        sys.total_memory_bytes,
        sys.poll_interval_secs,
        sys.enabled,
    )
}

pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    pub fn new(path: &str) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS systems (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                url TEXT NOT NULL,
                token TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'unknown',
                last_seen TEXT NOT NULL DEFAULT '',
                last_error TEXT,
                os TEXT,
                hostname TEXT,
                kernel TEXT,
                cpu_model TEXT,
                cpu_cores INTEGER,
                total_memory_display TEXT,
                total_memory_bytes INTEGER,
                poll_interval_secs INTEGER NOT NULL DEFAULT 10,
                enabled INTEGER NOT NULL DEFAULT 1
            );

            CREATE TABLE IF NOT EXISTS metrics (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                system_id TEXT NOT NULL,
                metric TEXT NOT NULL,
                value REAL NOT NULL,
                timestamp INTEGER NOT NULL,
                FOREIGN KEY (system_id) REFERENCES systems(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_metrics_system_time
                ON metrics(system_id, metric, timestamp);

            CREATE TABLE IF NOT EXISTS alerts (
                id TEXT PRIMARY KEY,
                system_id TEXT NOT NULL,
                system_name TEXT NOT NULL,
                severity TEXT NOT NULL,
                message TEXT NOT NULL,
                current_value REAL NOT NULL,
                fired_at TEXT NOT NULL,
                stored_at TEXT NOT NULL,
                acknowledged INTEGER NOT NULL DEFAULT 0,
                FOREIGN KEY (system_id) REFERENCES systems(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_alerts_system
                ON alerts(system_id, stored_at);

            CREATE TABLE IF NOT EXISTS metric_retention (
                system_id TEXT NOT NULL,
                metric TEXT NOT NULL,
                retention_secs INTEGER NOT NULL DEFAULT 86400,
                PRIMARY KEY (system_id, metric),
                FOREIGN KEY (system_id) REFERENCES systems(id) ON DELETE CASCADE
            );

            -- RFC 0017: one row per accepted mail report. No foreign key: deleting a system
            -- retires its receipts, which then refuse replays of its reports.
            CREATE TABLE IF NOT EXISTS mail_receipts (
                system_id     TEXT    NOT NULL,
                run           TEXT    NOT NULL,
                seq           INTEGER NOT NULL,
                created_at    INTEGER NOT NULL,
                interval_secs INTEGER NOT NULL,
                received_at   INTEGER NOT NULL,
                retired       INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (system_id, run, seq)
            );
            CREATE INDEX IF NOT EXISTS mail_receipts_newest
                ON mail_receipts (system_id, retired, created_at);
            ",
        )?;
        Ok(())
    }

    // ── Systems CRUD ───────────────────────────────────

    pub fn list_systems(&self) -> Result<Vec<SystemInfo>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, url, token, status, last_seen, last_error,
                    os, hostname, kernel, cpu_model, cpu_cores,
                    total_memory_display, total_memory_bytes,
                    poll_interval_secs, enabled
             FROM systems ORDER BY name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(SystemInfo {
                id: row.get(0)?,
                name: row.get(1)?,
                url: row.get(2)?,
                token: row.get(3)?,
                status: parse_status(&row.get::<_, String>(4)?),
                last_seen: row.get(5)?,
                last_error: row.get(6)?,
                os: row.get(7)?,
                hostname: row.get(8)?,
                kernel: row.get(9)?,
                cpu_model: row.get(10)?,
                cpu_cores: row.get(11)?,
                total_memory_display: row.get(12)?,
                total_memory_bytes: row.get(13)?,
                poll_interval_secs: row.get(14)?,
                enabled: row.get(15)?,
            })
        })?;
        rows.collect()
    }

    pub fn get_system(&self, id: &str) -> Result<Option<SystemInfo>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, url, token, status, last_seen, last_error,
                    os, hostname, kernel, cpu_model, cpu_cores,
                    total_memory_display, total_memory_bytes,
                    poll_interval_secs, enabled
             FROM systems WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map([id], |row| {
            Ok(SystemInfo {
                id: row.get(0)?,
                name: row.get(1)?,
                url: row.get(2)?,
                token: row.get(3)?,
                status: parse_status(&row.get::<_, String>(4)?),
                last_seen: row.get(5)?,
                last_error: row.get(6)?,
                os: row.get(7)?,
                hostname: row.get(8)?,
                kernel: row.get(9)?,
                cpu_model: row.get(10)?,
                cpu_cores: row.get(11)?,
                total_memory_display: row.get(12)?,
                total_memory_bytes: row.get(13)?,
                poll_interval_secs: row.get(14)?,
                enabled: row.get(15)?,
            })
        })?;
        rows.next().transpose()
    }

    pub fn insert_system(&self, sys: &SystemInfo) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO systems
                (id, name, url, token, status, last_seen, last_error,
                 os, hostname, kernel, cpu_model, cpu_cores,
                 total_memory_display, total_memory_bytes,
                 poll_interval_secs, enabled)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            system_params(sys),
        )?;
        Ok(())
    }

    /// Registers `sys` unless a row with its id exists, under one hold of the mutex (RFC 0007
    /// §4). A known id runs only the existence check, which reads only whether the row's `url`
    /// is `sys.url`, compared by SQLite (RFC 0016 §2), so neither a row that doesn't map nor a
    /// database that refuses writes fails it. An absent id is inserted
    /// with `ON CONFLICT(id) DO NOTHING`, so no registration replaces a row.
    pub fn insert_system_if_absent(
        &self,
        sys: &SystemInfo,
    ) -> Result<Registration, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        if let Some(same_url) = known_url(&conn, &sys.id, &sys.url)? {
            return Ok(Registration::Known { same_url });
        }
        conn.execute(INSERT_SYSTEM, system_params(sys))?;
        Ok(Registration::Inserted)
    }

    /// Stores `poll_interval_secs` as `value` for `id`, as an older hub could, making the row
    /// one `list_systems` can't map.
    #[cfg(test)]
    pub fn set_poll_interval_for_test(&self, id: &str, value: i64) {
        self.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE systems SET poll_interval_secs = ?2 WHERE id = ?1",
                (id, value),
            )
            .unwrap();
    }

    /// Poisons the connection mutex: a thread panics while holding its guard.
    #[cfg(test)]
    pub fn poison_for_test(&self) {
        std::thread::scope(|scope| {
            let poisoner = scope.spawn(|| {
                let _guard = self.conn.lock().unwrap();
                panic!("poisoning the database mutex for a test");
            });
            assert!(poisoner.join().is_err(), "the poisoning thread panicked");
        });
        assert!(self.conn.is_poisoned());
    }

    pub fn update_system_status(
        &self,
        id: &str,
        status: &SystemStatus,
        last_seen: &str,
        error: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE systems SET status = ?1, last_seen = ?2, last_error = ?3 WHERE id = ?4",
            rusqlite::params![status.to_string(), last_seen, error, id],
        )?;
        Ok(())
    }

    /// Writes a system's info. Not its memory capacity, which follows the agent and has its
    /// own writer, `update_memory_capacity`.
    pub fn update_system_info(
        &self,
        id: &str,
        os: Option<&str>,
        hostname: Option<&str>,
        kernel: Option<&str>,
        cpu_model: Option<&str>,
        cpu_cores: Option<usize>,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE systems SET os=?1, hostname=?2, kernel=?3, cpu_model=?4, cpu_cores=?5
             WHERE id=?6",
            rusqlite::params![os, hostname, kernel, cpu_model, cpu_cores, id],
        )?;
        Ok(())
    }

    /// Stores `capacity` as the system's memory total, both columns in one statement, leaving
    /// every other column as it is.
    pub fn update_memory_capacity(
        &self,
        id: &str,
        capacity: &MemoryCapacity,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE systems SET total_memory_display=?1, total_memory_bytes=?2 WHERE id=?3",
            rusqlite::params![capacity.display(), capacity.bytes(), id],
        )?;
        Ok(())
    }

    pub fn update_system_config(
        &self,
        id: &str,
        name: Option<&str>,
        url: Option<&str>,
        token: Option<&str>,
        poll_interval_secs: Option<PollInterval>,
        enabled: Option<bool>,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut sets = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(v) = name {
            sets.push("name = ?");
            params.push(Box::new(v.to_string()));
        }
        if let Some(v) = url {
            sets.push("url = ?");
            params.push(Box::new(v.to_string()));
        }
        if let Some(v) = token {
            sets.push("token = ?");
            params.push(Box::new(v.to_string()));
        }
        if let Some(v) = poll_interval_secs {
            sets.push("poll_interval_secs = ?");
            params.push(Box::new(v.column_value()));
        }
        if let Some(v) = enabled {
            sets.push("enabled = ?");
            params.push(Box::new(v as i32));
        }

        if sets.is_empty() {
            return Ok(());
        }

        let sql = format!("UPDATE systems SET {} WHERE id = ?", sets.join(", "));
        params.push(Box::new(id.to_string()));

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        conn.execute(&sql, param_refs.as_slice())?;
        Ok(())
    }

    /// Deletes a system and its history in one transaction, and retires its mail receipts,
    /// which then refuse replays of its reports (RFC 0017 §6).
    pub fn delete_system(&self, id: &str) -> Result<(), rusqlite::Error> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM metrics WHERE system_id = ?1", [id])?;
        tx.execute("DELETE FROM alerts WHERE system_id = ?1", [id])?;
        tx.execute("DELETE FROM metric_retention WHERE system_id = ?1", [id])?;
        tx.execute(
            "UPDATE mail_receipts SET retired = 1 WHERE system_id = ?1",
            [id],
        )?;
        tx.execute("DELETE FROM systems WHERE id = ?1", [id])?;
        tx.commit()
    }

    // ── Alerts ─────────────────────────────────────────

    pub fn insert_alert(&self, alert: &AlertRecord) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        insert_alert_record(&conn, alert)
    }

    pub fn get_alerts(
        &self,
        system_id: Option<&str>,
        acknowledged: Option<bool>,
        limit: usize,
    ) -> Result<Vec<AlertRecord>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut conditions = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(sid) = system_id {
            conditions.push("system_id = ?");
            params.push(Box::new(sid.to_string()));
        }
        if let Some(ack) = acknowledged {
            conditions.push("acknowledged = ?");
            params.push(Box::new(ack as i32));
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        let sql = format!(
            "SELECT id, system_id, system_name, severity, message, current_value,
                    fired_at, stored_at, acknowledged
             FROM alerts {} ORDER BY stored_at DESC LIMIT ?",
            where_clause
        );
        params.push(Box::new(limit as i64));

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(AlertRecord {
                id: row.get(0)?,
                system_id: row.get(1)?,
                system_name: row.get(2)?,
                severity: row.get(3)?,
                message: row.get(4)?,
                current_value: row.get(5)?,
                fired_at: row.get(6)?,
                stored_at: row.get(7)?,
                acknowledged: row.get(8)?,
            })
        })?;
        rows.collect()
    }

    pub fn acknowledge_alert(&self, alert_id: &str) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE alerts SET acknowledged = 1 WHERE id = ?1",
            [alert_id],
        )?;
        Ok(())
    }

    pub fn count_active_alerts(&self) -> Result<usize, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM alerts WHERE acknowledged = 0",
            [],
            |row| row.get(0),
        )
    }
}

/// The alert-record rule: one record per incident, keeping the values first seen.
fn insert_alert_record(conn: &Connection, alert: &AlertRecord) -> Result<(), rusqlite::Error> {
    conn.prepare_cached(
        "INSERT OR IGNORE INTO alerts (id, system_id, system_name, severity, message,
         current_value, fired_at, stored_at, acknowledged)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
    )?
    .execute(rusqlite::params![
        alert.id,
        alert.system_id,
        alert.system_name,
        alert.severity,
        alert.message,
        alert.current_value,
        alert.fired_at,
        alert.stored_at,
        alert.acknowledged,
    ])?;
    Ok(())
}

fn parse_status(s: &str) -> SystemStatus {
    match s {
        "online" => SystemStatus::Online,
        "offline" => SystemStatus::Offline,
        _ => SystemStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn temp_db() -> (Database, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Database::new(path.to_str().unwrap()).unwrap();
        (db, dir)
    }

    pub(super) fn sample_system(id: &str, name: &str) -> SystemInfo {
        SystemInfo {
            id: id.to_string(),
            name: name.to_string(),
            url: "http://example.com".to_string(),
            token: "secret".to_string(),
            status: SystemStatus::Unknown,
            last_seen: String::new(),
            last_error: None,
            os: None,
            hostname: None,
            kernel: None,
            cpu_model: None,
            cpu_cores: None,
            total_memory_display: None,
            total_memory_bytes: None,
            poll_interval_secs: 10,
            enabled: true,
        }
    }

    #[test]
    fn parse_status_covers_all_variants_and_default() {
        assert_eq!(parse_status("online"), SystemStatus::Online);
        assert_eq!(parse_status("offline"), SystemStatus::Offline);
        assert_eq!(parse_status("unknown"), SystemStatus::Unknown);
        assert_eq!(parse_status("garbage"), SystemStatus::Unknown);
    }

    #[test]
    fn migrate_runs_on_new_and_is_idempotent() {
        let (db, dir) = temp_db();
        let path = dir.path().join("test.db");
        drop(db);
        // Re-opening the same file must not fail even though tables already exist.
        let db2 = Database::new(path.to_str().unwrap()).unwrap();
        assert!(db2.list_systems().unwrap().is_empty());
    }

    #[test]
    fn insert_and_get_system_round_trip() {
        let (db, _dir) = temp_db();
        let sys = sample_system("id-1", "web-01");
        db.insert_system(&sys).unwrap();

        let fetched = db.get_system("id-1").unwrap().unwrap();
        assert_eq!(fetched.id, "id-1");
        assert_eq!(fetched.name, "web-01");
        assert_eq!(fetched.url, "http://example.com");
        assert_eq!(fetched.token, "secret");
        assert_eq!(fetched.poll_interval_secs, 10);
        assert!(fetched.enabled);
    }

    #[test]
    fn get_system_returns_none_for_missing_id() {
        let (db, _dir) = temp_db();
        assert!(db.get_system("nope").unwrap().is_none());
    }

    #[test]
    fn insert_system_replaces_existing_row_with_same_id() {
        let (db, _dir) = temp_db();
        let mut sys = sample_system("id-1", "original");
        db.insert_system(&sys).unwrap();
        sys.name = "renamed".to_string();
        db.insert_system(&sys).unwrap();

        let all = db.list_systems().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "renamed");
    }

    #[test]
    fn list_systems_orders_by_name() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-b", "bravo")).unwrap();
        db.insert_system(&sample_system("id-a", "alpha")).unwrap();
        let all = db.list_systems().unwrap();
        assert_eq!(
            all.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "bravo"]
        );
    }

    #[test]
    fn update_system_status_updates_fields() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "web-01")).unwrap();
        db.update_system_status("id-1", &SystemStatus::Online, "2026-01-01T00:00:00Z", None)
            .unwrap();
        let sys = db.get_system("id-1").unwrap().unwrap();
        assert_eq!(sys.status, SystemStatus::Online);
        assert_eq!(sys.last_seen, "2026-01-01T00:00:00Z");
        assert_eq!(sys.last_error, None);

        db.update_system_status("id-1", &SystemStatus::Offline, "", Some("timeout"))
            .unwrap();
        let sys = db.get_system("id-1").unwrap().unwrap();
        assert_eq!(sys.status, SystemStatus::Offline);
        assert_eq!(sys.last_error.as_deref(), Some("timeout"));
    }

    #[test]
    fn update_system_info_updates_hardware_fields() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "web-01")).unwrap();
        db.update_system_info(
            "id-1",
            Some("Ubuntu 22.04"),
            Some("web01"),
            Some("6.6.0"),
            Some("Generic CPU"),
            Some(8),
        )
        .unwrap();
        let sys = db.get_system("id-1").unwrap().unwrap();
        assert_eq!(sys.os.as_deref(), Some("Ubuntu 22.04"));
        assert_eq!(sys.hostname.as_deref(), Some("web01"));
        assert_eq!(sys.cpu_cores, Some(8));
        assert_eq!(
            (sys.total_memory_display, sys.total_memory_bytes),
            (None, None),
            "the memory capacity has its own writer"
        );
    }

    #[test]
    fn update_memory_capacity_writes_both_columns_and_nothing_else() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "web-01")).unwrap();
        db.insert_system(&sample_system("id-2", "web-02")).unwrap();
        let known = |db: &Database, id: &str| {
            db.update_system_info(
                id,
                Some("Ubuntu 22.04"),
                Some("web01"),
                Some("6.6.0"),
                Some("Generic CPU"),
                Some(8),
            )
            .unwrap();
            db.update_memory_capacity(id, &MemoryCapacity::fixture("16.0 GB", 16_000_000_000))
                .unwrap();
        };
        known(&db, "id-1");
        known(&db, "id-2");
        let capacity = MemoryCapacity::fixture("512.0 MB", 536_870_912);
        let before = db.get_system("id-1").unwrap().unwrap();

        db.update_memory_capacity("id-1", &capacity).unwrap();

        let sys = db.get_system("id-1").unwrap().unwrap();
        // The whole row, so a column added later is covered too.
        assert_eq!(
            sys,
            SystemInfo {
                total_memory_display: Some("512.0 MB".into()),
                total_memory_bytes: Some(536_870_912),
                ..before
            },
            "both columns written, every other one untouched"
        );
        let other = db.get_system("id-2").unwrap().unwrap();
        assert_eq!(
            other.total_memory_bytes,
            Some(16_000_000_000),
            "only the named system"
        );
    }

    #[test]
    fn update_system_config_partial_update_only_touches_given_fields() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "original"))
            .unwrap();
        db.update_system_config("id-1", Some("renamed"), None, None, None, None)
            .unwrap();
        let sys = db.get_system("id-1").unwrap().unwrap();
        assert_eq!(sys.name, "renamed");
        assert_eq!(sys.url, "http://example.com"); // unchanged
        assert!(sys.enabled); // unchanged
    }

    #[test]
    fn update_system_config_all_fields() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "original"))
            .unwrap();
        db.update_system_config(
            "id-1",
            Some("renamed"),
            Some("http://new.example.com"),
            Some("newtoken"),
            PollInterval::try_from(30).ok(),
            Some(false),
        )
        .unwrap();
        let sys = db.get_system("id-1").unwrap().unwrap();
        assert_eq!(sys.name, "renamed");
        assert_eq!(sys.url, "http://new.example.com");
        assert_eq!(sys.token, "newtoken");
        assert_eq!(sys.poll_interval_secs, 30);
        assert!(!sys.enabled);
    }

    #[test]
    fn update_system_config_no_fields_is_a_no_op() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "original"))
            .unwrap();
        // All None: the function should return Ok(()) without touching the row.
        db.update_system_config("id-1", None, None, None, None, None)
            .unwrap();
        let sys = db.get_system("id-1").unwrap().unwrap();
        assert_eq!(sys.name, "original");
    }

    #[test]
    fn delete_system_cascades_metrics_alerts_and_system_row() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "web-01")).unwrap();
        db.plant_point("id-1", "cpu", 50.0, 100).unwrap();
        db.insert_alert(&AlertRecord {
            id: "alert-1".to_string(),
            system_id: "id-1".to_string(),
            system_name: "web-01".to_string(),
            severity: "warning".to_string(),
            message: "high cpu".to_string(),
            current_value: 95.0,
            fired_at: "2026-01-01T00:00:00Z".to_string(),
            stored_at: "2026-01-01T00:00:00Z".to_string(),
            acknowledged: false,
        })
        .unwrap();

        db.delete_system("id-1").unwrap();

        assert!(db.get_system("id-1").unwrap().is_none());
        assert!(db.get_metrics("id-1", "cpu", 100, None).unwrap().is_empty());
        assert!(db.get_alerts(Some("id-1"), None, 100).unwrap().is_empty());
    }

    #[test]
    fn insert_alert_and_get_alerts_round_trip() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "web-01")).unwrap();
        let alert = AlertRecord {
            id: "alert-1".to_string(),
            system_id: "id-1".to_string(),
            system_name: "web-01".to_string(),
            severity: "critical".to_string(),
            message: "disk full".to_string(),
            current_value: 99.0,
            fired_at: "2026-01-01T00:00:00Z".to_string(),
            stored_at: "2026-01-01T00:00:01Z".to_string(),
            acknowledged: false,
        };
        db.insert_alert(&alert).unwrap();

        let alerts = db.get_alerts(None, None, 100).unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].id, "alert-1");
    }

    #[test]
    fn insert_alert_is_idempotent_on_duplicate_id() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "web-01")).unwrap();
        let alert = AlertRecord {
            id: "dup".to_string(),
            system_id: "id-1".to_string(),
            system_name: "web-01".to_string(),
            severity: "warning".to_string(),
            message: "m1".to_string(),
            current_value: 1.0,
            fired_at: "t1".to_string(),
            stored_at: "t1".to_string(),
            acknowledged: false,
        };
        db.insert_alert(&alert).unwrap();
        let mut second = alert.clone();
        second.message = "m2".to_string();
        db.insert_alert(&second).unwrap(); // INSERT OR IGNORE: should not overwrite.

        let alerts = db.get_alerts(None, None, 100).unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].message, "m1");
    }

    #[test]
    fn get_alerts_filters_by_system_id_and_acknowledged() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "sys-1")).unwrap();
        db.insert_system(&sample_system("id-2", "sys-2")).unwrap();
        db.insert_alert(&AlertRecord {
            id: "a1".into(),
            system_id: "id-1".into(),
            system_name: "sys-1".into(),
            severity: "warning".into(),
            message: "m".into(),
            current_value: 1.0,
            fired_at: "t".into(),
            stored_at: "t1".into(),
            acknowledged: false,
        })
        .unwrap();
        db.insert_alert(&AlertRecord {
            id: "a2".into(),
            system_id: "id-2".into(),
            system_name: "sys-2".into(),
            severity: "warning".into(),
            message: "m".into(),
            current_value: 1.0,
            fired_at: "t".into(),
            stored_at: "t2".into(),
            acknowledged: true,
        })
        .unwrap();

        let sys1_alerts = db.get_alerts(Some("id-1"), None, 100).unwrap();
        assert_eq!(sys1_alerts.len(), 1);
        assert_eq!(sys1_alerts[0].id, "a1");

        let unacked = db.get_alerts(None, Some(false), 100).unwrap();
        assert_eq!(unacked.len(), 1);
        assert_eq!(unacked[0].id, "a1");

        let acked = db.get_alerts(None, Some(true), 100).unwrap();
        assert_eq!(acked.len(), 1);
        assert_eq!(acked[0].id, "a2");
    }

    #[test]
    fn get_alerts_respects_limit_and_orders_newest_first() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "sys-1")).unwrap();
        for i in 0..5 {
            db.insert_alert(&AlertRecord {
                id: format!("a{i}"),
                system_id: "id-1".into(),
                system_name: "sys-1".into(),
                severity: "warning".into(),
                message: "m".into(),
                current_value: 1.0,
                fired_at: "t".into(),
                stored_at: format!("t{i}"),
                acknowledged: false,
            })
            .unwrap();
        }
        let alerts = db.get_alerts(None, None, 2).unwrap();
        assert_eq!(alerts.len(), 2);
        assert_eq!(alerts[0].id, "a4");
        assert_eq!(alerts[1].id, "a3");
    }

    #[test]
    fn acknowledge_alert_flips_flag() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "sys-1")).unwrap();
        db.insert_alert(&AlertRecord {
            id: "a1".into(),
            system_id: "id-1".into(),
            system_name: "sys-1".into(),
            severity: "warning".into(),
            message: "m".into(),
            current_value: 1.0,
            fired_at: "t".into(),
            stored_at: "t1".into(),
            acknowledged: false,
        })
        .unwrap();

        db.acknowledge_alert("a1").unwrap();
        let alerts = db.get_alerts(None, None, 100).unwrap();
        assert!(alerts[0].acknowledged);
    }

    #[test]
    fn acknowledge_alert_on_missing_id_is_a_no_op() {
        let (db, _dir) = temp_db();
        // Should not error even though the alert doesn't exist.
        db.acknowledge_alert("does-not-exist").unwrap();
    }

    #[test]
    fn count_active_alerts_only_counts_unacknowledged() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "sys-1")).unwrap();
        db.insert_alert(&AlertRecord {
            id: "a1".into(),
            system_id: "id-1".into(),
            system_name: "sys-1".into(),
            severity: "warning".into(),
            message: "m".into(),
            current_value: 1.0,
            fired_at: "t".into(),
            stored_at: "t1".into(),
            acknowledged: false,
        })
        .unwrap();
        db.insert_alert(&AlertRecord {
            id: "a2".into(),
            system_id: "id-1".into(),
            system_name: "sys-1".into(),
            severity: "warning".into(),
            message: "m".into(),
            current_value: 1.0,
            fired_at: "t".into(),
            stored_at: "t2".into(),
            acknowledged: true,
        })
        .unwrap();

        assert_eq!(db.count_active_alerts().unwrap(), 1);
    }

    /// RFC 0007 §4: push registration checks, then inserts only an absent id.
    mod insert_system_if_absent {
        use super::*;
        use rusqlite::OptionalExtension;

        /// A known system with a name of its own and one metric point, whose row no read can
        /// map: its `poll_interval_secs` is stored as −1, as an older hub's `PUT` stored
        /// `u64::MAX`.
        fn db_with_unmappable_known_system() -> (Database, tempfile::TempDir) {
            let (db, dir) = temp_db();
            db.insert_system(&sample_system("known", "web-01")).unwrap();
            db.conn
                .lock()
                .unwrap()
                .execute_batch(
                    "INSERT INTO metrics (system_id, metric, value, timestamp)
                         VALUES ('known', 'cpu', 1.0, 100);
                     UPDATE systems SET poll_interval_secs = -1 WHERE id = 'known';",
                )
                .unwrap();
            assert!(db.get_system("known").is_err(), "the known row doesn't map");
            (db, dir)
        }

        /// The row as a registration may never change it: its name and its points.
        fn raw_row(db: &Database, id: &str) -> Option<(String, i64)> {
            let conn = db.conn.lock().unwrap();
            conn.query_row(
                "SELECT name, (SELECT COUNT(*) FROM metrics WHERE system_id = ?1)
                 FROM systems WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .unwrap()
        }

        const REFUSE_INSERTS: &str = "CREATE TRIGGER refuse BEFORE INSERT ON systems
             BEGIN SELECT RAISE(ABORT, 'refused'); END;";
        const REFUSE_WRITES: &str = "PRAGMA query_only = ON;";

        #[test]
        fn only_an_absent_id_is_inserted_and_a_known_row_is_never_changed() {
            let known_row = Some(("web-01".to_string(), 1));
            let cases = [
                ("a known id", "", "known", true, known_row.clone()),
                ("a new id", "", "new", true, Some(("new".to_string(), 0))),
                (
                    "a known id while inserts fail",
                    REFUSE_INSERTS,
                    "known",
                    true,
                    known_row.clone(),
                ),
                (
                    "a new id while inserts fail",
                    REFUSE_INSERTS,
                    "new",
                    false,
                    None,
                ),
                (
                    "a known id while writes are refused",
                    REFUSE_WRITES,
                    "known",
                    true,
                    known_row.clone(),
                ),
                (
                    "a new id while writes are refused",
                    REFUSE_WRITES,
                    "new",
                    false,
                    None,
                ),
            ];
            for (case, setup, id, succeeds, expected_row) in cases {
                let (db, _dir) = db_with_unmappable_known_system();
                db.conn.lock().unwrap().execute_batch(setup).unwrap();

                let outcome = db.insert_system_if_absent(&sample_system(id, id));

                assert_eq!(outcome.is_ok(), succeeds, "case: {case}: {outcome:?}");
                assert_eq!(raw_row(&db, id), expected_row, "case: {case}");
                assert_eq!(raw_row(&db, "known"), known_row, "case: {case}: known row");
            }
        }

        /// RFC 0016 §2: what a registration found, compared by SQLite so a stored `url` of any
        /// type maps (RFC 0007 §4's rule), whatever the row's other columns hold.
        #[test]
        fn a_registration_says_whether_the_known_row_has_its_url() {
            let cases = [
                ("an absent id", "new", "push://", Registration::Inserted),
                (
                    "a known id, the same url",
                    "known",
                    "http://example.com",
                    Registration::Known { same_url: true },
                ),
                (
                    "a known id, another url",
                    "known",
                    "push://",
                    Registration::Known { same_url: false },
                ),
                (
                    "a known id stored with a blob url",
                    "blob",
                    "push://",
                    Registration::Known { same_url: false },
                ),
            ];
            for (case, id, url, expected) in cases {
                let (db, _dir) = db_with_unmappable_known_system();
                let mut blob = sample_system("blob", "blob");
                blob.url = "push://".to_string();
                db.insert_system(&blob).unwrap();
                db.conn
                    .lock()
                    .unwrap()
                    .execute_batch(
                        "UPDATE systems SET url = CAST('push://' AS BLOB) WHERE id = 'blob';",
                    )
                    .unwrap();
                let mut sys = sample_system(id, id);
                sys.url = url.to_string();

                let outcome = db.insert_system_if_absent(&sys);

                assert_eq!(outcome.ok(), Some(expected), "case: {case}");
            }
        }

        #[test]
        fn a_known_id_runs_no_insert_at_all() {
            let (db, _dir) = db_with_unmappable_known_system();
            db.conn
                .lock()
                .unwrap()
                .execute_batch(
                    "CREATE TABLE probe (id TEXT);
                     CREATE TRIGGER record BEFORE INSERT ON systems
                     BEGIN INSERT INTO probe VALUES (NEW.id); END;",
                )
                .unwrap();

            db.insert_system_if_absent(&sample_system("known", "known"))
                .unwrap();

            let attempts: i64 = db
                .conn
                .lock()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM probe", [], |row| row.get(0))
                .unwrap();
            assert_eq!(attempts, 0, "no INSERT was tried for a known id");
        }
    }
}
