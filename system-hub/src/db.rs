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
        Ok(rows.next().transpose()?)
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
