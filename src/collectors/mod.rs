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


pub mod containers;
pub mod packages;
pub mod ports;
pub mod services;
pub mod system;

use crate::state::AppState;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{Duration, interval};

/// Background task: collect metrics every 2s, push to history, evaluate alerts.
pub async fn background_collector(state: Arc<AppState>) {
    let mut tick = interval(Duration::from_secs(2));
    loop {
        tick.tick().await;
        let snap = system::collect();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Push to history
        {
            let mut hist = state.history.write();
            hist.push_cpu(snap.cpu.usage_percent, now);
            hist.push_memory(snap.memory.usage_percent, now);
            hist.push_swap(snap.swap.usage_percent, now);
            hist.push_load1(snap.load_average.one as f32, now);
            hist.push_load5(snap.load_average.five as f32, now);
            hist.push_load15(snap.load_average.fifteen as f32, now);
            for disk in &snap.disks {
                hist.push_disk(&disk.mount_point, disk.usage_percent, now);
            }
        }

        // Evaluate alerts
        let disk_usages: HashMap<String, f32> = snap
            .disks
            .iter()
            .map(|d| (d.mount_point.clone(), d.usage_percent))
            .collect();

        let new_alerts = {
            let mut mgr = state.alert_manager.write();
            mgr.evaluate(
                snap.cpu.usage_percent,
                snap.memory.usage_percent,
                snap.swap.usage_percent,
                &disk_usages,
                snap.load_average.one as f32,
                snap.load_average.five as f32,
                snap.load_average.fifteen as f32,
                snap.cpu.logical_cores,
                now,
            )
        };

        for alert in &new_alerts {
            tracing::warn!("🚨 ALERT: {}", alert.message);
        }
    }
}
