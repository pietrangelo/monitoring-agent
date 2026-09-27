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
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;

use crate::collectors::monotonic_now;
use crate::snapshot::{PublishedSnapshot, StreamEmit, StreamState};
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/ws/system", get(ws_handler))
        .with_state(state)
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: Arc<AppState>) {
    tracing::info!("WebSocket client connected");

    let mut tick = interval(Duration::from_secs(2));
    // A stale snapshot is told once, then nothing until a fresh one; the socket stays open.
    let mut told = StreamState::Live;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let msg = match told.next(state.snapshot(), monotonic_now()) {
                    StreamEmit::Snapshot(snapshot) => system_message(&snapshot, &state),
                    StreamEmit::StaleNotice => serde_json::json!({ "type": "stale" }).to_string(),
                    StreamEmit::Nothing => continue,
                };
                if socket.send(Message::Text(msg)).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(data))) => {
                        let _ = socket.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Text(txt))) => {
                        if let Ok(req) = serde_json::from_str::<serde_json::Value>(&txt)
                            && req.get("type").and_then(|v| v.as_str()) == Some("ping")
                        {
                            let _ = socket
                                .send(Message::Text(serde_json::json!({"type":"pong"}).to_string()))
                                .await;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    tracing::info!("WebSocket client disconnected");
}

/// The live message for one fresh snapshot, with the active alerts.
fn system_message(published: &PublishedSnapshot, state: &AppState) -> String {
    let snap = &published.system;
    let alerts = state.alert_manager.read().active_alerts().to_vec();
    let payload = serde_json::json!({
        "type": "system",
        "timestamp": snap.uptime_seconds,
        "collected_at": published.collected_at,
        "cpu_percent": snap.cpu.usage_percent,
        "cpu_logical_cores": snap.cpu.logical_cores,
        "cpu_capacity_cpus": snap.cpu.capacity_cpus,
        "memory_percent": snap.memory.usage_percent,
        "memory_used_display": snap.memory.used_display,
        "memory_total_display": snap.memory.total_display,
        "memory_used_bytes": snap.memory.used_bytes,
        "memory_total_bytes": snap.memory.total_bytes,
        "swap_percent": snap.swap.usage_percent,
        "load_one": snap.load_average.one,
        "load_five": snap.load_average.five,
        "load_fifteen": snap.load_average.fifteen,
        "uptime_display": snap.uptime_display,
        "disks": &snap.disks,
        "top_processes": &snap.top_processes[..10.min(snap.top_processes.len())],
        "alerts": &alerts,
    });
    serde_json::to_string(&payload).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn ws_upgrade_request_without_headers_is_rejected() {
        let state = AppState::new(crate::snapshot::fixtures::receiver());
        let res = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/ws/system")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Missing Upgrade/Connection/Sec-WebSocket-* headers -> extractor rejection.
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn ws_upgrade_request_with_headers_but_no_real_connection_is_not_upgradable() {
        // `oneshot()` doesn't drive a real hyper connection, so there's no `OnUpgrade`
        // extension available even with a fully valid handshake -- axum reports 426.
        // The 101 path is covered by `ws_connects_and_streams_system_payloads` below,
        // which runs a real server.
        let state = AppState::new(crate::snapshot::fixtures::receiver());
        let res = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/ws/system")
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

    #[tokio::test]
    async fn ws_connects_and_streams_system_payloads() {
        use futures_util::StreamExt;

        let fixture = crate::snapshot::fixtures::published();
        let state = AppState::new(crate::snapshot::fixtures::receiver());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(state);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let url = format!("ws://{addr}/api/ws/system");
        let (mut ws_stream, resp) = tokio_tungstenite::connect_async(url).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);

        // The background tick fires immediately on the first `interval().tick()` call.
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws_stream.next())
            .await
            .expect("timed out waiting for first message")
            .expect("stream ended unexpectedly")
            .unwrap();

        match msg {
            tokio_tungstenite::tungstenite::Message::Text(txt) => {
                let json: serde_json::Value = serde_json::from_str(&txt).unwrap();
                assert_eq!(json["type"], "system");
                assert_eq!(json["collected_at"], fixture.collected_at);
                assert_eq!(json["cpu_percent"], fixture.system.cpu.usage_percent);
                assert_eq!(json["cpu_capacity_cpus"], fixture.system.cpu.capacity_cpus);
                assert_eq!(
                    json["top_processes"],
                    serde_json::to_value(&fixture.system.top_processes).unwrap()
                );
            }
            other => panic!("expected a text message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_stale_snapshot_is_told_once_and_the_socket_waits_for_a_fresh_one() {
        use crate::snapshot::fixtures;
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message as Received;

        let (publisher, snapshots) = fixtures::channel(fixtures::stale());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(AppState::new(snapshots));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let url = format!("ws://{addr}/api/ws/system");
        let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let mut next_type = async |wait: u64| {
            let wait = std::time::Duration::from_millis(wait);
            match tokio::time::timeout(wait, ws.next()).await {
                Ok(Some(Ok(Received::Text(txt)))) => {
                    let json: serde_json::Value = serde_json::from_str(&txt).unwrap();
                    Some(json["type"].as_str().unwrap_or_default().to_string())
                }
                Ok(other) => panic!("expected a text message, got {other:?}"),
                Err(_) => None,
            }
        };
        assert_eq!(
            next_type(1_000).await.as_deref(),
            Some("stale"),
            "told at once"
        );
        assert_eq!(
            next_type(2_500).await,
            None,
            "nothing more on the next tick"
        );
        publisher.send_replace(Arc::new(fixtures::published()));
        assert_eq!(
            next_type(2_500).await.as_deref(),
            Some("system"),
            "the same socket resumes with a fresh snapshot"
        );
    }

    #[tokio::test]
    async fn unknown_ws_path_is_not_found() {
        let state = AppState::new(crate::snapshot::fixtures::receiver());
        let res = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/ws/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
}
