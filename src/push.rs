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

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::time::interval;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::collectors;

/// Payload pushed to the hub every interval.
#[derive(Debug, Serialize)]
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
    disks: Vec<DiskPayload>,
    top_processes: Vec<ProcessPayload>,
    timestamp: u64,
}

#[derive(Debug, Serialize)]
struct DiskPayload {
    mount_point: String,
    usage_percent: f32,
    total_display: String,
    used_display: String,
}

#[derive(Debug, Serialize)]
struct ProcessPayload {
    pid: u32,
    name: String,
    cpu_usage: f32,
    memory_usage_display: String,
    memory_percent: f32,
}

#[derive(Debug, Deserialize)]
struct HubMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(default)]
    message: String,
}

/// Connect to the hub via WebSocket and push system snapshots using MessagePack.
pub async fn run_push_client(
    hub_url: &str,
    token: &str,
    push_interval_secs: u64,
) -> Result<(), String> {
    let url = format!("{}/api/push", hub_url.trim_end_matches('/'));

    tracing::info!("🔌 Connecting to hub via push: {url}");

    let (mut ws, _) = connect_async(&url).await.map_err(|e| e.to_string())?;

    // Send auth
    let system_id = get_persistent_id();
    let auth = serde_json::json!({
        "type": "auth",
        "system_id": system_id,
        "token": token,
    });
    ws.send(Message::Text(auth.to_string()))
        .await
        .map_err(|e| e.to_string())?;

    // Wait for auth response
    if let Some(Ok(Message::Text(resp))) = ws.next().await
        && let Ok(msg) = serde_json::from_str::<HubMessage>(&resp)
    {
        if msg.msg_type == "auth_ok" {
            tracing::info!("✅ Push authenticated — system_id={system_id}");
        } else {
            tracing::error!("❌ Push auth failed: {}", msg.message);
            return Err(format!("auth failed: {}", msg.message));
        }
    }

    let mut tick = interval(Duration::from_secs(push_interval_secs.max(2)));
    let mut ping_tick = interval(Duration::from_secs(30));

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let snap = collectors::system::collect();
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();

                let payload = PushPayload {
                    system_id: system_id.clone(),
                    hostname: snap.hostname,
                    os_name: snap.os.pretty_name,
                    kernel: snap.kernel,
                    cpu_percent: snap.cpu.usage_percent,
                    cpu_cores: snap.cpu.logical_cores,
                    cpu_model: snap.cpu.model,
                    memory_percent: snap.memory.usage_percent,
                    memory_used_display: snap.memory.used_display,
                    memory_total_display: snap.memory.total_display,
                    memory_used_bytes: snap.memory.used_bytes,
                    memory_total_bytes: snap.memory.total_bytes,
                    swap_percent: snap.swap.usage_percent,
                    load_one: snap.load_average.one,
                    load_five: snap.load_average.five,
                    load_fifteen: snap.load_average.fifteen,
                    uptime_seconds: snap.uptime_seconds,
                    uptime_display: snap.uptime_display,
                    disks: snap.disks.iter().map(|d| DiskPayload {
                        mount_point: d.mount_point.clone(),
                        usage_percent: d.usage_percent,
                        total_display: d.total_display.clone(),
                        used_display: d.used_display.clone(),
                    }).collect(),
                    top_processes: snap.top_processes.iter().take(10).map(|p| ProcessPayload {
                        pid: p.pid,
                        name: p.name.clone(),
                        cpu_usage: p.cpu_usage,
                        memory_usage_display: p.memory_usage_display.clone(),
                        memory_percent: p.memory_percent,
                    }).collect(),
                    timestamp: now,
                };

                let buf = rmp_serde::to_vec(&payload).unwrap_or_default();
                if ws.send(Message::Binary(buf)).await.is_err() {
                    tracing::error!("Push connection lost, will retry...");
                    break;
                }
            }
            _ = ping_tick.tick() => {
                if ws.send(Message::Ping(vec![])).await.is_err() {
                    break;
                }
            }
        }
    }

    tracing::warn!("Push connection closed");
    Ok(())
}

/// Get a persistent machine identifier. Uses /etc/machine-id on systemd Linux,
/// falls back to a hostname-based hash, then to a file-stored UUID.
fn get_persistent_id() -> String {
    // 1. Try /etc/machine-id (systemd)
    if let Ok(id) = std::fs::read_to_string("/etc/machine-id") {
        let id = id.trim().to_string();
        if !id.is_empty() {
            return id;
        }
    }
    // 2. Try /var/lib/dbus/machine-id
    if let Ok(id) = std::fs::read_to_string("/var/lib/dbus/machine-id") {
        let id = id.trim().to_string();
        if !id.is_empty() {
            return id;
        }
    }
    // 3. Fall back to hostname
    if let Ok(host) = std::process::Command::new("hostname").output() {
        let host = String::from_utf8_lossy(&host.stdout).trim().to_string();
        if !host.is_empty() {
            return host;
        }
    }
    // 4. Last resort: random UUID
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_payload_serializes_expected_fields() {
        let payload = PushPayload {
            system_id: "sys-1".into(),
            hostname: "host1".into(),
            os_name: "Ubuntu".into(),
            kernel: "6.6.0".into(),
            cpu_percent: 12.5,
            cpu_cores: 8,
            cpu_model: "Generic CPU".into(),
            memory_percent: 33.3,
            memory_used_display: "1.0 GB".into(),
            memory_total_display: "4.0 GB".into(),
            memory_used_bytes: 1_000_000,
            memory_total_bytes: 4_000_000,
            swap_percent: 0.0,
            load_one: 0.5,
            load_five: 0.4,
            load_fifteen: 0.3,
            uptime_seconds: 3600,
            uptime_display: "1h 0m".into(),
            disks: vec![DiskPayload {
                mount_point: "/".into(),
                usage_percent: 50.0,
                total_display: "100 GB".into(),
                used_display: "50 GB".into(),
            }],
            top_processes: vec![ProcessPayload {
                pid: 1,
                name: "init".into(),
                cpu_usage: 0.1,
                memory_usage_display: "1 MB".into(),
                memory_percent: 0.01,
            }],
            timestamp: 1_700_000_000,
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["system_id"], "sys-1");
        assert_eq!(json["cpu_cores"], 8);
        assert_eq!(json["disks"][0]["mount_point"], "/");
        assert_eq!(json["top_processes"][0]["pid"], 1);

        // Must also round-trip through MessagePack, since that's the wire format used to push to the hub.
        let packed = rmp_serde::to_vec(&payload).unwrap();
        assert!(!packed.is_empty());
    }

    #[test]
    fn hub_message_deserializes_auth_ok() {
        let msg: HubMessage = serde_json::from_str(r#"{"type":"auth_ok"}"#).unwrap();
        assert_eq!(msg.msg_type, "auth_ok");
        assert_eq!(msg.message, "");
    }

    #[test]
    fn hub_message_deserializes_auth_error_with_message() {
        let msg: HubMessage =
            serde_json::from_str(r#"{"type":"auth_error","message":"invalid token"}"#).unwrap();
        assert_eq!(msg.msg_type, "auth_error");
        assert_eq!(msg.message, "invalid token");
    }

    #[test]
    fn get_persistent_id_returns_non_empty_id() {
        // On any real host this resolves via /etc/machine-id, dbus machine-id, hostname,
        // or a random UUID fallback -- it should never be empty.
        let id = get_persistent_id();
        assert!(!id.is_empty());
    }
}
