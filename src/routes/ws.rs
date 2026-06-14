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
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;

use crate::collectors;
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

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let snap = collectors::system::collect();

                let alerts = {
                    let mgr = state.alert_manager.read();
                    mgr.active_alerts.clone()
                };

                let payload = serde_json::json!({
                    "type": "system",
                    "timestamp": snap.uptime_seconds,
                    "cpu_percent": snap.cpu.usage_percent,
                    "cpu_logical_cores": snap.cpu.logical_cores,
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

                let msg = Message::Text(serde_json::to_string(&payload).unwrap_or_default());
                if socket.send(msg).await.is_err() {
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
                        if let Ok(req) = serde_json::from_str::<serde_json::Value>(&txt) {
                            if req.get("type").and_then(|v| v.as_str()) == Some("ping") {
                                let _ = socket.send(Message::Text(
                                    serde_json::json!({"type":"pong"}).to_string()
                                )).await;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    tracing::info!("WebSocket client disconnected");
}
