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

use serde::Deserialize;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{Duration, interval};

use crate::models::{
    AlertRecord, DiskSnapshot, MetricSnapshot, ProcessSnapshot, SystemInfo, SystemStatus,
};
use crate::state::AppState;

// ── Agent response shape (maps the system-agent /api/system JSON) ──

#[derive(Debug, Deserialize)]
struct AgentResponse {
    hostname: Option<String>,
    os: Option<AgentOs>,
    kernel: Option<String>,
    cpu: Option<AgentCpu>,
    memory: Option<AgentMemory>,
    swap: Option<AgentSwap>,
    load_average: Option<AgentLoad>,
    uptime_seconds: Option<u64>,
    uptime_display: Option<String>,
    disks: Option<Vec<AgentDisk>>,
    top_processes: Option<Vec<AgentProcess>>,
}

#[derive(Debug, Deserialize)]
struct AgentOs {
    name: Option<String>,
    pretty_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AgentCpu {
    model: Option<String>,
    logical_cores: Option<usize>,
    usage_percent: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct AgentMemory {
    total_bytes: Option<u64>,
    total_display: Option<String>,
    used_display: Option<String>,
    used_bytes: Option<u64>,
    usage_percent: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct AgentSwap {
    usage_percent: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct AgentLoad {
    one: Option<f64>,
    five: Option<f64>,
    fifteen: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct AgentDisk {
    mount_point: Option<String>,
    usage_percent: Option<f32>,
    total_display: Option<String>,
    used_display: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AgentProcess {
    pid: Option<u32>,
    name: Option<String>,
    cpu_usage: Option<f32>,
    memory_usage_display: Option<String>,
    memory_percent: Option<f32>,
}

// ── Collector ──────────────────────────────────────────

fn now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    unix_to_iso8601(secs)
}

pub fn start_collectors(state: Arc<AppState>) {
    let state_clone = state.clone();
    tokio::spawn(async move {
        let mut refresh_tick = interval(Duration::from_secs(30));
        loop {
            refresh_tick.tick().await;
            state_clone.refresh_cache();
            let systems = state_clone.get_enabled_systems();
            for system in systems {
                let s = state_clone.clone();
                tokio::spawn(async move {
                    poll_system(s, &system).await;
                });
            }
        }
    });
}

async fn poll_system(state: Arc<AppState>, system: &SystemInfo) {
    let url = format!("{}/api/system", system.url.trim_end_matches('/'));

    let mut headers = reqwest::header::HeaderMap::new();
    if !system.token.is_empty()
        && let Ok(val) = reqwest::header::HeaderValue::from_str(&system.token)
    {
        headers.insert("X-API-Key", val);
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .default_headers(headers)
        .build()
        .ok();

    let Some(client) = client else {
        return;
    };

    let now = now_iso();
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            // Deserialize into typed structs
            let agent: AgentResponse = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    let _ = state.db.update_system_status(
                        &system.id,
                        &SystemStatus::Offline,
                        &now,
                        Some(&format!("JSON parse error: {e}")),
                    );
                    return;
                }
            };

            // Mark online
            let _ = state
                .db
                .update_system_status(&system.id, &SystemStatus::Online, &now, None);

            // Update static info on first poll
            if system.hostname.is_none() || system.os.is_none() {
                let os_name = agent
                    .os
                    .as_ref()
                    .and_then(|o| o.pretty_name.as_deref().or(o.name.as_deref()));
                let cpu_model = agent.cpu.as_ref().and_then(|c| c.model.as_deref());
                let cpu_cores = agent.cpu.as_ref().and_then(|c| c.logical_cores);
                let mem_display = agent
                    .memory
                    .as_ref()
                    .and_then(|m| m.total_display.as_deref());
                let mem_bytes = agent.memory.as_ref().and_then(|m| m.total_bytes);

                let _ = state.db.update_system_info(
                    &system.id,
                    os_name,
                    agent.hostname.as_deref(),
                    agent.kernel.as_deref(),
                    cpu_model,
                    cpu_cores,
                    mem_display,
                    mem_bytes,
                );
            }

            // Build MetricSnapshot from agent response
            let snapshot = MetricSnapshot {
                system_id: system.id.clone(),
                timestamp: now_secs,
                cpu_percent: agent
                    .cpu
                    .as_ref()
                    .and_then(|c| c.usage_percent)
                    .unwrap_or(0.0),
                memory_percent: agent
                    .memory
                    .as_ref()
                    .and_then(|m| m.usage_percent)
                    .unwrap_or(0.0),
                swap_percent: agent
                    .swap
                    .as_ref()
                    .and_then(|s| s.usage_percent)
                    .unwrap_or(0.0),
                load_one: agent
                    .load_average
                    .as_ref()
                    .and_then(|l| l.one)
                    .unwrap_or(0.0),
                load_five: agent
                    .load_average
                    .as_ref()
                    .and_then(|l| l.five)
                    .unwrap_or(0.0),
                load_fifteen: agent
                    .load_average
                    .as_ref()
                    .and_then(|l| l.fifteen)
                    .unwrap_or(0.0),
                uptime_seconds: agent.uptime_seconds.unwrap_or(0),
                uptime_display: agent.uptime_display.unwrap_or_default(),
                memory_used_display: agent
                    .memory
                    .as_ref()
                    .and_then(|m| m.used_display.clone())
                    .unwrap_or_default(),
                memory_total_display: agent
                    .memory
                    .as_ref()
                    .and_then(|m| m.total_display.clone())
                    .unwrap_or_default(),
                memory_used_bytes: agent
                    .memory
                    .as_ref()
                    .and_then(|m| m.used_bytes)
                    .unwrap_or(0),
                memory_total_bytes: agent
                    .memory
                    .as_ref()
                    .and_then(|m| m.total_bytes)
                    .unwrap_or(0),
                cpu_logical_cores: agent
                    .cpu
                    .as_ref()
                    .and_then(|c| c.logical_cores)
                    .unwrap_or(0),
                disks: agent
                    .disks
                    .unwrap_or_default()
                    .into_iter()
                    .map(|d| DiskSnapshot {
                        mount_point: d.mount_point.unwrap_or_default(),
                        usage_percent: d.usage_percent.unwrap_or(0.0),
                        total_display: d.total_display.unwrap_or_default(),
                        used_display: d.used_display.unwrap_or_default(),
                    })
                    .collect(),
                top_processes: agent
                    .top_processes
                    .unwrap_or_default()
                    .into_iter()
                    .map(|p| ProcessSnapshot {
                        pid: p.pid.unwrap_or(0),
                        name: p.name.unwrap_or_default(),
                        cpu_usage: p.cpu_usage.unwrap_or(0.0),
                        memory_usage_display: p.memory_usage_display.unwrap_or_default(),
                        memory_percent: p.memory_percent.unwrap_or(0.0),
                    })
                    .collect(),
            };

            // Store metrics from the snapshot
            store_metrics(&state, &snapshot);

            // Fetch alerts from the remote system
            let alerts_url = format!("{}/api/alerts", system.url.trim_end_matches('/'));
            if let Ok(resp) = client.get(&alerts_url).send().await
                && let Ok(body) = resp.json::<serde_json::Value>().await
                && let Some(active) = body.get("active").and_then(|v| v.as_array())
            {
                let stored_at = now_iso();
                for alert_val in active {
                    let id = alert_val
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(|s| format!("{}_{}", &system.id, s))
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

                    let severity = alert_val
                        .get("rule")
                        .and_then(|v| v.get("severity"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("warning");

                    let message = alert_val
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    let current_value = alert_val
                        .get("current_value")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0) as f32;

                    let fired_at = alert_val
                        .get("fired_at")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    let record = AlertRecord {
                        id,
                        system_id: system.id.clone(),
                        system_name: system.name.clone(),
                        severity: severity.to_string(),
                        message: message.to_string(),
                        current_value,
                        fired_at: fired_at.to_string(),
                        stored_at: stored_at.clone(),
                        acknowledged: false,
                    };
                    let _ = state.db.insert_alert(&record);
                }
            }
        }
        Ok(resp) => {
            let msg = format!("HTTP {}", resp.status());
            let _ =
                state
                    .db
                    .update_system_status(&system.id, &SystemStatus::Offline, &now, Some(&msg));
        }
        Err(e) => {
            let _ = state.db.update_system_status(
                &system.id,
                &SystemStatus::Offline,
                &now,
                Some(&format!("{e}")),
            );
        }
    }
}

fn store_metrics(state: &AppState, snap: &MetricSnapshot) {
    let ts = snap.timestamp;
    let sid = &snap.system_id;

    let _ = state.db.insert_metric(sid, "cpu", snap.cpu_percent, ts);
    let _ = state
        .db
        .insert_metric(sid, "memory", snap.memory_percent, ts);
    let _ = state.db.insert_metric(sid, "swap", snap.swap_percent, ts);
    let _ = state
        .db
        .insert_metric(sid, "load1", snap.load_one as f32, ts);
    let _ = state
        .db
        .insert_metric(sid, "load5", snap.load_five as f32, ts);

    for disk in &snap.disks {
        let metric = format!("disk:{}", disk.mount_point);
        let _ = state.db.insert_metric(sid, &metric, disk.usage_percent, ts);
    }

    // Update live metrics cache
    let mut live = state.live_metrics.write().unwrap();
    live.insert(
        sid.clone(),
        crate::state::LiveMetrics {
            cpu_percent: snap.cpu_percent,
            memory_percent: snap.memory_percent,
            load_one: snap.load_one,
            disks: snap
                .disks
                .iter()
                .map(|d| (d.mount_point.clone(), d.usage_percent))
                .collect(),
            updated_at: ts,
        },
    );
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
    format!("{y:04}-{m:02}-{day:02}T{hours:02}:{mins:02}:{s:02}Z")
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use axum::{Json, Router, routing::get};

    fn temp_state() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Arc::new(Database::new(path.to_str().unwrap()).unwrap());
        let state = AppState::new(db);
        (state, dir)
    }

    fn sample_system(id: &str, url: String) -> SystemInfo {
        SystemInfo {
            id: id.to_string(),
            name: "test-sys".into(),
            url,
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

    async fn spawn_mock_agent(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn poll_system_success_updates_status_hardware_info_and_metrics() {
        let (state, _dir) = temp_state();
        let system_json = serde_json::json!({
            "hostname": "host1",
            "os": {"name": "Ubuntu", "pretty_name": "Ubuntu 22.04"},
            "kernel": "6.6.0",
            "cpu": {"model": "Generic", "logical_cores": 4, "usage_percent": 12.5},
            "memory": {"total_bytes": 1000, "total_display": "1KB", "used_display": "500B", "used_bytes": 500, "usage_percent": 50.0},
            "swap": {"usage_percent": 0.0},
            "load_average": {"one": 0.1, "five": 0.2, "fifteen": 0.3},
            "uptime_seconds": 100,
            "uptime_display": "1m",
            "disks": [{"mount_point": "/", "usage_percent": 40.0, "total_display": "10G", "used_display": "4G"}],
            "top_processes": []
        });
        let alerts_json = serde_json::json!({"active": []});
        let app = Router::new()
            .route(
                "/api/system",
                get(move || {
                    let v = system_json.clone();
                    async move { Json(v) }
                }),
            )
            .route(
                "/api/alerts",
                get(move || {
                    let v = alerts_json.clone();
                    async move { Json(v) }
                }),
            );
        let url = spawn_mock_agent(app).await;

        let system = sample_system("id-1", url);
        state.db.insert_system(&system).unwrap();

        poll_system(state.clone(), &system).await;

        let updated = state.db.get_system("id-1").unwrap().unwrap();
        assert_eq!(updated.status, SystemStatus::Online);
        assert_eq!(updated.hostname.as_deref(), Some("host1"));
        assert_eq!(updated.cpu_cores, Some(4));

        let points = state.db.get_metrics("id-1", "cpu", 10, None).unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].value, 12.5);

        let disk_points = state.db.get_metrics("id-1", "disk:/", 10, None).unwrap();
        assert_eq!(disk_points.len(), 1);

        let live = state.live_metrics.read().unwrap();
        assert_eq!(live.get("id-1").unwrap().cpu_percent, 12.5);
    }

    #[tokio::test]
    async fn poll_system_stores_active_alerts_from_agent() {
        let (state, _dir) = temp_state();
        let system_json = serde_json::json!({});
        let alerts_json = serde_json::json!({
            "active": [{
                "id": "rule_0",
                "rule": {"severity": "critical"},
                "current_value": 97.5,
                "fired_at": "2026-01-01T00:00:00Z",
                "message": "CPU too high"
            }]
        });
        let app = Router::new()
            .route(
                "/api/system",
                get(move || {
                    let v = system_json.clone();
                    async move { Json(v) }
                }),
            )
            .route(
                "/api/alerts",
                get(move || {
                    let v = alerts_json.clone();
                    async move { Json(v) }
                }),
            );
        let url = spawn_mock_agent(app).await;
        let system = sample_system("id-1", url);
        state.db.insert_system(&system).unwrap();

        poll_system(state.clone(), &system).await;

        let alerts = state.db.get_alerts(Some("id-1"), None, 10).unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].severity, "critical");
        assert_eq!(alerts[0].message, "CPU too high");
        assert_eq!(alerts[0].id, "id-1_rule_0");
    }

    #[tokio::test]
    async fn poll_system_marks_offline_on_json_parse_error() {
        let (state, _dir) = temp_state();
        let app = Router::new().route("/api/system", get(|| async { "not json" }));
        let url = spawn_mock_agent(app).await;
        let system = sample_system("id-1", url);
        state.db.insert_system(&system).unwrap();

        poll_system(state.clone(), &system).await;

        let updated = state.db.get_system("id-1").unwrap().unwrap();
        assert_eq!(updated.status, SystemStatus::Offline);
        assert!(updated.last_error.unwrap().contains("JSON parse error"));
    }

    #[tokio::test]
    async fn poll_system_marks_offline_on_http_error_status() {
        let (state, _dir) = temp_state();
        let app = Router::new().route(
            "/api/system",
            get(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
        );
        let url = spawn_mock_agent(app).await;
        let system = sample_system("id-1", url);
        state.db.insert_system(&system).unwrap();

        poll_system(state.clone(), &system).await;

        let updated = state.db.get_system("id-1").unwrap().unwrap();
        assert_eq!(updated.status, SystemStatus::Offline);
        assert!(updated.last_error.unwrap().contains("500"));
    }

    #[tokio::test]
    async fn poll_system_marks_offline_on_connection_error() {
        let (state, _dir) = temp_state();
        // Nothing is listening on this port: reqwest should fail to connect.
        let system = sample_system("id-1", "http://127.0.0.1:1".to_string());
        state.db.insert_system(&system).unwrap();

        poll_system(state.clone(), &system).await;

        let updated = state.db.get_system("id-1").unwrap().unwrap();
        assert_eq!(updated.status, SystemStatus::Offline);
        assert!(updated.last_error.is_some());
    }

    #[test]
    fn store_metrics_writes_all_metrics_and_updates_live_cache() {
        let (state, _dir) = {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("test.db");
            let db = Arc::new(Database::new(path.to_str().unwrap()).unwrap());
            db.insert_system(&sample_system("id-1", "http://x".to_string()))
                .unwrap();
            (AppState::new(db), dir)
        };

        let snap = MetricSnapshot {
            system_id: "id-1".to_string(),
            timestamp: 100,
            cpu_percent: 10.0,
            memory_percent: 20.0,
            swap_percent: 5.0,
            load_one: 0.1,
            load_five: 0.2,
            load_fifteen: 0.3,
            uptime_seconds: 60,
            uptime_display: "1m".to_string(),
            memory_used_display: "1 GB".to_string(),
            memory_total_display: "4 GB".to_string(),
            memory_used_bytes: 1_000_000,
            memory_total_bytes: 4_000_000,
            cpu_logical_cores: 4,
            disks: vec![DiskSnapshot {
                mount_point: "/".to_string(),
                usage_percent: 33.0,
                total_display: "10G".to_string(),
                used_display: "3G".to_string(),
            }],
            top_processes: vec![],
        };

        store_metrics(&state, &snap);

        assert_eq!(
            state.db.get_metrics("id-1", "cpu", 10, None).unwrap()[0].value,
            10.0
        );
        assert_eq!(
            state.db.get_metrics("id-1", "memory", 10, None).unwrap()[0].value,
            20.0
        );
        assert_eq!(
            state.db.get_metrics("id-1", "swap", 10, None).unwrap()[0].value,
            5.0
        );
        assert_eq!(
            state.db.get_metrics("id-1", "disk:/", 10, None).unwrap()[0].value,
            33.0
        );

        let live = state.live_metrics.read().unwrap();
        let m = live.get("id-1").unwrap();
        assert_eq!(m.cpu_percent, 10.0);
        assert_eq!(m.disks, vec![("/".to_string(), 33.0)]);
        assert_eq!(m.updated_at, 100);
    }

    #[test]
    fn agent_response_deserializes_with_all_fields_missing() {
        let resp: AgentResponse = serde_json::from_str("{}").unwrap();
        assert!(resp.hostname.is_none());
        assert!(resp.os.is_none());
        assert!(resp.cpu.is_none());
        assert!(resp.disks.is_none());
        assert!(resp.top_processes.is_none());
    }

    #[test]
    fn agent_response_deserializes_partial_nested_fields() {
        let resp: AgentResponse =
            serde_json::from_str(r#"{"cpu":{"usage_percent":42.0}}"#).unwrap();
        assert_eq!(resp.cpu.unwrap().usage_percent, Some(42.0));
    }

    #[test]
    fn unix_to_iso8601_epoch_zero() {
        assert_eq!(unix_to_iso8601(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn unix_to_iso8601_leap_day_boundary() {
        assert_eq!(unix_to_iso8601(1_709_251_200), "2024-03-01T00:00:00Z");
    }

    #[test]
    fn is_leap_rules() {
        assert!(is_leap(2000));
        assert!(!is_leap(1900));
        assert!(is_leap(2024));
        assert!(!is_leap(2023));
    }
}
