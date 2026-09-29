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
//! push receiver and the poller. One step under the database mutex; the live
//! lock is taken only briefly inside it (lock order: database, then live state).

use std::sync::PoisonError;
use std::time::Instant;

use crate::applications::{
    Admission, HeldRound, RecentRounds, RoundDigest, RoundId, ScrapeRound, SourcePace, admit,
    application_points,
};
use crate::db::RoundStored;
use crate::models::SystemId;
use crate::state::{AppState, SystemApplications};

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

/// Stores a pushed round if admitted, under the connection's own pace, and returns the outcome
/// with that pace after it: spent on a stored round, unchanged otherwise. Blocking: runs on
/// the blocking pool.
pub fn store_round(
    app: &AppState,
    system_id: &SystemId,
    round: ScrapeRound,
    pace: SourcePace,
    arrival: Arrival,
) -> (Result<RoundStored, rusqlite::Error>, SourcePace) {
    let mut after = pace;
    let stored = store(
        app,
        system_id,
        round,
        arrival,
        |_| pace,
        |_, spent| after = spent,
    );
    (stored, after)
}

/// Stores a round polled from a system under that system's poll pace. The pace is read and
/// spent inside the admission step, under the database mutex, so polls that overlap (a slow
/// poll still running at the next tick) share one bucket. Blocking: runs on the blocking pool.
pub fn store_polled_round(
    app: &AppState,
    system_id: &SystemId,
    round: ScrapeRound,
    arrival: Arrival,
) -> Result<RoundStored, rusqlite::Error> {
    let fresh = SourcePace::new(arrival.now);
    store(
        app,
        system_id,
        round,
        arrival,
        |entry| entry.map_or(fresh, |entry| entry.poll_pace.unwrap_or(fresh)),
        |entry, spent| entry.poll_pace = Some(spent),
    )
}

/// The one admission-and-store step both sources share: `pace_of` reads the source's pace
/// when deciding, `keep` records the spent pace when the round is held.
fn store(
    app: &AppState,
    system_id: &SystemId,
    round: ScrapeRound,
    arrival: Arrival,
    pace_of: impl FnOnce(Option<&SystemApplications>) -> SourcePace,
    keep: impl FnOnce(&mut SystemApplications, SourcePace),
) -> Result<RoundStored, rusqlite::Error> {
    let incoming = Incoming {
        id: round.id(),
        digest: app.digester.digest(&round),
    };
    let points = application_points(&round);
    app.db.store_round(
        system_id.as_str(),
        points,
        arrival.received_at,
        || decide(app, system_id, incoming, pace_of, arrival.now),
        |spent| {
            let held = HeldRound {
                round,
                received_at: arrival.received_at,
            };
            hold(app, system_id, incoming, held, |entry| keep(entry, spent));
        },
    )
}

/// Reads the system's recent rounds and its source's pace, and decides; the live lock is
/// released on return.
fn decide(
    app: &AppState,
    system_id: &SystemId,
    incoming: Incoming,
    pace_of: impl FnOnce(Option<&SystemApplications>) -> SourcePace,
    now: Instant,
) -> Admission {
    let live = app
        .live_applications
        .read()
        .unwrap_or_else(PoisonError::into_inner);
    let entry = live.get(system_id.as_str());
    let none = RecentRounds::default();
    let recent = entry.map_or(&none, |entry| &entry.recent);
    admit(recent, pace_of(entry), incoming.id, incoming.digest, now)
}

/// Remembers an accepted round and shows it; the live lock is released on return.
fn hold(
    app: &AppState,
    system_id: &SystemId,
    incoming: Incoming,
    held: HeldRound,
    keep: impl FnOnce(&mut SystemApplications),
) {
    let mut live = app
        .live_applications
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    let entry = live.entry(system_id.as_str().to_string()).or_default();
    entry.recent.remember(incoming.id, incoming.digest);
    entry.shown = Some(held);
    keep(entry);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::applications::{RoundId, ScrapeInterval};
    use crate::db::Database;
    use crate::models::{SystemInfo, SystemStatus};
    use std::sync::{Arc, Barrier};

    fn state_with_system() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::new(dir.path().join("t.db").to_str().unwrap()).unwrap());
        db.insert_system(&SystemInfo {
            id: "id-1".into(),
            name: "id-1".into(),
            url: "http://x".into(),
            token: String::new(),
            status: SystemStatus::Unknown,
            last_seen: String::new(),
            last_error: None,
            os: None,
            hostname: None,
            kernel: None,
            cpu_model: None,
            cpu_cores: None,
            total_memory_display: None,
            total_memory_bytes: None,
            poll_interval_secs: 10,
            enabled: true,
        })
        .unwrap();
        (AppState::new(db).unwrap(), dir)
    }

    #[test]
    fn overlapping_polls_of_one_system_share_one_pace() {
        let (app, _dir) = state_with_system();
        let id = SystemId::try_from("id-1".to_string()).unwrap();
        let polls = 8;
        let barrier = Arc::new(Barrier::new(polls));
        let threads: Vec<_> = (1..=polls as u64)
            .map(|seq| {
                let (app, id, barrier) = (app.clone(), id.clone(), barrier.clone());
                std::thread::spawn(move || {
                    let round = ScrapeRound::new(
                        RoundId::parse("6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b", seq).unwrap(),
                        ScrapeInterval::from_secs(15).unwrap(),
                        vec![],
                    )
                    .unwrap();
                    let arrival = Arrival {
                        now: Instant::now(),
                        received_at: 1,
                    };
                    barrier.wait();
                    store_polled_round(&app, &id, round, arrival).unwrap()
                })
            })
            .collect();
        let stored = threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(|outcome| *outcome == RoundStored::Stored)
            .count();
        assert_eq!(stored, 2, "one burst of two across overlapping polls");
    }

    #[test]
    fn a_polled_rounds_pace_is_spent_only_on_stored_rounds_and_refills() {
        let (app, _dir) = state_with_system();
        let id = SystemId::try_from("id-1".to_string()).unwrap();
        let base = Instant::now();
        let round = |seq| {
            ScrapeRound::new(
                RoundId::parse("6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b", seq).unwrap(),
                ScrapeInterval::from_secs(15).unwrap(),
                vec![],
            )
            .unwrap()
        };
        let at = |secs| Arrival {
            now: base + std::time::Duration::from_secs(secs),
            received_at: 1,
        };
        // (case, seq, seconds after base, outcome) — one system, polled in order.
        let cases = [
            ("the first round", 1, 0, RoundStored::Stored),
            (
                "its exact re-send spends no token",
                1,
                0,
                RoundStored::Duplicate,
            ),
            (
                "a second round: the burst's second token",
                2,
                0,
                RoundStored::Stored,
            ),
            ("a third at once", 3, 0, RoundStored::TooSoon),
            (
                "a refused round spent nothing either",
                3,
                7,
                RoundStored::TooSoon,
            ),
            ("one token refilled at 8 s", 3, 8, RoundStored::Stored),
        ];
        for (case, seq, secs, expected) in cases {
            let outcome = store_polled_round(&app, &id, round(seq), at(secs)).unwrap();
            assert_eq!(outcome, expected, "case: {case}");
        }
    }

    #[test]
    fn a_push_connections_pace_and_the_systems_poll_pace_stay_apart() {
        let (app, _dir) = state_with_system();
        let id = SystemId::try_from("id-1".to_string()).unwrap();
        let now = Instant::now();
        let arrival = Arrival {
            now,
            received_at: 1,
        };
        let round = |seq| {
            ScrapeRound::new(
                RoundId::parse("6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b", seq).unwrap(),
                ScrapeInterval::from_secs(15).unwrap(),
                vec![],
            )
            .unwrap()
        };
        let poll_pace = |app: &AppState| app.live_applications.read().unwrap()["id-1"].poll_pace;
        for seq in [1, 2] {
            let polled = store_polled_round(&app, &id, round(seq), arrival).unwrap();
            assert_eq!(polled, RoundStored::Stored, "poll seq {seq}");
        }
        let spent = poll_pace(&app);

        let connection = SourcePace::new(now);
        let (pushed, after) = store_round(&app, &id, round(3), connection, arrival);
        assert_eq!(
            pushed.unwrap(),
            RoundStored::Stored,
            "the push has its own pace"
        );
        assert_eq!(
            after,
            connection.take(now).unwrap(),
            "the connection's pace is spent"
        );
        assert_eq!(
            poll_pace(&app),
            spent,
            "the push leaves the poll pace alone"
        );

        let polled = store_polled_round(&app, &id, round(4), arrival).unwrap();
        assert_eq!(polled, RoundStored::TooSoon, "the poll pace is still spent");
    }
}
