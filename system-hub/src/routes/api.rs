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
use crate::registry::PollInterval;
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
    Ok(Json(sys))
}

async fn update_system(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<UpdateSystemPayload>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    // Parsed before anything is stored: a refused interval refuses the whole body.
    let poll_interval = payload
        .poll_interval_secs
        .map(PollInterval::try_from)
        .transpose()
        .map_err(|_| axum::http::StatusCode::UNPROCESSABLE_ENTITY)?;

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
        poll_interval,
        payload.enabled,
    )
    .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(serde_json::json!({"status": "ok"})))
}

/// Takes the stored id as it is, deliberately not parsed into `SystemId`: rows stored before
/// RFC 0005 may hold ids that break its rule, and this route is how an operator removes them.
async fn delete_system(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    s.db.delete_system(&id)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    // After the row: a store racing this delete then finds the system gone instead of
    // recreating an entry nothing would evict (RFC 0007 §4).
    drop(s.evict_live_metrics(&id));
    s.live_applications
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&id);
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

    async fn register(app: &Router, name: &str) -> String {
        let body = serde_json::json!({"name": name, "url": "http://x"});
        let res = app
            .clone()
            .oneshot(json_request("POST", "/api/systems", body))
            .await
            .unwrap();
        body_json(res).await["id"].as_str().unwrap().to_string()
    }

    async fn put_poll_interval(app: &Router, id: &str, raw: serde_json::Value) -> StatusCode {
        let body = serde_json::json!({"poll_interval_secs": raw});
        app.clone()
            .oneshot(json_request("PUT", &format!("/api/systems/{id}"), body))
            .await
            .unwrap()
            .status()
    }

    // This pins the PUT edge's behaviour only. That the handler parses the body
    // into `PollInterval` and `update_system_config` binds its value (no `as i64`
    // cast) is enforced at the type level, by `update_system_config` taking
    // `Option<PollInterval>`, and is for rosette-auditor to verify.
    #[tokio::test]
    async fn update_system_stores_only_a_poll_interval_the_row_can_hold() {
        let i64_max = i64::MAX as u64;
        let cases = [
            (
                "the column's maximum",
                serde_json::json!(i64_max),
                StatusCode::OK,
                i64_max,
            ),
            (
                "one past the column's maximum",
                serde_json::json!(i64_max + 1),
                StatusCode::UNPROCESSABLE_ENTITY,
                10,
            ),
            (
                "two past the column's maximum",
                serde_json::json!(i64_max + 2),
                StatusCode::UNPROCESSABLE_ENTITY,
                10,
            ),
            (
                "the top bit plus change",
                serde_json::json!((1u64 << 63) | 0x1234_5678),
                StatusCode::UNPROCESSABLE_ENTITY,
                10,
            ),
            (
                "one below u64::MAX",
                serde_json::json!(u64::MAX - 1),
                StatusCode::UNPROCESSABLE_ENTITY,
                10,
            ),
            (
                "u64::MAX",
                serde_json::json!(u64::MAX),
                StatusCode::UNPROCESSABLE_ENTITY,
                10,
            ),
            // Pins current behaviour, not the new rule: serde already refuses a
            // negative number for the body's u64, so this row passes on the stub.
            (
                "a negative interval",
                serde_json::json!(-1),
                StatusCode::UNPROCESSABLE_ENTITY,
                10,
            ),
            (
                "an ordinary interval",
                serde_json::json!(30),
                StatusCode::OK,
                30,
            ),
        ];
        for (name, raw, expected_status, expected_interval) in cases {
            let (state, _dir) = temp_state();
            let app = router(state.clone());
            let id = register(&app, "w").await;

            let status = put_poll_interval(&app, &id, raw).await;

            assert_eq!(status, expected_status, "{name}: status");
            let stored = state.db.get_system(&id);
            assert!(stored.is_ok(), "{name}: get_system maps the row");
            let stored = stored.ok().flatten().map(|s| s.poll_interval_secs);
            assert_eq!(stored, Some(expected_interval), "{name}: stored interval");
            let listed = state.db.list_systems();
            assert!(listed.is_ok(), "{name}: list_systems maps every row");
            let intervals: Vec<u64> = listed
                .unwrap_or_default()
                .iter()
                .map(|s| s.poll_interval_secs)
                .collect();
            assert_eq!(
                intervals,
                vec![expected_interval],
                "{name}: listed intervals"
            );
        }

        // Every value above the column's range is refused, not only the tabled ones.
        let (state, _dir) = temp_state();
        let app = router(state.clone());
        let id = register(&app, "w").await;
        for k in 0..63 {
            let raw = serde_json::json!(i64_max + (1u64 << k));

            let status = put_poll_interval(&app, &id, raw).await;

            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "i64::MAX + 2^{k}: status"
            );
            let stored = state.db.get_system(&id);
            assert!(stored.is_ok(), "i64::MAX + 2^{k}: get_system maps the row");
            let stored = stored.ok().flatten().map(|s| s.poll_interval_secs);
            assert_eq!(stored, Some(10), "i64::MAX + 2^{k}: stored interval");
            let listed = state.db.list_systems();
            assert!(
                listed.is_ok(),
                "i64::MAX + 2^{k}: list_systems maps every row"
            );
            let intervals: Vec<u64> = listed
                .unwrap_or_default()
                .iter()
                .map(|s| s.poll_interval_secs)
                .collect();
            assert_eq!(intervals, vec![10], "i64::MAX + 2^{k}: listed intervals");
        }
    }

    // A refused interval refuses the whole body: the other fields sent with it
    // are not stored either.
    #[tokio::test]
    async fn update_system_refusing_a_poll_interval_stores_no_other_field() {
        let (state, _dir) = temp_state();
        let app = router(state.clone());
        let id = register(&app, "w").await;
        let token_before = state.db.get_system(&id).ok().flatten().map(|s| s.token);
        let body = serde_json::json!({
            "name": "renamed",
            "url": "http://renamed",
            "token": "a-replacement-token",
            "enabled": false,
            "poll_interval_secs": i64::MAX as u64 + 1,
        });

        let res = app
            .clone()
            .oneshot(json_request("PUT", &format!("/api/systems/{id}"), body))
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let stored = state.db.get_system(&id).ok().flatten();
        let token_after = stored.as_ref().map(|s| s.token.clone());
        let fields = stored.map(|s| (s.name, s.url, s.enabled, s.poll_interval_secs));
        assert_eq!(
            fields,
            Some(("w".to_string(), "http://x".to_string(), true, 10))
        );
        assert_eq!(token_after, token_before);
        assert_ne!(token_after.as_deref(), Some("a-replacement-token"));
    }

    /// Plants a live metrics entry and an applications entry for `system`.
    fn plant_live_state(state: &AppState, system: &str) {
        use crate::snapshot::{LiveMetrics, ReportedSnapshot, SnapshotTime, snapshot_rule};
        let (snapshot, left_out) = snapshot_rule(ReportedSnapshot::default());
        let time = SnapshotTime::try_from(1_700_000_000).unwrap();
        let now = std::time::Instant::now();
        let (entry, _) = LiveMetrics::following(None, snapshot, time, &left_out, now);
        let mut live = state.live_metrics.write().unwrap();
        live.insert(system.to_string(), Arc::new(entry));
        let mut applications = state.live_applications.write().unwrap();
        applications.insert(
            system.to_string(),
            crate::state::SystemApplications::default(),
        );
    }

    /// How a system reaches the hub.
    #[derive(Debug, Clone, Copy)]
    enum Source {
        Poll,
        Push,
    }

    /// Registers a polled system through the API, or a push system as its handshake does.
    async fn registered(state: &Arc<AppState>, source: Source) -> String {
        match source {
            Source::Poll => {
                let body = serde_json::json!({"name": "w", "url": "http://x"});
                let request = json_request("POST", "/api/systems", body);
                let res = router(state.clone()).oneshot(request).await.unwrap();
                body_json(res).await["id"].as_str().unwrap().to_string()
            }
            Source::Push => {
                state
                    .db
                    .insert_system(&legacy_push_system("sys-push"))
                    .unwrap();
                "sys-push".to_string()
            }
        }
    }

    /// RFC 0007 §4: a delete evicts the system's live metrics and applications, whichever
    /// path it reports through, and no other system's.
    #[tokio::test]
    async fn deleting_a_system_forgets_its_live_metrics_and_applications() {
        for source in [Source::Poll, Source::Push] {
            let (state, _dir) = temp_state();
            let id = registered(&state, source).await;
            for system in [id.as_str(), "other"] {
                plant_live_state(&state, system);
            }

            let res = router(state.clone())
                .oneshot(
                    Request::builder()
                        .method("DELETE")
                        .uri(format!("/api/systems/{id}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(res.status(), StatusCode::OK, "{source:?}");
            let live = state.live_metrics.read().unwrap();
            let applications = state.live_applications.read().unwrap();
            let kept =
                |system: &str| (live.contains_key(system), applications.contains_key(system));
            assert_eq!(
                kept(&id),
                (false, false),
                "{source:?}: the deleted system's"
            );
            assert_eq!(kept("other"), (true, true), "{source:?}: another system's");
        }
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

    /// Rows stored before RFC 0005 may hold a dot-segment id, which no dashboard URL can
    /// reach; the operator's way out is a percent-encoded delete, whose captured segment the
    /// router decodes before the handler sees it.
    #[tokio::test]
    async fn a_stored_dot_segment_system_is_deleted_through_its_percent_encoded_id() {
        let cases = [
            ("single dot", ".", "/api/systems/%2E"),
            ("single dot, lowercase", ".", "/api/systems/%2e"),
            ("double dot", "..", "/api/systems/%2E%2E"),
            ("double dot, mixed case", "..", "/api/systems/%2e%2E"),
            // The path is decoded exactly once, which is why a `%2e`-style id is safe.
            ("encoded dot stays one id", "%2e", "/api/systems/%252e"),
        ];
        // Bystanders: the other dot ids, and ids a prefix or "delete everything" would hit.
        let stored = [".", "..", ".hidden", "a.b", "%2e"];
        for (name, target, uri) in cases {
            let (state, _dir) = temp_state();
            for id in stored {
                state.db.insert_system(&legacy_push_system(id)).unwrap();
            }

            let res = router(state.clone())
                .oneshot(
                    Request::builder()
                        .method("DELETE")
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(res.status(), StatusCode::OK, "{name}");
            // The handler answers 200 whatever it deleted, so the rows are what show that the
            // decoded id matched exactly the target and nothing else.
            let remaining: Vec<String> = state
                .db
                .list_systems()
                .unwrap()
                .into_iter()
                .map(|system| system.id)
                .collect();
            let mut expected: Vec<&str> = stored.into_iter().filter(|id| *id != target).collect();
            let mut remaining: Vec<&str> = remaining.iter().map(String::as_str).collect();
            expected.sort_unstable();
            remaining.sort_unstable();
            assert_eq!(remaining, expected, "{name}");
        }
    }

    fn legacy_push_system(id: &str) -> SystemInfo {
        SystemInfo {
            id: id.into(),
            name: "legacy".into(),
            url: "push://".into(),
            token: String::new(),
            status: SystemStatus::Offline,
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
        state.db.plant_point("id-1", "cpu", 42.0, 100).unwrap();

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
        state.db.plant_point("id-1", "cpu", 10.0, 1).unwrap();
        state.db.plant_point("id-1", "memory", 20.0, 1).unwrap();

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
