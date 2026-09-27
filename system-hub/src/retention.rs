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

//! The periodic task that prunes `app:*` points (RFC 0009 §8), an interim until RFC 0010's
//! store takes retention over. Snapshot metrics keep their per-insert pruning.

use std::sync::Arc;
use std::time::Duration;

use crate::applications::APPLICATION_RETENTION_SECS;
use crate::clock::unix_now;
use crate::state::AppState;

/// How often every system's `app:*` points are pruned.
const PRUNE_EVERY: Duration = Duration::from_secs(600);

/// Prunes every registered system's `app:*` points past their retention at `now` (the hub's
/// clock). Blocking: runs on the blocking pool. Returns how many points went.
pub fn prune_all(app: &AppState, now: u64) -> usize {
    let systems = match app.db.list_systems() {
        Ok(systems) => systems,
        Err(err) => {
            tracing::warn!("Couldn't list systems to prune application points: {err}");
            return 0;
        }
    };
    systems
        .iter()
        .map(|sys| {
            app.db
                .prune_application_points(&sys.id, now, APPLICATION_RETENTION_SECS)
                .unwrap_or_else(|err| {
                    tracing::warn!("Pruning application points of {:?} failed: {err}", sys.id);
                    0
                })
        })
        .sum()
}

/// Starts the pruning task for the hub's lifetime.
pub fn start(app: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(PRUNE_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let app = Arc::clone(&app);
            let pass = tokio::task::spawn_blocking(move || prune_all(&app, unix_now())).await;
            match pass {
                Ok(pruned) => tracing::debug!("Pruned {pruned} application point(s)"),
                Err(err) => tracing::error!("Application point pruning failed: {err}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::models::{SystemInfo, SystemStatus};

    fn system(id: &str) -> SystemInfo {
        SystemInfo {
            id: id.into(),
            name: id.into(),
            url: "push://".into(),
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

    #[test]
    fn every_systems_application_points_are_pruned_past_a_day() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::new(dir.path().join("t.db").to_str().unwrap()).unwrap());
        let now = 10 * 86_400;
        for id in ["a", "b", "disabled"] {
            let mut sys = system(id);
            sys.enabled = id != "disabled";
            db.insert_system(&sys).unwrap();
            db.insert_metric(id, "cpu", 1.0, now).unwrap();
        }
        let app = AppState::new(db);
        for id in ["a", "b", "disabled"] {
            for (metric, ts) in [
                ("app:x:up", now - 86_401),
                ("app:x:up", now - 86_400),
                ("app:x:up", now - 86_399),
                ("app:x:up", now - 10),
            ] {
                app.db
                    .store_round(
                        id,
                        [(metric.to_string(), 1.0)],
                        ts,
                        || crate::applications::Admission::Accept {
                            pace: crate::applications::SourcePace::new(std::time::Instant::now()),
                        },
                        |_| {},
                    )
                    .unwrap();
            }
        }

        assert_eq!(
            prune_all(&app, now),
            3,
            "one old point per system, disabled ones too"
        );
        for id in ["a", "b", "disabled"] {
            let left = app.db.get_metrics(id, "app:x:up", 10, None).unwrap();
            assert_eq!(
                left.iter().map(|p| p.timestamp).collect::<Vec<_>>(),
                [now - 86_400, now - 86_399, now - 10],
                "{id}: kept within 24 h"
            );
        }
    }
}
