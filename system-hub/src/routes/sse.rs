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
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;
use tokio::task::JoinError;
use tokio::time::{Instant, MissedTickBehavior, interval_at};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::WatchStream;

use crate::db::Database;
use crate::models::{SystemInfo, SystemStatus};
use crate::snapshot::LiveMetrics;
use crate::snapshot::Scalar;
use crate::state::AppState;

/// The summary event's JSON, serialised once per tick and shared by every subscriber
/// (RFC 0007 §5).
#[derive(Clone, Debug)]
pub struct Summary(Arc<str>);

impl Summary {
    /// Builds the summary from the registry, the active alerts and a copy of the live metrics'
    /// pointers. A read that fails shows no systems, or no active alerts, as the summary always
    /// has (RFC 0007, Open architectural questions); the poller warns when the registry can't
    /// be read.
    pub fn build(
        db: &Database,
        live_metrics: &RwLock<HashMap<String, Arc<LiveMetrics>>>,
    ) -> Result<Self, serde_json::Error> {
        let systems = db.list_systems().unwrap_or_default();
        let active_alerts = db.count_active_alerts().unwrap_or(0);
        // A copy of the entries' pointers, converted once the read lock is released.
        let live = live_metrics
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let document = SummaryDto::new(&systems, active_alerts, &live);
        // Through a `Value`, which orders the keys and widens each `f32` as the summary's
        // bytes always have.
        let value = serde_json::to_value(&document)?;
        Ok(Self(Arc::from(value.to_string())))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a summary couldn't be built.
#[derive(Debug)]
pub enum SummaryFailure {
    /// Serialising it failed.
    Serialise(serde_json::Error),
    /// The blocking task building it panicked.
    Task(JoinError),
}

impl std::fmt::Display for SummaryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Serialise(err) => write!(f, "Serialising the summary failed: {err}"),
            Self::Task(err) => write!(f, "Building the summary failed: {err}"),
        }
    }
}

/// The summary event's JSON document.
#[derive(Debug, serde::Serialize)]
struct SummaryDto<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    total_systems: usize,
    online_count: usize,
    offline_count: usize,
    active_alerts: usize,
    systems: &'a [SystemInfo],
    live_metrics: HashMap<&'a str, LiveMetricsDto<'a>>,
}

impl<'a> SummaryDto<'a> {
    fn new(
        systems: &'a [SystemInfo],
        active_alerts: usize,
        live: &'a HashMap<String, Arc<LiveMetrics>>,
    ) -> Self {
        let count = |status: SystemStatus| systems.iter().filter(|s| s.status == status).count();
        Self {
            kind: "summary",
            total_systems: systems.len(),
            online_count: count(SystemStatus::Online),
            offline_count: count(SystemStatus::Offline),
            active_alerts,
            systems,
            live_metrics: live
                .iter()
                .map(|(id, metrics)| (id.as_str(), LiveMetricsDto::from(&**metrics)))
                .collect(),
        }
    }
}

/// A system's live metrics as the summary event shows them: a value the snapshot rule left out
/// is `null`, as a non-finite one always serialised.
#[derive(Debug, serde::Serialize)]
struct LiveMetricsDto<'a> {
    cpu_percent: Option<f32>,
    memory_percent: Option<f32>,
    load_one: Option<f64>,
    disks: Vec<(&'a str, f32)>,
    updated_at: i64,
}

/// A kept load as the `f64` its shortest decimal names. The agent sends loads as `f64`, and the
/// summary showed them as sent (0.52); the `f32` the snapshot keeps would widen to
/// 0.5199999809265137. The percentages were `f32` on the wire already, and keep their bytes.
fn shortest_decimal(value: f32) -> f64 {
    value.to_string().parse().unwrap_or(f64::from(value))
}

impl<'a> From<&'a LiveMetrics> for LiveMetricsDto<'a> {
    fn from(live: &'a LiveMetrics) -> Self {
        Self {
            cpu_percent: live.snapshot().scalar(Scalar::Cpu),
            memory_percent: live.snapshot().scalar(Scalar::Memory),
            load_one: live.snapshot().scalar(Scalar::Load1).map(shortest_decimal),
            disks: live.snapshot().disks().collect(),
            updated_at: live.time().seconds(),
        }
    }
}

/// Starts the one summary publisher (RFC 0007 §5), beside the collectors.
pub fn start_publisher(app: Arc<AppState>) {
    tokio::spawn(publish_every(app, Duration::from_secs(5)));
}

/// Replaces the published summary every `period`, from one `period` on: `AppState::new` built
/// the first.
async fn publish_every(app: Arc<AppState>, period: Duration) {
    let mut tick = interval_at(Instant::now() + period, period);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        publish(&app).await;
    }
}

/// Builds a summary on the blocking pool and stores it whether or not anyone subscribes:
/// `send_replace`, since `send` stores nothing while no receiver exists. A build that fails
/// keeps the previous summary.
async fn publish(app: &Arc<AppState>) {
    match build_on_blocking_pool(Arc::clone(app)).await {
        Ok(summary) => drop(app.summary.send_replace(summary)),
        Err(failure) => tracing::error!("{failure}; keeping the previous summary"),
    }
}

/// Builds a summary where its database reads can block.
async fn build_on_blocking_pool(app: Arc<AppState>) -> Result<Summary, SummaryFailure> {
    tokio::task::spawn_blocking(move || Summary::build(&app.db, &app.live_metrics))
        .await
        .map_err(SummaryFailure::Task)?
        .map_err(SummaryFailure::Serialise)
}

/// The published summaries, the current one first; a slow subscriber skips to the latest.
fn summaries(app: &AppState) -> WatchStream<Summary> {
    WatchStream::new(app.summary.subscribe())
}

/// One summary as its event: `data: <json>\nevent: summary\n\n`.
fn summary_event(summary: &Summary) -> Event {
    Event::default().data(summary.as_str()).event("summary")
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/stream/summary", get(summary_stream))
        .with_state(state)
}

async fn summary_stream(
    State(s): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let events = summaries(&s).map(|summary| Ok(summary_event(&summary)));
    Sse::new(events).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{ReportedDisk, ReportedSnapshot, SnapshotTime, snapshot_rule};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn temp_state() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Arc::new(Database::new(path.to_str().unwrap()).unwrap());
        (AppState::new(db).unwrap(), dir)
    }

    #[tokio::test]
    async fn summary_stream_responds_with_event_stream_content_type() {
        let (state, _dir) = temp_state();
        let res = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/stream/summary")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        let content_type = res
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert_eq!(content_type, "text/event-stream");
    }

    /// A new subscriber's first event, up to its blank line, if it comes within `wait`.
    async fn first_event_within(state: Arc<AppState>, wait: Duration) -> Option<String> {
        let res = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/stream/summary")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let mut body = res.into_body().into_data_stream();
        let mut event = String::new();
        let read = async {
            while !event.contains("\n\n") {
                let chunk = futures_util::StreamExt::next(&mut body).await;
                let chunk = chunk.expect("the stream goes on").unwrap();
                event.push_str(std::str::from_utf8(&chunk).unwrap());
            }
        };
        tokio::time::timeout(wait, read).await.ok().map(|()| event)
    }

    /// A new subscriber's first event, which comes within 5 s.
    async fn first_event(state: Arc<AppState>) -> String {
        let event = first_event_within(state, Duration::from_secs(5)).await;
        event.expect("an event within 5 s")
    }

    /// The JSON document a summary event carries.
    fn document(event: &str) -> serde_json::Value {
        let json = event
            .strip_prefix("data: ")
            .and_then(|rest| rest.strip_suffix("\nevent: summary\n\n"))
            .unwrap_or_else(|| panic!("a summary event: {event:?}"));
        serde_json::from_str(json).unwrap()
    }

    /// The ids of the systems a summary event lists.
    fn listed(event: &str) -> Vec<String> {
        let document = document(event);
        let systems = document["systems"].as_array().cloned().unwrap_or_default();
        systems
            .iter()
            .filter_map(|system| system["id"].as_str().map(str::to_owned))
            .collect()
    }

    fn system(id: &str) -> crate::models::SystemInfo {
        crate::models::SystemInfo {
            id: id.into(),
            name: "web-01".into(),
            url: "push://".into(),
            token: "secret".into(),
            status: crate::models::SystemStatus::Online,
            last_seen: "1m".into(),
            last_error: None,
            os: Some("Ubuntu".into()),
            hostname: Some("web-01".into()),
            kernel: None,
            cpu_model: None,
            cpu_cores: Some(4),
            total_memory_display: Some("4 GB".into()),
            total_memory_bytes: Some(4_000),
            poll_interval_secs: 10,
            enabled: true,
        }
    }

    /// Characterisation (RFC 0007, Testing plan): the summary event's bytes for a fixed state,
    /// one system with live metrics. RFC 0007 keeps them as they are, and every subscriber
    /// gets the same ones (§5).
    #[tokio::test]
    async fn the_summary_event_for_a_fixed_state_keeps_its_bytes() {
        let (state, _dir) = temp_state();
        state.db.insert_system(&system("sys-1")).unwrap();
        plant_live_metrics(&state);
        publish(&state).await;

        let events = [
            first_event(Arc::clone(&state)).await,
            first_event(state).await,
        ];

        let expected_json = concat!(
            r#"{"active_alerts":0,"live_metrics":{"sys-1":{"cpu_percent":11.5,"#,
            r#""disks":[["/",50.25],["/home",70.0]],"load_one":0.52,"memory_percent":22.0,"#,
            r#""updated_at":1700000000}},"offline_count":0,"online_count":1,"#,
            r#""systems":[{"cpu_cores":4,"cpu_model":null,"enabled":true,"hostname":"web-01","#,
            r#""id":"sys-1","kernel":null,"last_error":null,"last_seen":"1m","name":"web-01","#,
            r#""os":"Ubuntu","poll_interval_secs":10,"status":"online","#,
            r#""total_memory_bytes":4000,"total_memory_display":"4 GB","url":"push://"}],"#,
            r#""total_systems":1,"type":"summary"}"#,
        );
        let expected = format!("data: {expected_json}\nevent: summary\n\n");
        assert_eq!(events, [expected.clone(), expected]);
    }

    /// RFC 0007 §5: a new subscriber gets the summary at once, the one `AppState::new` built
    /// from the database, with no tick of the publisher's due.
    #[tokio::test]
    async fn a_new_subscribers_first_summary_arrives_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(dir.path().join("test.db").to_str().unwrap()).unwrap();
        db.insert_system(&system("sys-1")).unwrap();
        let state = AppState::new(Arc::new(db)).unwrap();
        tokio::spawn(publish_every(Arc::clone(&state), Duration::from_secs(60)));

        let event = first_event_within(state, Duration::from_secs(1)).await;

        assert_eq!(
            event.as_deref().map(listed),
            Some(vec!["sys-1".to_owned()]),
            "the startup summary, within 1 s"
        );
    }

    /// RFC 0007 §5: a subscriber gets the published summary, not one built for it.
    #[tokio::test]
    async fn a_new_subscriber_gets_the_published_summary_not_a_fresh_one() {
        let (state, _dir) = temp_state();
        state.db.insert_system(&system("sys-1")).unwrap();
        publish(&state).await;
        state.db.insert_system(&system("sys-late")).unwrap();

        let event = first_event(state).await;

        assert_eq!(
            listed(&event),
            ["sys-1"],
            "the system inserted after the publish is absent"
        );
    }

    /// RFC 0007 §5: the publisher stores a summary with no one subscribed (`send_replace`), so
    /// the next subscriber doesn't get an older one.
    #[tokio::test]
    async fn a_summary_published_with_no_subscriber_is_kept() {
        let (state, _dir) = temp_state();
        state.db.insert_system(&system("sys-1")).unwrap();
        publish(&state).await;

        let event = first_event(state).await;

        assert_eq!(listed(&event), ["sys-1"]);
    }

    /// The next summary a subscriber gets, if one comes within a second.
    async fn next_summary(subscriber: &mut WatchStream<Summary>) -> Option<Summary> {
        let next = tokio::time::timeout(Duration::from_secs(1), subscriber.next()).await;
        next.ok().flatten()
    }

    /// RFC 0007 §5: each tick is serialised once, whatever the number of subscribers. Every
    /// serialisation is its own allocation, so three subscribers over two ticks hold two.
    #[tokio::test]
    async fn every_subscriber_shares_each_ticks_one_serialisation() {
        let (state, _dir) = temp_state();
        let mut subscribers: Vec<_> = (0..3).map(|_| summaries(&state)).collect();
        for subscriber in &mut subscribers {
            assert!(
                next_summary(subscriber).await.is_some(),
                "the startup summary"
            );
        }

        let mut received = Vec::new();
        for _ in 0..2 {
            publish(&state).await;
            for subscriber in &mut subscribers {
                received.extend(next_summary(subscriber).await);
            }
        }

        let serialisations: std::collections::HashSet<*const u8> = received
            .iter()
            .map(|summary| Arc::as_ptr(&summary.0).cast::<u8>())
            .collect();
        assert_eq!(received.len(), 6, "every subscriber gets both ticks");
        assert_eq!(serialisations.len(), 2, "two serialisations, not six");
    }

    /// The live metrics `the_summary_event_for_a_fixed_state_keeps_its_bytes` shows.
    fn plant_live_metrics(state: &AppState) {
        plant(state, reported(Some(11.5)));
    }

    fn reported(cpu: Option<f32>) -> ReportedSnapshot {
        let disk = |mount: &str, usage| ReportedDisk {
            mount_point: Some(mount.to_string()),
            usage_percent: Some(usage),
        };
        ReportedSnapshot {
            cpu,
            memory: Some(22.0),
            swap: Some(33.0),
            // A load as an agent reads it from /proc/loadavg: two decimals, not exact in f32.
            load1: Some(0.52),
            load5: Some(0.25),
            disks: vec![disk("/", 50.25), disk("/home", 70.0)],
        }
    }

    fn plant(state: &AppState, reported: ReportedSnapshot) {
        let (snapshot, left_out) = snapshot_rule(reported);
        let time = SnapshotTime::try_from(1_700_000_000).unwrap();
        let now = std::time::Instant::now();
        let (live, _) = LiveMetrics::following(None, snapshot, time, &left_out, now);
        let mut entries = state.live_metrics.write().unwrap();
        entries.insert("sys-1".into(), Arc::new(live));
    }

    /// RFC 0007 §4: the dashboard's live values are the snapshot's kept values, and one left
    /// out is `null` on the wire.
    #[tokio::test]
    async fn a_live_value_the_snapshot_rule_left_out_is_null_in_the_summary() {
        let (state, _dir) = temp_state();
        plant(&state, reported(None));
        publish(&state).await;

        let event = first_event(state).await;

        assert_eq!(
            document(&event)["live_metrics"]["sys-1"],
            serde_json::json!({
                "cpu_percent": null,
                "memory_percent": 22.0,
                "load_one": 0.52,
                "disks": [["/", 50.25], ["/home", 70.0]],
                "updated_at": 1_700_000_000,
            })
        );
    }

    #[test]
    fn a_live_load_shows_as_the_decimal_the_agent_sent() {
        let cases = [
            ("two decimals", 0.52_f32, 0.52_f64),
            ("another", 1.07, 1.07),
            ("exact in f32", 0.5, 0.5),
            ("zero", 0.0, 0.0),
            ("a busy host", 123.45, 123.45),
            ("the largest f32", f32::MAX, 3.402_823_5e38),
        ];
        for (case, kept, shown) in cases {
            assert_eq!(shortest_decimal(kept), shown, "case: {case}");
        }
    }

    #[tokio::test]
    async fn unknown_stream_path_is_not_found() {
        let (state, _dir) = temp_state();
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
