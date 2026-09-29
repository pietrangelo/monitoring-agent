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

//! The state one push connection keeps between messages, and how its recurring refusals are
//! logged.

use std::time::Instant;

use crate::applications::SourcePace;
use crate::models::SystemId;

/// What one push connection keeps between messages: its rounds' pace, and how many of its
/// frames it refused or failed to store.
pub(super) struct ConnectionState {
    pub(super) pace: SourcePace,
    pub(super) refused_application_frames: Tally,
    pub(super) refused_snapshot_frames: Tally,
    pub(super) store_errors: Tally,
}

impl ConnectionState {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            pace: SourcePace::new(now),
            refused_application_frames: Tally::default(),
            refused_snapshot_frames: Tally::default(),
            store_errors: Tally::default(),
        }
    }

    /// Logs each count the connection ended with at `info`, beside the per-frame lines.
    pub(super) fn log_counts(&self, system_id: &SystemId) {
        let counts = [
            (
                self.refused_application_frames,
                "application frame(s) refused",
            ),
            (self.refused_snapshot_frames, "snapshot frame(s) refused"),
            (self.store_errors, "snapshot(s) that failed to store"),
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

#[cfg(test)]
mod tests {
    use super::*;

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
            let mut connection = ConnectionState::new(Instant::now());
            let seen: Vec<_> = (0..4).map(|_| tally(&mut connection).note()).collect();
            assert_eq!(seen, [First, Repeat, Repeat, Repeat], "case: {case}");
            assert_eq!(
                tally(&mut connection).0,
                4,
                "case: {case}: every one is counted"
            );
            let mut another = ConnectionState::new(Instant::now());
            assert_eq!(
                tally(&mut another).note(),
                First,
                "case: {case}: per connection"
            );
        }
    }
}
