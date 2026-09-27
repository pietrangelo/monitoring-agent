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

//! The applications poll (RFC 0009 §8): the agent's latest scrape round, fetched after a
//! successful system poll, admitted and stored like a pushed one under the system's poll pace.

use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use crate::application_wire::{ApplicationsResponseDto, PolledRound};
use crate::applications::{ScrapeRound, ScrapeRoundError};
use crate::clock::unix_now;
use crate::models::{SystemId, SystemInfo};
use crate::round_intake::{self, Arrival};
use crate::state::AppState;

/// Polls the agent's latest scrape round and stores it, or forgets the shown round when the
/// agent has none. Never changes the system's status: that describes the agent, not its
/// applications.
pub async fn poll_applications(
    state: &Arc<AppState>,
    system: &SystemInfo,
    client: &reqwest::Client,
) {
    // Parsed once, so every live entry this poll touches is keyed by a valid id. A stored id
    // that breaks today's rule can't be pushed to either, and keeps no rounds.
    let Ok(id) = SystemId::try_from(system.id.clone()) else {
        tracing::debug!("Not polling applications of an invalid system id");
        return;
    };
    match fetch_applications(client, system).await {
        ApplicationsAnswer::Absent | ApplicationsAnswer::Polled(PolledRound::NoRound) => {
            forget_shown_round(state, &id)
        }
        ApplicationsAnswer::Polled(PolledRound::Round(round)) => {
            store_polled_round(state, id, round).await
        }
        ApplicationsAnswer::Unusable(Unusable::Refused(err)) => log_refused_round(state, &id, err),
        ApplicationsAnswer::Unusable(Unusable::Status(code)) => {
            tracing::debug!(
                "Applications poll of {:?} answered HTTP {code}",
                id.as_str()
            )
        }
        ApplicationsAnswer::Unusable(Unusable::Transport(err)) => {
            tracing::debug!("Applications poll of {:?} failed: {err}", id.as_str())
        }
        ApplicationsAnswer::Unusable(Unusable::TooLarge) => {
            tracing::debug!("Applications poll of {:?}: body over 256 KiB", id.as_str())
        }
        ApplicationsAnswer::Unusable(Unusable::BadJson(err)) => {
            tracing::debug!("Applications poll of {:?}: bad JSON: {err}", id.as_str())
        }
    }
}

/// The largest applications poll body the hub reads.
const MAX_APPLICATIONS_BODY: usize = 256 * 1024;

/// What a poll of `/api/applications` answered.
enum ApplicationsAnswer {
    /// 404: an agent older than RFC 0009.
    Absent,
    Polled(PolledRound),
    Unusable(Unusable),
}

/// Why an applications poll answer was of no use.
#[derive(Debug)]
enum Unusable {
    Transport(reqwest::Error),
    Status(u16),
    TooLarge,
    BadJson(serde_json::Error),
    Refused(ScrapeRoundError),
}

async fn fetch_applications(client: &reqwest::Client, system: &SystemInfo) -> ApplicationsAnswer {
    let url = format!("{}/api/applications", system.url.trim_end_matches('/'));
    let mut resp = match client.get(&url).send().await {
        Ok(resp) => resp,
        Err(err) => return ApplicationsAnswer::Unusable(Unusable::Transport(err)),
    };
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return ApplicationsAnswer::Absent;
    }
    if !status.is_success() {
        return ApplicationsAnswer::Unusable(Unusable::Status(status.as_u16()));
    }
    let body = match read_capped(&mut resp, MAX_APPLICATIONS_BODY).await {
        Ok(body) => body,
        Err(why) => return ApplicationsAnswer::Unusable(why),
    };
    let response = match serde_json::from_slice::<ApplicationsResponseDto>(&body) {
        Ok(response) => response,
        Err(err) => return ApplicationsAnswer::Unusable(Unusable::BadJson(err)),
    };
    match PolledRound::try_from(response) {
        Ok(polled) => ApplicationsAnswer::Polled(polled),
        Err(err) => ApplicationsAnswer::Unusable(Unusable::Refused(err)),
    }
}

/// Reads a body chunk by chunk, refusing it once it passes `cap` bytes, whatever
/// `Content-Length` claims.
async fn read_capped(resp: &mut reqwest::Response, cap: usize) -> Result<Vec<u8>, Unusable> {
    if resp.content_length().is_some_and(|len| len > cap as u64) {
        return Err(Unusable::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(Unusable::Transport)? {
        if body.len() + chunk.len() > cap {
            return Err(Unusable::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The agent has no round: it was downgraded, or its applications were unconfigured. The
/// recent rounds and the poll pace stay.
fn forget_shown_round(state: &AppState, id: &SystemId) {
    let mut live = state
        .live_applications
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(entry) = live.get_mut(id.as_str()) {
        entry.shown = None;
    }
}

/// Stores a polled round under the system's poll pace, off the async runtime.
async fn store_polled_round(state: &Arc<AppState>, id: SystemId, round: ScrapeRound) {
    let app = Arc::clone(state);
    let system = id.clone();
    let stored = tokio::task::spawn_blocking(move || {
        let arrival = Arrival {
            now: Instant::now(),
            received_at: unix_now(),
        };
        round_intake::store_polled_round(&app, &system, round, arrival)
    })
    .await;
    match stored {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => tracing::warn!("Storing a polled round of {:?} failed: {err}", id.as_str()),
        Err(err) => tracing::error!("Storing a polled round of {:?} failed: {err}", id.as_str()),
    }
}

/// Logs a refused polled round: at `warn` at most once an hour per system, else at `debug`.
fn log_refused_round(state: &AppState, id: &SystemId, err: ScrapeRoundError) {
    let system_id = id.as_str();
    let now = Instant::now();
    let mut live = state
        .live_applications
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    let entry = live.entry(system_id.to_string()).or_default();
    match refused_poll(entry.poll_refusal_warned_at, now) {
        RefusedPoll::Warn => {
            entry.poll_refusal_warned_at = Some(now);
            tracing::warn!("Refused a polled round from {system_id:?}: {err:?}");
        }
        RefusedPoll::Quiet => tracing::debug!("Refused a polled round from {system_id:?}: {err:?}"),
    }
}

/// How loudly to log a refused polled round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RefusedPoll {
    /// The system's first refusal in the last hour: logged at `warn`.
    Warn,
    /// Logged at `debug`.
    Quiet,
}

/// A system's refused polled rounds are logged at `warn` at most once an hour.
pub(super) fn refused_poll(last_warned: Option<Instant>, now: Instant) -> RefusedPoll {
    match last_warned {
        Some(at) if now.saturating_duration_since(at) < REFUSAL_WARNING_EVERY => RefusedPoll::Quiet,
        _ => RefusedPoll::Warn,
    }
}

/// How often a system's refused polled rounds may be logged at `warn`.
const REFUSAL_WARNING_EVERY: Duration = Duration::from_secs(3_600);
