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

use crate::state::AppState;

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

        // Include live metrics
        let live = s.live_metrics.read().unwrap().clone();

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
