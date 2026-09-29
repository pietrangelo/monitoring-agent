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

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Instant;

use crate::applications::{HeldRound, RecentRounds, RoundDigester, SourcePace};
use crate::db::Database;
use crate::snapshot::LiveMetrics;

/// What the hub holds in memory of one system's applications (RFC 0009 §8).
#[derive(Debug, Default)]
pub struct SystemApplications {
    /// The round the dashboard shows. A push disconnect keeps it; it goes stale on its own.
    pub shown: Option<HeldRound>,
    /// The rounds accepted most recently, surviving disconnects, so a re-send is recognised.
    pub recent: RecentRounds,
    /// The poller's pace for this system; `None` until its first stored round (a full bucket).
    pub poll_pace: Option<SourcePace>,
    /// When a refused polled round was last logged at `warn`.
    pub poll_refusal_warned_at: Option<Instant>,
}

pub struct AppState {
    pub db: Arc<Database>,
    /// Per-system live metrics, written with each stored snapshot, keyed by system id, and
    /// evicted when a push connection ends or the system is deleted. Lock order: the database
    /// mutex, then this; never take the database mutex while holding it.
    pub live_metrics: RwLock<HashMap<String, Arc<LiveMetrics>>>,
    /// Per-system applications, keyed by system id. Lock order: the database mutex, then
    /// this; never take the database mutex while holding it.
    pub live_applications: RwLock<HashMap<String, SystemApplications>>,
    /// The one digest key every ingestion path shares.
    pub digester: RoundDigester,
}

impl AppState {
    pub fn new(db: Arc<Database>) -> Arc<Self> {
        Arc::new(Self {
            db,
            live_metrics: RwLock::new(HashMap::new()),
            live_applications: RwLock::new(HashMap::new()),
            digester: RoundDigester::new(),
        })
    }

    /// Removes a system's live metrics (RFC 0007 §4) and hands the entry back, so the caller
    /// drops it after the live lock is released.
    pub fn evict_live_metrics(&self, system_id: &str) -> Option<Arc<LiveMetrics>> {
        self.live_metrics
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(system_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{ReportedSnapshot, SnapshotTime, snapshot_rule};

    fn app() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Arc::new(Database::new(path.to_str().unwrap()).unwrap());
        (AppState::new(db), dir)
    }

    fn plant(app: &AppState, system_id: &str) -> Arc<LiveMetrics> {
        let (snapshot, left_out) = snapshot_rule(ReportedSnapshot::default());
        let time = SnapshotTime::try_from(1_700_000_000).unwrap();
        let (entry, _) = LiveMetrics::following(None, snapshot, time, &left_out, Instant::now());
        let entry = Arc::new(entry);
        let mut live = app.live_metrics.write().unwrap();
        live.insert(system_id.to_string(), Arc::clone(&entry));
        entry
    }

    /// RFC 0007 §4: an eviction hands back the system's own entry, for its caller to drop
    /// outside the live lock, and leaves every other entry in place.
    #[test]
    fn evicting_live_metrics_hands_back_only_that_systems_entry() {
        let (app, _dir) = app();
        let planted = plant(&app, "sys-1");
        plant(&app, "sys-2");

        let evicted = app.evict_live_metrics("sys-1");
        let again = app.evict_live_metrics("sys-1");

        assert!(
            evicted.is_some_and(|entry| Arc::ptr_eq(&entry, &planted)),
            "the system's entry comes back"
        );
        assert!(again.is_none(), "nothing is left to evict");
        let live = app.live_metrics.read().unwrap();
        let kept: Vec<&str> = live.keys().map(String::as_str).collect();
        assert_eq!(kept, ["sys-2"], "another system's entry stays");
    }
}
