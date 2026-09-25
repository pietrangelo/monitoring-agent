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
    Json, Router,
    extract::{Query, State},
    routing::{get, post},
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
        "active": mgr.active_alerts(),
        "rules_count": mgr.rules().len(),
    }))
}

async fn get_alert_config(State(s): State<Arc<AppState>>) -> Json<Vec<AlertRule>> {
    let mgr = s.alert_manager.read();
    Json(mgr.rules().to_vec())
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
    mgr.replace_rules(body.rules);
    Json(serde_json::json!({
        "status": "ok",
        "rules_count": mgr.rules().len(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MetricPoint as ModelMetricPoint;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn body_json(res: axum::response::Response) -> serde_json::Value {
        let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn app() -> (Router, Arc<AppState>) {
        let state = AppState::new();
        (router(state.clone()), state)
    }

    #[tokio::test]
    async fn health_returns_ok_status_and_version() {
        let (app, _) = app();
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let json = body_json(res).await;
        assert_eq!(json["status"], "ok");
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
        assert!(json["timestamp"].as_str().unwrap().ends_with('Z'));
    }

    #[tokio::test]
    async fn system_full_returns_snapshot_shape() {
        let (app, _) = app();
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/system")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let json = body_json(res).await;
        assert!(json.get("hostname").is_some());
        assert!(json.get("cpu").is_some());
        assert!(json.get("memory").is_some());
    }

    #[tokio::test]
    async fn system_processes_respects_limit_query_param() {
        let (app, _) = app();
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/system/processes?limit=2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let json = body_json(res).await;
        assert!(json.as_array().unwrap().len() <= 2);
    }

    #[tokio::test]
    async fn system_processes_default_limit_is_twenty() {
        let (app, _) = app();
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/system/processes")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let json = body_json(res).await;
        assert!(json.as_array().unwrap().len() <= 20);
    }

    #[tokio::test]
    async fn history_cpu_empty_when_no_data_collected() {
        let (app, _) = app();
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/history/cpu")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let json = body_json(res).await;
        assert_eq!(json["metric"], "cpu");
        assert_eq!(json["points"].as_array().unwrap().len(), 0);
        assert_eq!(json["start_time"], 0);
        assert_eq!(json["end_time"], 0);
    }

    #[tokio::test]
    async fn history_cpu_returns_seeded_points_and_respects_limit() {
        let (app, state) = app();
        {
            let mut hist = state.history.write();
            for i in 0..5 {
                hist.push_cpu(i as f32, 1000 + i);
            }
        }
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/history/cpu?limit=2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let json = body_json(res).await;
        let points = json["points"].as_array().unwrap();
        assert_eq!(points.len(), 2);
        // tail() keeps the most recent points.
        assert_eq!(points[0]["timestamp"], 1003);
        assert_eq!(points[1]["timestamp"], 1004);
        assert_eq!(json["start_time"], 1003);
        assert_eq!(json["end_time"], 1004);
    }

    #[tokio::test]
    async fn history_memory_swap_and_load_and_disk_endpoints_respond_ok() {
        let (app, state) = app();
        {
            let mut hist = state.history.write();
            hist.push_memory(10.0, 1);
            hist.push_swap(5.0, 1);
            hist.push_load1(0.5, 1);
            hist.push_load5(0.6, 1);
            hist.push_load15(0.7, 1);
            hist.push_disk("/", 42.0, 1);
        }
        for path in [
            "/api/history/memory",
            "/api/history/swap",
            "/api/history/load",
            "/api/history/disk",
        ] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "path {path}");
        }
    }

    #[tokio::test]
    async fn history_disk_keys_response_by_mount_point() {
        let (app, state) = app();
        {
            let mut hist = state.history.write();
            hist.push_disk("/data", 33.0, 1);
        }
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/history/disk")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let json = body_json(res).await;
        assert!(json.get("/data").is_some());
        assert_eq!(json["/data"]["metric"], "disk:/data");
    }

    #[tokio::test]
    async fn history_load_includes_all_three_windows() {
        let (app, _) = app();
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/history/load")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let json = body_json(res).await;
        assert_eq!(json["load1"]["metric"], "load1");
        assert_eq!(json["load5"]["metric"], "load5");
        assert_eq!(json["load15"]["metric"], "load15");
    }

    #[tokio::test]
    async fn alerts_endpoints_default_state() {
        let (app, _) = app();
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/alerts")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let json = body_json(res).await;
        assert_eq!(json["active"].as_array().unwrap().len(), 0);
        assert_eq!(json["rules_count"], 7);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/alerts/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let json = body_json(res).await;
        assert_eq!(json.as_array().unwrap().len(), 7);
    }

    #[tokio::test]
    async fn set_alert_config_replaces_rules_and_clears_active_state() {
        let (app, state) = app();
        let disks = std::collections::HashMap::new();
        let breach_cpu = |now| {
            let mut mgr = state.alert_manager.write();
            mgr.evaluate(99.0, 0.0, 0.0, &disks, 0.0, 0.0, 0.0, 4, now);
            mgr.active_alerts()
                .iter()
                .map(|a| a.id.clone())
                .collect::<Vec<_>>()
        };
        breach_cpu(1000);
        let ids_before = breach_cpu(1060);
        assert!(
            !ids_before.is_empty(),
            "an incident of the default rules is active"
        );
        let body = serde_json::json!({
            "rules": [{
                "metric": "cpu",
                "operator": "gt",
                "threshold": 50.0,
                "severity": "warning"
            }]
        });
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/alerts/config")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let json = body_json(res).await;
        assert_eq!(json["status"], "ok");
        assert_eq!(json["rules_count"], 1);
        assert_eq!(state.alert_manager.read().rules().len(), 1);
        assert!(
            state.alert_manager.read().active_alerts().is_empty(),
            "no incident survives the replacement"
        );
        let ids_after = breach_cpu(1062);
        assert_eq!(ids_after.len(), 1, "the new rule's incident is active");
        assert!(
            !ids_before.contains(&ids_after[0]),
            "the new rule's incident {} reuses an id from before the replacement {ids_before:?}",
            ids_after[0]
        );

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/alerts/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let json = body_json(res).await;
        assert_eq!(json.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn set_alert_config_rejects_malformed_body() {
        let (app, _) = app();
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/alerts/config")
                    .header("content-type", "application/json")
                    .body(Body::from("{not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Invalid JSON syntax -> axum's Json extractor rejects with 400 (not 422,
        // which is reserved for well-formed JSON that doesn't match the target type).
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn set_alert_config_rejects_well_formed_json_with_wrong_shape() {
        let (app, _) = app();
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/alerts/config")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"not_rules": []}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn application_endpoints_respond_ok_with_arrays() {
        let (app, _) = app();
        for path in [
            "/api/packages",
            "/api/services",
            "/api/containers",
            "/api/ports",
        ] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "path {path}");
            let json = body_json(res).await;
            assert!(json.is_array(), "path {path} should return a JSON array");
        }
    }

    #[test]
    fn tail_returns_all_when_under_limit() {
        let mut deque = std::collections::VecDeque::new();
        deque.push_back(ModelMetricPoint {
            timestamp: 1,
            value: 1.0,
        });
        deque.push_back(ModelMetricPoint {
            timestamp: 2,
            value: 2.0,
        });
        let out = tail(&deque, 10);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn tail_truncates_to_most_recent_when_over_limit() {
        let mut deque = std::collections::VecDeque::new();
        for i in 0..10 {
            deque.push_back(ModelMetricPoint {
                timestamp: i,
                value: i as f32,
            });
        }
        let out = tail(&deque, 3);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].timestamp, 7);
        assert_eq!(out[2].timestamp, 9);
    }

    #[test]
    fn tail_empty_deque() {
        let deque: std::collections::VecDeque<ModelMetricPoint> = std::collections::VecDeque::new();
        assert!(tail(&deque, 5).is_empty());
    }

    #[test]
    fn unix_to_iso8601_epoch_zero() {
        assert_eq!(unix_to_iso8601(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn unix_to_iso8601_leap_day() {
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
