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

use crate::registry::{
    LastSeen, MemoryCapacity, enabled_systems, memory_capacity_refresh, needs_system_info,
};
use serde::Deserialize;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Instant;
use tokio::time::{Duration, interval};

use crate::clock::{now_iso, unix_now};
use crate::db::SnapshotStored;
use crate::models::{AlertRecord, SystemId, SystemInfo, SystemStatus};
use crate::snapshot::{LeftOutLog, ReportedDisk, ReportedSnapshot, SnapshotTime};
use crate::snapshot_intake;
use crate::state::AppState;

mod application_poll;
mod capped_body;
use application_poll::poll_applications;
use capped_body::{CappedBodyError, read_capped};

// ── Agent response shape (maps the system-agent /api/system JSON) ──

#[derive(Debug, Deserialize)]
struct AgentResponse {
    hostname: Option<String>,
    os: Option<AgentOs>,
    kernel: Option<String>,
    cpu: Option<AgentCpu>,
    memory: Option<AgentMemory>,
    swap: Option<AgentSwap>,
    load_average: Option<AgentLoad>,
    disks: Option<Vec<AgentDisk>>,
}

#[derive(Debug, Deserialize)]
struct AgentOs {
    name: Option<String>,
    pretty_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AgentCpu {
    model: Option<String>,
    logical_cores: Option<usize>,
    usage_percent: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct AgentMemory {
    total_bytes: Option<u64>,
    total_display: Option<String>,
    usage_percent: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct AgentSwap {
    usage_percent: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct AgentLoad {
    one: Option<f64>,
    five: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct AgentDisk {
    mount_point: Option<String>,
    usage_percent: Option<f32>,
}

// ── Collector ──────────────────────────────────────────

pub fn start_collectors(state: Arc<AppState>) {
    tokio::spawn(poll_every(state, Duration::from_secs(30)));
}

/// Polls the registry's enabled systems every `period`. The registry is read at each tick, so
/// no cache can go stale; a tick whose read fails polls the systems of the last read that
/// succeeded (RFC 0007 §2).
async fn poll_every(state: Arc<AppState>, period: Duration) {
    let mut tick = interval(period);
    let mut systems = Vec::new();
    loop {
        tick.tick().await;
        systems = read_registry(&state, systems).await;
        for system in enabled_systems(&systems) {
            let (state, system) = (Arc::clone(&state), system.clone());
            tokio::spawn(async move { poll_system(state, &system).await });
        }
    }
}

/// The registry's systems, read on the blocking pool, or `last_read` when the read fails.
async fn read_registry(state: &Arc<AppState>, last_read: Vec<SystemInfo>) -> Vec<SystemInfo> {
    let app = Arc::clone(state);
    match tokio::task::spawn_blocking(move || app.db.list_systems()).await {
        Ok(Ok(systems)) => systems,
        Ok(Err(err)) => {
            tracing::warn!("Couldn't read the registry; polling the last read's systems: {err}");
            last_read
        }
        Err(err) => {
            tracing::error!("Reading the registry failed; polling the last read's systems: {err}");
            last_read
        }
    }
}

async fn poll_system(state: Arc<AppState>, system: &SystemInfo) {
    let now = now_iso();
    // Parsed once, first. Only a row stored before RFC 0005 can fail; no agent can push to it
    // again, so without this marking it would keep its last status for good.
    let Ok(id) = SystemId::try_from(system.id.clone()) else {
        return mark_offline(&state, system, now, "invalid system id".to_string()).await;
    };
    let Some(client) = poll_client(system) else {
        return;
    };
    let agent = match fetch_system(&client, system).await {
        Ok(agent) => agent,
        Err(failure) => {
            // Dropped before the marking's await: a client holds its CA store, and a woken
            // poll can queue behind the rest of the tick's polls.
            drop(client);
            return mark_offline(&state, system, now, failure.last_error()).await;
        }
    };
    if store_answer(&state, system, &id, PolledAnswer::from(agent), now)
        .await
        .is_break()
    {
        return;
    }
    store_agent_alerts(&state, system, &client).await;
    poll_applications(&state, system, &id, &client).await;
}

/// The largest `/api/system` answer the hub reads: eight times the push message limit, so
/// every snapshot a push frame can carry fits, with room for the answer's processes and
/// networks (RFC 0007 §1).
const MAX_SYSTEM_BODY: usize = 4 * 1024 * 1024;

/// Why a system poll found its system offline.
#[derive(Debug)]
enum PollFailure {
    Transport(reqwest::Error),
    Status(reqwest::StatusCode),
    TooLarge,
    BadJson(serde_json::Error),
}

impl PollFailure {
    /// The system's `last_error` for this failure.
    fn last_error(&self) -> String {
        match self {
            Self::Transport(err) => err.to_string(),
            Self::Status(status) => format!("HTTP {status}"),
            Self::TooLarge => "body over 4 MiB".to_string(),
            Self::BadJson(err) => format!("JSON parse error: {err}"),
        }
    }
}

/// Fetches the system's `/api/system` answer, reading at most `MAX_SYSTEM_BODY`, and parses it.
async fn fetch_system(
    client: &reqwest::Client,
    system: &SystemInfo,
) -> Result<AgentResponse, PollFailure> {
    let url = format!("{}/api/system", system.url.trim_end_matches('/'));
    let mut resp = client
        .get(&url)
        .send()
        .await
        .map_err(PollFailure::Transport)?;
    if !resp.status().is_success() {
        return Err(PollFailure::Status(resp.status()));
    }
    let body = read_capped(&mut resp, MAX_SYSTEM_BODY)
        .await
        .map_err(|err| match err {
            CappedBodyError::TooLarge => PollFailure::TooLarge,
            CappedBodyError::Transport(err) => PollFailure::Transport(err),
        })?;
    serde_json::from_slice(&body).map_err(PollFailure::BadJson)
}

/// The HTTP client for one system's polls: its token as `X-API-Key`, a 10 s timeout, and no
/// redirects.
fn poll_client(system: &SystemInfo) -> Option<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    if !system.token.is_empty()
        && let Ok(val) = reqwest::header::HeaderValue::from_str(&system.token)
    {
        headers.insert("X-API-Key", val);
    }
    // No redirects: reqwest would carry `X-API-Key` across hosts (RFC 0009 §8). A registered
    // URL that redirects shows as offline, naming the 3xx.
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .default_headers(headers)
        .build()
        .inspect_err(|err| tracing::warn!("No poll client for {:?}: {err}", system.id))
        .ok()
}

/// Logs a failed registry or history write of a poll: the poll goes on.
fn warn_on_failure(result: Result<(), rusqlite::Error>, what: &str, system: &SystemInfo) {
    if let Err(err) = result {
        tracing::warn!("Poll of {:?}: couldn't {what}: {err}", system.id);
    }
}

/// Marks the system offline, `error` as its last error, off the async runtime.
async fn mark_offline(state: &Arc<AppState>, system: &SystemInfo, now: String, error: String) {
    let (app, id) = (Arc::clone(state), system.id.clone());
    let unit = move || {
        app.db
            .update_system_status(&id, &SystemStatus::Offline, &now, Some(&error))
    };
    match tokio::task::spawn_blocking(unit).await {
        Ok(offline) => warn_on_failure(offline, "mark offline", system),
        Err(err) => tracing::error!("Poll of {:?}: the offline marking failed: {err}", system.id),
    }
}

/// What a successful poll's answer reports, parsed at the edge (RFC 0007 §1): the snapshot,
/// the system info, and the memory capacity. A value the agent didn't report stays `None`.
struct PolledAnswer {
    reported: ReportedSnapshot,
    info: PolledInfo,
    /// `None` unless the answer reports both halves within `MemoryCapacity::reported`'s bounds.
    memory: Option<MemoryCapacity>,
}

/// The system info a poll's answer reports.
struct PolledInfo {
    os_name: Option<String>,
    hostname: Option<String>,
    kernel: Option<String>,
    cpu_model: Option<String>,
    cpu_cores: Option<usize>,
}

impl From<AgentResponse> for PolledAnswer {
    fn from(agent: AgentResponse) -> Self {
        let memory = agent.memory.as_ref();
        let memory = MemoryCapacity::reported(
            memory.and_then(|m| m.total_display.as_deref()),
            memory.and_then(|m| m.total_bytes),
        );
        let cpu = agent.cpu.as_ref();
        let info = PolledInfo {
            os_name: agent
                .os
                .as_ref()
                .and_then(|o| o.pretty_name.clone().or(o.name.clone())),
            hostname: agent.hostname.clone(),
            kernel: agent.kernel.clone(),
            cpu_model: cpu.and_then(|c| c.model.clone()),
            cpu_cores: cpu.and_then(|c| c.logical_cores),
        };
        Self {
            reported: reported_snapshot(agent),
            info,
            memory,
        }
    }
}

/// The snapshot an agent's `/api/system` answer reports, before the snapshot rule.
fn reported_snapshot(agent: AgentResponse) -> ReportedSnapshot {
    let load = agent.load_average.as_ref();
    let disks = agent.disks.unwrap_or_default().into_iter();
    ReportedSnapshot {
        cpu: agent.cpu.as_ref().and_then(|c| c.usage_percent),
        memory: agent.memory.as_ref().and_then(|m| m.usage_percent),
        swap: agent.swap.as_ref().and_then(|s| s.usage_percent),
        load1: load.and_then(|l| l.one),
        load5: load.and_then(|l| l.five),
        disks: disks
            .map(|d| ReportedDisk {
                mount_point: d.mount_point,
                usage_percent: d.usage_percent,
            })
            .collect(),
    }
}

/// Stores a successful poll's answer off the async runtime, stamped with the hub's clock.
/// The poll ends when the system is gone; a store that fails is logged, and the poll goes on.
async fn store_answer(
    state: &Arc<AppState>,
    system: &SystemInfo,
    id: &SystemId,
    answer: PolledAnswer,
    polled_at: String,
) -> ControlFlow<()> {
    let Ok(time) = SnapshotTime::try_from(unix_now()) else {
        tracing::warn!("Poll of {:?}: the hub's clock is past i64::MAX", system.id);
        return ControlFlow::Break(());
    };
    let (app, sys, id) = (Arc::clone(state), system.clone(), id.clone());
    let unit = move || store_polled(&app, &sys, &id, answer, time, polled_at);
    match tokio::task::spawn_blocking(unit).await {
        Ok(Ok(SnapshotStored::Stored(_))) => ControlFlow::Continue(()),
        Ok(Ok(SnapshotStored::SystemGone)) => {
            tracing::debug!("Poll of {:?}: the system is gone", system.id);
            ControlFlow::Break(())
        }
        Ok(Err(err)) => {
            warn_on_failure(Err(err), "store the snapshot", system);
            ControlFlow::Continue(())
        }
        Err(err) => {
            tracing::error!(
                "Poll of {:?}: the snapshot's store failed: {err}",
                system.id
            );
            ControlFlow::Continue(())
        }
    }
}

/// The poll's one unit of blocking work: the snapshot's store, which marks the system online,
/// then, only once it is stored, the registry fill.
fn store_polled(
    app: &AppState,
    system: &SystemInfo,
    id: &SystemId,
    answer: PolledAnswer,
    time: SnapshotTime,
    polled_at: String,
) -> Result<SnapshotStored<LeftOutLog>, rusqlite::Error> {
    let last_seen = LastSeen::PolledAt(polled_at);
    let now = Instant::now();
    let stored = snapshot_intake::store_snapshot(app, id, answer.reported, time, last_seen, now)?;
    if let SnapshotStored::Stored(_) = stored {
        if needs_system_info(system) {
            record_system_info(app, system, &answer.info);
        }
        refresh_memory_capacity(app, system, answer.memory);
    }
    Ok(stored)
}

/// Fills in a system's static info from its first successful poll.
fn record_system_info(app: &AppState, system: &SystemInfo, info: &PolledInfo) {
    let recorded = app.db.update_system_info(
        &system.id,
        info.os_name.as_deref(),
        info.hostname.as_deref(),
        info.kernel.as_deref(),
        info.cpu_model.as_deref(),
        info.cpu_cores,
    );
    warn_on_failure(recorded, "record system info", system);
}

/// Stores the memory capacity the answer reports when it differs from the stored one
/// (RFC 0014 §8): unlike the rest of the system's info, it follows every poll.
fn refresh_memory_capacity(app: &AppState, system: &SystemInfo, reported: Option<MemoryCapacity>) {
    let stored = MemoryCapacity::stored(system);
    if let Some(capacity) = memory_capacity_refresh(stored.as_ref(), reported) {
        let refreshed = app.db.update_memory_capacity(&system.id, &capacity);
        warn_on_failure(refreshed, "refresh the memory capacity", system);
    }
}

/// Stores one alert record per active alert incident the agent reports.
async fn store_agent_alerts(state: &AppState, system: &SystemInfo, client: &reqwest::Client) {
    let alerts_url = format!("{}/api/alerts", system.url.trim_end_matches('/'));
    let Ok(resp) = client.get(&alerts_url).send().await else {
        return;
    };
    let Ok(body) = resp.json::<serde_json::Value>().await else {
        return;
    };
    let Some(active) = body.get("active").and_then(|v| v.as_array()) else {
        return;
    };
    let stored_at = now_iso();
    for alert_val in active {
        let stored = state
            .db
            .insert_alert(&alert_record(system, alert_val, &stored_at));
        warn_on_failure(stored, "store an alert record", system);
    }
}

/// A string field of the agent's untyped JSON, if present.
fn text(value: Option<&serde_json::Value>) -> Option<&str> {
    value.and_then(|v| v.as_str())
}

/// One active alert, read field by field from the agent's untyped JSON.
fn alert_record(
    system: &SystemInfo,
    alert_val: &serde_json::Value,
    stored_at: &str,
) -> AlertRecord {
    let id = text(alert_val.get("id"))
        .map(|s| format!("{}_{}", system.id, s))
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    AlertRecord {
        id,
        system_id: system.id.clone(),
        system_name: system.name.clone(),
        severity: text(alert_val.get("rule").and_then(|v| v.get("severity")))
            .unwrap_or("warning")
            .to_string(),
        message: text(alert_val.get("message")).unwrap_or("").to_string(),
        current_value: alert_val
            .get("current_value")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0) as f32,
        fired_at: text(alert_val.get("fired_at")).unwrap_or("").to_string(),
        stored_at: stored_at.to_string(),
        acknowledged: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::snapshot::Scalar;
    use axum::{Json, Router, routing::get};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_state() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Arc::new(Database::new(path.to_str().unwrap()).unwrap());
        let state = AppState::new(db).unwrap();
        (state, dir)
    }

    fn sample_system(id: &str, url: String) -> SystemInfo {
        SystemInfo {
            id: id.to_string(),
            name: "test-sys".into(),
            url,
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
        }
    }

    async fn spawn_mock_agent(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn poll_system_success_updates_status_hardware_info_and_metrics() {
        let (state, _dir) = temp_state();
        let system_json = serde_json::json!({
            "hostname": "host1",
            "os": {"name": "Ubuntu", "pretty_name": "Ubuntu 22.04"},
            "kernel": "6.6.0",
            "cpu": {"model": "Generic", "logical_cores": 4, "usage_percent": 12.5},
            "memory": {"total_bytes": 1000, "total_display": "1KB", "used_display": "500B", "used_bytes": 500, "usage_percent": 50.0},
            "swap": {"usage_percent": 0.0},
            "load_average": {"one": 0.1, "five": 0.2, "fifteen": 0.3},
            "uptime_seconds": 100,
            "uptime_display": "1m",
            "disks": [{"mount_point": "/", "usage_percent": 40.0, "total_display": "10G", "used_display": "4G"}],
            "top_processes": []
        });
        let alerts_json = serde_json::json!({"active": []});
        let app = Router::new()
            .route(
                "/api/system",
                get(move || {
                    let v = system_json.clone();
                    async move { Json(v) }
                }),
            )
            .route(
                "/api/alerts",
                get(move || {
                    let v = alerts_json.clone();
                    async move { Json(v) }
                }),
            );
        let url = spawn_mock_agent(app).await;

        let system = sample_system("id-1", url);
        state.db.insert_system(&system).unwrap();

        poll_system(state.clone(), &system).await;

        let updated = state.db.get_system("id-1").unwrap().unwrap();
        assert_eq!(updated.status, SystemStatus::Online);
        assert_eq!(updated.hostname.as_deref(), Some("host1"));
        assert_eq!(updated.cpu_cores, Some(4));

        let points = state.db.get_metrics("id-1", "cpu", 10, None).unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].value, 12.5);

        let disk_points = state.db.get_metrics("id-1", "disk:/", 10, None).unwrap();
        assert_eq!(disk_points.len(), 1);

        let live = state.live_metrics.read().unwrap();
        let cpu = live.get("id-1").unwrap().snapshot().scalar(Scalar::Cpu);
        assert_eq!(cpu, Some(12.5));
    }

    /// A mock agent answering `/api/system` with `system_json` and reporting no alerts.
    async fn agent_answering(system_json: serde_json::Value) -> String {
        let app = Router::new()
            .route(
                "/api/system",
                get(move || {
                    let v = system_json.clone();
                    async move { Json(v) }
                }),
            )
            .route(
                "/api/alerts",
                get(|| async { Json(serde_json::json!({"active": []})) }),
            );
        spawn_mock_agent(app).await
    }

    fn point_count(state: &AppState, metric: &str) -> usize {
        state
            .db
            .get_metrics("id-1", metric, 2_000, None)
            .unwrap()
            .len()
    }

    /// RFC 0007 §1: a poll keeps what the snapshot rule keeps. A value the agent didn't report
    /// is no point (not `0.0`), a disk without a mount point is no `disk:` point, and only the
    /// first 1024 valid disks are kept.
    #[tokio::test]
    async fn a_poll_stores_what_the_snapshot_rule_keeps() {
        let (state, _dir) = temp_state();
        let mut disks: Vec<_> = (0..1025)
            .map(|i| serde_json::json!({"mount_point": format!("/d{i}"), "usage_percent": 1.0}))
            .collect();
        disks.insert(1, serde_json::json!({"usage_percent": 99.0}));
        let url = agent_answering(serde_json::json!({
            "cpu": {"logical_cores": 4},
            "memory": {"usage_percent": 50.0},
            "swap": {"usage_percent": 5.0},
            "load_average": {"one": 0.5, "five": 0.25},
            "disks": disks,
        }))
        .await;
        let system = sample_system("id-1", url);
        state.db.insert_system(&system).unwrap();

        poll_system(state.clone(), &system).await;

        let cases = [
            ("cpu", 0),
            ("memory", 1),
            ("swap", 1),
            ("load1", 1),
            ("load5", 1),
            ("disk:", 0),
            ("disk:/d0", 1),
            ("disk:/d1023", 1),
            ("disk:/d1024", 0),
        ];
        for (metric, expected) in cases {
            assert_eq!(point_count(&state, metric), expected, "metric: {metric}");
        }
        let live = state
            .live_metrics
            .read()
            .unwrap()
            .get("id-1")
            .cloned()
            .unwrap();
        assert_eq!(
            live.snapshot().scalar(Scalar::Cpu),
            None,
            "live cpu left out"
        );
        assert_eq!(live.snapshot().disks().count(), 1024, "live disks");
        let sys = state.db.get_system("id-1").unwrap().unwrap();
        assert_eq!(sys.status, SystemStatus::Online);
    }

    /// RFC 0007 §1: the poll parses the system's id once, before anything else. An id that
    /// breaks the rule (a row stored before RFC 0005) is marked offline, naming why, and is
    /// never fetched.
    #[tokio::test]
    async fn a_system_whose_id_breaks_the_rule_is_marked_offline_without_a_fetch() {
        let (state, _dir) = temp_state();
        let (url, polls) = counted_agent().await;
        let system = sample_system("..", url);
        state.db.insert_system(&system).unwrap();

        poll_system(state.clone(), &system).await;

        assert_eq!(polls_seen(&polls), 0, "never fetched");
        let sys = state.db.get_system("..").unwrap().unwrap();
        let status = (sys.status, sys.last_error.as_deref());
        assert_eq!(status, (SystemStatus::Offline, Some("invalid system id")));
    }

    /// RFC 0007 §4: a snapshot that fails to store (here, a trigger aborts every metric
    /// insert) leaves no point and no online status, and the poll goes on to the alerts.
    #[tokio::test]
    async fn a_failed_snapshot_store_writes_nothing_and_the_poll_goes_on() {
        let (state, dir) = temp_state();
        let app = Router::new()
            .route(
                "/api/system",
                get(|| async { Json(serde_json::json!({"cpu": {"usage_percent": 12.5}})) }),
            )
            .route(
                "/api/alerts",
                get(|| async { Json(serde_json::json!({"active": [agent_alert("inc-1", 95.0)]})) }),
            );
        let system = sample_system("id-1", spawn_mock_agent(app).await);
        state.db.insert_system(&system).unwrap();
        rusqlite::Connection::open(dir.path().join("test.db"))
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER boom BEFORE INSERT ON metrics
                 BEGIN SELECT RAISE(ABORT, 'boom'); END;",
            )
            .unwrap();

        poll_system(state.clone(), &system).await;

        assert_eq!(point_count(&state, "cpu"), 0, "nothing stored");
        let sys = state.db.get_system("id-1").unwrap().unwrap();
        assert_eq!(sys.status, SystemStatus::Unknown, "not marked online");
        assert!(!state.live_metrics.read().unwrap().contains_key("id-1"));
        let alerts = state.db.get_alerts(Some("id-1"), None, 10).unwrap();
        assert_eq!(alerts.len(), 1, "the alerts are still polled");
    }

    /// RFC 0007 §4: a poll racing a delete writes nothing, not even live metrics.
    #[tokio::test]
    async fn a_system_deleted_before_its_poll_gets_no_points_and_no_live_metrics() {
        let (state, _dir) = temp_state();
        let url = agent_answering(serde_json::json!({"cpu": {"usage_percent": 12.5}})).await;
        let system = sample_system("id-1", url);
        state.db.insert_system(&system).unwrap();
        state.db.delete_system("id-1").unwrap();

        poll_system(state.clone(), &system).await;

        assert_eq!(point_count(&state, "cpu"), 0);
        assert!(!state.live_metrics.read().unwrap().contains_key("id-1"));
        assert!(state.db.get_system("id-1").unwrap().is_none());
    }

    /// A mock agent answering every `/api/system` poll with `{}`, and how many it answered.
    async fn counted_agent() -> (String, Arc<std::sync::atomic::AtomicU64>) {
        use std::sync::atomic::{AtomicU64, Ordering};
        let polls = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&polls);
        let app = Router::new().route(
            "/api/system",
            get(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                async { Json(serde_json::json!({})) }
            }),
        );
        (spawn_mock_agent(app).await, polls)
    }

    /// Whether `check` holds within 5 seconds.
    async fn within_5_s(mut check: impl FnMut() -> bool) -> bool {
        for _ in 0..500 {
            if check() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    fn polls_seen(polls: &std::sync::atomic::AtomicU64) -> u64 {
        polls.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// RFC 0007 §2: the poller reads the registry at every tick, so a system inserted after
    /// startup is polled on the next one.
    #[tokio::test]
    async fn a_system_inserted_after_startup_is_polled_on_the_next_tick() {
        let (state, _dir) = temp_state();
        let (url, polls) = counted_agent().await;
        tokio::spawn(poll_every(state.clone(), Duration::from_millis(50)));
        tokio::time::sleep(Duration::from_millis(120)).await;

        state.db.insert_system(&sample_system("id-1", url)).unwrap();

        assert!(within_5_s(|| polls_seen(&polls) > 0).await, "polled");
    }

    /// RFC 0007 §2: a tick whose registry read fails polls the systems of the last read that
    /// succeeded. One row that no read can map fails the read for every row.
    #[tokio::test]
    async fn a_tick_whose_registry_read_fails_polls_the_last_reads_systems() {
        let (state, dir) = temp_state();
        let (url, polls) = counted_agent().await;
        state.db.insert_system(&sample_system("id-1", url)).unwrap();
        tokio::spawn(poll_every(state.clone(), Duration::from_millis(50)));
        assert!(
            within_5_s(|| polls_seen(&polls) > 0).await,
            "polled at first"
        );

        state
            .db
            .insert_system(&sample_system("id-2", "http://127.0.0.1:9".into()))
            .unwrap();
        rusqlite::Connection::open(dir.path().join("test.db"))
            .unwrap()
            .execute_batch("UPDATE systems SET poll_interval_secs = -1 WHERE id = 'id-2'")
            .unwrap();
        assert!(state.db.list_systems().is_err(), "the registry read fails");
        let before = polls_seen(&polls);

        let still_polled = within_5_s(|| polls_seen(&polls) >= before + 3).await;
        assert!(still_polled, "the last read's system is still polled");
    }

    /// RFC 0014 §8: a poll refreshes the memory capacity when its answer has both halves, and
    /// leaves the stored one, and the rest of its system info, alone otherwise.
    #[tokio::test]
    async fn poll_system_refreshes_the_memory_capacity_only_from_a_whole_report() {
        let resized = serde_json::json!({"total_bytes": 536_870_912, "total_display": "512.0 MB"});
        let host = (Some("31.0 GB"), Some(33_285_996_544));
        // (name, the answer's memory object, expected stored capacity)
        let cases = [
            (
                "a whole report",
                Some(resized),
                (Some("512.0 MB"), Some(536_870_912)),
            ),
            ("no memory", None, host),
            (
                "bytes only",
                Some(serde_json::json!({"total_bytes": 536_870_912})),
                host,
            ),
            (
                "an empty display",
                Some(serde_json::json!({"total_bytes": 536_870_912, "total_display": ""})),
                host,
            ),
            (
                "display only",
                Some(serde_json::json!({"total_display": "512.0 MB"})),
                host,
            ),
        ];
        for (name, memory, expected) in cases {
            let (state, _dir) = temp_state();
            let mut answer = serde_json::json!({
                "hostname": "polled-host",
                "os": {"pretty_name": "Polled OS"},
                "kernel": "polled-kernel",
                "cpu": {"model": "polled-cpu", "logical_cores": 2},
            });
            if let Some(memory) = memory {
                answer["memory"] = memory;
            }
            let app = Router::new().route(
                "/api/system",
                get(move || {
                    let v = answer.clone();
                    async move { Json(v) }
                }),
            );
            let url = spawn_mock_agent(app).await;
            let system = SystemInfo {
                os: Some("Preset OS".into()),
                hostname: Some("preset-host".into()),
                kernel: Some("preset-kernel".into()),
                cpu_model: Some("preset-cpu".into()),
                cpu_cores: Some(64),
                total_memory_display: host.0.map(str::to_owned),
                total_memory_bytes: host.1,
                ..sample_system("id-1", url)
            };
            state.db.insert_system(&system).unwrap();

            poll_system(state.clone(), &system).await;

            let sys = state.db.get_system("id-1").unwrap().unwrap();
            assert_eq!(
                (sys.total_memory_display.as_deref(), sys.total_memory_bytes),
                expected,
                "{name}: capacity"
            );
            assert_eq!(
                (
                    sys.os.as_deref(),
                    sys.hostname.as_deref(),
                    sys.kernel.as_deref(),
                    sys.cpu_model.as_deref(),
                    sys.cpu_cores,
                ),
                (
                    Some("Preset OS"),
                    Some("preset-host"),
                    Some("preset-kernel"),
                    Some("preset-cpu"),
                    Some(64)
                ),
                "{name}: the rest of the system info keeps its fill-once rule"
            );
        }
    }

    #[tokio::test]
    async fn poll_system_stores_active_alerts_from_agent() {
        let (state, _dir) = temp_state();
        let system_json = serde_json::json!({});
        let alerts_json = serde_json::json!({
            "active": [{
                "id": "4c0a8f0e-2b1d-4f5e-9a37-6c1e2d3b4a50-1",
                "rule": {"severity": "critical"},
                "current_value": 97.5,
                "fired_at": "2026-01-01T00:00:00Z",
                "message": "CPU too high"
            }]
        });
        let app = Router::new()
            .route(
                "/api/system",
                get(move || {
                    let v = system_json.clone();
                    async move { Json(v) }
                }),
            )
            .route(
                "/api/alerts",
                get(move || {
                    let v = alerts_json.clone();
                    async move { Json(v) }
                }),
            );
        let url = spawn_mock_agent(app).await;
        let system = sample_system("id-1", url);
        state.db.insert_system(&system).unwrap();

        poll_system(state.clone(), &system).await;

        let alerts = state.db.get_alerts(Some("id-1"), None, 10).unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].severity, "critical");
        assert_eq!(alerts[0].message, "CPU too high");
        assert_eq!(alerts[0].id, "id-1_4c0a8f0e-2b1d-4f5e-9a37-6c1e2d3b4a50-1");
    }

    fn agent_alert(incident_id: &str, cpu_percent: f64) -> serde_json::Value {
        serde_json::json!({
            "id": incident_id,
            "rule": {"severity": "warning"},
            "current_value": cpu_percent,
            "fired_at": "2026-01-01T00:00:00Z",
            "message": format!("CPU at {cpu_percent}%")
        })
    }

    #[tokio::test]
    async fn poll_system_stores_one_alert_record_per_incident() {
        const RUN: &str = "4c0a8f0e-2b1d-4f5e-9a37-6c1e2d3b4a50";
        let (state, _dir) = temp_state();
        let active = Arc::new(std::sync::Mutex::new(serde_json::json!([])));
        let served = active.clone();
        let app = Router::new()
            .route("/api/system", get(|| async { Json(serde_json::json!({})) }))
            .route(
                "/api/alerts",
                get(move || {
                    let v = served.lock().unwrap().clone();
                    async move { Json(serde_json::json!({ "active": v })) }
                }),
            );
        let url = spawn_mock_agent(app).await;
        let system = sample_system("id-1", url);
        state.db.insert_system(&system).unwrap();

        // (name, (incident sequence, cpu) served by the agent, sequence to acknowledge after
        //  the poll, expected (sequence, acknowledged, cpu, message) per record, by sequence).
        // Each row runs against the records the previous rows left behind.
        type Served = &'static [(u64, f64)];
        type Records = &'static [(u64, bool, f32, &'static str)];
        let cases: [(&str, Served, Option<u64>, Records); 4] = [
            (
                "first incident is recorded",
                &[(1, 95.5)],
                Some(1),
                &[(1, true, 95.5, "CPU at 95.5%")],
            ),
            (
                "the same incident on a later poll keeps its first-seen value, message and acknowledgement",
                &[(1, 97.0)],
                None,
                &[(1, true, 95.5, "CPU at 95.5%")],
            ),
            (
                "two incidents on one poll get a record each",
                &[(2, 96.0), (3, 98.0)],
                None,
                &[
                    (1, true, 95.5, "CPU at 95.5%"),
                    (2, false, 96.0, "CPU at 96%"),
                    (3, false, 98.0, "CPU at 98%"),
                ],
            ),
            (
                "an incident the agent no longer reports stays unacknowledged",
                &[(3, 98.0)],
                None,
                &[
                    (1, true, 95.5, "CPU at 95.5%"),
                    (2, false, 96.0, "CPU at 96%"),
                    (3, false, 98.0, "CPU at 98%"),
                ],
            ),
        ];
        let record_id = |sequence: u64| format!("id-1_{RUN}-{sequence}");
        for (name, served, acknowledge, expected) in cases {
            *active.lock().unwrap() = served
                .iter()
                .map(|&(sequence, cpu)| agent_alert(&format!("{RUN}-{sequence}"), cpu))
                .collect();
            poll_system(state.clone(), &system).await;
            if let Some(sequence) = acknowledge {
                state.db.acknowledge_alert(&record_id(sequence)).unwrap();
            }
            let mut records: Vec<(String, bool, f32, String)> = state
                .db
                .get_alerts(Some("id-1"), None, 10)
                .unwrap()
                .into_iter()
                .map(|a| (a.id, a.acknowledged, a.current_value, a.message))
                .collect();
            records.sort_by(|a, b| a.0.cmp(&b.0));
            let expected: Vec<(String, bool, f32, String)> = expected
                .iter()
                .map(|&(sequence, acked, cpu, message)| {
                    (record_id(sequence), acked, cpu, message.to_string())
                })
                .collect();
            assert_eq!(records, expected, "{name}");
        }
    }

    #[tokio::test]
    async fn poll_system_marks_offline_on_json_parse_error() {
        let (state, _dir) = temp_state();
        let app = Router::new().route("/api/system", get(|| async { "not json" }));
        let url = spawn_mock_agent(app).await;
        let system = sample_system("id-1", url);
        state.db.insert_system(&system).unwrap();

        poll_system(state.clone(), &system).await;

        let updated = state.db.get_system("id-1").unwrap().unwrap();
        assert_eq!(updated.status, SystemStatus::Offline);
        assert!(updated.last_error.unwrap().contains("JSON parse error"));
    }

    /// A mock agent answering `/api/system` with `body`, as given, and no alerts.
    async fn agent_with_body(body: axum::body::Body) -> String {
        let body = Arc::new(std::sync::Mutex::new(Some(body)));
        let app = Router::new()
            .route(
                "/api/system",
                get(move || {
                    let body = body.lock().unwrap().take().unwrap_or_default();
                    async move { body }
                }),
            )
            .route(
                "/api/alerts",
                get(|| async { Json(serde_json::json!({"active": []})) }),
            );
        spawn_mock_agent(app).await
    }

    /// RFC 0007 §1: the poll reads `/api/system` up to 4 MiB, inclusive.
    #[tokio::test]
    async fn a_system_body_of_exactly_4_mib_is_read() {
        let frame = r#"{"cpu":{"usage_percent":12.5},"pad":""}"#;
        let pad = "x".repeat(4 * 1024 * 1024 - frame.len());
        let body = frame.replace(r#""pad":"""#, &format!(r#""pad":"{pad}""#));
        assert_eq!(body.len(), 4 * 1024 * 1024);
        let (state, _dir) = temp_state();
        let system = sample_system("id-1", agent_with_body(body.into()).await);
        state.db.insert_system(&system).unwrap();

        poll_system(state.clone(), &system).await;

        assert_eq!(point_count(&state, "cpu"), 1, "the answer was read");
        let sys = state.db.get_system("id-1").unwrap().unwrap();
        assert_eq!(sys.status, SystemStatus::Online);
    }

    /// RFC 0007 §1: a larger answer is refused while it streams, whatever its headers say, and
    /// the system shows offline, naming the cap.
    #[tokio::test]
    async fn a_chunked_system_body_over_4_mib_is_refused_and_marks_the_system_offline() {
        // No Content-Length: only a cap on the bytes read can refuse it.
        let pad = "x".repeat(1024);
        let chunks = std::iter::once(r#"{"cpu":{"usage_percent":12.5},"pad":""#.to_string())
            .chain(std::iter::repeat_n(pad, 4 * 1024 + 1))
            .chain(std::iter::once(r#""}"#.to_string()))
            .map(|chunk| Ok::<_, std::io::Error>(axum::body::Bytes::from(chunk)));
        let body = axum::body::Body::from_stream(futures_util::stream::iter(chunks));
        let (state, _dir) = temp_state();
        let system = sample_system("id-1", agent_with_body(body).await);
        state.db.insert_system(&system).unwrap();

        poll_system(state.clone(), &system).await;

        assert_eq!(point_count(&state, "cpu"), 0, "the answer wasn't stored");
        let sys = state.db.get_system("id-1").unwrap().unwrap();
        let status = (sys.status, sys.last_error.as_deref());
        assert_eq!(status, (SystemStatus::Offline, Some("body over 4 MiB")));
    }

    #[tokio::test]
    async fn poll_system_marks_offline_on_http_error_status() {
        let (state, _dir) = temp_state();
        let app = Router::new().route(
            "/api/system",
            get(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
        );
        let url = spawn_mock_agent(app).await;
        let system = sample_system("id-1", url);
        state.db.insert_system(&system).unwrap();

        poll_system(state.clone(), &system).await;

        let updated = state.db.get_system("id-1").unwrap().unwrap();
        assert_eq!(updated.status, SystemStatus::Offline);
        assert!(updated.last_error.unwrap().contains("500"));
    }

    #[tokio::test]
    async fn poll_system_marks_offline_on_connection_error() {
        let (state, _dir) = temp_state();
        // Nothing is listening on this port: reqwest should fail to connect.
        let system = sample_system("id-1", "http://127.0.0.1:1".to_string());
        state.db.insert_system(&system).unwrap();

        poll_system(state.clone(), &system).await;

        let updated = state.db.get_system("id-1").unwrap().unwrap();
        assert_eq!(updated.status, SystemStatus::Offline);
        assert!(updated.last_error.is_some());
    }

    /// Characterisation (`rosette-auditor` on RFC 0007): every offline marking of a poll writes
    /// the poll's own time as `last_seen`, so a polled system's last seen moves on while it is
    /// down. RFC 0010's contact time is where that changes.
    #[tokio::test]
    async fn a_failed_poll_writes_its_own_time_as_last_seen() {
        let (state, _dir) = temp_state();
        let failing = Router::new().route(
            "/api/system",
            get(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
        );
        let refused = "http://127.0.0.1:1".to_string();
        let cases = [
            (
                "an HTTP error",
                "http-error",
                spawn_mock_agent(failing).await,
            ),
            ("a connection error", "refused", refused.clone()),
            ("an id that breaks the rule", "..", refused),
        ];
        for (name, id, url) in cases {
            let system = SystemInfo {
                last_seen: "before the poll".into(),
                ..sample_system(id, url)
            };
            state.db.insert_system(&system).unwrap();

            let before = now_iso();
            poll_system(state.clone(), &system).await;
            let after = now_iso();

            let seen = state.db.get_system(id).unwrap().unwrap().last_seen;
            let polled = before.as_str()..=after.as_str();
            assert!(
                polled.contains(&seen.as_str()),
                "{name}: last seen {seen:?}"
            );
        }
    }

    /// RFC 0007 §1: the poll's edge maps what the agent reported, and nothing it didn't:
    /// a value missing from the answer stays `None`, never `0.0`, and so does a mount point.
    #[test]
    fn a_poll_answer_parses_into_what_the_agent_reported_and_nothing_more() {
        let full = r#"{"cpu":{"usage_percent":12.5},"memory":{"usage_percent":50.0},
            "swap":{"usage_percent":5.0},"load_average":{"one":0.5,"five":0.25},
            "disks":[{"mount_point":"/","usage_percent":40.0},{"usage_percent":60.0}]}"#;
        let cases = [
            (
                "every value reported",
                full,
                ReportedSnapshot {
                    cpu: Some(12.5),
                    memory: Some(50.0),
                    swap: Some(5.0),
                    load1: Some(0.5),
                    load5: Some(0.25),
                    disks: vec![
                        ReportedDisk {
                            mount_point: Some("/".into()),
                            usage_percent: Some(40.0),
                        },
                        ReportedDisk {
                            mount_point: None,
                            usage_percent: Some(60.0),
                        },
                    ],
                },
            ),
            ("nothing reported", "{}", ReportedSnapshot::default()),
            (
                "empty sections",
                r#"{"cpu":{},"memory":{},"swap":{},"load_average":{},"disks":[{}]}"#,
                ReportedSnapshot {
                    disks: vec![ReportedDisk::default()],
                    ..ReportedSnapshot::default()
                },
            ),
        ];
        for (case, body, expected) in cases {
            let agent: AgentResponse = serde_json::from_str(body).unwrap();
            let answer = PolledAnswer::from(agent);
            assert_eq!(answer.reported, expected, "case: {case}");
        }
    }

    #[test]
    fn agent_response_deserializes_with_all_fields_missing() {
        let resp: AgentResponse = serde_json::from_str("{}").unwrap();
        assert!(resp.hostname.is_none());
        assert!(resp.os.is_none());
        assert!(resp.cpu.is_none());
        assert!(resp.disks.is_none());
    }

    #[test]
    fn agent_response_deserializes_partial_nested_fields() {
        let resp: AgentResponse =
            serde_json::from_str(r#"{"cpu":{"usage_percent":42.0}}"#).unwrap();
        assert_eq!(resp.cpu.unwrap().usage_percent, Some(42.0));
    }

    mod applications {
        use super::*;
        use crate::applications::{
            ApplicationHealth, ApplicationName, ApplicationReport, Gauges, HeldRound, RecentRounds,
            RoundId, ScrapeInterval, ScrapeRound,
        };
        use crate::state::SystemApplications;
        use axum::http::{StatusCode, header};
        use axum::response::IntoResponse;
        use std::sync::atomic::{AtomicU64, Ordering};

        const RUN: &str = "6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b";

        fn system_json() -> serde_json::Value {
            serde_json::json!({"hostname": "host1", "cpu": {"usage_percent": 1.0}})
        }

        fn round_body(seq: u64, heap: f64, scraped_at: u64) -> String {
            serde_json::json!({
                "round": {"run": RUN, "seq": seq},
                "interval_secs": 15,
                "scraped_at": scraped_at,
                "applications": [{
                    "name": "orders", "health": "up", "version": "2.4.1",
                    "gauges": {"heap_used_bytes": heap}
                }]
            })
            .to_string()
        }

        /// An agent whose `/api/applications` answers `status` with `body`; `None` leaves
        /// the route out, as an agent older than RFC 0009 does.
        fn agent(applications: Option<(StatusCode, String)>) -> Router {
            let base = Router::new()
                .route("/api/system", get(|| async { Json(system_json()) }))
                .route(
                    "/api/alerts",
                    get(|| async { Json(serde_json::json!({"active": []})) }),
                );
            match applications {
                None => base,
                Some((status, body)) => base.route(
                    "/api/applications",
                    get(move || {
                        let body = body.clone();
                        async move { (status, [(header::CONTENT_TYPE, "application/json")], body) }
                    }),
                ),
            }
        }

        async fn polled(state: &Arc<AppState>, app: Router) -> SystemInfo {
            let url = spawn_mock_agent(app).await;
            let system = sample_system("id-1", url);
            state.db.insert_system(&system).unwrap();
            system
        }

        fn up_points(state: &AppState) -> Vec<u64> {
            state
                .db
                .get_metrics("id-1", "app:orders:up", 100, None)
                .unwrap()
                .iter()
                .map(|p| p.timestamp)
                .collect()
        }

        fn shown_seq(state: &AppState) -> Option<u64> {
            let live = state.live_applications.read().unwrap();
            live.get("id-1")?
                .shown
                .as_ref()
                .map(|held| held.round.id().seq())
        }

        fn status(state: &AppState) -> SystemStatus {
            state.db.get_system("id-1").unwrap().unwrap().status
        }

        fn now_secs() -> u64 {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        }

        /// A held round (seq 9) and a remembered id, as an earlier push or poll left them.
        fn seed(state: &AppState) -> (RoundId, crate::applications::RoundDigest) {
            let round = ScrapeRound::new(
                RoundId::parse(RUN, 9).unwrap(),
                ScrapeInterval::from_secs(15).unwrap(),
                vec![ApplicationReport {
                    name: ApplicationName::parse("orders").unwrap(),
                    health: ApplicationHealth::Up,
                    version: None,
                    gauges: Gauges::default(),
                }],
            )
            .unwrap();
            let digest = state.digester.digest(&round);
            let mut recent = RecentRounds::default();
            recent.remember(round.id(), digest);
            let id = round.id();
            let entry = SystemApplications {
                shown: Some(HeldRound {
                    round,
                    received_at: 1,
                }),
                recent,
                ..SystemApplications::default()
            };
            state
                .live_applications
                .write()
                .unwrap()
                .insert("id-1".into(), entry);
            (id, digest)
        }

        #[tokio::test]
        async fn a_polled_round_is_stored_at_hub_time_and_shown() {
            let (state, _dir) = temp_state();
            // A far-future agent clock changes nothing.
            let body = round_body(1, 300.0, u64::MAX);
            let system = polled(&state, agent(Some((StatusCode::OK, body)))).await;
            let before = now_secs();
            poll_system(state.clone(), &system).await;
            let after = now_secs();
            let points = up_points(&state);
            assert_eq!(points.len(), 1, "one up point");
            // A few seconds of slack for a host clock that steps (WSL2 resyncs); the agent's
            // clock here says u64::MAX, so any slack still tells the two apart.
            assert!(
                (before.saturating_sub(30)..=after + 30).contains(&points[0]),
                "hub time: {} not near {before}..={after}",
                points[0]
            );
            assert_eq!(shown_seq(&state), Some(1));
            assert_eq!(status(&state), SystemStatus::Online);
        }

        #[tokio::test]
        async fn the_same_round_on_two_polls_is_stored_once() {
            let (state, _dir) = temp_state();
            let body = round_body(1, 300.0, 1);
            let system = polled(&state, agent(Some((StatusCode::OK, body)))).await;
            poll_system(state.clone(), &system).await;
            poll_system(state.clone(), &system).await;
            assert_eq!(up_points(&state).len(), 1);
        }

        #[tokio::test]
        async fn a_systems_polls_are_paced_two_rounds_then_one_per_8_seconds() {
            let (state, _dir) = temp_state();
            let seq = Arc::new(AtomicU64::new(0));
            let next = seq.clone();
            let app = agent(None).route(
                "/api/applications",
                get(move || {
                    let n = next.fetch_add(1, Ordering::SeqCst) + 1;
                    async move {
                        (
                            [(header::CONTENT_TYPE, "application/json")],
                            round_body(n, 300.0, 1),
                        )
                            .into_response()
                    }
                }),
            );
            let system = polled(&state, app).await;
            for _ in 0..3 {
                poll_system(state.clone(), &system).await;
            }
            assert_eq!(
                seq.load(Ordering::SeqCst),
                3,
                "three distinct rounds were polled"
            );
            assert_eq!(up_points(&state).len(), 2, "the burst is two");
            assert_eq!(shown_seq(&state), Some(2));
        }

        #[tokio::test]
        async fn no_round_or_an_older_agent_forgets_the_shown_round_and_keeps_the_rest() {
            let null = r#"{"round":null,"interval_secs":null,"scraped_at":null,"applications":[]}"#;
            let cases = [
                ("an agent older than RFC 0009 (404)", agent(None)),
                (
                    "applications off (null round)",
                    agent(Some((StatusCode::OK, null.into()))),
                ),
            ];
            for (case, app) in cases {
                let (state, _dir) = temp_state();
                let system = polled(&state, app).await;
                let (id, digest) = seed(&state);
                poll_system(state.clone(), &system).await;
                let live = state.live_applications.read().unwrap();
                let entry = live.get("id-1").expect("the entry stays");
                assert!(entry.shown.is_none(), "case: {case}: nothing shown");
                assert!(
                    entry.recent.contains(id, digest),
                    "case: {case}: recent kept"
                );
                drop(live);
                assert_eq!(status(&state), SystemStatus::Online, "case: {case}");
            }
        }

        #[tokio::test]
        async fn an_unusable_answer_leaves_the_shown_round_and_the_status_alone() {
            let oversize = format!(r#"{{"round":null,"pad":"{}"}}"#, "x".repeat(300 * 1024));
            let just_over = format!(r#"{{"round":null,"pad":"{}"}}"#, "x".repeat(256 * 1024));
            assert!(just_over.len() > 256 * 1024);
            let refused = round_body(1, 300.0, 1).replace(RUN, "not-a-uuid");
            let cases = [
                (
                    "a server error",
                    StatusCode::INTERNAL_SERVER_ERROR,
                    round_body(1, 300.0, 1),
                ),
                (
                    "a wrong token",
                    StatusCode::UNAUTHORIZED,
                    round_body(1, 300.0, 1),
                ),
                ("bad JSON", StatusCode::OK, "{not json".to_string()),
                ("a body over 256 KiB", StatusCode::OK, oversize),
                ("a body just over 256 KiB", StatusCode::OK, just_over),
                ("a refused round", StatusCode::OK, refused),
            ];
            for (case, code, body) in cases {
                let (state, _dir) = temp_state();
                let system = polled(&state, agent(Some((code, body)))).await;
                seed(&state);
                poll_system(state.clone(), &system).await;
                assert_eq!(shown_seq(&state), Some(9), "case: {case}: still shown");
                assert!(up_points(&state).is_empty(), "case: {case}: nothing stored");
                assert_eq!(status(&state), SystemStatus::Online, "case: {case}");
            }
        }

        #[tokio::test]
        async fn a_body_of_exactly_256_kib_is_read() {
            let frame = r#"{"round":null,"pad":""}"#;
            let body = frame.replace(
                r#""pad":"""#,
                &format!(r#""pad":"{}""#, "x".repeat(256 * 1024 - frame.len())),
            );
            assert_eq!(body.len(), 256 * 1024);
            let (state, _dir) = temp_state();
            let system = polled(&state, agent(Some((StatusCode::OK, body)))).await;
            seed(&state);
            poll_system(state.clone(), &system).await;
            assert_eq!(
                shown_seq(&state),
                None,
                "the null round was read: nothing shown"
            );
        }

        #[tokio::test]
        async fn a_chunked_body_over_256_kib_is_refused_while_it_streams() {
            // No Content-Length: only a cap on the bytes read can refuse it.
            let app = agent(None).route(
                "/api/applications",
                get(|| async {
                    let pad = "x".repeat(1024);
                    let chunks = std::iter::once(r#"{"round":null,"pad":""#.to_string())
                        .chain(std::iter::repeat_n(pad, 300))
                        .chain(std::iter::once(r#""}"#.to_string()))
                        .map(|chunk| Ok::<_, std::io::Error>(axum::body::Bytes::from(chunk)));
                    axum::body::Body::from_stream(futures_util::stream::iter(chunks))
                }),
            );
            let (state, _dir) = temp_state();
            let system = polled(&state, app).await;
            seed(&state);
            poll_system(state.clone(), &system).await;
            assert_eq!(shown_seq(&state), Some(9), "the null round wasn't read");
        }

        /// An agent answering a new round (seq 1, 2, …) on every poll.
        fn counting_agent() -> Router {
            let seq = Arc::new(AtomicU64::new(0));
            agent(None).route(
                "/api/applications",
                get(move || {
                    let n = seq.fetch_add(1, Ordering::SeqCst) + 1;
                    async move {
                        (
                            [(header::CONTENT_TYPE, "application/json")],
                            round_body(n, 300.0, 1),
                        )
                            .into_response()
                    }
                }),
            )
        }

        #[tokio::test]
        async fn each_system_polls_under_a_pace_of_its_own() {
            let (state, _dir) = temp_state();
            let mut systems = Vec::new();
            for id in ["id-1", "id-2"] {
                let url = spawn_mock_agent(counting_agent()).await;
                let system = sample_system(id, url);
                state.db.insert_system(&system).unwrap();
                systems.push(system);
            }
            for system in &systems {
                for _ in 0..3 {
                    poll_system(state.clone(), system).await;
                }
            }
            for id in ["id-1", "id-2"] {
                let stored = state.db.get_metrics(id, "app:orders:up", 10, None).unwrap();
                assert_eq!(stored.len(), 2, "{id}: its own burst of two");
            }
        }

        #[tokio::test]
        async fn a_systems_poll_pace_refills_one_round_per_8_seconds() {
            let (state, _dir) = temp_state();
            let system = polled(&state, counting_agent()).await;
            for _ in 0..3 {
                poll_system(state.clone(), &system).await;
            }
            assert_eq!(up_points(&state).len(), 2, "the burst is two");
            tokio::time::sleep(Duration::from_millis(8_200)).await;
            poll_system(state.clone(), &system).await;
            assert_eq!(up_points(&state).len(), 3, "one token refilled after 8 s");
            assert_eq!(shown_seq(&state), Some(4));
        }

        #[tokio::test]
        async fn a_poll_never_follows_a_redirect() {
            let hits = Arc::new(AtomicU64::new(0));
            let counted = hits.clone();
            let target = spawn_mock_agent(Router::new().fallback(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                async { Json(system_json()) }
            }))
            .await;
            let redirect = move |path: &'static str| {
                let location = format!("{target}{path}");
                move || {
                    let location = location.clone();
                    async move {
                        (
                            StatusCode::TEMPORARY_REDIRECT,
                            [(header::LOCATION, location)],
                        )
                            .into_response()
                    }
                }
            };
            let cases = [
                (
                    "the system poll",
                    Router::new().route("/api/system", get(redirect("/api/system"))),
                    SystemStatus::Offline,
                ),
                (
                    "the applications poll",
                    agent(None).route("/api/applications", get(redirect("/api/applications"))),
                    SystemStatus::Online,
                ),
                (
                    "the alerts poll",
                    Router::new()
                        .route("/api/system", get(|| async { Json(system_json()) }))
                        .route("/api/alerts", get(redirect("/api/alerts")))
                        .route(
                            "/api/applications",
                            get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
                        ),
                    SystemStatus::Online,
                ),
            ];
            for (case, app, expected) in cases {
                let (state, _dir) = temp_state();
                let system = polled(&state, app).await;
                seed(&state);
                poll_system(state.clone(), &system).await;
                assert_eq!(
                    shown_seq(&state),
                    Some(9),
                    "case: {case}: a 3xx clears nothing"
                );
                assert_eq!(hits.load(Ordering::SeqCst), 0, "case: {case}: not followed");
                let updated = state.db.get_system("id-1").unwrap().unwrap();
                assert_eq!(updated.status, expected, "case: {case}");
                if expected == SystemStatus::Offline {
                    let error = updated.last_error.unwrap_or_default();
                    assert!(
                        error.contains("307"),
                        "case: {case}: names the 3xx: {error}"
                    );
                }
            }
        }
    }
}
