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
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
    routing::get,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::state::{AppState, LiveMetrics};

/// Deserialized from MessagePack binary payloads sent by agents.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct PushPayload {
    system_id: String,
    hostname: String,
    os_name: String,
    kernel: String,
    cpu_percent: f32,
    cpu_cores: usize,
    cpu_model: String,
    memory_percent: f32,
    memory_used_display: String,
    memory_total_display: String,
    memory_used_bytes: u64,
    memory_total_bytes: u64,
    swap_percent: f32,
    load_one: f64,
    load_five: f64,
    load_fifteen: f64,
    uptime_seconds: u64,
    uptime_display: String,
    disks: Vec<DiskItem>,
    #[serde(default)]
    top_processes: Vec<ProcessItem>,
    timestamp: u64,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct DiskItem {
    mount_point: String,
    usage_percent: f32,
    total_display: String,
    used_display: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct ProcessItem {
    pid: u32,
    name: String,
    cpu_usage: f32,
    memory_usage_display: String,
    memory_percent: f32,
}

#[derive(Debug, Deserialize)]
struct AuthMessage {
    #[serde(rename = "type")]
    msg_type: String,
    system_id: String,
    #[serde(default)]
    token: String,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/push", get(push_handler))
        .with_state(state)
}

async fn push_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let expected_token = std::env::var("HUB_PUSH_TOKEN").unwrap_or_default();
    ws.on_upgrade(move |socket| handle_push(socket, state, expected_token))
}

async fn handle_push(mut socket: WebSocket, state: Arc<AppState>, expected_token: String) {
    tracing::info!("Push client connected");

    // Wait for auth message
    let system_id = match socket.recv().await {
        Some(Ok(Message::Text(text))) => {
            match serde_json::from_str::<AuthMessage>(&text) {
                Ok(auth) if auth.msg_type == "auth" => {
                    if !expected_token.is_empty() && auth.token != expected_token {
                        let _ = socket
                            .send(Message::Text(
                                r#"{"type":"auth_error","message":"invalid token"}"#.into(),
                            ))
                            .await;
                        return;
                    }
                    // Register or update the system in DB
                    let existing = state.db.get_system(&auth.system_id).ok().flatten();
                    if existing.is_none() {
                        let sys = crate::models::SystemInfo {
                            id: auth.system_id.clone(),
                            name: auth.system_id[..8].to_string(),
                            url: "push://".to_string(),
                            token: String::new(),
                            status: crate::models::SystemStatus::Online,
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
                        };
                        let _ = state.db.insert_system(&sys);
                    }
                    let _ = socket
                        .send(Message::Text(r#"{"type":"auth_ok"}"#.into()))
                        .await;
                    auth.system_id
                }
                _ => {
                    let _ = socket
                        .send(Message::Text(
                            r#"{"type":"auth_error","message":"expected auth message"}"#.into(),
                        ))
                        .await;
                    return;
                }
            }
        }
        _ => return,
    };

    tracing::info!("Push client authenticated: {system_id}");

    // Process binary messages
    while let Some(msg) = socket.recv().await {
        match msg {
            Ok(Message::Binary(data)) => {
                if let Ok(payload) = rmp_serde::from_slice::<PushPayload>(&data) {
                    let now = payload.timestamp;

                    // Store metrics
                    let _ = state
                        .db
                        .insert_metric(&system_id, "cpu", payload.cpu_percent, now);
                    let _ =
                        state
                            .db
                            .insert_metric(&system_id, "memory", payload.memory_percent, now);
                    let _ = state
                        .db
                        .insert_metric(&system_id, "swap", payload.swap_percent, now);
                    let _ =
                        state
                            .db
                            .insert_metric(&system_id, "load1", payload.load_one as f32, now);
                    let _ =
                        state
                            .db
                            .insert_metric(&system_id, "load5", payload.load_five as f32, now);

                    for disk in &payload.disks {
                        let metric = format!("disk:{}", disk.mount_point);
                        let _ =
                            state
                                .db
                                .insert_metric(&system_id, &metric, disk.usage_percent, now);
                    }

                    // Update live metrics cache
                    {
                        let mut live = state.live_metrics.write().unwrap();
                        live.insert(
                            system_id.clone(),
                            LiveMetrics {
                                cpu_percent: payload.cpu_percent,
                                memory_percent: payload.memory_percent,
                                load_one: payload.load_one,
                                disks: payload
                                    .disks
                                    .iter()
                                    .map(|d| (d.mount_point.clone(), d.usage_percent))
                                    .collect(),
                                updated_at: now,
                            },
                        );
                    }

                    // Update system info if needed
                    let existing = state.db.get_system(&system_id).ok().flatten();
                    if let Some(sys) = existing {
                        if sys.hostname.is_none() || sys.os.is_none() {
                            let _ = state.db.update_system_info(
                                &system_id,
                                Some(&payload.os_name),
                                Some(&payload.hostname),
                                Some(&payload.kernel),
                                Some(&payload.cpu_model),
                                Some(payload.cpu_cores),
                                Some(&payload.memory_total_display),
                                Some(payload.memory_total_bytes),
                            );
                        }
                        let _ = state.db.update_system_status(
                            &system_id,
                            &crate::models::SystemStatus::Online,
                            &payload.uptime_display,
                            None,
                        );
                    }

                    // Re-register system name if still default
                    if let Some(sys) = state.db.get_system(&system_id).ok().flatten() {
                        if sys.name.len() <= 8 && sys.name == system_id[..8] {
                            let _ = state.db.update_system_config(
                                &system_id,
                                Some(&payload.hostname),
                                None,
                                None,
                                None,
                                None,
                            );
                        }
                    }

                    state.refresh_cache();
                }
            }
            Ok(Message::Ping(data)) => {
                let _ = socket.send(Message::Pong(data)).await;
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }

    // Mark offline on disconnect
    let _ = state.db.update_system_status(
        &system_id,
        &crate::models::SystemStatus::Offline,
        "",
        Some("push disconnected"),
    );
    state.refresh_cache();
    tracing::info!("Push client disconnected: {system_id}");
}
