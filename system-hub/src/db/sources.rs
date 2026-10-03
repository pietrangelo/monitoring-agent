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

//! The registry reads and writes of RFC 0016 §4: the sweep's narrow read of every system's
//! source and status, and the startup reset of push systems' status.

use super::{Database, parse_status};
use crate::models::SystemStatus;
use crate::registry::{PUSH_URL, SystemSource};

/// One system as the disconnection sweep reads it: its source decided at this edge, from
/// whether its `url` is the push sentinel (any stored type maps; only text can equal it).
#[derive(Debug, Clone, PartialEq)]
pub struct SourceRow {
    pub id: String,
    pub source: SystemSource,
    pub status: SystemStatus,
}

/// Every system the sweep could read, and how many rows it skipped (an id that isn't text).
#[derive(Debug, Default, PartialEq)]
pub struct SourceRows {
    pub rows: Vec<SourceRow>,
    pub skipped: usize,
}

impl Database {
    /// Every system's id, source and status. Each row maps on its own, so no other column, and no
    /// other row, can fail the read; it reads no token.
    pub fn system_sources(&self) -> Result<SourceRows, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, url IS ?1, status FROM systems")?;
        let mut read = SourceRows::default();
        let rows = stmt.query_map([PUSH_URL], |row| {
            Ok(source_row(row.get(0), row.get(1), row.get(2)))
        })?;
        for row in rows {
            match row? {
                Some(row) => read.rows.push(row),
                None => read.skipped += 1,
            }
        }
        Ok(read)
    }

    /// Sets `to` on the systems whose url is `url` and whose status is `from`, keeping their
    /// last seen and last error. Returns how many rows changed.
    pub fn reset_status(
        &self,
        url: &str,
        from: &SystemStatus,
        to: &SystemStatus,
    ) -> Result<usize, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE systems SET status = ?3 WHERE url = ?1 AND status = ?2",
            (url, from.to_string(), to.to_string()),
        )
    }
}

/// One row, or `None` when its id isn't text: a row the sweep skips.
fn source_row(
    id: rusqlite::Result<String>,
    is_push: rusqlite::Result<bool>,
    status: rusqlite::Result<String>,
) -> Option<SourceRow> {
    let source = match is_push {
        Ok(true) => SystemSource::Push,
        Ok(false) | Err(_) => SystemSource::Poll,
    };
    Some(SourceRow {
        id: id.ok()?,
        source,
        status: status.map_or(SystemStatus::Unknown, |status| parse_status(&status)),
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{sample_system, temp_db};
    use super::*;
    use crate::models::SystemInfo;

    fn system(id: &str, url: &str, status: SystemStatus, last_error: Option<&str>) -> SystemInfo {
        SystemInfo {
            url: url.to_string(),
            status,
            last_seen: "3h 2m".to_string(),
            last_error: last_error.map(str::to_owned),
            ..sample_system(id, id)
        }
    }

    /// RFC 0016 §4: the startup reset turns only `Online` push systems `Unknown`, and keeps
    /// what they showed last.
    #[test]
    fn the_reset_turns_only_online_push_systems_unknown() {
        let (db, _dir) = temp_db();
        let rows = [
            (
                "push-online",
                "push://",
                SystemStatus::Online,
                SystemStatus::Unknown,
            ),
            (
                "push-offline",
                "push://",
                SystemStatus::Offline,
                SystemStatus::Offline,
            ),
            (
                "push-unknown",
                "push://",
                SystemStatus::Unknown,
                SystemStatus::Unknown,
            ),
            (
                "polled-online",
                "http://a:9090",
                SystemStatus::Online,
                SystemStatus::Online,
            ),
            (
                "almost-push",
                "push:",
                SystemStatus::Online,
                SystemStatus::Online,
            ),
        ];
        for (id, url, status, _) in &rows {
            db.insert_system(&system(id, url, status.clone(), Some("earlier")))
                .unwrap();
        }

        let changed = db
            .reset_status("push://", &SystemStatus::Online, &SystemStatus::Unknown)
            .unwrap();

        assert_eq!(changed, 1, "one row changed");
        for (id, _, _, expected) in rows {
            let sys = db.get_system(id).unwrap().unwrap();
            assert_eq!(sys.status, expected, "row {id}");
            assert_eq!(sys.last_seen, "3h 2m", "row {id}: last seen kept");
            assert_eq!(
                sys.last_error.as_deref(),
                Some("earlier"),
                "row {id}: last error kept"
            );
        }
    }

    /// RFC 0016 §4: the sweep's read maps each row on its own: a row `list_systems` can't map
    /// is still read, an id that isn't text is skipped and counted, and a status the hub
    /// doesn't know reads `Unknown`.
    #[test]
    fn the_source_read_maps_each_row_on_its_own() {
        let (db, _dir) = temp_db();
        db.insert_system(&system("a-push", "push://", SystemStatus::Online, None))
            .unwrap();
        db.insert_system(&system("b-polled", "http://b", SystemStatus::Offline, None))
            .unwrap();
        db.insert_system(&system("c-odd", "push://", SystemStatus::Online, None))
            .unwrap();
        db.insert_system(&system("d-blob-url", "push://", SystemStatus::Online, None))
            .unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute_batch(
                "UPDATE systems SET poll_interval_secs = -1 WHERE id = 'b-polled';
                 UPDATE systems SET status = 'sleeping' WHERE id = 'c-odd';
                 UPDATE systems SET url = CAST('push://' AS BLOB) WHERE id = 'd-blob-url';
                 INSERT INTO systems (id, name, url, token, status, last_seen,
                                      poll_interval_secs, enabled)
                     VALUES (NULL, 'z', 'push://', '', 'online', '', 10, 1),
                            (CAST('blob' AS BLOB), 'z', 'push://', '', 'online', '', 10, 1);",
            )
            .unwrap();
        assert!(
            db.list_systems().is_err(),
            "list_systems can't map these rows"
        );

        let mut read = db.system_sources().unwrap();
        read.rows.sort_by(|a, b| a.id.cmp(&b.id));

        let row = |id: &str, url: &str, status| SourceRow {
            id: id.to_string(),
            source: SystemSource::of(url),
            status,
        };
        assert_eq!(
            read,
            SourceRows {
                rows: vec![
                    row("a-push", "push://", SystemStatus::Online),
                    row("b-polled", "http://b", SystemStatus::Offline),
                    row("c-odd", "push://", SystemStatus::Unknown),
                    row("d-blob-url", "a blob is no sentinel", SystemStatus::Online),
                ],
                skipped: 2,
            }
        );
    }
}
