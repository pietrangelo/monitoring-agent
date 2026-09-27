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

//! Admitting and storing one scrape round, whichever source it came from (RFC 0009 §8): the
//! push receiver today, the poller in commit 4. One step under the database mutex; the live
//! lock is taken only briefly inside it (lock order: database, then live state).

use std::sync::PoisonError;
use std::time::Instant;

use crate::applications::{
    Admission, HeldRound, RecentRounds, RoundDigest, RoundId, ScrapeRound, SourcePace, admit,
    application_points,
};
use crate::db::RoundStored;
use crate::models::SystemId;
use crate::state::AppState;

/// When a round reached the hub: the monotonic instant pacing uses, and the wall-clock second
/// it is stored and aged at.
#[derive(Debug, Clone, Copy)]
pub struct Arrival {
    pub now: Instant,
    pub received_at: u64,
}

/// A round's identity for admission: its id and its content digest.
#[derive(Debug, Clone, Copy)]
struct Incoming {
    id: RoundId,
    digest: RoundDigest,
}

/// Stores a round if admitted, and returns the outcome with the source's pace after it:
/// spent on a stored round, unchanged otherwise. Blocking: runs on the blocking pool.
pub fn store_round(
    app: &AppState,
    system_id: &SystemId,
    round: ScrapeRound,
    pace: SourcePace,
    arrival: Arrival,
) -> (Result<RoundStored, rusqlite::Error>, SourcePace) {
    let incoming = Incoming {
        id: round.id(),
        digest: app.digester.digest(&round),
    };
    let points = application_points(&round);
    let mut after = pace;
    let stored = app.db.store_round(
        system_id.as_str(),
        points,
        arrival.received_at,
        || decide(app, system_id, incoming, pace, arrival.now),
        |spent| {
            after = spent;
            let received_at = arrival.received_at;
            hold(app, system_id, incoming, HeldRound { round, received_at });
        },
    );
    (stored, after)
}

/// Reads the system's recent rounds and decides; the live lock is released on return.
fn decide(
    app: &AppState,
    system_id: &SystemId,
    incoming: Incoming,
    pace: SourcePace,
    now: Instant,
) -> Admission {
    let live = app
        .live_applications
        .read()
        .unwrap_or_else(PoisonError::into_inner);
    let none = RecentRounds::default();
    let recent = live
        .get(system_id.as_str())
        .map_or(&none, |entry| &entry.recent);
    admit(recent, pace, incoming.id, incoming.digest, now)
}

/// Remembers an accepted round and shows it; the live lock is released on return.
fn hold(app: &AppState, system_id: &SystemId, incoming: Incoming, held: HeldRound) {
    let mut live = app
        .live_applications
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    let entry = live.entry(system_id.as_str().to_string()).or_default();
    entry.recent.remember(incoming.id, incoming.digest);
    entry.shown = Some(held);
}
