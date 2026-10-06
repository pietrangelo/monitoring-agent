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

//! Hub time and the guarded retention clock (RFC 0010 §2): pure functions of the clocks
//! passed in. The writer thread reads the system clock and `Instant`s and passes them here.

use std::time::Duration;

/// Hub time: never runs backwards. `last_issued` is persisted in the same redb transaction
/// as the points it stamped, so a committed point is never later than the committed clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HubClock {
    last_issued: u64,
}

/// How far the system clock trails hub time while hub time holds (a backward step).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hold {
    None,
    Held { gap_secs: u64 },
}

/// The time since the previous retention pass, as the monotonic clock measured it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Elapsed {
    /// The first pass after the store opened: nothing to measure from.
    SinceOpen,
    Measured(Duration),
}

impl HubClock {
    /// The clock as last committed.
    pub fn resume(last_issued: u64) -> HubClock {
        HubClock { last_issued }
    }

    /// `max(system_now, last_issued)`, issued.
    pub fn now(&mut self, system_now: u64) -> u64 {
        self.last_issued = self.last_issued.max(system_now);
        self.last_issued
    }

    pub fn last_issued(&self) -> u64 {
        self.last_issued
    }

    /// Whether hub time is held ahead of the system clock, and by how much.
    pub fn hold(&self, system_now: u64) -> Hold {
        match self.last_issued.checked_sub(system_now) {
            Some(gap_secs) if gap_secs > 0 => Hold::Held { gap_secs },
            _ => Hold::None,
        }
    }
}

/// The retention clock after a pass: at most twice as fast as the monotonic clock says time
/// passed, never past hub time or the system clock, and unchanged by the first pass after open.
pub fn retention_now(previous: u64, hub_now: u64, system_now: u64, since: Elapsed) -> u64 {
    let allowed = match since {
        Elapsed::SinceOpen => 0,
        Elapsed::Measured(d) => d.as_secs().saturating_mul(2),
    };
    hub_now
        .min(system_now)
        .min(previous.saturating_add(allowed))
}

/// The retention clock of a new store: created at `min(hub_now, system_now)`.
pub fn first_retention_clock(hub_now: u64, system_now: u64) -> u64 {
    hub_now.min(system_now)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;
    const YEAR: u64 = 365 * 86_400;

    #[test]
    fn hub_time_follows_the_system_clock_forward_and_holds_on_a_backward_step() {
        let mut clock = HubClock::resume(NOW);
        let cases = [
            ("steady", NOW + 2, NOW + 2, Hold::None),
            (
                "a year forward is stamped at once",
                NOW + YEAR,
                NOW + YEAR,
                Hold::None,
            ),
            (
                "backward: held",
                NOW + 10,
                NOW + YEAR,
                Hold::Held {
                    gap_secs: YEAR - 10,
                },
            ),
            (
                "still behind: held",
                NOW + YEAR - 1,
                NOW + YEAR,
                Hold::Held { gap_secs: 1 },
            ),
            ("caught up", NOW + YEAR, NOW + YEAR, Hold::None),
            ("past it", NOW + YEAR + 5, NOW + YEAR + 5, Hold::None),
        ];
        for (name, system, hub, hold) in cases {
            assert_eq!(clock.now(system), hub, "{name}");
            assert_eq!(clock.last_issued(), hub, "{name}: issued");
            assert_eq!(clock.hold(system), hold, "{name}");
        }
    }

    #[test]
    fn a_resumed_clock_never_issues_before_its_committed_value() {
        let mut clock = HubClock::resume(NOW + 100);
        assert_eq!(clock.now(NOW), NOW + 100);
        assert_eq!(clock.now(0), NOW + 100);
    }

    #[test]
    fn the_retention_clock_is_bounded_by_twice_real_time_and_both_clocks() {
        let ten_min = Elapsed::Measured(Duration::from_secs(600));
        let cases = [
            ("steady", NOW, NOW + 600, NOW + 600, ten_min, NOW + 600),
            (
                "a year forward, ten minutes elapsed",
                NOW,
                NOW + YEAR,
                NOW + YEAR,
                ten_min,
                NOW + 1_200,
            ),
            (
                "the system clock back after the fault",
                NOW + 1_200,
                NOW + YEAR,
                NOW + 1_300,
                ten_min,
                NOW + 1_300,
            ),
            (
                "the first pass after open",
                NOW,
                NOW + YEAR,
                NOW + YEAR,
                Elapsed::SinceOpen,
                NOW,
            ),
            (
                "two weeks down, second pass",
                NOW,
                NOW + 14 * 86_400,
                NOW + 14 * 86_400,
                ten_min,
                NOW + 1_200,
            ),
            (
                "never past hub time",
                NOW,
                NOW + 100,
                NOW + 5_000,
                ten_min,
                NOW + 100,
            ),
            (
                "never past the system clock",
                NOW,
                NOW + 5_000,
                NOW + 100,
                ten_min,
                NOW + 100,
            ),
            (
                "follows a system clock stepped back",
                NOW + 1_000,
                NOW + 2_000,
                NOW + 500,
                ten_min,
                NOW + 500,
            ),
            (
                "sub-second elapsed advances nothing",
                NOW,
                NOW + 10,
                NOW + 10,
                Elapsed::Measured(Duration::from_millis(400)),
                NOW,
            ),
        ];
        for (name, previous, hub, system, since, expected) in cases {
            assert_eq!(
                retention_now(previous, hub, system, since),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn a_restart_loop_under_a_forward_fault_moves_the_retention_clock_by_nothing() {
        let clock = (0..100).fold(NOW, |previous, _| {
            retention_now(previous, NOW + YEAR, NOW + YEAR, Elapsed::SinceOpen)
        });
        assert_eq!(clock, NOW);
    }

    #[test]
    fn a_new_stores_retention_clock_starts_at_the_earlier_clock() {
        assert_eq!(first_retention_clock(NOW + 5, NOW), NOW);
        assert_eq!(first_retention_clock(NOW, NOW + 5), NOW);
    }
}
