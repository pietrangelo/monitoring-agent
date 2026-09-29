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

//! The push ingestion: everything that runs as `FnOnce(&AppState, &SystemId)` on the blocking
//! pool. The snapshot DTOs are the anti-corruption layer between the agent's push frame and
//! the hub's registry and history.

use crate::models::{SystemId, SystemInfo, SystemStatus};
use crate::registry::{MemoryCapacity, memory_capacity_refresh};
use crate::state::{AppState, LiveMetrics};
use serde::Deserialize;

/// Deserialized from MessagePack binary payloads sent by agents.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub(super) struct PushPayload {
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

/// Registers a system the hub hasn't seen before, under its default name. A known id is
/// never written, and a database that can't check or insert returns its error (RFC 0007 §4).
pub(super) fn register_if_new(app: &AppState, system_id: &SystemId) -> Result<(), rusqlite::Error> {
    app.db.insert_system_if_absent(&SystemInfo {
        id: system_id.as_str().to_string(),
        name: system_id.default_name(),
        url: "push://".to_string(),
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
}

pub(super) fn ingest_frame(app: &AppState, system_id: &SystemId, payload: &PushPayload) {
    store_metrics(app, system_id.as_str(), payload);
    cache_live_metrics(app, system_id.as_str(), payload);
    update_registry(app, system_id, payload);
    app.refresh_cache();
}

fn store_metrics(app: &AppState, system_id: &str, payload: &PushPayload) {
    for (metric, value) in metric_points(payload) {
        let _ = app
            .db
            .insert_metric(system_id, &metric, value, payload.timestamp);
    }
}

/// The metric points one snapshot adds to the system's history, keyed by metric name.
fn metric_points(payload: &PushPayload) -> impl Iterator<Item = (String, f32)> + '_ {
    let scalars = [
        ("cpu", payload.cpu_percent),
        ("memory", payload.memory_percent),
        ("swap", payload.swap_percent),
        ("load1", payload.load_one as f32),
        ("load5", payload.load_five as f32),
    ];
    let disks = payload
        .disks
        .iter()
        .map(|disk| (format!("disk:{}", disk.mount_point), disk.usage_percent));
    scalars
        .into_iter()
        .map(|(metric, value)| (metric.to_string(), value))
        .chain(disks)
}

fn cache_live_metrics(app: &AppState, system_id: &str, payload: &PushPayload) {
    let live = LiveMetrics {
        cpu_percent: payload.cpu_percent,
        memory_percent: payload.memory_percent,
        load_one: payload.load_one,
        disks: payload
            .disks
            .iter()
            .map(|disk| (disk.mount_point.clone(), disk.usage_percent))
            .collect(),
        updated_at: payload.timestamp,
    };
    app.live_metrics
        .write()
        .unwrap()
        .insert(system_id.to_string(), live);
}

/// Fills in system info while its hostname or OS is missing, marks the system online, and
/// replaces a default name with the snapshot's hostname.
fn update_registry(app: &AppState, system_id: &SystemId, payload: &PushPayload) {
    let id = system_id.as_str();
    let Some(sys) = app.db.get_system(id).ok().flatten() else {
        return;
    };
    if (sys.hostname.is_none() || sys.os.is_none())
        && let Err(err) = app.db.update_system_info(
            id,
            Some(&payload.os_name),
            Some(&payload.hostname),
            Some(&payload.kernel),
            Some(&payload.cpu_model),
            Some(payload.cpu_cores),
        )
    {
        tracing::warn!("Push from {id:?}: couldn't record the system info: {err}");
    }
    let reported = MemoryCapacity::reported(
        Some(&payload.memory_total_display),
        Some(payload.memory_total_bytes),
    );
    if let Some(capacity) = memory_capacity_refresh(MemoryCapacity::stored(&sys).as_ref(), reported)
        && let Err(err) = app.db.update_memory_capacity(id, &capacity)
    {
        tracing::warn!("Push from {id:?}: couldn't refresh the memory capacity: {err}");
    }
    let _ = app
        .db
        .update_system_status(id, &SystemStatus::Online, &payload.uptime_display, None);
    if system_id.is_default_name(&sys.name) {
        let _ = app
            .db
            .update_system_config(id, Some(&payload.hostname), None, None, None, None);
    }
}

pub(super) fn mark_offline(app: &AppState, system_id: &SystemId) {
    let _ = app.db.update_system_status(
        system_id.as_str(),
        &SystemStatus::Offline,
        "",
        Some("push disconnected"),
    );
    app.refresh_cache();
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::application_wire;
    use serde::Serialize;

    #[derive(Serialize)]
    pub struct MirroredDisk {
        pub mount_point: String,
        pub usage_percent: f32,
        pub total_display: String,
        pub used_display: String,
    }

    #[derive(Serialize)]
    pub struct MirroredProcess {
        pub pid: u32,
        pub name: String,
        pub cpu_usage: f32,
        pub memory_usage_display: String,
        pub memory_percent: f32,
    }

    /// Mirrors `PushPayload`'s field order exactly, so a MessagePack encoding of this
    /// struct can be decoded by the real (deserialize-only) `PushPayload`.
    #[derive(Serialize)]
    pub struct MirroredPushPayload {
        pub system_id: String,
        pub hostname: String,
        pub os_name: String,
        pub kernel: String,
        pub cpu_percent: f32,
        pub cpu_cores: usize,
        pub cpu_model: String,
        pub memory_percent: f32,
        pub memory_used_display: String,
        pub memory_total_display: String,
        pub memory_used_bytes: u64,
        pub memory_total_bytes: u64,
        pub swap_percent: f32,
        pub load_one: f64,
        pub load_five: f64,
        pub load_fifteen: f64,
        pub uptime_seconds: u64,
        pub uptime_display: String,
        pub disks: Vec<MirroredDisk>,
        pub top_processes: Vec<MirroredProcess>,
        pub timestamp: u64,
    }

    pub fn sample_frame(hostname: &str) -> MirroredPushPayload {
        MirroredPushPayload {
            system_id: "sys-1".into(),
            hostname: hostname.into(),
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
            swap_percent: 33.0,
            load_one: 0.1,
            load_five: 0.2,
            load_fifteen: 0.3,
            uptime_seconds: 100,
            uptime_display: "1m".into(),
            disks: vec![
                MirroredDisk {
                    mount_point: "/".into(),
                    usage_percent: 50.0,
                    total_display: "10G".into(),
                    used_display: "5G".into(),
                },
                MirroredDisk {
                    mount_point: "/home".into(),
                    usage_percent: 70.0,
                    total_display: "100G".into(),
                    used_display: "70G".into(),
                },
            ],
            top_processes: vec![],
            timestamp: 1_700_000_000,
        }
    }

    /// The agent's test pins the same bytes, so this is the round trip across the two
    /// independent `PushPayload` declarations (CLAUDE.md, the push frame contract).
    #[test]
    fn the_golden_snapshot_frame_decodes_to_every_field_in_place() {
        let golden: &[u8] = include_bytes!("../../../testdata/snapshot-frame-v1.msgpack");
        let p: PushPayload = rmp_serde::from_slice(golden).unwrap();
        let identity = (
            p.system_id.as_str(),
            p.hostname.as_str(),
            p.os_name.as_str(),
        );
        assert_eq!(identity, ("system-s", "host-h", "os-pretty"));
        assert_eq!(
            (p.kernel.as_str(), p.cpu_model.as_str()),
            ("kernel-k", "cpu-model")
        );
        let percents = (p.cpu_percent, p.memory_percent, p.swap_percent);
        assert_eq!(percents, (12.5, 33.25, 7.75));
        assert_eq!(p.cpu_cores, 8);
        let memory = (
            p.memory_used_display.as_str(),
            p.memory_total_display.as_str(),
            p.memory_used_bytes,
            p.memory_total_bytes,
        );
        assert_eq!(memory, ("mem-used", "mem-total", 1111, 2222));
        assert_eq!(
            (p.load_one, p.load_five, p.load_fifteen),
            (0.5, 0.25, 0.125)
        );
        assert_eq!(
            (p.uptime_seconds, p.uptime_display.as_str()),
            (3333, "uptime-u")
        );
        let disks: Vec<(&str, f32, &str, &str)> = p
            .disks
            .iter()
            .map(|d| {
                let (total, used) = (d.total_display.as_str(), d.used_display.as_str());
                (d.mount_point.as_str(), d.usage_percent, total, used)
            })
            .collect();
        assert_eq!(
            disks,
            vec![
                ("disk-mount", 50.5, "disk-total", "disk-used"),
                ("disk2-mount", 60.25, "disk2-total", "disk2-used"),
            ]
        );
        let processes: Vec<(u32, &str, f32, &str, f32)> = p
            .top_processes
            .iter()
            .map(|q| {
                let display = q.memory_usage_display.as_str();
                (
                    q.pid,
                    q.name.as_str(),
                    q.cpu_usage,
                    display,
                    q.memory_percent,
                )
            })
            .collect();
        assert_eq!(processes, vec![(1, "proc-1", 1.5, "proc-mem", 0.75)]);
        assert_eq!(p.timestamp, 4_444_444);
    }

    #[test]
    fn a_snapshot_frame_and_an_application_frame_never_decode_as_each_other() {
        let golden: &[u8] = include_bytes!("../../../testdata/application-frame-v1.msgpack");
        let snapshot = rmp_serde::to_vec(&sample_frame("host")).unwrap();
        assert!(
            rmp_serde::from_slice::<PushPayload>(golden).is_err(),
            "an application frame isn't a snapshot"
        );
        assert!(
            application_wire::decode(&snapshot).is_none(),
            "a snapshot isn't an application frame"
        );
        assert!(
            rmp_serde::from_slice::<PushPayload>(&snapshot).is_ok(),
            "the snapshot itself decodes"
        );
    }

    /// RFC 0007 §4: registration keeps `Database`'s one lock convention, and panics on a
    /// poisoned mutex, so the handshake answers the `JoinError` rather than accepting an agent
    /// whose every frame would then panic its store.
    #[test]
    fn registration_panics_on_a_poisoned_database_mutex() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = std::sync::Arc::new(crate::db::Database::new(path.to_str().unwrap()).unwrap());
        let app = AppState::new(db);
        let id = SystemId::try_from("sys-poisoned".to_string()).unwrap();
        app.db.poison_for_test();

        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| register_if_new(&app, &id)));

        assert!(outcome.is_err(), "registration panicked");
    }

    #[test]
    fn push_payload_decodes_from_messagepack_wire_format() {
        let mirrored = sample_frame("host1");
        let packed = rmp_serde::to_vec(&mirrored).unwrap();
        let payload: PushPayload = rmp_serde::from_slice(&packed).unwrap();
        assert_eq!(payload.system_id, "sys-1");
        assert_eq!(payload.cpu_cores, 4);
        assert_eq!(payload.disks[0].mount_point, "/");
    }
}
