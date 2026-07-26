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

use rusqlite::Connection;
use std::sync::Mutex;

use crate::models::*;

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
            rusqlite::params![
                sys.id,
                sys.name,
                sys.url,
                sys.token,
                sys.status.to_string(),
                sys.last_seen,
                sys.last_error,
                sys.os,
                sys.hostname,
                sys.kernel,
                sys.cpu_model,
                sys.cpu_cores,
                sys.total_memory_display,
                sys.total_memory_bytes,
                sys.poll_interval_secs,
                sys.enabled,
            ],
        )?;
        Ok(())
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

    #[allow(clippy::too_many_arguments)]
    pub fn update_system_info(
        &self,
        id: &str,
        os: Option<&str>,
        hostname: Option<&str>,
        kernel: Option<&str>,
        cpu_model: Option<&str>,
        cpu_cores: Option<usize>,
        total_memory_display: Option<&str>,
        total_memory_bytes: Option<u64>,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE systems SET os=?1, hostname=?2, kernel=?3, cpu_model=?4,
             cpu_cores=?5, total_memory_display=?6, total_memory_bytes=?7
             WHERE id=?8",
            rusqlite::params![
                os,
                hostname,
                kernel,
                cpu_model,
                cpu_cores,
                total_memory_display,
                total_memory_bytes,
                id
            ],
        )?;
        Ok(())
    }

    pub fn update_system_config(
        &self,
        id: &str,
        name: Option<&str>,
        url: Option<&str>,
        token: Option<&str>,
        poll_interval_secs: Option<u64>,
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
            params.push(Box::new(v as i64));
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

    pub fn delete_system(&self, id: &str) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM metrics WHERE system_id = ?1", [id])?;
        conn.execute("DELETE FROM alerts WHERE system_id = ?1", [id])?;
        conn.execute("DELETE FROM metric_retention WHERE system_id = ?1", [id])?;
        conn.execute("DELETE FROM systems WHERE id = ?1", [id])?;
        Ok(())
    }

    // ── Metrics ────────────────────────────────────────

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

    pub fn get_metrics(
        &self,
        system_id: &str,
        metric: &str,
        limit: usize,
        since_secs: Option<u64>,
    ) -> Result<Vec<MetricPoint>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let sql = if let Some(since) = since_secs {
            format!(
                "SELECT timestamp, value FROM metrics
                 WHERE system_id = ?1 AND metric = ?2 AND timestamp >= {}
                 ORDER BY timestamp ASC LIMIT ?3",
                since
            )
        } else {
            "SELECT timestamp, value FROM metrics
             WHERE system_id = ?1 AND metric = ?2
             ORDER BY timestamp DESC LIMIT ?3"
                .to_string()
        };

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params![system_id, metric, limit as i64], |row| {
            Ok(MetricPoint {
                timestamp: row.get(0)?,
                value: row.get(1)?,
            })
        })?;

        let mut points: Vec<MetricPoint> = rows.filter_map(|r| r.ok()).collect();
        if since_secs.is_none() {
            points.reverse();
        }
        Ok(points)
    }

    // ── Alerts ─────────────────────────────────────────

    pub fn insert_alert(&self, alert: &AlertRecord) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO alerts (id, system_id, system_name, severity, message,
             current_value, fired_at, stored_at, acknowledged)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            rusqlite::params![
                alert.id,
                alert.system_id,
                alert.system_name,
                alert.severity,
                alert.message,
                alert.current_value,
                alert.fired_at,
                alert.stored_at,
                alert.acknowledged,
            ],
        )?;
        Ok(())
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

    fn temp_db() -> (Database, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Database::new(path.to_str().unwrap()).unwrap();
        (db, dir)
    }

    fn sample_system(id: &str, name: &str) -> SystemInfo {
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
            Some("16.0 GB"),
            Some(16_000_000_000),
        )
        .unwrap();
        let sys = db.get_system("id-1").unwrap().unwrap();
        assert_eq!(sys.os.as_deref(), Some("Ubuntu 22.04"));
        assert_eq!(sys.hostname.as_deref(), Some("web01"));
        assert_eq!(sys.cpu_cores, Some(8));
        assert_eq!(sys.total_memory_bytes, Some(16_000_000_000));
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
            Some(30),
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
        db.insert_metric("id-1", "cpu", 50.0, 100).unwrap();
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
}
