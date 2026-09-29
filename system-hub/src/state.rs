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
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::applications::{HeldRound, RecentRounds, RoundDigester, SourcePace};
use crate::db::Database;
use crate::models::SystemInfo;
use crate::snapshot::{Snapshot, SnapshotTime};

/// A system's latest snapshot, as the snapshot rule kept it, held for the dashboard (RFC 0007
/// §4). Shared as an `Arc`, so a reader copies a pointer, never a disk.
#[derive(Debug)]
pub struct LiveMetrics {
    pub snapshot: Snapshot,
    pub time: SnapshotTime,
    /// When this system's left-out values were last logged at `warn` (RFC 0007 §1).
    pub left_out_warned_at: Option<Instant>,
}

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
    pub systems_cache: RwLock<Vec<SystemInfo>>,
    /// Per-system live metrics, written with each stored snapshot, keyed by system id. Lock
    /// order: the database mutex, then this; never take the database mutex while holding it.
    pub live_metrics: RwLock<HashMap<String, Arc<LiveMetrics>>>,
    /// Per-system applications, keyed by system id. Lock order: the database mutex, then
    /// this; never take the database mutex while holding it.
    pub live_applications: RwLock<HashMap<String, SystemApplications>>,
    /// The one digest key every ingestion path shares.
    pub digester: RoundDigester,
}

impl AppState {
    pub fn new(db: Arc<Database>) -> Arc<Self> {
        let systems = db.list_systems().unwrap_or_default();
        Arc::new(Self {
            db,
            systems_cache: RwLock::new(systems),
            live_metrics: RwLock::new(HashMap::new()),
            live_applications: RwLock::new(HashMap::new()),
            digester: RoundDigester::new(),
        })
    }

    pub fn refresh_cache(&self) {
        if let Ok(systems) = self.db.list_systems() {
            *self.systems_cache.write().unwrap() = systems;
        }
    }

    pub fn get_enabled_systems(&self) -> Vec<SystemInfo> {
        self.systems_cache
            .read()
            .unwrap()
            .iter()
            .filter(|s| s.enabled)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn temp_db() -> (Arc<Database>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Arc::new(Database::new(path.to_str().unwrap()).unwrap());
        (db, dir)
    }

    fn sample_system(id: &str, name: &str, enabled: bool) -> SystemInfo {
        SystemInfo {
            id: id.to_string(),
            name: name.to_string(),
            url: "http://example.com".to_string(),
            token: String::new(),
            status: crate::models::SystemStatus::Unknown,
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
            enabled,
        }
    }

    #[test]
    fn app_state_new_loads_systems_from_db() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "web", true))
            .unwrap();
        let state = AppState::new(db);
        assert_eq!(state.systems_cache.read().unwrap().len(), 1);
    }

    #[test]
    fn refresh_cache_picks_up_new_rows() {
        let (db, _dir) = temp_db();
        let state = AppState::new(db.clone());
        assert!(state.systems_cache.read().unwrap().is_empty());

        db.insert_system(&sample_system("id-1", "web", true))
            .unwrap();
        state.refresh_cache();
        assert_eq!(state.systems_cache.read().unwrap().len(), 1);
    }

    #[test]
    fn get_enabled_systems_filters_disabled() {
        let (db, _dir) = temp_db();
        db.insert_system(&sample_system("id-1", "enabled-sys", true))
            .unwrap();
        db.insert_system(&sample_system("id-2", "disabled-sys", false))
            .unwrap();
        let state = AppState::new(db);

        let enabled = state.get_enabled_systems();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].id, "id-1");
    }
}
