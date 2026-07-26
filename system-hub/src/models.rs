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

use serde::{Deserialize, Serialize};

// ── Registered system ──────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInfo {
    pub id: String,
    pub name: String,
    pub url: String,
    #[serde(skip_serializing)]
    pub token: String,
    pub status: SystemStatus,
    pub last_seen: String,
    pub last_error: Option<String>,
    pub os: Option<String>,
    pub hostname: Option<String>,
    pub kernel: Option<String>,
    pub cpu_model: Option<String>,
    pub cpu_cores: Option<usize>,
    pub total_memory_display: Option<String>,
    pub total_memory_bytes: Option<u64>,
    pub poll_interval_secs: u64,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SystemStatus {
    Online,
    Offline,
    Unknown,
}

impl std::fmt::Display for SystemStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SystemStatus::Online => write!(f, "online"),
            SystemStatus::Offline => write!(f, "offline"),
            SystemStatus::Unknown => write!(f, "unknown"),
        }
    }
}

// ── Register / update payloads ─────────────────────────

#[derive(Debug, Deserialize)]
pub struct RegisterSystemPayload {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub token: String,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
}

fn default_poll_interval() -> u64 {
    10
}

#[derive(Debug, Deserialize)]
pub struct UpdateSystemPayload {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub poll_interval_secs: Option<u64>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

// ── Metric snapshot (received from agent) ──────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricSnapshot {
    pub system_id: String,
    pub timestamp: u64,
    pub cpu_percent: f32,
    pub memory_percent: f32,
    pub swap_percent: f32,
    pub load_one: f64,
    pub load_five: f64,
    pub load_fifteen: f64,
    pub uptime_seconds: u64,
    pub uptime_display: String,
    pub memory_used_display: String,
    pub memory_total_display: String,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub cpu_logical_cores: usize,
    pub disks: Vec<DiskSnapshot>,
    pub top_processes: Vec<ProcessSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskSnapshot {
    pub mount_point: String,
    pub usage_percent: f32,
    pub total_display: String,
    pub used_display: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessSnapshot {
    pub pid: u32,
    pub name: String,
    pub cpu_usage: f32,
    pub memory_usage_display: String,
    pub memory_percent: f32,
}

// ── Alert ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRecord {
    pub id: String,
    pub system_id: String,
    pub system_name: String,
    pub severity: String,
    pub message: String,
    pub current_value: f32,
    pub fired_at: String,
    pub stored_at: String,
    pub acknowledged: bool,
}

// ── Dashboard summary ──────────────────────────────────

#[derive(Debug, Serialize)]
pub struct HubSummary {
    pub total_systems: usize,
    pub online_count: usize,
    pub offline_count: usize,
    pub total_alerts_active: usize,
    pub total_alerts_today: usize,
    pub systems: Vec<SystemInfo>,
}

// ── History ────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct MetricPoint {
    pub timestamp: u64,
    pub value: f32,
}

#[derive(Debug, Serialize)]
pub struct SystemHistory {
    pub system_id: String,
    pub system_name: String,
    pub cpu: Vec<MetricPoint>,
    pub memory: Vec<MetricPoint>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_status_display_matches_serde_rename() {
        assert_eq!(SystemStatus::Online.to_string(), "online");
        assert_eq!(SystemStatus::Offline.to_string(), "offline");
        assert_eq!(SystemStatus::Unknown.to_string(), "unknown");
    }

    #[test]
    fn system_status_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&SystemStatus::Online).unwrap(),
            "\"online\""
        );
    }

    #[test]
    fn system_info_serialization_skips_token() {
        let sys = SystemInfo {
            id: "id-1".into(),
            name: "web".into(),
            url: "http://x".into(),
            token: "super-secret".into(),
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
        };
        let json = serde_json::to_value(&sys).unwrap();
        assert!(
            json.get("token").is_none(),
            "token must never be serialized back to clients"
        );
        assert_eq!(json["id"], "id-1");
    }

    #[test]
    fn register_system_payload_defaults_poll_interval_and_token() {
        let payload: RegisterSystemPayload =
            serde_json::from_str(r#"{"name":"web","url":"http://x"}"#).unwrap();
        assert_eq!(payload.poll_interval_secs, 10);
        assert_eq!(payload.token, "");
    }

    #[test]
    fn register_system_payload_explicit_values() {
        let payload: RegisterSystemPayload = serde_json::from_str(
            r#"{"name":"web","url":"http://x","token":"t","poll_interval_secs":30}"#,
        )
        .unwrap();
        assert_eq!(payload.poll_interval_secs, 30);
        assert_eq!(payload.token, "t");
    }

    #[test]
    fn update_system_payload_all_fields_default_to_none() {
        let payload: UpdateSystemPayload = serde_json::from_str("{}").unwrap();
        assert!(payload.name.is_none());
        assert!(payload.url.is_none());
        assert!(payload.token.is_none());
        assert!(payload.poll_interval_secs.is_none());
        assert!(payload.enabled.is_none());
    }

    #[test]
    fn update_system_payload_partial_fields() {
        let payload: UpdateSystemPayload = serde_json::from_str(r#"{"enabled":false}"#).unwrap();
        assert_eq!(payload.enabled, Some(false));
        assert!(payload.name.is_none());
    }
}
