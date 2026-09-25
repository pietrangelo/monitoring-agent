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

use crate::collectors;
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/stream/system", get(system_stream))
        .route("/api/stream/processes", get(process_stream))
        .route("/api/stream/alerts", get(alerts_stream))
        .with_state(state)
}

async fn system_stream() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let tick = interval(Duration::from_secs(2));
    let stream = IntervalStream::new(tick).map(move |_| {
        let snap = collectors::system::collect();
        let payload = serde_json::json!({
            "type": "system",
            "timestamp": snap.uptime_seconds,
            "cpu_percent": snap.cpu.usage_percent,
            "cpu_logical_cores": snap.cpu.logical_cores,
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
        Ok(Event::default()
            .data(serde_json::to_string(&payload).unwrap_or_default())
            .event("system"))
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(10))
            .text("keep-alive"),
    )
}

async fn process_stream() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let tick = interval(Duration::from_secs(3));
    let stream = IntervalStream::new(tick).map(move |_| {
        let snap = collectors::system::collect();
        let payload = serde_json::json!({
            "type": "processes",
            "processes": &snap.top_processes[..10.min(snap.top_processes.len())],
        });
        Ok(Event::default()
            .data(serde_json::to_string(&payload).unwrap_or_default())
            .event("processes"))
    });

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
        let state = AppState::new();
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

    #[tokio::test]
    async fn unknown_stream_path_is_not_found() {
        let state = AppState::new();
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
