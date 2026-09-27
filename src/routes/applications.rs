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

//! `GET /api/applications` (RFC 0009 §6): the latest scrape round, as the poll contract's JSON.

use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;
use std::sync::Arc;

use crate::applications::round::ScrapeRound;
use crate::applications::scrape_loop::RoundReceiver;
use crate::applications::wire::ApplicationReportDto;
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/applications", get(applications))
        .with_state(state)
}

async fn applications(State(state): State<Arc<AppState>>) -> Json<ApplicationsResponse> {
    let round = state.rounds.as_ref().and_then(current_round);
    Json(ApplicationsResponse::from(round.as_deref()))
}

/// The round the scrape loop last published, if the loop still runs: a closed channel still
/// holds the ended loop's last round, which is no longer current.
fn current_round(rounds: &RoundReceiver) -> Option<Arc<ScrapeRound>> {
    rounds.has_changed().ok()?;
    rounds.borrow().clone()
}

/// The applications poll response. Every field is always present; `round`, `interval_secs`
/// and `scraped_at` are `null` together, when there is no round.
#[derive(Debug, Serialize)]
struct ApplicationsResponse {
    round: Option<RoundIdDto>,
    interval_secs: Option<u64>,
    scraped_at: Option<u64>,
    applications: Vec<ApplicationReportDto>,
}

#[derive(Debug, Serialize)]
struct RoundIdDto {
    run: String,
    seq: u64,
}

impl From<Option<&ScrapeRound>> for ApplicationsResponse {
    fn from(round: Option<&ScrapeRound>) -> Self {
        let Some(round) = round else {
            return Self {
                round: None,
                interval_secs: None,
                scraped_at: None,
                applications: Vec::new(),
            };
        };
        Self {
            round: Some(RoundIdDto {
                run: round.id.run.as_uuid().hyphenated().to_string(),
                seq: round.id.seq,
            }),
            interval_secs: Some(round.interval.as_duration().as_secs()),
            scraped_at: Some(round.scraped_at),
            applications: round
                .applications
                .iter()
                .map(ApplicationReportDto::from)
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::AgentRun;
    use crate::applications::wire::sample_round;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
    use tokio::sync::watch;
    use tower::ServiceExt;

    fn round() -> ScrapeRound {
        sample_round(AgentRun::new(uuid::Uuid::from_u128(0x2a)), 7)
    }

    async fn get_applications(state: Arc<AppState>) -> Value {
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/applications")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn a_published_round_is_served_in_full() {
        let (sender, receiver) = watch::channel(None);
        sender.send_replace(Some(Arc::new(round())));
        let body = get_applications(AppState::with_rounds(receiver)).await;
        assert_eq!(
            body,
            json!({
                "round": { "run": "00000000-0000-0000-0000-00000000002a", "seq": 7 },
                "interval_secs": 20,
                "scraped_at": 1_790_000_000,
                "applications": [
                    {
                        "name": "orders",
                        "health": "up",
                        "version": "2.4.1",
                        "gauges": { "heap_used_bytes": 300.0, "uptime_seconds": 600.5 }
                    },
                    { "name": "billing", "health": "unreachable", "version": null, "gauges": {} }
                ]
            })
        );
        drop(sender);
    }

    #[tokio::test]
    async fn without_a_running_loop_or_before_its_first_round_there_is_no_round() {
        let no_round = json!({
            "round": null,
            "interval_secs": null,
            "scraped_at": null,
            "applications": []
        });
        let (waiting, before_first) = watch::channel(None);
        let (ended, after_end) = watch::channel(Some(Arc::new(round())));
        // The loop ended: its sender is gone, though the channel still holds its last round.
        drop(ended);
        let cases = [
            ("applications off", AppState::new()),
            (
                "before the first round",
                AppState::with_rounds(before_first),
            ),
            ("the loop ended", AppState::with_rounds(after_end)),
        ];
        for (name, state) in cases {
            assert_eq!(get_applications(state).await, no_round, "case: {name}");
        }
        drop(waiting);
    }

    #[test]
    fn a_round_serves_the_golden_applications_poll_body() {
        // Written by hand, the same round as the golden application frame; the hub's poll
        // test decodes the same file.
        let golden: Value =
            serde_json::from_str(include_str!("../../testdata/applications-v1.json")).unwrap();
        let run = uuid::Uuid::parse_str("6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b").unwrap();
        let round = crate::applications::wire::sample_round(AgentRun::new(run), 42);
        let served = serde_json::to_value(ApplicationsResponse::from(Some(&round))).unwrap();
        assert_eq!(served, golden);
    }
}
