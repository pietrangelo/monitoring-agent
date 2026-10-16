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

//! Live and dead chunks (RFC 0010 §5): one pure function decides whether a chunk, in `chunks`
//! or in a block file, may still be read or kept.

use std::collections::BTreeSet;

use super::policy::{Policies, TierPeriod};
use crate::name::{Generation, SystemKey};
use crate::tier::{SpanStart, Tier};

/// Whose a chunk is: its series' system and generation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Owner {
    pub system: SystemKey,
    pub generation: Generation,
}

/// The generations deleted from the hub (RFC 0011 §3): their data is unreadable at once.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tombstones(BTreeSet<Owner>);

impl Tombstones {
    pub fn insert(&mut self, owner: Owner) {
        self.0.insert(owner);
    }

    pub fn contains(&self, owner: &Owner) -> bool {
        self.0.contains(owner)
    }
}

/// Whether a chunk may still be read or kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    Live,
    Dead(Death),
}

/// Why a chunk is dead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Death {
    /// Unmapped: its owner is unknown, since its series id maps to no series or its
    /// generation to no system (defence in depth).
    Unmapped,
    /// Its generation is tombstoned.
    Tombstoned,
    /// Its span ended before the retention clock minus its system's period for the tier.
    Expired,
}

/// Whether a span of `tier` is past `period` at `retention_now`: every second of it is older
/// than the retention clock minus the period.
pub fn expired(tier: Tier, span: SpanStart, period: TierPeriod, retention_now: u64) -> bool {
    let cutoff = retention_now.saturating_sub(period.period().as_secs());
    span.end(tier) <= cutoff
}

/// The liveness of a chunk of `tier` in `span`, owned by `owner` (`None`: its series id maps
/// to no series).
pub fn chunk_liveness(
    tier: Tier,
    span: SpanStart,
    owner: Option<&Owner>,
    policies: &Policies,
    tombstones: &Tombstones,
    retention_now: u64,
) -> Liveness {
    match owner {
        None => Liveness::Dead(Death::Unmapped),
        Some(owner) if tombstones.contains(owner) => Liveness::Dead(Death::Tombstoned),
        Some(owner) => {
            let period = policies.effective(&owner.system, tier);
            match expired(tier, span, period, retention_now) {
                true => Liveness::Dead(Death::Expired),
                false => Liveness::Live,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::retention::policy::{RetentionChange, RetentionPolicy};

    const HOUR: u64 = 3_600;
    const DAY: u64 = 86_400;
    /// A day boundary, so every tier's spans start on it.
    const NOW: u64 = 1_800_057_600;

    fn owner(name: &str, generation: u64) -> Owner {
        Owner {
            system: SystemKey::try_from(name.as_bytes()).expect("key"),
            generation: Generation::new(generation),
        }
    }

    fn span(tier: Tier, start: u64) -> SpanStart {
        SpanStart::new(tier, start).expect("grid")
    }

    fn period(tier: Tier, secs: u64) -> TierPeriod {
        TierPeriod::new(tier, Duration::from_secs(secs)).expect("bounds")
    }

    /// The default policy, with `long` keeping 48 h of raw points.
    fn policies() -> Policies {
        let mut policies = Policies::new(RetentionPolicy::default());
        let change = RetentionChange::new([period(Tier::Raw, 48 * HOUR)]).expect("one tier");
        policies.change(&owner("long", 1).system, &change, Duration::from_secs(600));
        policies
    }

    #[test]
    fn a_span_expires_once_all_of_it_is_older_than_the_clock_minus_the_period() {
        let day = period(Tier::Raw, 24 * HOUR);
        let cutoff = NOW - 24 * HOUR;
        let cases = [
            ("ends exactly at the cutoff", cutoff - HOUR, NOW, true),
            (
                "its last second is at the cutoff",
                cutoff - HOUR,
                NOW - 1,
                false,
            ),
            ("starts at the cutoff", cutoff, NOW, false),
            ("long before", cutoff - 10 * HOUR, NOW, true),
            ("the current span", NOW - HOUR, NOW, false),
            ("a clock younger than the period", 0, 10 * HOUR, false),
        ];
        for (name, start, now, dead) in cases {
            assert_eq!(
                expired(Tier::Raw, span(Tier::Raw, start), day, now),
                dead,
                "{name}"
            );
        }
        let hours = period(Tier::Hour, 7 * DAY);
        let start = NOW - 8 * DAY;
        assert!(
            expired(Tier::Hour, span(Tier::Hour, start), hours, NOW),
            "a day span, past"
        );
        assert!(
            !expired(Tier::Hour, span(Tier::Hour, start), hours, NOW - 1),
            "a day span ends a day after its start"
        );
    }

    #[test]
    fn a_chunk_is_dead_when_unmapped_tombstoned_or_past_its_systems_period() {
        let mut tombstones = Tombstones::default();
        tombstones.insert(owner("gone", 7));
        let policies = policies();
        let recent = span(Tier::Raw, NOW - 2 * HOUR);
        let day_and_a_half = span(Tier::Raw, NOW - 36 * HOUR);
        let cases = [
            (
                "a recent chunk",
                Some(owner("a", 1)),
                recent,
                Liveness::Live,
            ),
            ("no series", None, recent, Liveness::Dead(Death::Unmapped)),
            (
                "a tombstoned generation",
                Some(owner("gone", 7)),
                recent,
                Liveness::Dead(Death::Tombstoned),
            ),
            (
                "the same system's next generation",
                Some(owner("gone", 8)),
                recent,
                Liveness::Live,
            ),
            (
                "past the global period",
                Some(owner("a", 1)),
                day_and_a_half,
                Liveness::Dead(Death::Expired),
            ),
            (
                "kept by a longer override",
                Some(owner("long", 1)),
                day_and_a_half,
                Liveness::Live,
            ),
            (
                "tombstoned before expired",
                Some(owner("gone", 7)),
                day_and_a_half,
                Liveness::Dead(Death::Tombstoned),
            ),
            (
                "unmapped before anything else",
                None,
                day_and_a_half,
                Liveness::Dead(Death::Unmapped),
            ),
        ];
        for (name, who, at, liveness) in cases {
            assert_eq!(
                chunk_liveness(Tier::Raw, at, who.as_ref(), &policies, &tombstones, NOW),
                liveness,
                "{name}"
            );
        }
        let straddling = span(Tier::Raw, NOW - 24 * HOUR);
        let off_grid = NOW + HOUR / 2;
        assert_eq!(
            chunk_liveness(
                Tier::Raw,
                straddling,
                Some(&owner("a", 1)),
                &policies,
                &tombstones,
                off_grid
            ),
            Liveness::Live,
            "a span the cutoff falls inside is live"
        );
    }

    #[test]
    fn each_tier_is_judged_by_its_own_period() {
        let policies = policies();
        let tombstones = Tombstones::default();
        let who = owner("long", 1);
        let cases = [
            (
                "raw, three days back",
                Tier::Raw,
                NOW - 3 * DAY,
                Liveness::Dead(Death::Expired),
            ),
            (
                "minute, three days back",
                Tier::Minute,
                NOW - 3 * DAY,
                Liveness::Live,
            ),
            (
                "minute, 15 days back",
                Tier::Minute,
                NOW - 15 * DAY,
                Liveness::Dead(Death::Expired),
            ),
            (
                "hour, 15 days back",
                Tier::Hour,
                NOW - 15 * DAY,
                Liveness::Live,
            ),
            (
                "hour, 31 days back",
                Tier::Hour,
                NOW - 31 * DAY,
                Liveness::Dead(Death::Expired),
            ),
        ];
        for (name, tier, start, liveness) in cases {
            let at = tier.span_of(start);
            assert_eq!(
                chunk_liveness(tier, at, Some(&who), &policies, &tombstones, NOW),
                liveness,
                "{name}"
            );
        }
        let straddling = Tier::Hour.span_of(NOW - 30 * DAY);
        assert_eq!(
            chunk_liveness(
                Tier::Hour,
                straddling,
                Some(&who),
                &policies,
                &tombstones,
                NOW + 12 * HOUR
            ),
            Liveness::Live,
            "a day span the cutoff falls inside is live"
        );
    }
}
