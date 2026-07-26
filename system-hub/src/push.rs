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
                    if let Some(sys) = state.db.get_system(&system_id).ok().flatten()
                        && sys.name.len() <= 8
                        && sys.name == system_id[..8]
                    {
                        let _ = state.db.update_system_config(
                            &system_id,
                            Some(&payload.hostname),
                            None,
                            None,
                            None,
                            None,
                        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde::Serialize;
    use tower::ServiceExt;

    fn temp_state() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Arc::new(Database::new(path.to_str().unwrap()).unwrap());
        (AppState::new(db), dir)
    }

    #[test]
    fn auth_message_deserializes_from_json() {
        let msg: AuthMessage =
            serde_json::from_str(r#"{"type":"auth","system_id":"sys-1","token":"tok"}"#).unwrap();
        assert_eq!(msg.msg_type, "auth");
        assert_eq!(msg.system_id, "sys-1");
        assert_eq!(msg.token, "tok");
    }

    #[test]
    fn auth_message_token_defaults_to_empty() {
        let msg: AuthMessage =
            serde_json::from_str(r#"{"type":"auth","system_id":"sys-1"}"#).unwrap();
        assert_eq!(msg.token, "");
    }

    #[derive(Serialize)]
    struct MirroredDisk {
        mount_point: String,
        usage_percent: f32,
        total_display: String,
        used_display: String,
    }

    #[derive(Serialize)]
    struct MirroredProcess {
        pid: u32,
        name: String,
        cpu_usage: f32,
        memory_usage_display: String,
        memory_percent: f32,
    }

    /// Mirrors `PushPayload`'s field order exactly, so a MessagePack encoding of this
    /// struct can be decoded by the real (deserialize-only) `PushPayload`.
    #[derive(Serialize)]
    struct MirroredPushPayload {
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
        disks: Vec<MirroredDisk>,
        top_processes: Vec<MirroredProcess>,
        timestamp: u64,
    }

    #[test]
    fn push_payload_decodes_from_messagepack_wire_format() {
        let mirrored = MirroredPushPayload {
            system_id: "sys-1".into(),
            hostname: "host1".into(),
            os_name: "Ubuntu".into(),
            kernel: "6.6.0".into(),
            cpu_percent: 11.0,
            cpu_cores: 4,
            cpu_model: "Generic".into(),
            memory_percent: 22.0,
            memory_used_display: "1 GB".into(),
            memory_total_display: "4 GB".into(),
            memory_used_bytes: 1_000,
            memory_total_bytes: 4_000,
            swap_percent: 0.0,
            load_one: 0.1,
            load_five: 0.2,
            load_fifteen: 0.3,
            uptime_seconds: 100,
            uptime_display: "1m".into(),
            disks: vec![MirroredDisk {
                mount_point: "/".into(),
                usage_percent: 50.0,
                total_display: "10G".into(),
                used_display: "5G".into(),
            }],
            top_processes: vec![],
            timestamp: 1_700_000_000,
        };
        let packed = rmp_serde::to_vec(&mirrored).unwrap();
        let payload: PushPayload = rmp_serde::from_slice(&packed).unwrap();
        assert_eq!(payload.system_id, "sys-1");
        assert_eq!(payload.cpu_cores, 4);
        assert_eq!(payload.disks[0].mount_point, "/");
    }

    #[tokio::test]
    async fn push_upgrade_request_without_headers_is_rejected() {
        let (state, _dir) = temp_state();
        let res = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/push")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn push_upgrade_request_with_headers_but_no_real_connection_is_not_upgradable() {
        // `oneshot()` doesn't drive a real hyper connection, so there's no `OnUpgrade`
        // extension available even with a fully valid handshake -- axum reports 426.
        // The full auth handshake is covered by `push_connects_authenticates_and_registers_system`
        // below, which runs a real server.
        let (state, _dir) = temp_state();
        let res = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/push")
                    .header("Connection", "Upgrade")
                    .header("Upgrade", "websocket")
                    .header("Sec-WebSocket-Version", "13")
                    .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UPGRADE_REQUIRED);
    }

    /// Runs both the "no token configured" and "wrong token" scenarios in one test
    /// function. HUB_PUSH_TOKEN is process-global and no other test touches it, but
    /// keeping both env mutations sequential in a single test (rather than two tests
    /// that `cargo test` could schedule concurrently) avoids racing on it, without
    /// needing to hold a lock guard across `.await` points.
    #[tokio::test]
    async fn push_auth_handshake_accepts_and_rejects_tokens() {
        use futures_util::{SinkExt, StreamExt};

        async fn connect_and_auth(
            addr: std::net::SocketAddr,
            system_id: &str,
            token: &str,
        ) -> serde_json::Value {
            let url = format!("ws://{addr}/api/push");
            let (mut ws_stream, resp) = tokio_tungstenite::connect_async(url).await.unwrap();
            assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);

            let auth_msg = serde_json::json!({
                "type": "auth",
                "system_id": system_id,
                "token": token,
            });
            ws_stream
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    auth_msg.to_string(),
                ))
                .await
                .unwrap();

            let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws_stream.next())
                .await
                .expect("timed out waiting for auth response")
                .expect("stream ended unexpectedly")
                .unwrap();

            match msg {
                tokio_tungstenite::tungstenite::Message::Text(txt) => {
                    serde_json::from_str(&txt).unwrap()
                }
                other => panic!("expected a text message, got {other:?}"),
            }
        }

        // Scenario 1: no HUB_PUSH_TOKEN configured -> any token is accepted.
        unsafe {
            std::env::remove_var("HUB_PUSH_TOKEN");
        }
        let (state, _dir) = temp_state();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(state.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let resp = connect_and_auth(addr, "test-sys-123", "").await;
        assert_eq!(resp["type"], "auth_ok");
        assert!(state.db.get_system("test-sys-123").unwrap().is_some());

        // Scenario 2: HUB_PUSH_TOKEN configured -> mismatched token is rejected.
        unsafe {
            std::env::set_var("HUB_PUSH_TOKEN", "expected-token");
        }
        let (state2, _dir2) = temp_state();
        let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr2 = listener2.local_addr().unwrap();
        let app2 = router(state2);
        tokio::spawn(async move {
            let _ = axum::serve(listener2, app2).await;
        });

        let resp = connect_and_auth(addr2, "test-sys-456", "wrong-token").await;
        assert_eq!(resp["type"], "auth_error");

        unsafe {
            std::env::remove_var("HUB_PUSH_TOKEN");
        }
    }
}
