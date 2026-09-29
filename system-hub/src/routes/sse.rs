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

use crate::snapshot::LiveMetrics;
use crate::snapshot::Scalar;
use crate::state::AppState;

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

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/stream/summary", get(summary_stream))
        .with_state(state)
}

async fn summary_stream(
    State(s): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let tick = interval(Duration::from_secs(5));
    let stream = IntervalStream::new(tick).map(move |_| {
        let systems = s.db.list_systems().unwrap_or_default();
        let online = systems
            .iter()
            .filter(|sys| sys.status == crate::models::SystemStatus::Online)
            .count();
        let offline = systems
            .iter()
            .filter(|sys| sys.status == crate::models::SystemStatus::Offline)
            .count();
        let active_alerts = s.db.count_active_alerts().unwrap_or(0);

        // A copy of the entries' pointers, converted once the read lock is released.
        let live = s.live_metrics.read().unwrap().clone();
        let live: std::collections::HashMap<&str, LiveMetricsDto> = live
            .iter()
            .map(|(id, metrics)| (id.as_str(), LiveMetricsDto::from(&**metrics)))
            .collect();

        let payload = serde_json::json!({
            "type": "summary",
            "total_systems": systems.len(),
            "online_count": online,
            "offline_count": offline,
            "active_alerts": active_alerts,
            "systems": systems,
            "live_metrics": live,
        });
        Ok(Event::default()
            .data(serde_json::to_string(&payload).unwrap_or_default())
            .event("summary"))
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::snapshot::{ReportedDisk, ReportedSnapshot, SnapshotTime, snapshot_rule};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn temp_state() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Arc::new(Database::new(path.to_str().unwrap()).unwrap());
        (AppState::new(db), dir)
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

    /// Reads the stream's first event, up to its blank line.
    async fn first_event(state: Arc<AppState>) -> String {
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
        while !event.contains("\n\n") {
            let chunk = tokio::time::timeout(
                Duration::from_secs(5),
                futures_util::StreamExt::next(&mut body),
            )
            .await
            .expect("an event within 5 s")
            .expect("the stream goes on")
            .unwrap();
            event.push_str(std::str::from_utf8(&chunk).unwrap());
        }
        event
    }

    /// Characterisation (RFC 0007, Testing plan): the summary event's bytes for a fixed state,
    /// one system with live metrics. RFC 0007 keeps them as they are.
    #[tokio::test]
    async fn the_summary_event_for_a_fixed_state_keeps_its_bytes() {
        let (state, _dir) = temp_state();
        state
            .db
            .insert_system(&crate::models::SystemInfo {
                id: "sys-1".into(),
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
            })
            .unwrap();
        plant_live_metrics(&state);

        let event = first_event(state).await;

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
        assert_eq!(event, format!("data: {expected_json}\nevent: summary\n\n"));
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

        let event = first_event(state).await;

        let json = event
            .strip_prefix("data: ")
            .and_then(|rest| rest.strip_suffix("\nevent: summary\n\n"))
            .unwrap();
        let summary: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(
            summary["live_metrics"]["sys-1"],
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
