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

use axum::response::sse::{Event, KeepAlive};
use axum::{Router, extract::State, response::Sse, routing::get};
use futures_core::Stream;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::IntervalStream;

use crate::collectors::monotonic_now;
use crate::snapshot::{PublishedSnapshot, StreamEmit, StreamState};
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/stream/system", get(system_stream))
        .route("/api/stream/processes", get(process_stream))
        .route("/api/stream/alerts", get(alerts_stream))
        .with_state(state)
}

async fn system_stream(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    sse(snapshot_events(state, Duration::from_secs(2), system_event))
}

async fn process_stream(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    sse(snapshot_events(
        state,
        Duration::from_secs(3),
        process_event,
    ))
}

/// A stream of the published snapshot every `period`, as `to_event` renders it. A stale
/// snapshot sends one `stale` event and then nothing until a fresh one: the stream stays
/// open, since a browser's `EventSource` gives up on an error answer.
fn snapshot_events(
    state: Arc<AppState>,
    period: Duration,
    to_event: fn(&PublishedSnapshot) -> Event,
) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
    let mut told = StreamState::Live;
    IntervalStream::new(interval(period)).filter_map(move |_| {
        match told.next(state.snapshot(), monotonic_now()) {
            StreamEmit::Snapshot(snapshot) => Some(Ok(to_event(&snapshot))),
            StreamEmit::StaleNotice => Some(Ok(Event::default()
                .data(r#"{"type":"stale"}"#)
                .event("stale"))),
            StreamEmit::Nothing => None,
        }
    })
}

fn system_event(published: &PublishedSnapshot) -> Event {
    let snap = &published.system;
    let payload = serde_json::json!({
        "type": "system",
        "timestamp": snap.uptime_seconds,
        "collected_at": published.collected_at,
        "cpu_percent": snap.cpu.usage_percent,
        "cpu_logical_cores": snap.cpu.logical_cores,
        "cpu_capacity_cpus": snap.cpu.capacity_cpus,
        "cpu_steal_percent": snap.cpu.steal_percent,
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
    });
    Event::default()
        .data(serde_json::to_string(&payload).unwrap_or_default())
        .event("system")
}

fn process_event(published: &PublishedSnapshot) -> Event {
    let processes = &published.system.top_processes;
    let payload = serde_json::json!({
        "type": "processes",
        "processes": &processes[..10.min(processes.len())],
    });
    Event::default()
        .data(serde_json::to_string(&payload).unwrap_or_default())
        .event("processes")
}

fn sse<S>(stream: S) -> Sse<S>
where
    S: Stream<Item = Result<Event, Infallible>> + Send + 'static,
{
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(10))
            .text("keep-alive"),
    )
}

async fn alerts_stream(
    State(s): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let tick = interval(Duration::from_secs(3));
    let stream = IntervalStream::new(tick).map(move |_| {
        let mgr = s.alert_manager.read();
        let payload = serde_json::json!({
            "type": "alerts",
            "active": mgr.active_alerts(),
        });
        Ok(Event::default()
            .data(serde_json::to_string(&payload).unwrap_or_default())
            .event("alerts"))
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(10))
            .text("keep-alive"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn all_stream_routes_respond_with_event_stream_content_type() {
        let state = AppState::new(crate::snapshot::fixtures::receiver());
        for path in [
            "/api/stream/system",
            "/api/stream/processes",
            "/api/stream/alerts",
        ] {
            let res = router(state.clone())
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), axum::http::StatusCode::OK, "path {path}");
            let content_type = res
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            assert_eq!(content_type, "text/event-stream", "path {path}");
        }
    }

    type Events = axum::body::BodyDataStream;

    /// The event stream at `path`, served from `state`.
    async fn open(state: Arc<AppState>, path: &str) -> Events {
        let res = router(state)
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        res.into_body().into_data_stream()
    }

    /// The next event's name and JSON, skipping keep-alives; `None` if none comes in `wait`.
    async fn next_event(
        events: &mut Events,
        wait: Duration,
    ) -> Option<(String, serde_json::Value)> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let chunk = tokio::time::timeout_at(deadline, events.next())
                .await
                .ok()??;
            let text = String::from_utf8(chunk.unwrap().to_vec()).unwrap();
            let field = |name: &str| {
                text.lines()
                    .find_map(|line| line.strip_prefix(name))
                    .map(str::to_string)
            };
            if let (Some(event), Some(data)) = (field("event: "), field("data: ")) {
                return Some((event, serde_json::from_str(&data).unwrap()));
            }
        }
    }

    /// The first event's JSON on `path`, at once: the streams tick immediately.
    async fn first_event(path: &str) -> serde_json::Value {
        first_event_of(crate::snapshot::fixtures::published(), path).await
    }

    async fn first_event_of(snapshot: PublishedSnapshot, path: &str) -> serde_json::Value {
        let (_publisher, snapshots) = crate::snapshot::fixtures::channel(snapshot);
        let state = AppState::new(snapshots);
        let mut events = open(state, path).await;
        next_event(&mut events, Duration::from_secs(5))
            .await
            .expect("an event at once")
            .1
    }

    #[tokio::test(start_paused = true)]
    async fn a_stale_snapshot_is_told_once_and_the_stream_waits_for_a_fresh_one() {
        use crate::snapshot::fixtures;
        let paths = [
            ("/api/stream/system", "system"),
            ("/api/stream/processes", "processes"),
        ];
        for (path, live) in paths {
            let (publisher, snapshots) = fixtures::channel(fixtures::stale());
            let mut events = open(AppState::new(snapshots), path).await;
            let first = next_event(&mut events, Duration::from_secs(1)).await;
            assert_eq!(
                first,
                Some(("stale".into(), serde_json::json!({ "type": "stale" }))),
                "{path}: told at once"
            );
            let quiet = next_event(&mut events, Duration::from_secs(9)).await;
            assert_eq!(
                quiet, None,
                "{path}: nothing more while stale, several ticks"
            );
            publisher.send_replace(Arc::new(fixtures::published()));
            let resumed = next_event(&mut events, Duration::from_secs(4)).await;
            assert_eq!(
                resumed.map(|(event, _)| event),
                Some(live.to_string()),
                "{path}: the same stream resumes with a fresh snapshot"
            );
        }
    }

    #[tokio::test]
    async fn the_system_stream_sends_steal_as_measured_or_null() {
        // (name, steal, expected)
        let cases = [
            ("measured", Some(7.5), serde_json::json!(7.5)),
            ("a real zero", Some(0.0), serde_json::json!(0.0)),
            ("unmeasured", None, serde_json::Value::Null),
        ];
        for (name, steal, expected) in cases {
            let mut snapshot = crate::snapshot::fixtures::published();
            snapshot.system.cpu.steal_percent = steal;
            let system = first_event_of(snapshot, "/api/stream/system").await;
            assert_eq!(system.get("cpu_steal_percent"), Some(&expected), "{name}");
        }
    }

    #[tokio::test]
    async fn streams_send_the_published_snapshot() {
        let fixture = crate::snapshot::fixtures::published();
        let system = first_event("/api/stream/system").await;
        assert_eq!(system["collected_at"], fixture.collected_at);
        assert_eq!(system["cpu_percent"], fixture.system.cpu.usage_percent);
        assert_eq!(
            system["cpu_capacity_cpus"],
            fixture.system.cpu.capacity_cpus
        );
        assert_eq!(
            system["cpu_steal_percent"],
            serde_json::json!(fixture.system.cpu.steal_percent)
        );
        assert_eq!(
            system["memory_used_bytes"],
            fixture.system.memory.used_bytes
        );
        let processes = first_event("/api/stream/processes").await;
        assert_eq!(
            processes["processes"],
            serde_json::to_value(&fixture.system.top_processes).unwrap()
        );
    }

    #[tokio::test]
    async fn unknown_stream_path_is_not_found() {
        let state = AppState::new(crate::snapshot::fixtures::receiver());
        let res = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/stream/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::NOT_FOUND);
    }
}
