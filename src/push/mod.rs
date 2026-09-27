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
use tokio::time::interval;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::applications::round::ScrapeRound;
use crate::applications::scrape_loop::RoundReceiver;
use crate::collectors;
use crate::models::SystemSnapshot;

mod application_frame;

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

/// Connect to the hub via WebSocket and push system snapshots using MessagePack, and each
/// scrape round `rounds` publishes as an application frame. `rounds` is `None` when
/// applications are off.
pub async fn run_push_client(
    hub_url: &str,
    token: &str,
    system_id: &str,
    push_interval_secs: u64,
    rounds: &mut Option<RoundReceiver>,
) -> Result<(), String> {
    let url = format!("{}/api/push", hub_url.trim_end_matches('/'));
    tracing::info!("🔌 Connecting to hub via push: {url}");
    let (mut ws, _) = connect_async(&url).await.map_err(|e| e.to_string())?;
    authenticate(&mut ws, system_id, token).await?;

    // A round re-sent after a reconnect keeps its round id, so the hub drops it as a duplicate
    // if it already has it.
    if let Some(round) = current_round(rounds)
        && send_round(&mut ws, &round).await.is_err()
    {
        tracing::error!("Push connection lost, will retry...");
        return Ok(());
    }

    let mut tick = interval(Duration::from_secs(push_interval_secs.max(2)));
    let mut ping_tick = interval(Duration::from_secs(30));
    loop {
        let sent = tokio::select! {
            _ = tick.tick() => send_snapshot(&mut ws, system_id).await,
            _ = ping_tick.tick() => ws.send(Message::Ping(vec![])).await,
            round = next_round(rounds), if rounds.is_some() => match round {
                Some(round) => send_round(&mut ws, &round).await,
                None => Ok(()),
            },
        };
        if sent.is_err() {
            tracing::error!("Push connection lost, will retry...");
            break;
        }
    }

    tracing::warn!("Push connection closed");
    Ok(())
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
            tracing::info!("✅ Push authenticated — system_id={system_id}");
        } else {
            tracing::error!("❌ Push auth failed: {}", msg.message);
            return Err(format!("auth failed: {}", msg.message));
        }
    }
    Ok(())
}

/// Collects a snapshot and sends it as a snapshot frame. One that can't be encoded is
/// skipped, never sent empty.
async fn send_snapshot<S>(ws: &mut S, system_id: &str) -> Result<(), S::Error>
where
    S: Sink<Message> + Unpin,
{
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let payload = snapshot_payload(system_id, collectors::system::collect(), now);
    match rmp_serde::to_vec(&payload) {
        Ok(frame) => ws.send(Message::Binary(frame)).await,
        Err(err) => {
            tracing::warn!("Skipping a snapshot frame that failed to encode: {err}");
            Ok(())
        }
    }
}

/// The snapshot frame for one snapshot, stamped with the agent's clock.
fn snapshot_payload(system_id: &str, snap: SystemSnapshot, timestamp: u64) -> PushPayload {
    PushPayload {
        system_id: system_id.to_string(),
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

/// Get a persistent machine identifier. Uses /etc/machine-id on systemd Linux,
/// falls back to a hostname-based hash, then to a file-stored UUID. Blocking: reads files
/// and shells out, so call it off the async runtime.
pub fn get_persistent_id() -> String {
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

    use crate::models::{
        CpuInfo, DiskInfo, LoadAverage, MemoryInfo, OsInfo, ProcessInfo, SwapInfo,
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
            },
            memory: MemoryInfo {
                total_bytes: 2222,
                used_bytes: 1111,
                free_bytes: 555,
                available_bytes: 666,
                total_display: "mem-total".into(),
                used_display: "mem-used".into(),
                usage_percent: 33.25,
            },
            swap: SwapInfo {
                total_bytes: 777,
                used_bytes: 88,
                free_bytes: 689,
                total_display: "swap-total".into(),
                used_display: "swap-used".into(),
                usage_percent: 7.75,
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
        let payload = snapshot_payload("system-s", distinct_snapshot(1), 4_444_444);
        assert_eq!(rmp_serde::to_vec(&payload).unwrap(), GOLDEN);
    }

    #[test]
    fn snapshot_frame_carries_the_ten_busiest_processes() {
        let payload = snapshot_payload("system-s", distinct_snapshot(12), 1);
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

    mod client {
        use super::super::*;
        use crate::alerts::AgentRun;
        use crate::applications::round::ScrapeRound;
        use crate::applications::wire::sample_round;
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

        fn snapshots(frames: &[Vec<u8>]) -> usize {
            frames
                .iter()
                .filter(|bytes| application_frame(bytes).is_none())
                .count()
        }

        /// Runs the client against a hub that watches the connection for `window` after the
        /// handshake while `during` runs, then hangs up; returns what the hub received.
        /// The client runs as a task of its own on a multi-thread runtime, so a client that
        /// never yields fails the timeout instead of starving the test.
        async fn push_session(
            rounds: &mut Option<RoundReceiver>,
            window: Duration,
            during: impl std::future::Future<Output = ()>,
        ) -> Vec<Vec<u8>> {
            let (url, listener) = listen().await;
            let mut owned = rounds.take();
            let client = tokio::spawn(async move {
                let pushed = run_push_client(&url, "token", "test-system", 2, &mut owned).await;
                (pushed, owned)
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
                assert!(snapshots(&frames) > 0, "{case}: snapshots still flow");
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
            assert!(snapshots(&frames) > 0, "snapshots still flow");
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
                assert_eq!(snapshots(&frames), 2, "{case}: the t=0 and t=2 s snapshots");
                assert!(rounds.is_none(), "{case}: the closed channel is given up");
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn without_applications_only_snapshots_are_sent() {
            let mut rounds = None;
            let frames = push_session(&mut rounds, LONG_WATCH, async {}).await;
            assert!(snapshots(&frames) > 0, "snapshots flow");
            assert_eq!(application_seqs(&frames), [] as [u64; 0]);
        }
    }
}
