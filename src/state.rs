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
        push_max(
            &mut self.cpu,
            MetricPoint {
                timestamp: ts,
                value,
            },
            MAX_HISTORY,
        );
    }
    pub fn push_memory(&mut self, value: f32, ts: u64) {
        push_max(
            &mut self.memory,
            MetricPoint {
                timestamp: ts,
                value,
            },
            MAX_HISTORY,
        );
    }
    pub fn push_swap(&mut self, value: f32, ts: u64) {
        push_max(
            &mut self.swap,
            MetricPoint {
                timestamp: ts,
                value,
            },
            MAX_HISTORY,
        );
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
        push_max(
            entry,
            MetricPoint {
                timestamp: ts,
                value,
            },
            MAX_HISTORY,
        );
    }
}

fn push_max(buf: &mut VecDeque<MetricPoint>, point: MetricPoint, max: usize) {
    buf.push_back(point);
    while buf.len() > max {
        buf.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_history_is_empty() {
        let hist = MetricsHistory::new();
        assert!(hist.cpu.is_empty());
        assert!(hist.memory.is_empty());
        assert!(hist.swap.is_empty());
        assert!(hist.load1.is_empty());
        assert!(hist.load5.is_empty());
        assert!(hist.load15.is_empty());
        assert!(hist.disks.is_empty());
    }

    #[test]
    fn push_cpu_appends_point() {
        let mut hist = MetricsHistory::new();
        hist.push_cpu(42.5, 100);
        assert_eq!(hist.cpu.len(), 1);
        assert_eq!(hist.cpu[0].value, 42.5);
        assert_eq!(hist.cpu[0].timestamp, 100);
    }

    #[test]
    fn push_memory_and_swap_append_points() {
        let mut hist = MetricsHistory::new();
        hist.push_memory(10.0, 1);
        hist.push_swap(20.0, 2);
        assert_eq!(hist.memory[0].value, 10.0);
        assert_eq!(hist.swap[0].value, 20.0);
    }

    #[test]
    fn push_load_rounds_to_two_decimal_places() {
        let mut hist = MetricsHistory::new();
        hist.push_load1(1.23456, 1);
        hist.push_load5(2.5, 2);
        hist.push_load15(0.001, 3);
        assert_eq!(hist.load1[0].value, 1.23);
        assert_eq!(hist.load5[0].value, 2.5);
        assert_eq!(hist.load15[0].value, 0.0);
    }

    #[test]
    fn push_disk_creates_entry_per_mount_point() {
        let mut hist = MetricsHistory::new();
        hist.push_disk("/", 50.0, 1);
        hist.push_disk("/data", 75.0, 1);
        assert_eq!(hist.disks.len(), 2);
        assert_eq!(hist.disks["/"][0].value, 50.0);
        assert_eq!(hist.disks["/data"][0].value, 75.0);
    }

    #[test]
    fn push_disk_appends_to_existing_mount_point() {
        let mut hist = MetricsHistory::new();
        hist.push_disk("/", 50.0, 1);
        hist.push_disk("/", 55.0, 2);
        assert_eq!(hist.disks["/"].len(), 2);
    }

    #[test]
    fn ring_buffer_evicts_oldest_when_over_capacity() {
        let mut buf = VecDeque::new();
        for i in 0..5 {
            push_max(
                &mut buf,
                MetricPoint {
                    timestamp: i,
                    value: i as f32,
                },
                3,
            );
        }
        assert_eq!(buf.len(), 3);
        let values: Vec<u64> = buf.iter().map(|p| p.timestamp).collect();
        assert_eq!(values, vec![2, 3, 4]);
    }

    #[test]
    fn ring_buffer_under_capacity_keeps_everything() {
        let mut buf = VecDeque::new();
        push_max(
            &mut buf,
            MetricPoint {
                timestamp: 1,
                value: 1.0,
            },
            10,
        );
        push_max(
            &mut buf,
            MetricPoint {
                timestamp: 2,
                value: 2.0,
            },
            10,
        );
        assert_eq!(buf.len(), 2);
    }

    #[test]
    fn app_state_new_has_default_alert_manager_and_empty_history() {
        let state = AppState::new();
        let hist = state.history.read();
        assert!(hist.cpu.is_empty());
        let mgr = state.alert_manager.read();
        assert_eq!(mgr.rules.len(), 7);
    }
}
