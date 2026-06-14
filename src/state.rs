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


use parking_lot::RwLock;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::alerts::AlertManager;
use crate::models::MetricPoint;

/// Maximum data points stored per metric (e.g., 1 hour at 2s intervals = 1800 points).
const MAX_HISTORY: usize = 3600;

/// Global shared application state.
pub struct AppState {
    pub history: RwLock<MetricsHistory>,
    pub alert_manager: RwLock<AlertManager>,
}

impl AppState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            history: RwLock::new(MetricsHistory::new()),
            alert_manager: RwLock::new(AlertManager::with_defaults()),
        })
    }
}

/// Ring buffers for each tracked metric.
#[derive(Default)]
pub struct MetricsHistory {
    pub cpu: VecDeque<MetricPoint>,
    pub memory: VecDeque<MetricPoint>,
    pub swap: VecDeque<MetricPoint>,
    pub load1: VecDeque<MetricPoint>,
    pub load5: VecDeque<MetricPoint>,
    pub load15: VecDeque<MetricPoint>,
    /// Per-disk usage (keyed by mount point)
    pub disks: HashMap<String, VecDeque<MetricPoint>>,
}

impl MetricsHistory {
    fn new() -> Self {
        Self {
            disks: HashMap::new(),
            ..Default::default()
        }
    }

    pub fn push_cpu(&mut self, value: f32, ts: u64) {
        push_max(&mut self.cpu, MetricPoint { timestamp: ts, value }, MAX_HISTORY);
    }
    pub fn push_memory(&mut self, value: f32, ts: u64) {
        push_max(&mut self.memory, MetricPoint { timestamp: ts, value }, MAX_HISTORY);
    }
    pub fn push_swap(&mut self, value: f32, ts: u64) {
        push_max(&mut self.swap, MetricPoint { timestamp: ts, value }, MAX_HISTORY);
    }
    pub fn push_load1(&mut self, value: f32, ts: u64) {
        push_max(
            &mut self.load1,
            MetricPoint {
                timestamp: ts,
                value: (value * 100.0).round() / 100.0,
            },
            MAX_HISTORY,
        );
    }
    pub fn push_load5(&mut self, value: f32, ts: u64) {
        push_max(
            &mut self.load5,
            MetricPoint {
                timestamp: ts,
                value: (value * 100.0).round() / 100.0,
            },
            MAX_HISTORY,
        );
    }
    pub fn push_load15(&mut self, value: f32, ts: u64) {
        push_max(
            &mut self.load15,
            MetricPoint {
                timestamp: ts,
                value: (value * 100.0).round() / 100.0,
            },
            MAX_HISTORY,
        );
    }
    pub fn push_disk(&mut self, mount: &str, value: f32, ts: u64) {
        let entry = self.disks.entry(mount.to_string()).or_default();
        push_max(entry, MetricPoint { timestamp: ts, value }, MAX_HISTORY);
    }
}

fn push_max(buf: &mut VecDeque<MetricPoint>, point: MetricPoint, max: usize) {
    buf.push_back(point);
    while buf.len() > max {
        buf.pop_front();
    }
}
