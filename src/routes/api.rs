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


use axum::{
    extract::{Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::alerts::AlertRule;
use crate::collectors;
use crate::models::{HealthStatus, HistoryResponse};
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        // Health
        .route("/api/health", get(health))
        // System
        .route("/api/system", get(system_full))
        .route("/api/system/cpu", get(system_cpu))
        .route("/api/system/memory", get(system_memory))
        .route("/api/system/disk", get(system_disk))
        .route("/api/system/network", get(system_network))
        .route("/api/system/processes", get(system_processes))
        // History
        .route("/api/history/cpu", get(history_cpu))
        .route("/api/history/memory", get(history_memory))
        .route("/api/history/swap", get(history_swap))
        .route("/api/history/load", get(history_load))
        .route("/api/history/disk", get(history_disk))
        // Alerts
        .route("/api/alerts", get(get_alerts))
        .route("/api/alerts/config", get(get_alert_config))
        .route("/api/alerts/config", post(set_alert_config))
        // Applications
        .route("/api/packages", get(packages))
        .route("/api/services", get(services))
        .route("/api/containers", get(containers))
        .route("/api/ports", get(ports))
        .with_state(state)
}

// ── Health ─────────────────────────────────────────────

async fn health() -> Json<HealthStatus> {
    Json(HealthStatus {
        status: "ok".into(),
        timestamp: chrono_now(),
        version: env!("CARGO_PKG_VERSION").into(),
    })
}

// ── System ─────────────────────────────────────────────

async fn system_full() -> Json<crate::models::SystemSnapshot> {
    Json(collectors::system::collect())
}

async fn system_cpu() -> Json<crate::models::CpuInfo> {
    Json(collectors::system::collect().cpu)
}

async fn system_memory() -> Json<serde_json::Value> {
    let snap = collectors::system::collect();
    Json(serde_json::json!({
        "memory": snap.memory,
        "swap": snap.swap,
    }))
}

async fn system_disk() -> Json<Vec<crate::models::DiskInfo>> {
    Json(collectors::system::collect().disks)
}

async fn system_network() -> Json<Vec<crate::models::NetworkInfo>> {
    Json(collectors::system::collect().networks)
}

#[derive(Deserialize)]
struct ProcessQuery {
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    20
}

async fn system_processes(Query(q): Query<ProcessQuery>) -> Json<Vec<crate::models::ProcessInfo>> {
    let snap = collectors::system::collect();
    let procs: Vec<_> = snap.top_processes.into_iter().take(q.limit).collect();
    Json(procs)
}

// ── History ────────────────────────────────────────────

#[derive(Deserialize)]
struct HistoryQuery {
    #[serde(default = "default_limit_hist")]
    limit: usize,
}

fn default_limit_hist() -> usize {
    300
}

async fn history_cpu(
    State(s): State<Arc<AppState>>,
    Query(q): Query<HistoryQuery>,
) -> Json<HistoryResponse> {
    let hist = s.history.read();
    let points = tail(&hist.cpu, q.limit);
    Json(HistoryResponse {
        metric: "cpu".into(),
        start_time: points.first().map(|p| p.timestamp).unwrap_or(0),
        end_time: points.last().map(|p| p.timestamp).unwrap_or(0),
        points,
    })
}

async fn history_memory(
    State(s): State<Arc<AppState>>,
    Query(q): Query<HistoryQuery>,
) -> Json<HistoryResponse> {
    let hist = s.history.read();
    let points = tail(&hist.memory, q.limit);
    Json(HistoryResponse {
        metric: "memory".into(),
        start_time: points.first().map(|p| p.timestamp).unwrap_or(0),
        end_time: points.last().map(|p| p.timestamp).unwrap_or(0),
        points,
    })
}

async fn history_swap(
    State(s): State<Arc<AppState>>,
    Query(q): Query<HistoryQuery>,
) -> Json<HistoryResponse> {
    let hist = s.history.read();
    let points = tail(&hist.swap, q.limit);
    Json(HistoryResponse {
        metric: "swap".into(),
        start_time: points.first().map(|p| p.timestamp).unwrap_or(0),
        end_time: points.last().map(|p| p.timestamp).unwrap_or(0),
        points,
    })
}

async fn history_load(
    State(s): State<Arc<AppState>>,
    Query(q): Query<HistoryQuery>,
) -> Json<serde_json::Value> {
    let hist = s.history.read();
    Json(serde_json::json!({
        "load1": {
            "metric": "load1",
            "points": tail(&hist.load1, q.limit),
        },
        "load5": {
            "metric": "load5",
            "points": tail(&hist.load5, q.limit),
        },
        "load15": {
            "metric": "load15",
            "points": tail(&hist.load15, q.limit),
        },
    }))
}

async fn history_disk(
    State(s): State<Arc<AppState>>,
    Query(q): Query<HistoryQuery>,
) -> Json<serde_json::Value> {
    let hist = s.history.read();
    let disks: serde_json::Map<String, serde_json::Value> = hist
        .disks
        .iter()
        .map(|(mount, points)| {
            let pts = tail(points, q.limit);
            (
                mount.clone(),
                serde_json::json!({
                    "metric": format!("disk:{}", mount),
                    "points": pts,
                }),
            )
        })
        .collect();
    Json(serde_json::Value::Object(disks))
}

fn tail(
    deque: &std::collections::VecDeque<crate::models::MetricPoint>,
    limit: usize,
) -> Vec<crate::models::MetricPoint> {
    let len = deque.len();
    if len <= limit {
        deque.iter().cloned().collect()
    } else {
        deque.iter().skip(len - limit).cloned().collect()
    }
}

// ── Alerts ─────────────────────────────────────────────

async fn get_alerts(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let mgr = s.alert_manager.read();
    Json(serde_json::json!({
        "active": &mgr.active_alerts,
        "rules_count": mgr.rules.len(),
    }))
}

async fn get_alert_config(State(s): State<Arc<AppState>>) -> Json<Vec<AlertRule>> {
    let mgr = s.alert_manager.read();
    Json(mgr.rules.clone())
}

#[derive(Deserialize)]
struct AlertConfigBody {
    rules: Vec<AlertRule>,
}

async fn set_alert_config(
    State(s): State<Arc<AppState>>,
    Json(body): Json<AlertConfigBody>,
) -> Json<serde_json::Value> {
    let mut mgr = s.alert_manager.write();
    mgr.rules = body.rules;
    mgr.active_alerts.clear();
    mgr.states.clear();
    Json(serde_json::json!({
        "status": "ok",
        "rules_count": mgr.rules.len(),
    }))
}

// ── Applications ───────────────────────────────────────

async fn packages() -> Json<Vec<crate::models::PackageInfo>> {
    Json(collectors::packages::collect())
}

async fn services() -> Json<Vec<crate::models::ServiceInfo>> {
    Json(collectors::services::collect())
}

async fn containers() -> Json<Vec<crate::models::ContainerInfo>> {
    Json(collectors::containers::collect())
}

async fn ports() -> Json<Vec<crate::models::ListeningPort>> {
    Json(collectors::ports::collect())
}

// ── Helpers ────────────────────────────────────────────

fn chrono_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    unix_to_iso8601(now.as_secs())
}

fn unix_to_iso8601(secs: u64) -> String {
    let days_since_epoch = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let mins = (time_of_day % 3600) / 60;
    let s = time_of_day % 60;

    let mut y = 1970i64;
    let mut d = days_since_epoch as i64;
    loop {
        let days_in_year = if is_leap(y) { 366 } else { 365 };
        if d < days_in_year {
            break;
        }
        d -= days_in_year;
        y += 1;
    }
    let month_days = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 1;
    for &md in &month_days {
        if d < md as i64 {
            break;
        }
        d -= md as i64;
        m += 1;
    }
    let day = d + 1;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, m, day, hours, mins, s
    )
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}
