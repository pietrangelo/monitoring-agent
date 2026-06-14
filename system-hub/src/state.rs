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

use crate::db::Database;
use crate::models::SystemInfo;
use serde::Serialize;

/// Latest snapshot per system_id, kept in memory for the dashboard.
#[derive(Debug, Clone, Serialize)]
pub struct LiveMetrics {
    pub cpu_percent: f32,
    pub memory_percent: f32,
    pub load_one: f64,
    pub disks: Vec<(String, f32)>,
    pub updated_at: u64,
}

impl Default for LiveMetrics {
    fn default() -> Self {
        Self {
            cpu_percent: 0.0,
            memory_percent: 0.0,
            load_one: 0.0,
            disks: Vec::new(),
            updated_at: 0,
        }
    }
}

pub struct AppState {
    pub db: Arc<Database>,
    pub systems_cache: RwLock<Vec<SystemInfo>>,
    /// Per-system latest live metrics (updated by push or poll).
    pub live_metrics: RwLock<HashMap<String, LiveMetrics>>,
}

impl AppState {
    pub fn new(db: Arc<Database>) -> Arc<Self> {
        let systems = db.list_systems().unwrap_or_default();
        Arc::new(Self {
            db,
            systems_cache: RwLock::new(systems),
            live_metrics: RwLock::new(HashMap::new()),
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
