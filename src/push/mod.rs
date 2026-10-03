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

use futures_util::{Sink, SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::interval;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::applications::round::ScrapeRound;
use crate::applications::scrape_loop::RoundReceiver;
use crate::collectors::{SnapshotReceiver, monotonic_now};
use crate::models::SystemSnapshot;
use crate::snapshot::{PublishedSnapshot, SnapshotFreshness, SnapshotSeq};

pub(crate) mod application_frame;
pub mod identity;

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

/// What the push client sends, kept across its connections.
pub struct PushFeed {
    pub snapshots: SnapshotCursor,
    /// The scrape loop's rounds: `None` when applications are off.
    pub rounds: Option<RoundReceiver>,
}

/// The published snapshots, and which of them the hub was last sent.
pub struct SnapshotCursor {
    snapshots: SnapshotReceiver,
    last_sent: Option<SnapshotSeq>,
}

impl SnapshotCursor {
    pub fn new(snapshots: SnapshotReceiver) -> Self {
        Self {
            snapshots,
            last_sent: None,
        }
    }

    /// What a push tick at `now` owes the hub. Sent snapshots are told apart by seq, never
    /// by time, so a wall clock step neither repeats nor hides one.
    fn due(&self, now: std::time::Instant) -> Due {
        let snapshot = self.snapshots.borrow().clone();
        match snapshot.freshness(now) {
            SnapshotFreshness::Stale => Due::Stale,
            SnapshotFreshness::Fresh if self.last_sent == Some(snapshot.seq) => Due::Unchanged,
            SnapshotFreshness::Fresh => Due::Send(snapshot),
        }
    }

    /// Waits until the published snapshot is fresh, so a hung collector makes no handshake.
    /// `Err` if the collector has ended, when none ever will be.
    async fn fresh(&mut self) -> Result<(), watch::error::RecvError> {
        let is_fresh =
            |s: &Arc<PublishedSnapshot>| s.freshness(monotonic_now()) == SnapshotFreshness::Fresh;
        self.snapshots.wait_for(is_fresh).await.map(drop)
    }

    fn mark_sent(&mut self, seq: SnapshotSeq) {
        self.last_sent = Some(seq);
    }
}

/// What a push tick owes the hub.
enum Due {
    Send(Arc<PublishedSnapshot>),
    /// The hub has the published snapshot already.
    Unchanged,
    /// The published snapshot is stale: the connection closes, so the hub marks the system
    /// offline.
    Stale,
}

/// Why `run_push_client` returned an error.
#[derive(Debug)]
pub enum PushError {
    /// The system collector has ended, so no fresh snapshot will come: pushing is over.
    CollectorEnded,
    /// Connecting or authenticating failed; worth another try.
    Connection(String),
}

impl std::fmt::Display for PushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CollectorEnded => write!(f, "the system collector has ended"),
            Self::Connection(err) => write!(f, "{err}"),
        }
    }
}

/// Why a connected push session ended.
enum SessionEnd {
    /// A send failed, or the hub hung up.
    Lost,
    /// The published snapshot went stale.
    Stale,
}

/// Connect to the hub via WebSocket, once the published snapshot is fresh, and push each
/// published snapshot once using MessagePack, and each scrape round as an application frame.
pub async fn run_push_client(
    hub_url: &str,
    token: &str,
    system_id: &str,
    push_interval_secs: u64,
    feed: &mut PushFeed,
) -> Result<(), PushError> {
    feed.snapshots
        .fresh()
        .await
        .map_err(|_| PushError::CollectorEnded)?;
    let url = format!("{}/api/push", hub_url.trim_end_matches('/'));
    tracing::info!("🔌 Connecting to hub via push: {url}");
    let (mut ws, _) = connect_async(&url)
        .await
        .map_err(|e| PushError::Connection(e.to_string()))?;
    authenticate(&mut ws, system_id, token)
        .await
        .map_err(PushError::Connection)?;

    // A round re-sent after a reconnect keeps its round id, so the hub drops it as a duplicate
    // if it already has it.
    let resent = match current_round(&mut feed.rounds) {
        Some(round) => send_round(&mut ws, &round).await.is_ok(),
        None => true,
    };
    let end = if resent {
        pump(&mut ws, system_id, push_interval_secs, feed).await
    } else {
        SessionEnd::Lost
    };
    match end {
        SessionEnd::Lost => tracing::error!("Push connection lost, will retry..."),
        SessionEnd::Stale => {
            tracing::warn!("The snapshot is stale; closing the push connection");
            if let Err(err) = ws.close(None).await {
                tracing::debug!("Closing the push connection failed: {err}");
            }
        }
    }
    tracing::warn!("Push connection closed");
    Ok(())
}

/// Sends snapshots, pings and rounds over a connected, authenticated `ws` until the session
/// ends.
async fn pump<S>(ws: &mut S, system_id: &str, interval_secs: u64, feed: &mut PushFeed) -> SessionEnd
where
    S: Sink<Message>
        + futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let rounds = &mut feed.rounds;
    let mut tick = interval(Duration::from_secs(interval_secs.max(2)));
    let mut ping_tick = interval(Duration::from_secs(30));
    loop {
        let alive = tokio::select! {
            _ = tick.tick() => match feed.snapshots.due(monotonic_now()) {
                Due::Send(snapshot) => {
                    send_snapshot(ws, system_id, &mut feed.snapshots, &snapshot).await.is_ok()
                }
                Due::Unchanged => true,
                Due::Stale => return SessionEnd::Stale,
            },
            _ = ping_tick.tick() => ws.send(Message::Ping(vec![])).await.is_ok(),
            round = next_round(rounds), if rounds.is_some() => match round {
                Some(round) => send_round(ws, &round).await.is_ok(),
                None => true,
            },
            // A tick with no new snapshot sends nothing, so a hub hanging up is noticed here.
            incoming = ws.next() => !hub_hung_up(&incoming),
        };
        if !alive {
            return SessionEnd::Lost;
        }
    }
}

/// Whether what the hub's side of the socket yielded means the connection is gone.
fn hub_hung_up<E>(incoming: &Option<Result<Message, E>>) -> bool {
    matches!(incoming, Some(Ok(Message::Close(_)) | Err(_)) | None)
}

/// The line logged when the hub accepts the handshake.
fn authenticated_line(system_id: &str) -> String {
    format!("✅ Push authenticated — system_id={system_id:?}")
}

/// Sends the auth message and reads the hub's answer; `Err` on an `auth_error`.
async fn authenticate<S>(ws: &mut S, system_id: &str, token: &str) -> Result<(), String>
where
    S: Sink<Message>
        + futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
    S::Error: std::fmt::Display,
{
    let auth = serde_json::json!({
        "type": "auth",
        "system_id": system_id,
        "token": token,
    });
    ws.send(Message::Text(auth.to_string()))
        .await
        .map_err(|e| e.to_string())?;

    if let Some(Ok(Message::Text(resp))) = ws.next().await
        && let Ok(msg) = serde_json::from_str::<HubMessage>(&resp)
    {
        if msg.msg_type == "auth_ok" {
            tracing::info!("{}", authenticated_line(system_id));
        } else {
            tracing::error!("❌ Push auth failed: {}", msg.message);
            return Err(format!("auth failed: {}", msg.message));
        }
    }
    Ok(())
}

/// Sends `snapshot` as a snapshot frame stamped with its `collected_at`, and marks it sent.
/// One that can't be encoded is skipped, never sent empty.
async fn send_snapshot<S>(
    ws: &mut S,
    system_id: &str,
    cursor: &mut SnapshotCursor,
    snapshot: &PublishedSnapshot,
) -> Result<(), S::Error>
where
    S: Sink<Message> + Unpin,
{
    let payload = snapshot_payload(system_id, &snapshot.system, snapshot.collected_at);
    let sent = match rmp_serde::to_vec(&payload) {
        Ok(frame) => ws.send(Message::Binary(frame)).await,
        Err(err) => {
            tracing::warn!("Skipping a snapshot frame that failed to encode: {err}");
            Ok(())
        }
    };
    if sent.is_ok() {
        cursor.mark_sent(snapshot.seq);
    }
    sent
}

/// The snapshot frame for one snapshot, stamped with `timestamp`.
fn snapshot_payload(system_id: &str, snap: &SystemSnapshot, timestamp: u64) -> PushPayload {
    PushPayload {
        system_id: system_id.to_string(),
        hostname: snap.hostname.clone(),
        os_name: snap.os.pretty_name.clone(),
        kernel: snap.kernel.clone(),
        cpu_percent: snap.cpu.usage_percent,
        cpu_cores: snap.cpu.logical_cores,
        cpu_model: snap.cpu.model.clone(),
        memory_percent: snap.memory.usage_percent,
        memory_used_display: snap.memory.used_display.clone(),
        memory_total_display: snap.memory.total_display.clone(),
        memory_used_bytes: snap.memory.used_bytes,
        memory_total_bytes: snap.memory.total_bytes,
        swap_percent: snap.swap.usage_percent,
        load_one: snap.load_average.one,
        load_five: snap.load_average.five,
        load_fifteen: snap.load_average.fifteen,
        uptime_seconds: snap.uptime_seconds,
        uptime_display: snap.uptime_display.clone(),
        disks: snap
            .disks
            .iter()
            .map(|d| DiskPayload {
                mount_point: d.mount_point.clone(),
                usage_percent: d.usage_percent,
                total_display: d.total_display.clone(),
                used_display: d.used_display.clone(),
            })
            .collect(),
        top_processes: snap
            .top_processes
            .iter()
            .take(10)
            .map(|p| ProcessPayload {
                pid: p.pid,
                name: p.name.clone(),
                cpu_usage: p.cpu_usage,
                memory_usage_display: p.memory_usage_display.clone(),
                memory_percent: p.memory_percent,
            })
            .collect(),
        timestamp,
    }
}

/// The latest round, to send right after the handshake, if the scrape loop still runs. Marks
/// it seen, so the publish arm doesn't send it again.
fn current_round(rounds: &mut Option<RoundReceiver>) -> Option<Arc<ScrapeRound>> {
    let receiver = rounds.as_mut()?;
    if receiver.has_changed().is_err() {
        give_up_rounds(rounds);
        return None;
    }
    receiver.borrow_and_update().clone()
}

/// Waits for the scrape loop's next round. A closed channel fails on every call, so it is
/// given up at once: the caller's guard then disables the arm instead of spinning on it.
async fn next_round(rounds: &mut Option<RoundReceiver>) -> Option<Arc<ScrapeRound>> {
    let receiver = rounds.as_mut()?;
    match receiver.changed().await {
        Ok(()) => receiver.borrow_and_update().clone(),
        Err(_) => {
            give_up_rounds(rounds);
            None
        }
    }
}

/// Stops looking at a closed round channel, for the agent's lifetime: the caller's loop
/// keeps `rounds` across reconnects, so this logs once.
fn give_up_rounds(rounds: &mut Option<RoundReceiver>) {
    tracing::error!("The application scrape loop ended; no more application frames");
    *rounds = None;
}

/// Sends one round as an application frame. A round that can't be encoded is skipped.
async fn send_round<S>(ws: &mut S, round: &ScrapeRound) -> Result<(), S::Error>
where
    S: Sink<Message> + Unpin,
{
    match application_frame::encode(round) {
        Ok(frame) => ws.send(Message::Binary(frame)).await,
        Err(err) => {
            tracing::warn!("Skipping an application frame that failed to encode: {err}");
            Ok(())
        }
    }
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

    use crate::models::{
        CpuInfo, DiskInfo, LimitInfo, LoadAverage, MemoryInfo, OsInfo, ProcessInfo,
        ReadingSourceName, SwapInfo,
    };

    /// A snapshot whose same-typed fields all hold different values, so a frame that moves
    /// one field onto another's position can't encode to the same bytes.
    fn distinct_snapshot(processes: u32) -> SystemSnapshot {
        SystemSnapshot {
            hostname: "host-h".into(),
            os: OsInfo {
                name: "os-name".into(),
                version: "os-version".into(),
                id: "os-id".into(),
                pretty_name: "os-pretty".into(),
            },
            kernel: "kernel-k".into(),
            uptime_seconds: 3333,
            uptime_display: "uptime-u".into(),
            load_average: LoadAverage {
                one: 0.5,
                five: 0.25,
                fifteen: 0.125,
            },
            cpu: CpuInfo {
                model: "cpu-model".into(),
                physical_cores: 4,
                logical_cores: 8,
                usage_percent: 12.5,
                frequency_mhz: 2400,
                // Not on the frame: its shape doesn't change (RFC 0014 §8).
                capacity_cpus: 1.5,
                steal_percent: Some(3.5),
                source: ReadingSourceName::Cgroup,
            },
            memory: MemoryInfo {
                total_bytes: 2222,
                used_bytes: 1111,
                free_bytes: 555,
                available_bytes: 666,
                total_display: "mem-total".into(),
                used_display: "mem-used".into(),
                usage_percent: 33.25,
                limit: Some(LimitInfo::Bounded(3333)),
                source: ReadingSourceName::Cgroup,
            },
            swap: SwapInfo {
                total_bytes: 777,
                used_bytes: 88,
                free_bytes: 689,
                total_display: "swap-total".into(),
                used_display: "swap-used".into(),
                usage_percent: 7.75,
                limit: Some(LimitInfo::Unbounded),
                source: ReadingSourceName::Unavailable,
            },
            disks: vec![
                DiskInfo {
                    mount_point: "disk-mount".into(),
                    filesystem: "disk-fs".into(),
                    total_bytes: 9999,
                    used_bytes: 4444,
                    free_bytes: 5555,
                    total_display: "disk-total".into(),
                    used_display: "disk-used".into(),
                    usage_percent: 50.5,
                },
                DiskInfo {
                    mount_point: "disk2-mount".into(),
                    filesystem: "disk2-fs".into(),
                    total_bytes: 8888,
                    used_bytes: 3333,
                    free_bytes: 5555,
                    total_display: "disk2-total".into(),
                    used_display: "disk2-used".into(),
                    usage_percent: 60.25,
                },
            ],
            networks: vec![],
            top_processes: (1..=processes)
                .map(|pid| ProcessInfo {
                    pid,
                    name: format!("proc-{pid}"),
                    cpu_usage: 1.5,
                    memory_usage_bytes: 1234,
                    memory_usage_display: "proc-mem".into(),
                    memory_percent: 0.75,
                    status: "proc-status".into(),
                })
                .collect(),
        }
    }

    /// The snapshot frame is a positional MessagePack array (`rmp_serde::to_vec`), so field
    /// order is the contract with every hub already deployed. The golden is written by
    /// `testdata/generate_snapshot_frame_v1.py`, an encoder independent of rmp-serde, and the
    /// hub's test decodes the same bytes. A change here is a push frame change, which needs an
    /// RFC (CLAUDE.md).
    #[test]
    fn snapshot_frame_encodes_to_the_published_bytes() {
        const GOLDEN: &[u8] = include_bytes!("../../testdata/snapshot-frame-v1.msgpack");
        let payload = snapshot_payload("system-s", &distinct_snapshot(1), 4_444_444);
        assert_eq!(rmp_serde::to_vec(&payload).unwrap(), GOLDEN);
    }

    #[test]
    fn snapshot_frame_carries_the_ten_busiest_processes() {
        let payload = snapshot_payload("system-s", &distinct_snapshot(12), 1);
        let pids: Vec<u32> = payload.top_processes.iter().map(|p| p.pid).collect();
        assert_eq!(pids, (1..=10).collect::<Vec<u32>>());
    }

    #[test]
    fn hub_message_deserializes_auth_ok() {
        let msg: HubMessage = serde_json::from_str(r#"{"type":"auth_ok"}"#).unwrap();
        assert_eq!(msg.msg_type, "auth_ok");
        assert_eq!(msg.message, "");
    }

    #[test]
    fn only_a_close_an_error_or_the_end_means_the_hub_hung_up() {
        type Incoming = Option<Result<Message, ()>>;
        let cases: [(&str, Incoming, bool); 5] = [
            ("a close frame", Some(Ok(Message::Close(None))), true),
            ("a read error", Some(Err(())), true),
            ("the end of the stream", None, true),
            (
                "a text message",
                Some(Ok(Message::Text("hi".into()))),
                false,
            ),
            ("a ping", Some(Ok(Message::Ping(vec![]))), false),
        ];
        for (name, incoming, expected) in cases {
            assert_eq!(hub_hung_up(&incoming), expected, "case: {name}");
        }
    }

    #[test]
    fn a_push_tick_owes_each_fresh_snapshot_once() {
        use crate::snapshot::fixtures;
        let (publisher, snapshots) = fixtures::channel(fixtures::published());
        let mut cursor = SnapshotCursor::new(snapshots);
        let due = |cursor: &SnapshotCursor| match cursor.due(monotonic_now()) {
            Due::Send(snapshot) => format!("send {:?}", snapshot.seq),
            Due::Unchanged => "unchanged".into(),
            Due::Stale => "stale".into(),
        };
        assert_eq!(due(&cursor), "send SnapshotSeq(0)", "nothing sent yet");
        cursor.mark_sent(SnapshotSeq::FIRST);
        assert_eq!(due(&cursor), "unchanged", "sent already");
        // The next snapshot was read after the wall clock stepped back an hour.
        let mut next = fixtures::published_as(SnapshotSeq::FIRST.next());
        next.collected_at = fixtures::COLLECTED_AT - 3600;
        publisher.send_replace(Arc::new(next));
        assert_eq!(
            due(&cursor),
            "send SnapshotSeq(1)",
            "a new seq, whatever its time"
        );
        cursor.mark_sent(SnapshotSeq::FIRST.next());
        publisher.send_replace(Arc::new(fixtures::stale()));
        assert_eq!(due(&cursor), "stale", "a stale snapshot, sent or not");
    }

    #[test]
    fn hub_message_deserializes_auth_error_with_message() {
        let msg: HubMessage =
            serde_json::from_str(r#"{"type":"auth_error","message":"invalid token"}"#).unwrap();
        assert_eq!(msg.msg_type, "auth_error");
        assert_eq!(msg.message, "invalid token");
    }

    /// RFC 0016 §6: the accepted handshake's line prints the id in `Debug` form, so an id holding
    /// a control character is escaped and can't start a forged log line.
    #[test]
    fn the_authenticated_line_prints_the_id_escaped() {
        let line = authenticated_line("host\nforged: line");

        assert!(!line.contains('\n'), "no raw newline: {line:?}");
        assert!(
            line.contains(r#""host\nforged: line""#),
            "the id in Debug form: {line:?}"
        );
    }

    mod client {
        use super::super::*;
        use crate::alerts::AgentRun;
        use crate::applications::round::ScrapeRound;
        use crate::applications::wire::sample_round;
        use crate::snapshot::fixtures;
        use std::collections::BTreeMap;
        use std::sync::Arc;
        use tokio::net::{TcpListener, TcpStream};
        use tokio::sync::watch;
        use tokio_tungstenite::WebSocketStream;

        type AgentSocket = WebSocketStream<TcpStream>;

        /// Longer than one push interval (2 s), so the t=2 s snapshot arrives within it.
        const LONG_WATCH: Duration = Duration::from_millis(2_500);
        /// Shorter than one push interval, so only the t=0 snapshot tick fires within it.
        const SHORT_WATCH: Duration = Duration::from_millis(1_500);

        const RUN: &str = "00000000-0000-0000-0000-000000000009";

        fn round(seq: u64) -> Arc<ScrapeRound> {
            let run = uuid::Uuid::parse_str(RUN).unwrap();
            Arc::new(sample_round(AgentRun::new(run), seq))
        }

        type Report = (String, String, Option<String>, BTreeMap<String, f64>);
        type Frame = (String, String, u64, u64, Vec<Report>);

        /// The whole application frame `sample_round(RUN, seq)` must encode to.
        fn expected_frame(seq: u64) -> Frame {
            let orders = (
                "orders".to_string(),
                "up".to_string(),
                Some("2.4.1".to_string()),
                BTreeMap::from([
                    ("heap_used_bytes".to_string(), 300.0),
                    ("uptime_seconds".to_string(), 600.5),
                ]),
            );
            let billing = (
                "billing".to_string(),
                "unreachable".to_string(),
                None,
                BTreeMap::new(),
            );
            (
                "applications.v1".to_string(),
                RUN.to_string(),
                seq,
                20,
                vec![orders, billing],
            )
        }

        /// The frame, if `bytes` is an application frame; a snapshot frame isn't.
        fn application_frame(bytes: &[u8]) -> Option<Frame> {
            rmp_serde::from_slice(bytes).ok()
        }

        async fn listen() -> (String, TcpListener) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("ws://{}", listener.local_addr().unwrap());
            (url, listener)
        }

        /// Accepts the agent's connection and answers its auth message with `auth_ok`.
        async fn accept_agent(listener: &TcpListener) -> AgentSocket {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            match ws.next().await {
                Some(Ok(Message::Text(auth))) => assert!(auth.contains("\"auth\"")),
                other => panic!("expected the auth message, got {other:?}"),
            }
            ws.send(Message::Text(r#"{"type":"auth_ok"}"#.into()))
                .await
                .unwrap();
            ws
        }

        /// Every binary message the agent sends within `window`, in order.
        async fn binaries_within(ws: &mut AgentSocket, window: Duration) -> Vec<Vec<u8>> {
            let mut frames = Vec::new();
            let deadline = tokio::time::Instant::now() + window;
            while let Ok(Some(Ok(message))) = tokio::time::timeout_at(deadline, ws.next()).await {
                if let Message::Binary(frame) = message {
                    frames.push(frame);
                }
            }
            frames
        }

        /// The seqs of the application frames among `frames`, each checked whole.
        fn application_seqs(frames: &[Vec<u8>]) -> Vec<u64> {
            frames
                .iter()
                .filter_map(|bytes| application_frame(bytes))
                .map(|frame| {
                    assert_eq!(frame, expected_frame(frame.2), "the whole frame");
                    frame.2
                })
                .collect()
        }

        fn snapshots_in(frames: &[Vec<u8>]) -> usize {
            frames
                .iter()
                .filter(|bytes| application_frame(bytes).is_none())
                .count()
        }

        /// A snapshot channel whose collector publishes a new snapshot every 500 ms, so every
        /// push tick has one to send, until the returned task is aborted.
        fn collecting() -> (SnapshotReceiver, tokio::task::JoinHandle<()>) {
            let (publisher, snapshots) = watch::channel(Arc::new(fixtures::published()));
            let collector = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let seq = publisher.borrow().seq.next();
                    publisher.send_replace(Arc::new(fixtures::published_as(seq)));
                }
            });
            (snapshots, collector)
        }

        /// `push_session_on` with a collector publishing throughout.
        async fn push_session(
            rounds: &mut Option<RoundReceiver>,
            window: Duration,
            during: impl std::future::Future<Output = ()>,
        ) -> Vec<Vec<u8>> {
            let (snapshots, collector) = collecting();
            let frames = push_session_on(snapshots, rounds, window, during).await;
            collector.abort();
            frames
        }

        /// Runs the client against a hub that watches the connection for `window` after the
        /// handshake while `during` runs, then hangs up; returns what the hub received.
        /// The client runs as a task of its own on a multi-thread runtime, so a client that
        /// never yields fails the timeout instead of starving the test.
        async fn push_session_on(
            snapshots: SnapshotReceiver,
            rounds: &mut Option<RoundReceiver>,
            window: Duration,
            during: impl std::future::Future<Output = ()>,
        ) -> Vec<Vec<u8>> {
            let (url, listener) = listen().await;
            let mut feed = PushFeed {
                snapshots: SnapshotCursor::new(snapshots),
                rounds: rounds.take(),
            };
            let client = tokio::spawn(async move {
                let pushed = run_push_client(&url, "token", "test-system", 2, &mut feed).await;
                (pushed, feed.rounds)
            });
            let mut ws = accept_agent(&listener).await;
            let (frames, ()) = tokio::join!(binaries_within(&mut ws, window), during);
            drop(ws);
            let ended = tokio::time::timeout(Duration::from_secs(30), client).await;
            let Ok(Ok((pushed, owned))) = ended else {
                panic!("the client didn't end within 30 s of the hub hanging up");
            };
            assert!(pushed.is_ok(), "the client ends once the hub hangs up");
            *rounds = owned;
            frames
        }

        /// Runs the client on `snapshots`, with applications off, until aborted.
        fn spawn_client(url: String, snapshots: SnapshotReceiver) -> tokio::task::JoinHandle<()> {
            tokio::spawn(async move {
                let mut feed = PushFeed {
                    snapshots: SnapshotCursor::new(snapshots),
                    rounds: None,
                };
                let ended = run_push_client(&url, "token", "test-system", 2, &mut feed).await;
                assert!(ended.is_ok(), "the client ends cleanly: {ended:?}");
            })
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_stale_snapshot_closes_the_connection() {
            let (publisher, snapshots) = fixtures::channel(fixtures::published());
            let (url, listener) = listen().await;
            let client = spawn_client(url, snapshots);
            let mut ws = accept_agent(&listener).await;
            publisher.send_replace(Arc::new(fixtures::stale()));
            let closed = tokio::time::timeout(Duration::from_secs(3), async {
                while let Some(message) = ws.next().await {
                    if matches!(message, Ok(Message::Close(_)) | Err(_)) {
                        return;
                    }
                }
            })
            .await;
            assert!(closed.is_ok(), "the agent closes by the next push tick");
            let ended = tokio::time::timeout(Duration::from_secs(1), client).await;
            assert!(matches!(ended, Ok(Ok(()))), "and its push session ends");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_stale_agent_makes_no_handshake_until_a_fresh_snapshot() {
            let (publisher, snapshots) = fixtures::channel(fixtures::stale());
            let (url, listener) = listen().await;
            let client = spawn_client(url, snapshots);
            let early = tokio::time::timeout(Duration::from_millis(1_500), listener.accept());
            assert!(
                early.await.is_err(),
                "no connection while the snapshot is stale"
            );
            // Fresh on the monotonic clock, though read after the wall clock stepped back.
            let mut fresh = fixtures::published_as(SnapshotSeq::FIRST.next());
            fresh.collected_at = fixtures::COLLECTED_AT - 3600;
            publisher.send_replace(Arc::new(fresh));
            let connected =
                tokio::time::timeout(Duration::from_secs(2), accept_agent(&listener)).await;
            assert!(
                connected.is_ok(),
                "a handshake once a fresh snapshot is published"
            );
            client.abort();
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_snapshot_is_pushed_once_however_many_ticks_see_it() {
            let (publisher, snapshots) = watch::channel(Arc::new(fixtures::published()));
            let frames = push_session_on(snapshots, &mut None, LONG_WATCH, async {}).await;
            assert_eq!(snapshots_in(&frames), 1, "the t=2 s tick has nothing new");
            drop(publisher);
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn the_current_round_is_sent_once_right_after_the_handshake() {
            let fresh = watch::channel(Some(round(5)));
            // Published after the channel was made, so the receiver hasn't seen it yet.
            let published = watch::channel(None);
            published.0.send_replace(Some(round(5)));
            for (case, (sender, receiver)) in [("initial", fresh), ("published", published)] {
                let mut rounds = Some(receiver);
                let frames = push_session(&mut rounds, LONG_WATCH, async {}).await;
                assert_eq!(
                    frames.first().and_then(|bytes| application_frame(bytes)),
                    Some(expected_frame(5)),
                    "{case}: the first message after the handshake is the current round"
                );
                assert_eq!(application_seqs(&frames), [5], "{case}: sent once");
                assert!(snapshots_in(&frames) > 0, "{case}: snapshots still flow");
                assert!(rounds.is_some(), "{case}: an open channel is kept");
                drop(sender);
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_round_published_while_connected_is_sent_at_once() {
            let (sender, receiver) = watch::channel(None);
            let mut rounds = Some(receiver);
            let publish = async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                sender.send_replace(Some(round(8)));
            };
            let frames = push_session(&mut rounds, SHORT_WATCH, publish).await;
            assert_eq!(application_seqs(&frames), [8], "sent before the next tick");
            assert!(
                rounds.is_some(),
                "an open channel is kept for the next connection"
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_channel_closed_before_connecting_sends_nothing_and_is_given_up() {
            // The scrape loop ended: its sender is gone, though the channel holds a round.
            let (sender, receiver) = watch::channel(Some(round(3)));
            drop(sender);
            let mut rounds = Some(receiver);
            let frames = push_session(&mut rounds, LONG_WATCH, async {}).await;
            assert_eq!(application_seqs(&frames), [] as [u64; 0], "no stale round");
            assert!(snapshots_in(&frames) > 0, "snapshots still flow");
            assert!(
                rounds.is_none(),
                "the closed channel is given up, so no later connection looks at it again"
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_channel_closed_mid_session_is_given_up_and_snapshots_go_on() {
            let empty = watch::channel(None);
            let holding = watch::channel(Some(round(5)));
            let cases = [
                ("empty", empty, vec![]),
                ("holding a round", holding, vec![5]),
            ];
            for (case, (sender, receiver), expected) in cases {
                let mut rounds = Some(receiver);
                let close = async {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    drop(sender);
                };
                let frames = push_session(&mut rounds, LONG_WATCH, close).await;
                assert_eq!(
                    application_seqs(&frames),
                    expected,
                    "{case}: only the handshake's round, never re-sent on close"
                );
                assert_eq!(
                    snapshots_in(&frames),
                    2,
                    "{case}: the t=0 and t=2 s snapshots"
                );
                assert!(rounds.is_none(), "{case}: the closed channel is given up");
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn without_applications_only_snapshots_are_sent() {
            let mut rounds = None;
            let frames = push_session(&mut rounds, LONG_WATCH, async {}).await;
            assert!(snapshots_in(&frames) > 0, "snapshots flow");
            assert_eq!(application_seqs(&frames), [] as [u64; 0]);
        }
    }
}
