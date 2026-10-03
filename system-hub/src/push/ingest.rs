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

use std::time::Instant;

use crate::db::{Registration, SnapshotStored};
use crate::models::{SystemId, SystemInfo, SystemStatus};
use crate::presence::{ConnectionLease, Ending, LeaseHandle};
use crate::registry::{
    LastSeen, MemoryCapacity, UptimeDisplay, memory_capacity_refresh, needs_system_info,
};
use crate::snapshot::{
    LeftOutLog, ReportedDisk, ReportedSnapshot, SnapshotTime, SnapshotTimeOutOfRange,
};
use crate::snapshot_intake;
use crate::state::AppState;
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

/// Why a decoded snapshot frame is refused whole (RFC 0007 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SnapshotRefusal {
    /// Its timestamp is above `i64::MAX`, which SQLite can't hold.
    TimestampOutOfRange,
}

/// A snapshot frame parsed at the edge (RFC 0007 §1): the snapshot it reports, its time, what
/// the system's last seen becomes, and the system info the registry fill reads.
#[derive(Debug)]
pub(super) struct SnapshotFrame {
    reported: ReportedSnapshot,
    time: SnapshotTime,
    last_seen: LastSeen,
    info: PushedInfo,
}

/// What a snapshot frame reports of its system, for the registry fill.
#[derive(Debug)]
struct PushedInfo {
    hostname: String,
    os_name: String,
    kernel: String,
    cpu_model: String,
    cpu_cores: usize,
    /// `None` when the frame's capacity breaks `MemoryCapacity::reported`'s bounds.
    memory: Option<MemoryCapacity>,
}

impl TryFrom<PushPayload> for SnapshotFrame {
    type Error = SnapshotRefusal;

    /// Every scalar of a push frame is reported; the uptime display becomes the last seen only
    /// under the display rule, else the stored one is kept. Moves what it keeps: a frame's
    /// mount points can add up to its 512 KiB.
    fn try_from(payload: PushPayload) -> Result<Self, Self::Error> {
        let PushPayload {
            hostname,
            os_name,
            kernel,
            cpu_percent,
            cpu_cores,
            cpu_model,
            memory_percent,
            memory_total_display,
            memory_total_bytes,
            swap_percent,
            load_one,
            load_five,
            uptime_display,
            disks,
            timestamp,
            ..
        } = payload;
        let time = SnapshotTime::try_from(timestamp)
            .map_err(|SnapshotTimeOutOfRange| SnapshotRefusal::TimestampOutOfRange)?;
        let reported = ReportedSnapshot {
            cpu: Some(cpu_percent),
            memory: Some(memory_percent),
            swap: Some(swap_percent),
            load1: Some(load_one),
            load5: Some(load_five),
            disks: disks.into_iter().map(ReportedDisk::from).collect(),
        };
        let memory =
            MemoryCapacity::reported(Some(&memory_total_display), Some(memory_total_bytes));
        let info = PushedInfo {
            hostname,
            os_name,
            kernel,
            cpu_model,
            cpu_cores,
            memory,
        };
        let last_seen =
            UptimeDisplay::try_from(uptime_display).map_or(LastSeen::Unchanged, LastSeen::Uptime);
        Ok(Self {
            reported,
            time,
            last_seen,
            info,
        })
    }
}

impl From<DiskItem> for ReportedDisk {
    fn from(disk: DiskItem) -> Self {
        Self {
            mount_point: Some(disk.mount_point),
            usage_percent: Some(disk.usage_percent),
        }
    }
}

/// Registers the id when it is new, then accepts the connection when the id is a push
/// system's, under the presence lock (RFC 0016 §2). A polled system's id gets no lease: its
/// connection claims nothing and its end writes nothing.
pub(super) fn register_and_accept(
    app: &AppState,
    system_id: &SystemId,
) -> Result<Option<ConnectionLease>, rusqlite::Error> {
    let mut presence = app.presence();
    let lease = match register_if_new(app, system_id)? {
        Registration::Inserted | Registration::Known { same_url: true } => {
            Some(presence.accept(system_id))
        }
        Registration::Known { same_url: false } => None,
    };
    Ok(lease)
}

/// Registers a system the hub hasn't seen before, under its default name. A known id is
/// never written, and a database that can't check or insert returns its error (RFC 0007 §4).
pub(super) fn register_if_new(
    app: &AppState,
    system_id: &SystemId,
) -> Result<Registration, rusqlite::Error> {
    app.db.insert_system_if_absent(&SystemInfo {
        id: system_id.as_str().to_string(),
        name: system_id.default_name(),
        url: crate::registry::PUSH_URL.to_string(),
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
}

/// Stores one snapshot frame, then fills the registry from it: one unit of blocking work.
pub(super) fn ingest_frame(
    app: &AppState,
    system_id: &SystemId,
    frame: SnapshotFrame,
    lease: Option<LeaseHandle>,
) -> Result<SnapshotStored<LeftOutLog>, rusqlite::Error> {
    if let Some(lease) = &lease {
        // Claimed just before the store, outside the presence lock (RFC 0016 §2).
        app.presence().claim(system_id, lease);
    }
    let SnapshotFrame {
        reported,
        time,
        last_seen,
        info,
    } = frame;
    if last_seen == LastSeen::Unchanged {
        tracing::debug!(
            "Push from {:?}: an uptime display the display rule refuses; last seen kept",
            system_id.as_str()
        );
    }
    let now = Instant::now();
    let stored = snapshot_intake::store_snapshot(app, system_id, reported, time, last_seen, now)?;
    if let SnapshotStored::Stored(_) = stored {
        update_registry(app, system_id, &info);
    }
    Ok(stored)
}

/// Fills in system info while its hostname or OS is missing, refreshes its memory capacity,
/// and replaces a default name with the frame's hostname. The status went into the store.
fn update_registry(app: &AppState, system_id: &SystemId, info: &PushedInfo) {
    let id = system_id.as_str();
    let sys = match app.db.get_system(id) {
        Ok(Some(sys)) => sys,
        Ok(None) => return,
        Err(err) => {
            // A row no read can map (RFC 0007 §4): its snapshot is stored, its fill skipped.
            tracing::debug!("Push from {id:?}: skipping the registry fill: {err}");
            return;
        }
    };
    if needs_system_info(&sys)
        && let Err(err) = app.db.update_system_info(
            id,
            Some(&info.os_name),
            Some(&info.hostname),
            Some(&info.kernel),
            Some(&info.cpu_model),
            Some(info.cpu_cores),
        )
    {
        tracing::warn!("Push from {id:?}: couldn't record the system info: {err}");
    }
    if let Some(capacity) =
        memory_capacity_refresh(MemoryCapacity::stored(&sys).as_ref(), info.memory.clone())
        && let Err(err) = app.db.update_memory_capacity(id, &capacity)
    {
        tracing::warn!("Push from {id:?}: couldn't refresh the memory capacity: {err}");
    }
    if system_id.is_default_name(&sys.name)
        && let Err(err) =
            app.db
                .update_system_config(id, Some(&info.hostname), None, None, None, None)
    {
        tracing::warn!("Push from {id:?}: couldn't rename the system to its hostname: {err}");
    }
}

/// Ends a push connection (RFC 0016 §2): only the current connection's end marks its system
/// offline and removes its live metrics (RFC 0007 §4), under the presence lock, in one unit
/// of blocking work. A connection without a lease (a polled system's id) writes nothing.
pub(super) fn end_connection(app: &AppState, system_id: &SystemId, lease: Option<ConnectionLease>) {
    let Some(lease) = lease else {
        return;
    };
    let mut presence = app.presence();
    match presence.end(system_id, lease) {
        Ending::Current => mark_disconnected(app, system_id),
        Ending::NotCurrent => tracing::info!(
            "Push connection of {:?} ended; another connection is current",
            system_id.as_str()
        ),
    }
}

/// Marks a system offline, then removes its live metrics.
fn mark_disconnected(app: &AppState, system_id: &SystemId) {
    let id = system_id.as_str();
    let offline =
        app.db
            .update_system_status(id, &SystemStatus::Offline, "", Some("push disconnected"));
    if let Err(err) = offline {
        tracing::warn!("Push from {id:?}: couldn't mark the system offline: {err}");
    }
    // The evicted entry is dropped here, after the live lock is released.
    drop(app.evict_live_metrics(id));
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::application_wire;
    use crate::snapshot::{LiveMetrics, snapshot_rule};
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

    /// Decodes a mirrored frame into the hub's `PushPayload`, as the receiver does.
    fn payload(frame: &MirroredPushPayload) -> PushPayload {
        rmp_serde::from_slice(&rmp_serde::to_vec(frame).unwrap()).unwrap()
    }

    /// RFC 0007 §1: a frame's timestamp becomes its snapshot time, at most `i64::MAX`.
    #[test]
    fn a_frame_is_refused_whole_only_for_a_timestamp_past_i64_max() {
        let cases = [
            ("zero", 0, Ok(0)),
            ("i64::MAX", i64::MAX as u64, Ok(i64::MAX)),
            (
                "i64::MAX + 1",
                i64::MAX as u64 + 1,
                Err(SnapshotRefusal::TimestampOutOfRange),
            ),
            (
                "u64::MAX",
                u64::MAX,
                Err(SnapshotRefusal::TimestampOutOfRange),
            ),
        ];
        for (case, timestamp, expected) in cases {
            let mut frame = sample_frame("host");
            frame.timestamp = timestamp;
            let parsed = SnapshotFrame::try_from(payload(&frame));
            let time = parsed.map(|frame| frame.time.seconds());
            assert_eq!(time, expected, "case: {case}");
        }
    }

    /// RFC 0007 §1, §2: every scalar of a push frame is reported, and the uptime display is the
    /// last seen only under the display rule.
    #[test]
    fn a_frame_reports_every_scalar_and_its_uptime_under_the_display_rule() {
        let cases = [
            ("an agent's display", "3d 4h 5m".to_string(), true),
            ("64 bytes", "u".repeat(64), true),
            (
                "65 bytes, a 2-byte character across the bound",
                format!("{}é", "u".repeat(63)),
                false,
            ),
            ("empty", String::new(), false),
            ("a newline", "3d\n4h".to_string(), false),
            ("NEL", "3d\u{85}4h".to_string(), false),
        ];
        for (case, display, kept) in cases {
            let mut frame = sample_frame("host");
            frame.uptime_display = display.clone();
            let parsed = SnapshotFrame::try_from(payload(&frame)).unwrap();
            let expected = match kept {
                true => LastSeen::Uptime(UptimeDisplay::try_from(display).unwrap()),
                false => LastSeen::Unchanged,
            };
            assert_eq!(parsed.last_seen, expected, "case: {case}");
            let reported = &parsed.reported;
            let scalars = (
                reported.cpu,
                reported.memory,
                reported.swap,
                reported.load1,
                reported.load5,
            );
            assert_eq!(
                scalars,
                (Some(11.0), Some(22.0), Some(33.0), Some(0.1), Some(0.2)),
                "case: {case}"
            );
            let mounts: Vec<Option<&str>> = reported
                .disks
                .iter()
                .map(|d| d.mount_point.as_deref())
                .collect();
            assert_eq!(mounts, [Some("/"), Some("/home")], "case: {case}");
        }
    }

    fn app() -> (std::sync::Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = std::sync::Arc::new(crate::db::Database::new(path.to_str().unwrap()).unwrap());
        (AppState::new(db).unwrap(), dir)
    }

    fn id(id: &str) -> SystemId {
        SystemId::try_from(id.to_string()).unwrap()
    }

    /// RFC 0007 §4: registration keeps `Database`'s one lock convention, and panics on a
    /// poisoned mutex, so the handshake answers the `JoinError` rather than accepting an agent
    /// whose every frame would then panic its store.
    #[test]
    fn registration_panics_on_a_poisoned_database_mutex() {
        let (app, _dir) = app();
        let id = id("sys-poisoned");
        app.db.poison_for_test();

        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| register_if_new(&app, &id)));

        assert!(outcome.is_err(), "registration panicked");
    }

    fn plant_live_metrics(app: &AppState, system_id: &str) {
        let (snapshot, left_out) = snapshot_rule(ReportedSnapshot::default());
        let time = SnapshotTime::try_from(1_700_000_000).unwrap();
        let (entry, _) = LiveMetrics::following(None, snapshot, time, &left_out, Instant::now());
        let mut live = app.live_metrics.write().unwrap();
        live.insert(system_id.to_string(), std::sync::Arc::new(entry));
    }

    /// RFC 0007 §4: a push connection's end marks its system offline and evicts its live
    /// metrics, even when its row is already gone, and leaves every other system alone.
    #[test]
    fn ending_a_connection_marks_its_system_offline_and_evicts_its_live_metrics() {
        let cases = [("a registered system", true), ("a deleted system", false)];
        for (case, registered) in cases {
            let (app, _dir) = app();
            let mut leases = Vec::new();
            for system in ["sys-ended", "sys-other"] {
                leases.push(register_and_accept(&app, &id(system)).unwrap());
                plant_live_metrics(&app, system);
            }
            if !registered {
                app.db.delete_system("sys-ended").unwrap();
            }

            end_connection(&app, &id("sys-ended"), leases.remove(0));

            let status = |system| {
                let row = app.db.get_system(system).unwrap();
                row.map(|sys| (sys.status, sys.last_error))
            };
            let offline = (SystemStatus::Offline, Some("push disconnected".to_string()));
            let expected = registered.then_some(offline);
            assert_eq!(status("sys-ended"), expected, "case: {case}");
            assert_eq!(
                status("sys-other"),
                Some((SystemStatus::Unknown, None)),
                "case: {case}: another system stays online"
            );
            let live = app.live_metrics.read().unwrap();
            assert!(!live.contains_key("sys-ended"), "case: {case}: evicted");
            assert!(
                live.contains_key("sys-other"),
                "case: {case}: another entry stays"
            );
        }
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
