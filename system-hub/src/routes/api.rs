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
        .route("/api/systems/{id}", get(get_system))
        .route("/api/systems/{id}", put(update_system))
        .route("/api/systems/{id}", delete(delete_system))
        // Summary
        .route("/api/summary", get(summary))
        // Metrics
        .route("/api/systems/{id}/metrics", get(get_metrics))
        .route("/api/systems/{id}/history", get(get_history))
        // Alerts
        .route("/api/alerts", get(get_alerts))
        .route(
            "/api/alerts/{alert_id}/acknowledge",
            post(acknowledge_alert),
        )
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
