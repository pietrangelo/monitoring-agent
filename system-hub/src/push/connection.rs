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

//! The state one push connection keeps between messages (its decode budget, its rounds' pace,
//! and its counts), and how its recurring refusals are logged.

use std::num::NonZeroU8;
use std::time::{Duration, Instant};

use crate::applications::SourcePace;
use crate::models::SystemId;
use crate::token_bucket::{Empty, Refill, TokenBucket};

/// What one push connection keeps between messages: its decode budget, its rounds' pace, and
/// how many of its messages it dropped, refused or failed to store.
pub(super) struct ConnectionState {
    budget: DecodeBudget,
    pub(super) pace: SourcePace,
    dropped_messages: Tally,
    pub(super) refused_application_frames: Tally,
    pub(super) refused_snapshot_frames: Tally,
    pub(super) store_errors: Tally,
    pub(super) undecodable_frames: Tally,
}

impl ConnectionState {
    pub(super) fn new(now: Instant, decode_refill: Duration) -> Self {
        Self {
            budget: DecodeBudget::new(decode_refill, now),
            pace: SourcePace::new(now),
            dropped_messages: Tally::default(),
            refused_application_frames: Tally::default(),
            refused_snapshot_frames: Tally::default(),
            store_errors: Tally::default(),
            undecodable_frames: Tally::default(),
        }
    }

    /// Spends a token of the decode budget for a message read at `now`. Out of budget, the
    /// message is counted as dropped, and is not to be decoded.
    pub(super) fn spend_decode_budget(&mut self, now: Instant) -> Result<(), OutOfBudget> {
        match self.budget.spend(now) {
            Ok(budget) => {
                self.budget = budget;
                Ok(())
            }
            Err(OutOfBudget) => {
                self.dropped_messages.note();
                Err(OutOfBudget)
            }
        }
    }

    /// Logs each count the connection ended with at `info`, beside the per-frame lines.
    pub(super) fn log_counts(&self, system_id: &SystemId) {
        let counts = [
            (
                self.dropped_messages,
                "message(s) dropped by the decode budget",
            ),
            (
                self.refused_application_frames,
                "application frame(s) refused",
            ),
            (self.refused_snapshot_frames, "snapshot frame(s) refused"),
            (self.store_errors, "snapshot(s) that failed to store"),
            (self.undecodable_frames, "undecodable binary message(s)"),
        ];
        for (Tally(count), what) in counts.into_iter().filter(|(tally, _)| tally.0 > 0) {
            tracing::info!(
                "Push connection of {:?}: {count} {what}",
                system_id.as_str()
            );
        }
    }
}

/// How often one thing happened on a connection.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Tally(u64);

impl Tally {
    /// How many so far.
    #[cfg(test)]
    pub(super) fn count(self) -> u64 {
        self.0
    }

    /// Counts one more, and says whether it is the connection's first.
    pub(super) fn note(&mut self) -> Occurrence {
        self.0 += 1;
        match self.0 {
            1 => Occurrence::First,
            _ => Occurrence::Repeat,
        }
    }
}

/// Whether something happened on a connection for the first time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Occurrence {
    First,
    Repeat,
}

/// Logs a connection's first occurrence of something at `warn`, and the rest at `debug`, so a
/// sender can't flood the log.
pub(super) fn warn_first(occurrence: Occurrence, message: &str) {
    match occurrence {
        Occurrence::First => tracing::warn!("{message}"),
        Occurrence::Repeat => tracing::debug!("{message}"),
    }
}

/// The most messages a connection's decode budget holds: an agent's densest honest burst,
/// right after its handshake (a round, the first snapshot, and the next round).
const DECODE_BURST: NonZeroU8 = match NonZeroU8::new(3) {
    Some(burst) => burst,
    None => panic!("DECODE_BURST is not zero"),
};

/// A push connection's decode budget (RFC 0007 §3): every binary message it sends spends a
/// token before it is decoded, whatever its kind. `DECODE_BURST` tokens, then one more per
/// refill period; a zero period never refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DecodeBudget(TokenBucket);

/// The decode budget had no token left: the message is dropped undecoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OutOfBudget;

impl DecodeBudget {
    /// A new connection's budget: full, its refill clock starting at `now`.
    fn new(refill: Duration, now: Instant) -> Self {
        Self(TokenBucket::full(Refill::new(DECODE_BURST, refill), now))
    }

    /// Spends one token for a message read at `now`.
    fn spend(self, now: Instant) -> Result<Self, OutOfBudget> {
        self.0.take(now).map(Self).map_err(|Empty| OutOfBudget)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Batches of messages read back to back: (time in milliseconds from the start, negative
    /// before it; how many).
    type Batches = &'static [(i64, usize)];

    /// Reads each batch at its time, and returns how many of each batch the budget decoded.
    fn decoded(refill: Duration, batches: Batches) -> Vec<usize> {
        let base = Instant::now();
        let at = |ms: i64| match u64::try_from(ms) {
            Ok(after) => base + Duration::from_millis(after),
            Err(_) => base - Duration::from_millis(ms.unsigned_abs()),
        };
        let mut budget = DecodeBudget::new(refill, base);
        let mut spend = |now: Instant| match budget.spend(now) {
            Ok(next) => {
                budget = next;
                true
            }
            Err(OutOfBudget) => false,
        };
        batches
            .iter()
            .map(|&(ms, count)| (0..count).filter(|_| spend(at(ms))).count())
            .collect()
    }

    #[test]
    fn a_decode_budget_holds_3_messages_then_refills_one_per_period() {
        let second = Duration::from_secs(1);
        let cases: [(&str, Duration, Batches, &[usize]); 10] = [
            ("a new budget: three, then none", second, &[(0, 4)], &[3]),
            (
                "just under 1 s after it empties",
                second,
                &[(0, 3), (999, 1)],
                &[3, 0],
            ),
            ("exactly 1 s after", second, &[(0, 3), (1_000, 2)], &[3, 1]),
            (
                "2.5 s after: two, and the half is kept",
                second,
                &[(0, 3), (2_500, 3), (2_999, 1), (3_000, 1)],
                &[3, 2, 0, 1],
            ),
            (
                "5 s quiet banks no more than 3",
                second,
                &[(0, 3), (5_000, 4)],
                &[3, 3],
            ),
            (
                "an hour quiet banks no more than 3",
                second,
                &[(0, 3), (3_600_000, 4)],
                &[3, 3],
            ),
            (
                "a full budget banks nothing while it waits",
                second,
                &[(1_900, 3), (2_000, 1)],
                &[3, 0],
            ),
            // A connection's clock never runs backwards, but the arithmetic must not panic.
            ("an earlier now", second, &[(0, 3), (-1_000, 1)], &[3, 0]),
            (
                "a 100 ms refill: one per 100 ms, the remainder kept",
                Duration::from_millis(100),
                &[(0, 3), (99, 1), (150, 1), (250, 2)],
                &[3, 0, 1, 1],
            ),
            (
                "a zero refill never refuses",
                Duration::ZERO,
                &[(0, 10), (0, 10)],
                &[10, 10],
            ),
        ];
        for (case, refill, batches, expected) in cases {
            assert_eq!(decoded(refill, batches), expected, "case: {case}");
        }
    }

    #[test]
    fn a_connection_counts_the_messages_its_decode_budget_drops() {
        let now = Instant::now();
        let mut connection = ConnectionState::new(now, Duration::from_secs(1));
        let spent: Vec<_> = (0..5)
            .map(|_| connection.spend_decode_budget(now))
            .collect();
        let dropped = Err(OutOfBudget);
        assert_eq!(spent, [Ok(()), Ok(()), Ok(()), dropped, dropped]);
        assert_eq!(connection.dropped_messages.0, 2, "each drop is counted");
    }

    #[test]
    fn a_connections_first_occurrence_is_first_and_the_rest_repeat() {
        use Occurrence::{First, Repeat};
        type TallyOf = fn(&mut ConnectionState) -> &mut Tally;
        let tallies: [(&str, TallyOf); 3] = [
            ("refused application frames", |c| {
                &mut c.refused_application_frames
            }),
            ("refused snapshot frames", |c| {
                &mut c.refused_snapshot_frames
            }),
            ("store errors", |c| &mut c.store_errors),
        ];
        for (case, tally) in tallies {
            let mut connection = ConnectionState::new(Instant::now(), Duration::ZERO);
            let seen: Vec<_> = (0..4).map(|_| tally(&mut connection).note()).collect();
            assert_eq!(seen, [First, Repeat, Repeat, Repeat], "case: {case}");
            assert_eq!(
                tally(&mut connection).0,
                4,
                "case: {case}: every one is counted"
            );
            let mut another = ConnectionState::new(Instant::now(), Duration::ZERO);
            assert_eq!(
                tally(&mut another).note(),
                First,
                "case: {case}: per connection"
            );
        }
    }
}
