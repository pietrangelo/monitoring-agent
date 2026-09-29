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

//! `GET /api/systems/:id/applications` (RFC 0009 §9): the round the hub shows for a system,
//! read from live state only, with its age and freshness computed on the hub.

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError};

use crate::applications::{ApplicationReport, Freshness, HeldRound, freshness};
use crate::clock::unix_now;
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/systems/:id/applications", get(applications))
        .with_state(state)
}

async fn applications(
    State(state): State<Arc<AppState>>,
    Path(system_id): Path<String>,
) -> Json<SystemApplicationsResponse> {
    // Cloned, so the live lock isn't held while the response is built.
    let held = state
        .live_applications
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&system_id)
        .and_then(|entry| entry.shown.clone());
    Json(SystemApplicationsResponse::new(
        system_id,
        held.as_ref(),
        unix_now(),
    ))
}

/// `received_at`, `age_secs` and `freshness` are `null` together, when the hub holds no round.
#[derive(Debug, Serialize)]
struct SystemApplicationsResponse {
    system_id: String,
    received_at: Option<u64>,
    age_secs: Option<u64>,
    freshness: Option<&'static str>,
    applications: Vec<ApplicationDto>,
}

impl SystemApplicationsResponse {
    fn new(system_id: String, held: Option<&HeldRound>, now: u64) -> Self {
        let Some(held) = held else {
            return Self {
                system_id,
                received_at: None,
                age_secs: None,
                freshness: None,
                applications: Vec::new(),
            };
        };
        Self {
            system_id,
            received_at: Some(held.received_at),
            age_secs: Some(now.saturating_sub(held.received_at)),
            freshness: Some(match freshness(held, now) {
                Freshness::Fresh => "fresh",
                Freshness::Stale => "stale",
            }),
            applications: held
                .round
                .applications()
                .iter()
                .map(ApplicationDto::from)
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct ApplicationDto {
    name: String,
    health: &'static str,
    version: Option<String>,
    gauges: BTreeMap<&'static str, f64>,
}

impl From<&ApplicationReport> for ApplicationDto {
    fn from(report: &ApplicationReport) -> Self {
        Self {
            name: report.name.as_str().to_string(),
            health: report.health.wire_name(),
            version: report.version.as_ref().map(|v| v.as_str().to_string()),
            gauges: report
                .gauges
                .iter()
                .map(|(gauge, value)| (gauge.wire_name(), value))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::applications::{
        ApplicationHealth, ApplicationName, ApplicationReport, ApplicationVersion, Gauges,
        HeldRound, RoundId, ScrapeInterval, ScrapeRound,
    };
    use crate::db::Database;
    use crate::state::SystemApplications;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    fn temp_state() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Arc::new(Database::new(path.to_str().unwrap()).unwrap());
        (AppState::new(db).unwrap(), dir)
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn hold(state: &AppState, system: &str, received_at: u64) {
        hold_every(state, system, received_at, 15);
    }

    fn hold_every(state: &AppState, system: &str, received_at: u64, interval_secs: u64) {
        let round = ScrapeRound::new(
            RoundId::parse("6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b", 3).unwrap(),
            ScrapeInterval::from_secs(interval_secs).unwrap(),
            vec![
                ApplicationReport {
                    name: ApplicationName::parse("orders").unwrap(),
                    health: ApplicationHealth::OutOfService,
                    version: ApplicationVersion::parse("2.4.1"),
                    gauges: Gauges::from_wire([("heap_used_bytes", 300.0), ("live_threads", 12.0)]),
                },
                ApplicationReport {
                    name: ApplicationName::parse("billing").unwrap(),
                    health: ApplicationHealth::Unreachable,
                    version: None,
                    gauges: Gauges::default(),
                },
            ],
        )
        .unwrap();
        let entry = SystemApplications {
            shown: Some(HeldRound { round, received_at }),
            ..SystemApplications::default()
        };
        state
            .live_applications
            .write()
            .unwrap()
            .insert(system.to_string(), entry);
    }

    async fn get(state: Arc<AppState>, system: &str) -> Value {
        let res = router(state)
            .oneshot(
                Request::builder()
                    .uri(format!("/api/systems/{system}/applications"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn a_system_without_a_held_round_has_no_applications() {
        let (state, _dir) = temp_state();
        state
            .live_applications
            .write()
            .unwrap()
            .insert("quiet".into(), SystemApplications::default());
        for system in ["unknown", "quiet"] {
            assert_eq!(
                get(state.clone(), system).await,
                json!({
                    "system_id": system,
                    "received_at": null,
                    "age_secs": null,
                    "freshness": null,
                    "applications": []
                }),
                "case: {system}"
            );
        }
    }

    #[tokio::test]
    async fn a_held_round_is_served_with_its_age_and_freshness() {
        let (state, _dir) = temp_state();
        let received_at = now() - 5;
        hold(&state, "sys-1", received_at);
        let mut body = get(state, "sys-1").await;
        let age = body["age_secs"].take();
        assert!(
            (5..=7).contains(&age.as_u64().unwrap_or(u64::MAX)),
            "age from the hub's clock: {age}"
        );
        assert_eq!(
            body,
            json!({
                "system_id": "sys-1",
                "received_at": received_at,
                "age_secs": null,
                "freshness": "fresh",
                "applications": [
                    {
                        "name": "orders",
                        "health": "out_of_service",
                        "version": "2.4.1",
                        "gauges": { "heap_used_bytes": 300.0, "live_threads": 12.0 }
                    },
                    { "name": "billing", "health": "unreachable", "version": null, "gauges": {} }
                ]
            })
        );
    }

    #[tokio::test]
    async fn a_round_held_past_two_intervals_and_the_poll_tick_is_stale() {
        let (state, _dir) = temp_state();
        // Rows away from the bound by more than the test's own runtime, on both sides.
        let cases = [
            (
                "15 s interval, 45 s ago: within the 30 s slack",
                15,
                45,
                "fresh",
            ),
            ("15 s interval, 61 s ago", 15, 61, "stale"),
            ("60 s interval, 100 s ago", 60, 100, "fresh"),
            ("60 s interval, 151 s ago", 60, 151, "stale"),
        ];
        for (case, interval, ago, expected) in cases {
            hold_every(&state, "sys-1", now() - ago, interval);
            let body = get(state.clone(), "sys-1").await;
            assert_eq!(body["freshness"], expected, "case: {case}");
            assert_eq!(
                body["applications"].as_array().map(Vec::len),
                Some(2),
                "case: {case}"
            );
        }
    }

    #[test]
    fn freshness_flips_exactly_past_two_intervals_and_30_seconds() {
        let (state, _dir) = temp_state();
        hold(&state, "sys-1", 1_000); // 15 s interval: fresh through 1_060
        let held = state.live_applications.read().unwrap()["sys-1"]
            .shown
            .clone()
            .unwrap();
        let cases = [
            (1_060, Some("fresh"), 60),
            (1_061, Some("stale"), 61),
            (900, Some("fresh"), 0),
        ];
        for (now, freshness, age) in cases {
            let response = SystemApplicationsResponse::new("sys-1".into(), Some(&held), now);
            assert_eq!(response.freshness, freshness, "now {now}");
            assert_eq!(
                response.age_secs,
                Some(age),
                "now {now}: a clock behind reads 0"
            );
        }
    }
}
