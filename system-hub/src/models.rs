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

// ── System identity ────────────────────────────────────

/// The identifier an agent presents in the push handshake, and the system's primary key.
/// Always exactly one URL path segment: the dashboard builds `/api/systems/<id>` URLs from
/// it, so it is never empty, never longer than `MAX_SYSTEM_ID_BYTES`, and never `.` or `..`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemId(String);

/// The longest system id, in bytes. Real ids are machine ids (32 bytes), hostnames (at most
/// 64 on Linux) or UUIDs (36); encoded into a URL, 255 bytes stays far below any request-line
/// limit.
pub const MAX_SYSTEM_ID_BYTES: usize = 255;

/// Why a value isn't a system id.
#[derive(Debug, PartialEq, Eq)]
pub enum SystemIdError {
    /// The empty string.
    Empty,
    /// Longer than `MAX_SYSTEM_ID_BYTES`, so its URLs could outgrow a request line.
    TooLong,
    /// `.` or `..`, which the dashboard's URLs would resolve away.
    DotSegment,
}

impl TryFrom<String> for SystemId {
    type Error = SystemIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "" => Err(SystemIdError::Empty),
            "." | ".." => Err(SystemIdError::DotSegment),
            id if id.len() > MAX_SYSTEM_ID_BYTES => Err(SystemIdError::TooLong),
            _ => Ok(Self(value)),
        }
    }
}

const DEFAULT_NAME_MAX_BYTES: usize = 8;

impl SystemId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The name a newly pushed system gets until its first snapshot supplies a hostname:
    /// the longest prefix of at most 8 bytes that ends on a character boundary. Counted in
    /// bytes, not characters, so it equals the `id[..8]` names already stored by hubs
    /// before RFC 0003.
    pub fn default_name(&self) -> String {
        let end = self.0.floor_char_boundary(DEFAULT_NAME_MAX_BYTES);
        self.0[..end].to_string()
    }

    /// Whether `name` is still this system's default name.
    pub fn is_default_name(&self, name: &str) -> bool {
        name == self.default_name()
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
    fn system_id_is_exactly_one_url_path_segment() {
        // Literal lengths, not MAX_SYSTEM_ID_BYTES, so the test pins the limit itself.
        let ascii_255 = "a".repeat(255);
        let ascii_256 = "a".repeat(256);
        // 253 + 2 = 255 bytes, accepted; 254 + 2 = 256 bytes but 255 characters, refused.
        let multi_byte_255 = format!("{}é", "a".repeat(253));
        let multi_byte_256 = format!("{}é", "a".repeat(254));
        // About 22 KB, the size that makes a dashboard URL hit 414.
        let far_over = "é".repeat(11_000);
        let ascii_65536 = "a".repeat(65_536);
        let padded_256 = format!("{} ", "a".repeat(255));
        let cases = [
            ("empty id", "", Err(SystemIdError::Empty)),
            ("one-byte id", "x", Ok("x")),
            (
                "machine id",
                "0123456789abcdef0123456789abcdef",
                Ok("0123456789abcdef0123456789abcdef"),
            ),
            (
                "uuid",
                "123e4567-e89b-12d3-a456-426614174000",
                Ok("123e4567-e89b-12d3-a456-426614174000"),
            ),
            ("single dot", ".", Err(SystemIdError::DotSegment)),
            ("double dot", "..", Err(SystemIdError::DotSegment)),
            ("three dots are one segment", "...", Ok("...")),
            ("dotted hostname", "a.b", Ok("a.b")),
            ("leading dot", ".hidden", Ok(".hidden")),
            ("dot then letter", ".a", Ok(".a")),
            ("letter then dot", "a.", Ok("a.")),
            ("space before a dot", " .", Ok(" .")),
            ("encoded dot stays encoded in the url", "%2e", Ok("%2e")),
            (
                "255 ascii bytes",
                ascii_255.as_str(),
                Ok(ascii_255.as_str()),
            ),
            (
                "255 bytes with a multi-byte character",
                multi_byte_255.as_str(),
                Ok(multi_byte_255.as_str()),
            ),
            (
                "256 ascii bytes",
                ascii_256.as_str(),
                Err(SystemIdError::TooLong),
            ),
            (
                "256 bytes in 255 characters",
                multi_byte_256.as_str(),
                Err(SystemIdError::TooLong),
            ),
            (
                "far over the limit",
                far_over.as_str(),
                Err(SystemIdError::TooLong),
            ),
            (
                "past a 16-bit length",
                ascii_65536.as_str(),
                Err(SystemIdError::TooLong),
            ),
            (
                "255 bytes plus a trailing space",
                padded_256.as_str(),
                Err(SystemIdError::TooLong),
            ),
            // Only the literal "." and ".." are refused; "/" stays inside one encoded segment.
            ("dots after a slash", "a/..", Ok("a/..")),
            ("dot then space", ". ", Ok(". ")),
        ];
        for (name, input, expected) in cases {
            let parsed = SystemId::try_from(input.to_string()).map(|id| id.as_str().to_string());
            assert_eq!(parsed, expected.map(str::to_string), "{name}");
        }
    }

    #[test]
    fn system_still_carries_its_default_name_only_when_it_equals_it() {
        let cases = [
            (
                "default name of a long id",
                "0123456789abcdef",
                "01234567",
                true,
            ),
            ("whole id of a short id", "pi", "pi", true),
            (
                "char-aligned default of a multi-byte id",
                "aéééé",
                "aééé",
                true,
            ),
            (
                "hostname set after the first snapshot",
                "0123456789abcdef",
                "host1",
                false,
            ),
            (
                "full long id is not its default name",
                "0123456789abcdef",
                "0123456789abcdef",
                false,
            ),
            (
                "shorter prefix of the default name",
                "0123456789abcdef",
                "0123",
                false,
            ),
            (
                "byte-sliced prefix that splits a character",
                "aéééé",
                "aéééé",
                false,
            ),
        ];
        for (name, id, candidate, expected) in cases {
            let id = SystemId::try_from(id.to_string()).unwrap();
            assert_eq!(id.is_default_name(candidate), expected, "{name}");
        }
    }

    #[test]
    fn default_system_name_is_the_longest_char_aligned_prefix_of_at_most_8_bytes() {
        let cases = [
            ("id longer than 8 bytes", "0123456789abcdef", "01234567"),
            ("id exactly 8 bytes", "01234567", "01234567"),
            ("id shorter than 8 bytes", "pi", "pi"),
            ("one-byte id", "x", "x"),
            ("2-byte char ending exactly at byte 8", "éééééé", "éééé"),
            ("2-byte char across byte 8", "aéééé", "aééé"),
            ("3-byte char across byte 8", "日本語テキスト", "日本"),
        ];
        for (name, id, expected) in cases {
            let id = SystemId::try_from(id.to_string()).unwrap();
            assert_eq!(id.default_name(), expected, "{name}");
        }
    }

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
