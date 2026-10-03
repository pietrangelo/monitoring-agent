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

//! The overdue sweep (RFC 0017 §7): every 60 s, each enabled mail system whose reports are
//! overdue is marked offline and its live metrics evicted. Blocking: runs on the blocking pool.

use super::receipt::{MailPresence, mail_status};
use crate::db::MailPresenceRow;
use crate::models::SystemStatus;
use crate::snapshot::SnapshotTime;
use crate::state::AppState;

/// The `last_error` an overdue marking writes.
pub const OVERDUE: &str = "mail overdue";

/// One pass at `now`: how many systems it marked offline.
pub fn overdue_pass(app: &AppState, now: SnapshotTime) -> Result<usize, rusqlite::Error> {
    let mut marked = 0;
    for row in app.db.mail_presence()? {
        let seen = row.newest.map(|(at, _)| at);
        if is_overdue(&row, now) && app.db.mark_mail_overdue(&row.id, seen, OVERDUE)? {
            // The evicted entry is dropped here, after the live lock is released.
            drop(app.evict_live_metrics(&row.id));
            marked += 1;
        }
    }
    Ok(marked)
}

/// Whether the sweep marks `row` offline: enabled, not offline yet, and overdue.
fn is_overdue(row: &MailPresenceRow, now: SnapshotTime) -> bool {
    let swept = match row.status {
        SystemStatus::Offline => false,
        SystemStatus::Online | SystemStatus::Unknown => row.enabled,
    };
    swept && mail_status(row.newest, now) == MailPresence::Overdue
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail_intake::ingest::ingest_message;
    use crate::mail_intake::ingest::tests::{NOW, master, valid_mail};
    use crate::mail_intake::report::tests::CREATED;
    use std::sync::Arc;

    fn app() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Arc::new(crate::db::Database::new(path.to_str().unwrap()).unwrap());
        (AppState::new(db).unwrap(), dir)
    }

    fn at(seconds: u64) -> SnapshotTime {
        SnapshotTime::try_from(seconds).unwrap()
    }

    fn status(app: &AppState, id: &str) -> (SystemStatus, Option<String>) {
        let sys = app.db.get_system(id).unwrap().unwrap();
        (sys.status, sys.last_error)
    }

    /// RFC 0017 §7: a mail system is marked offline once `3 × interval + 15 min` have passed
    /// since its newest report, and its live metrics evicted; a disabled one is skipped, and a
    /// pushed one is never touched.
    #[test]
    fn an_overdue_mail_system_is_marked_offline() {
        let limit = 3 * 300 + 900;
        let cases = [
            ("on time", CREATED + limit, true, 0, SystemStatus::Online),
            (
                "overdue",
                CREATED + limit + 1,
                true,
                1,
                SystemStatus::Offline,
            ),
            (
                "overdue but disabled",
                CREATED + limit + 1,
                false,
                0,
                SystemStatus::Online,
            ),
        ];
        for (name, now, enabled, marked, expected) in cases {
            let (app, _dir) = app();
            assert!(ingest_message(&app, &master(), &valid_mail("web-01", 1), NOW).is_ok());
            if !enabled {
                app.db
                    .update_system_config("web-01", None, None, None, None, Some(false))
                    .unwrap();
            }
            let mut pushed = app.db.get_system("web-01").unwrap().unwrap();
            pushed.id = "pushed".into();
            pushed.url = "push://".into();
            pushed.status = SystemStatus::Online;
            app.db.insert_system(&pushed).unwrap();

            assert_eq!(overdue_pass(&app, at(now)).unwrap(), marked, "case {name}");

            assert_eq!(status(&app, "web-01").0, expected, "case {name}");
            let last_seen = app.db.get_system("web-01").unwrap().unwrap().last_seen;
            assert_eq!(
                last_seen,
                crate::clock::unix_to_iso8601(CREATED),
                "case {name}: last seen is the newest report's, overdue or not"
            );
            assert_eq!(
                status(&app, "pushed").0,
                SystemStatus::Online,
                "case {name}"
            );
            let live = app.live_metrics.read().unwrap().contains_key("web-01");
            assert_eq!(
                live,
                expected == SystemStatus::Online,
                "case {name}: live metrics"
            );
        }
    }

    /// RFC 0017 §7: a mail row with no current receipt (its receipts retired by a delete, then
    /// the row re-created by hand) is overdue; one already offline isn't written again.
    #[test]
    fn a_mail_row_with_no_receipt_is_overdue_once() {
        let (app, _dir) = app();
        let mut row = app_row("orphan");
        row.status = SystemStatus::Online;
        app.db.insert_system(&row).unwrap();

        assert_eq!(overdue_pass(&app, at(NOW)).unwrap(), 1);
        assert_eq!(
            status(&app, "orphan"),
            (SystemStatus::Offline, Some(OVERDUE.into()))
        );
        assert_eq!(overdue_pass(&app, at(NOW)).unwrap(), 0, "already offline");
    }

    /// RFC 0017 §7: a report stored after the sweep read a system wins: the marking writes
    /// only while the newest receipt is the one the sweep saw.
    #[test]
    fn a_report_stored_since_the_read_isnt_marked_overdue() {
        let (app, _dir) = app();
        assert!(ingest_message(&app, &master(), &valid_mail("web-01", 1), NOW).is_ok());
        let seen = app.db.mail_presence().unwrap()[0].newest.map(|(at, _)| at);
        let newer = crate::mail_intake::ingest::tests::mail_at("web-01", 2, CREATED + 60);
        assert!(ingest_message(&app, &master(), &newer, NOW).is_ok());

        let stale = app.db.mark_mail_overdue("web-01", seen, OVERDUE).unwrap();
        let current = app.db.mail_presence().unwrap()[0].newest.map(|(at, _)| at);
        let fresh = app
            .db
            .mark_mail_overdue("web-01", current, OVERDUE)
            .unwrap();

        assert!(!stale, "the stale read writes nothing");
        assert!(fresh, "the current read marks it");
        let sys = app.db.get_system("web-01").unwrap().unwrap();
        assert_eq!(
            (sys.status, sys.last_seen),
            (
                SystemStatus::Offline,
                crate::clock::unix_to_iso8601(CREATED + 60)
            )
        );
    }

    fn app_row(id: &str) -> crate::models::SystemInfo {
        crate::models::SystemInfo {
            id: id.into(),
            name: id.into(),
            url: "mail://".into(),
            token: String::new(),
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
}
