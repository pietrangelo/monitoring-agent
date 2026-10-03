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

//! The disconnection sweep (RFC 0016 §4): every 30 s, one unit on the blocking pool marks
//! offline each push system that `PushPresence::sweep` says has no live current connection.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::state::AppState;

/// How often the sweep runs.
const SWEEP_PERIOD: Duration = Duration::from_secs(30);

/// What one pass did: systems marked offline, writes that failed, rows it couldn't read.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct SweepPass {
    pub(super) marked: usize,
    pub(super) failed: usize,
    pub(super) skipped: usize,
}

/// Starts the sweep for the hub's lifetime; `started` is when the hub started, on the
/// monotonic clock, so a pass knows how long the hub has been up.
pub fn start_disconnection_sweep(app: Arc<AppState>, started: Instant) {
    let _ = (app, started, SWEEP_PERIOD);
}

/// One pass, the hub up for `up_for`: a blocking unit.
pub(super) fn sweep_pass(_app: &AppState, _up_for: Duration) -> Result<SweepPass, rusqlite::Error> {
    Ok(SweepPass::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::models::{SystemId, SystemInfo, SystemStatus};

    fn app() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Arc::new(Database::new(path.to_str().unwrap()).unwrap());
        (AppState::new(db).unwrap(), dir)
    }

    fn row(id: &str, url: &str, status: SystemStatus, enabled: bool) -> SystemInfo {
        SystemInfo {
            id: id.to_string(),
            name: id.to_string(),
            url: url.to_string(),
            token: String::new(),
            status,
            last_seen: "1h".to_string(),
            last_error: Some("earlier".to_string()),
            os: None,
            hostname: None,
            kernel: None,
            cpu_model: None,
            cpu_cores: None,
            total_memory_display: None,
            total_memory_bytes: None,
            poll_interval_secs: 10,
            enabled,
        }
    }

    /// RFC 0016 §4: one pass over a registry, at the grace and just inside it.
    #[test]
    fn a_pass_marks_offline_each_disconnected_push_system_past_the_grace() {
        use SystemStatus::{Offline, Online, Unknown};
        let marked = |reason: &str| (Offline, String::new(), Some(reason.to_string()));
        let kept = |status| (status, "1h".to_string(), Some("earlier".to_string()));
        // (id, url, stored status, enabled, has a live claimed connection, after the pass)
        let rows = [
            (
                "gone",
                "push://",
                Online,
                true,
                false,
                marked("push disconnected"),
            ),
            (
                "gone-unknown",
                "push://",
                Unknown,
                true,
                false,
                marked("push disconnected"),
            ),
            ("connected", "push://", Online, true, true, kept(Online)),
            (
                "disabled",
                "push://",
                Online,
                false,
                false,
                marked("push disconnected"),
            ),
            (
                "..",
                "push://",
                Online,
                true,
                false,
                marked("invalid system id"),
            ),
            ("polled", "http://a:9090", Online, true, false, kept(Online)),
            (
                "was-offline",
                "push://",
                Offline,
                true,
                false,
                kept(Offline),
            ),
        ];
        let cases = [
            ("at the grace", Duration::from_secs(120), true, 4),
            ("inside the grace", Duration::from_secs(119), false, 0),
        ];
        for (case, up_for, past_grace, expected_marked) in cases {
            let (app, _dir) = app();
            let mut leases = Vec::new();
            for (id, url, status, enabled, connected, _) in &rows {
                app.db
                    .insert_system(&row(id, url, status.clone(), *enabled))
                    .unwrap();
                if *connected {
                    let id = SystemId::try_from(id.to_string()).unwrap();
                    let mut presence = app.presence();
                    let lease = presence.accept(&id);
                    presence.claim(&id, &lease.handle());
                    leases.push(lease);
                }
            }
            // A row `list_systems` can't map doesn't stop the pass.
            app.db
                .insert_system(&row("odd", "http://b", Online, true))
                .unwrap();
            app.db.set_poll_interval_for_test("odd", -1);

            let pass = sweep_pass(&app, up_for).unwrap();

            assert_eq!(pass.marked, expected_marked, "case {case}");
            for (id, _, status, _, _, after) in &rows {
                let sys = app.db.get_system(id).unwrap().unwrap();
                let got = (sys.status, sys.last_seen, sys.last_error);
                let expected = if past_grace {
                    after.clone()
                } else {
                    kept(status.clone())
                };
                assert_eq!(got, expected, "case {case}: row {id}");
            }
            drop(leases);
        }
    }
}
