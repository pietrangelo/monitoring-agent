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

//! Storing one snapshot, whichever path it came from (RFC 0007 §1, §2, §4): the push receiver
//! and the poller. The snapshot rule, then one transaction under the database mutex; the live
//! lock is taken only briefly inside it (lock order: database, then live state). Blocking:
//! runs on the blocking pool.

use std::sync::{Arc, PoisonError};
use std::time::Instant;

use crate::db::SnapshotStored;
use crate::models::SystemId;
use crate::registry::{LastSeen, StatusUpdate};
use crate::snapshot::{
    LeftOut, LeftOutLog, LiveMetrics, ReportedSnapshot, Snapshot, SnapshotTime, snapshot_rule,
};
use crate::state::AppState;

/// Stores what an agent reported through the snapshot rule, marks the system online, seen as
/// `last_seen` says, and moves the kept snapshot into the system's live metrics. Logs what the
/// rule left out, and returns how it was logged.
pub fn store_snapshot(
    app: &AppState,
    system_id: &SystemId,
    reported: ReportedSnapshot,
    time: SnapshotTime,
    last_seen: LastSeen,
    now: Instant,
) -> Result<SnapshotStored<LeftOutLog>, rusqlite::Error> {
    let (snapshot, left_out) = snapshot_rule(reported);
    let status = StatusUpdate::after_snapshot(last_seen);
    let stored = app
        .db
        .store_snapshot(system_id, snapshot, time, status, |snapshot| {
            keep_live_metrics(app, system_id, snapshot, time, &left_out, now)
        })?;
    Ok(match stored {
        SnapshotStored::Stored((log, replaced)) => {
            // Both locks are released: the replaced entry is dropped outside them.
            drop(replaced);
            log_left_out(system_id, log, &left_out);
            SnapshotStored::Stored(log)
        }
        SnapshotStored::SystemGone => SnapshotStored::SystemGone,
    })
}

/// Moves a stored snapshot into the system's live metrics, carrying the warning time over from
/// the entry it replaces. Returns how the left-out values are logged, and the replaced entry,
/// for the caller to drop once both locks are released.
fn keep_live_metrics(
    app: &AppState,
    system_id: &SystemId,
    snapshot: Snapshot,
    time: SnapshotTime,
    left_out: &LeftOut,
    now: Instant,
) -> (LeftOutLog, Option<Arc<LiveMetrics>>) {
    let mut live = app
        .live_metrics
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    let previous = live.get(system_id.as_str()).map(Arc::as_ref);
    let (entry, log) = LiveMetrics::following(previous, snapshot, time, left_out, now);
    (
        log,
        live.insert(system_id.as_str().to_owned(), Arc::new(entry)),
    )
}

/// Logs what the snapshot rule left out, by reason, with the id in `Debug` form and never a
/// mount point.
fn log_left_out(system_id: &SystemId, log: LeftOutLog, left_out: &LeftOut) {
    let id = system_id.as_str();
    match log {
        LeftOutLog::Nothing => {}
        LeftOutLog::Warn => {
            tracing::warn!("The snapshot rule left values out for {id:?}: {left_out:?}")
        }
        LeftOutLog::Debug => {
            tracing::debug!("The snapshot rule left values out for {id:?}: {left_out:?}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::models::{SystemInfo, SystemStatus};
    use crate::registry::UptimeDisplay;
    use crate::snapshot::{ReportedDisk, Scalar, snapshot_rule};
    use std::time::Duration;

    fn app_with_system(id: &str) -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Arc::new(Database::new(path.to_str().unwrap()).unwrap());
        db.insert_system(&SystemInfo {
            id: id.to_string(),
            name: id.to_string(),
            url: "push://".to_string(),
            token: String::new(),
            status: SystemStatus::Offline,
            last_seen: "earlier".to_string(),
            last_error: Some("push disconnected".to_string()),
            os: None,
            hostname: None,
            kernel: None,
            cpu_model: None,
            cpu_cores: None,
            total_memory_display: None,
            total_memory_bytes: None,
            poll_interval_secs: 10,
            enabled: true,
        })
        .unwrap();
        (AppState::new(db), dir)
    }

    fn id(id: &str) -> SystemId {
        SystemId::try_from(id.to_string()).unwrap()
    }

    fn at(secs: u64) -> SnapshotTime {
        SnapshotTime::try_from(secs).unwrap()
    }

    fn uptime(display: &str) -> LastSeen {
        LastSeen::Uptime(UptimeDisplay::try_from(display.to_string()).unwrap())
    }

    /// A snapshot with `cpu` as given, the other scalars and one disk reported and finite.
    fn reported(cpu: f32) -> ReportedSnapshot {
        ReportedSnapshot {
            cpu: Some(cpu),
            memory: Some(20.0),
            swap: Some(30.0),
            load1: Some(1.5),
            load5: Some(2.5),
            disks: vec![ReportedDisk {
                mount_point: Some("/".to_string()),
                usage_percent: Some(50.0),
            }],
        }
    }

    fn live(app: &AppState, system_id: &str) -> Option<Arc<LiveMetrics>> {
        app.live_metrics.read().unwrap().get(system_id).cloned()
    }

    #[test]
    fn a_stored_snapshot_is_its_kept_values_in_history_status_and_live_metrics() {
        let (app, _dir) = app_with_system("sys-1");

        let stored = store_snapshot(
            &app,
            &id("sys-1"),
            reported(f32::NAN),
            at(1_000),
            uptime("3d 4h 5m"),
            Instant::now(),
        );

        assert_eq!(stored.unwrap(), SnapshotStored::Stored(LeftOutLog::Warn));
        let history: Vec<(String, usize)> = ["cpu", "memory", "swap", "load1", "load5", "disk:/"]
            .into_iter()
            .map(|metric| {
                let points = app.db.get_metrics("sys-1", metric, 10, None).unwrap();
                (metric.to_string(), points.len())
            })
            .collect();
        let expected: Vec<(String, usize)> = [
            ("cpu", 0),
            ("memory", 1),
            ("swap", 1),
            ("load1", 1),
            ("load5", 1),
            ("disk:/", 1),
        ]
        .map(|(metric, count)| (metric.to_string(), count))
        .to_vec();
        assert_eq!(history, expected, "the NaN cpu is left out of history");
        let system = app.db.get_system("sys-1").unwrap().unwrap();
        let status = (system.status, system.last_seen.as_str(), system.last_error);
        assert_eq!(status, (SystemStatus::Online, "3d 4h 5m", None));
        let entry = live(&app, "sys-1").expect("live metrics written");
        assert_eq!(entry.snapshot(), &snapshot_rule(reported(f32::NAN)).0);
        assert_eq!(entry.snapshot().scalar(Scalar::Cpu), None);
        assert_eq!(entry.time(), at(1_000));
    }

    /// RFC 0007 §4: every entry takes its warning time from `left_out_log`, so a host leaving
    /// something out of every snapshot warns once an hour, not every snapshot.
    #[test]
    fn the_warning_time_carries_over_from_entry_to_entry() {
        let (app, _dir) = app_with_system("sys-1");
        let start = Instant::now();
        let steps = [
            ("left out, first", f32::NAN, 0, LeftOutLog::Warn),
            ("left out, 1 s later", f32::NAN, 1, LeftOutLog::Debug),
            ("nothing left out", 10.0, 2, LeftOutLog::Nothing),
            (
                "left out, 59 min 59 s after the warning",
                f32::NAN,
                3_599,
                LeftOutLog::Debug,
            ),
        ];
        for (step, cpu, after_secs, expected_log) in steps {
            let now = start + Duration::from_secs(after_secs);
            let stored = store_snapshot(
                &app,
                &id("sys-1"),
                reported(cpu),
                at(1_000 + after_secs),
                uptime("1m"),
                now,
            );
            assert_eq!(
                stored.unwrap(),
                SnapshotStored::Stored(expected_log),
                "step: {step}"
            );
            let warned_at = live(&app, "sys-1").unwrap().left_out_warned_at();
            assert_eq!(
                warned_at,
                Some(start),
                "step: {step}: the first warning's time"
            );
        }
    }

    #[test]
    fn a_gone_system_gets_nothing_stored_and_no_live_metrics() {
        let (app, _dir) = app_with_system("sys-1");

        let stored = store_snapshot(
            &app,
            &id("sys-gone"),
            reported(10.0),
            at(1_000),
            uptime("1m"),
            Instant::now(),
        );

        assert_eq!(stored.unwrap(), SnapshotStored::SystemGone);
        assert!(live(&app, "sys-gone").is_none());
    }

    /// RFC 0007 §4: the replaced entry leaves the live lock as a return value, so it is dropped
    /// after both locks are released.
    #[test]
    fn keeping_live_metrics_hands_back_the_entry_it_replaced() {
        let (app, _dir) = app_with_system("sys-1");
        let (first, _) = snapshot_rule(reported(10.0));
        let (second, _) = snapshot_rule(reported(11.0));
        let now = Instant::now();

        let (_, none) =
            keep_live_metrics(&app, &id("sys-1"), first, at(1), &LeftOut::default(), now);
        let before = live(&app, "sys-1");
        assert!(before.is_some(), "the first entry is written");
        let before = before.unwrap();
        let (_, replaced) = keep_live_metrics(
            &app,
            &id("sys-1"),
            second.clone(),
            at(2),
            &LeftOut::default(),
            now,
        );

        assert!(none.is_none(), "nothing replaced at first");
        assert!(
            Arc::ptr_eq(&replaced.unwrap(), &before),
            "the first entry comes back"
        );
        let after = live(&app, "sys-1").unwrap();
        assert_eq!((after.snapshot(), after.time()), (&second, at(2)));
    }
}
