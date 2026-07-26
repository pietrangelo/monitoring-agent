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
    extract::{Path, Query, State},
    routing::{delete, get, post, put},
};
use serde::Deserialize;
use std::sync::Arc;

use crate::models::*;
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        // Health
        .route("/api/health", get(health))
        // Systems CRUD
        .route("/api/systems", get(list_systems))
        .route("/api/systems", post(register_system))
        .route("/api/systems/:id", get(get_system))
        .route("/api/systems/:id", put(update_system))
        .route("/api/systems/:id", delete(delete_system))
        // Summary
        .route("/api/summary", get(summary))
        // Metrics
        .route("/api/systems/:id/metrics", get(get_metrics))
        .route("/api/systems/:id/history", get(get_history))
        // Alerts
        .route("/api/alerts", get(get_alerts))
        .route("/api/alerts/:alert_id/acknowledge", post(acknowledge_alert))
        .with_state(state)
}

// ── Health ─────────────────────────────────────────────

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

// ── Systems ────────────────────────────────────────────

async fn list_systems(State(s): State<Arc<AppState>>) -> Json<Vec<SystemInfo>> {
    let systems = s.db.list_systems().unwrap_or_default();
    Json(systems)
}

async fn get_system(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<SystemInfo>, axum::http::StatusCode> {
    match s.db.get_system(&id) {
        Ok(Some(sys)) => Ok(Json(sys)),
        Ok(None) => Err(axum::http::StatusCode::NOT_FOUND),
        Err(_) => Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn register_system(
    State(s): State<Arc<AppState>>,
    Json(payload): Json<RegisterSystemPayload>,
) -> Result<Json<SystemInfo>, axum::http::StatusCode> {
    let id = uuid::Uuid::new_v4().to_string();
    let sys = SystemInfo {
        id,
        name: payload.name,
        url: payload.url.trim_end_matches('/').to_string(),
        token: payload.token,
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
        poll_interval_secs: payload.poll_interval_secs.max(5),
        enabled: true,
    };

    s.db.insert_system(&sys)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    s.refresh_cache();
    Ok(Json(sys))
}

async fn update_system(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<UpdateSystemPayload>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    // Verify system exists
    if s.db
        .get_system(&id)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?
        .is_none()
    {
        return Err(axum::http::StatusCode::NOT_FOUND);
    }

    s.db.update_system_config(
        &id,
        payload.name.as_deref(),
        payload.url.as_deref(),
        payload.token.as_deref(),
        payload.poll_interval_secs,
        payload.enabled,
    )
    .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    s.refresh_cache();

    Ok(Json(serde_json::json!({"status": "ok"})))
}

async fn delete_system(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    s.db.delete_system(&id)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    s.refresh_cache();
    Ok(Json(serde_json::json!({"status": "deleted"})))
}

// ── Summary ────────────────────────────────────────────

async fn summary(State(s): State<Arc<AppState>>) -> Json<HubSummary> {
    let systems = s.db.list_systems().unwrap_or_default();
    let online = systems
        .iter()
        .filter(|s| s.status == SystemStatus::Online)
        .count();
    let offline = systems
        .iter()
        .filter(|s| s.status == SystemStatus::Offline)
        .count();
    let active_alerts = s.db.count_active_alerts().unwrap_or(0);

    Json(HubSummary {
        total_systems: systems.len(),
        online_count: online,
        offline_count: offline,
        total_alerts_active: active_alerts,
        total_alerts_today: active_alerts, // simplified
        systems,
    })
}

// ── Metrics ────────────────────────────────────────────

#[derive(Deserialize)]
struct MetricsQuery {
    metric: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    since: Option<u64>,
}

fn default_limit() -> usize {
    300
}

async fn get_metrics(
    State(s): State<Arc<AppState>>,
    Path(system_id): Path<String>,
    Query(q): Query<MetricsQuery>,
) -> Json<serde_json::Value> {
    let metric = q.metric.unwrap_or_else(|| "cpu".to_string());

    match s.db.get_metrics(&system_id, &metric, q.limit, q.since) {
        Ok(points) => Json(serde_json::json!({
            "system_id": system_id,
            "metric": metric,
            "points": points,
        })),
        Err(_) => Json(serde_json::json!({
            "system_id": system_id,
            "metric": metric,
            "points": [],
        })),
    }
}

// ── Combined history ──────────────────────────────────

#[derive(Deserialize)]
struct HistoryQuery {
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    since: Option<u64>,
}

async fn get_history(
    State(s): State<Arc<AppState>>,
    Path(system_id): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> Json<SystemHistory> {
    let cpu =
        s.db.get_metrics(&system_id, "cpu", q.limit, q.since)
            .unwrap_or_default();
    let memory =
        s.db.get_metrics(&system_id, "memory", q.limit, q.since)
            .unwrap_or_default();

    let system_name =
        s.db.get_system(&system_id)
            .ok()
            .flatten()
            .map(|sys| sys.name)
            .unwrap_or_default();

    Json(SystemHistory {
        system_id,
        system_name,
        cpu,
        memory,
    })
}

// ── Alerts ─────────────────────────────────────────────

#[derive(Deserialize)]
struct AlertsQuery {
    system_id: Option<String>,
    #[serde(default)]
    acknowledged: Option<bool>,
    #[serde(default = "default_alert_limit")]
    limit: usize,
}

fn default_alert_limit() -> usize {
    100
}

async fn get_alerts(
    State(s): State<Arc<AppState>>,
    Query(q): Query<AlertsQuery>,
) -> Json<Vec<AlertRecord>> {
    let alerts =
        s.db.get_alerts(q.system_id.as_deref(), q.acknowledged, q.limit)
            .unwrap_or_default();
    Json(alerts)
}

async fn acknowledge_alert(
    State(s): State<Arc<AppState>>,
    Path(alert_id): Path<String>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    s.db.acknowledge_alert(&alert_id)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({"status": "acknowledged"})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn temp_state() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Arc::new(Database::new(path.to_str().unwrap()).unwrap());
        (AppState::new(db), dir)
    }

    async fn body_json(res: axum::response::Response) -> serde_json::Value {
        let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn json_request(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let (state, _dir) = temp_state();
        let res = router(state)
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
    }

    #[tokio::test]
    async fn list_systems_empty_then_after_register() {
        let (state, _dir) = temp_state();
        let app = router(state);

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/systems")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let json = body_json(res).await;
        assert_eq!(json.as_array().unwrap().len(), 0);

        let body = serde_json::json!({"name": "web-01", "url": "http://agent.local:9090/"});
        let res = app
            .clone()
            .oneshot(json_request("POST", "/api/systems", body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let created = body_json(res).await;
        assert_eq!(created["name"], "web-01");
        // Trailing slash must be stripped so `{url}/api/system` doesn't end up with `//`.
        assert_eq!(created["url"], "http://agent.local:9090");
        assert!(
            created.get("token").is_none(),
            "token must not be echoed back"
        );

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/systems")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let json = body_json(res).await;
        assert_eq!(json.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn register_system_clamps_poll_interval_to_minimum_five() {
        let (state, _dir) = temp_state();
        let body = serde_json::json!({"name": "w", "url": "http://x", "poll_interval_secs": 1});
        let res = router(state)
            .oneshot(json_request("POST", "/api/systems", body))
            .await
            .unwrap();
        let json = body_json(res).await;
        assert_eq!(json["poll_interval_secs"], 5);
    }

    #[tokio::test]
    async fn get_system_found_and_not_found() {
        let (state, _dir) = temp_state();
        let app = router(state);
        let body = serde_json::json!({"name": "w", "url": "http://x"});
        let res = app
            .clone()
            .oneshot(json_request("POST", "/api/systems", body))
            .await
            .unwrap();
        let created = body_json(res).await;
        let id = created["id"].as_str().unwrap().to_string();

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/systems/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/systems/does-not-exist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn update_system_not_found_returns_404() {
        let (state, _dir) = temp_state();
        let body = serde_json::json!({"name": "renamed"});
        let res = router(state)
            .oneshot(json_request("PUT", "/api/systems/nope", body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn update_system_partial_update_succeeds() {
        let (state, _dir) = temp_state();
        let app = router(state);
        let create_body = serde_json::json!({"name": "w", "url": "http://x"});
        let res = app
            .clone()
            .oneshot(json_request("POST", "/api/systems", create_body))
            .await
            .unwrap();
        let created = body_json(res).await;
        let id = created["id"].as_str().unwrap().to_string();

        let update_body = serde_json::json!({"enabled": false});
        let res = app
            .clone()
            .oneshot(json_request(
                "PUT",
                &format!("/api/systems/{id}"),
                update_body,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/systems/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let json = body_json(res).await;
        assert_eq!(json["enabled"], false);
        assert_eq!(json["name"], "w"); // untouched
    }

    #[tokio::test]
    async fn delete_system_removes_it() {
        let (state, _dir) = temp_state();
        let app = router(state);
        let create_body = serde_json::json!({"name": "w", "url": "http://x"});
        let res = app
            .clone()
            .oneshot(json_request("POST", "/api/systems", create_body))
            .await
            .unwrap();
        let created = body_json(res).await;
        let id = created["id"].as_str().unwrap().to_string();

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/systems/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/systems/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn summary_reports_counts() {
        let (state, _dir) = temp_state();
        state
            .db
            .insert_system(&SystemInfo {
                id: "id-1".into(),
                name: "online-sys".into(),
                url: "http://x".into(),
                token: String::new(),
                status: SystemStatus::Online,
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
            })
            .unwrap();

        let res = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/summary")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let json = body_json(res).await;
        assert_eq!(json["total_systems"], 1);
        assert_eq!(json["online_count"], 1);
        assert_eq!(json["offline_count"], 0);
    }

    #[tokio::test]
    async fn get_metrics_returns_seeded_points() {
        let (state, _dir) = temp_state();
        state
            .db
            .insert_system(&SystemInfo {
                id: "id-1".into(),
                name: "sys".into(),
                url: "http://x".into(),
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
            })
            .unwrap();
        state.db.insert_metric("id-1", "cpu", 42.0, 100).unwrap();

        let res = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/systems/id-1/metrics?metric=cpu")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let json = body_json(res).await;
        assert_eq!(json["points"][0]["value"], 42.0);
    }

    #[tokio::test]
    async fn get_metrics_defaults_to_cpu_metric() {
        let (state, _dir) = temp_state();
        let res = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/systems/id-1/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let json = body_json(res).await;
        assert_eq!(json["metric"], "cpu");
    }

    #[tokio::test]
    async fn get_history_combines_cpu_and_memory() {
        let (state, _dir) = temp_state();
        state
            .db
            .insert_system(&SystemInfo {
                id: "id-1".into(),
                name: "sys".into(),
                url: "http://x".into(),
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
            })
            .unwrap();
        state.db.insert_metric("id-1", "cpu", 10.0, 1).unwrap();
        state.db.insert_metric("id-1", "memory", 20.0, 1).unwrap();

        let res = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/systems/id-1/history")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let json = body_json(res).await;
        assert_eq!(json["system_name"], "sys");
        assert_eq!(json["cpu"][0]["value"], 10.0);
        assert_eq!(json["memory"][0]["value"], 20.0);
    }

    #[tokio::test]
    async fn alerts_endpoints_filter_and_acknowledge() {
        let (state, _dir) = temp_state();
        state
            .db
            .insert_system(&SystemInfo {
                id: "id-1".into(),
                name: "sys".into(),
                url: "http://x".into(),
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
            })
            .unwrap();
        state
            .db
            .insert_alert(&AlertRecord {
                id: "a1".into(),
                system_id: "id-1".into(),
                system_name: "sys".into(),
                severity: "warning".into(),
                message: "m".into(),
                current_value: 1.0,
                fired_at: "t".into(),
                stored_at: "t1".into(),
                acknowledged: false,
            })
            .unwrap();
        let app = router(state);

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
        assert_eq!(json.as_array().unwrap().len(), 1);

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/alerts/a1/acknowledge")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/alerts?acknowledged=true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let json = body_json(res).await;
        assert_eq!(json.as_array().unwrap().len(), 1);
        assert_eq!(json[0]["acknowledged"], true);
    }
}
