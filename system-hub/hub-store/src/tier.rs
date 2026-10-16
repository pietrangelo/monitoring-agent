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

//! Tiers, spans and buckets (RFC 0010 §5): raw points, 1-minute rollups and 1-hour rollups,
//! each kept in spans that no chunk crosses.

/// One resolution of a series' history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Tier {
    Raw,
    Minute,
    Hour,
}

/// A tier of rollups: buckets of one minute or one hour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RollupTier {
    Minute,
    Hour,
}

/// The start of a span, in hub-time seconds: a multiple of its tier's span length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SpanStart(u64);

impl Tier {
    pub const ALL: [Tier; 3] = [Tier::Raw, Tier::Minute, Tier::Hour];

    /// The length of the tier's spans, in seconds.
    pub fn span_secs(self) -> u64 {
        match self {
            Tier::Raw | Tier::Minute => 3_600,
            Tier::Hour => 86_400,
        }
    }

    /// The span holding a hub time.
    pub fn span_of(self, ts: u64) -> SpanStart {
        SpanStart(ts - ts % self.span_secs())
    }

    /// How long after its end a span stays open: one bucket (raw: one minute) plus one sweep
    /// interval, so the sweep has closed every bucket in it (§6 *Span handoff*).
    pub fn grace_secs(self) -> u64 {
        let bucket = match self {
            Tier::Raw | Tier::Minute => 60,
            Tier::Hour => 3_600,
        };
        bucket + SWEEP_SECS
    }

    /// The longest retention a policy may set for the tier (RFC 0010 §5): 30 days of raw
    /// points, 400 days of minutes, 10 years (3,650 days) of hours. No chunk older than this is
    /// ever live, so a query never needs to look further back.
    pub fn max_retention_secs(self) -> u64 {
        const DAY: u64 = 86_400;
        match self {
            Tier::Raw => 30 * DAY,
            Tier::Minute => 400 * DAY,
            Tier::Hour => 3_650 * DAY,
        }
    }

    /// The shortest retention a policy may set for the tier (RFC 0010 §5): an hour of raw
    /// points, a day of minutes, 7 days of hours.
    pub fn min_retention_secs(self) -> u64 {
        const DAY: u64 = 86_400;
        match self {
            Tier::Raw => 3_600,
            Tier::Minute => DAY,
            Tier::Hour => 7 * DAY,
        }
    }

    /// The tier's rollups, if it holds rollups.
    pub fn rollup(self) -> Option<RollupTier> {
        match self {
            Tier::Raw => None,
            Tier::Minute => Some(RollupTier::Minute),
            Tier::Hour => Some(RollupTier::Hour),
        }
    }

    /// The tier's persisted code.
    pub fn code(self) -> u8 {
        match self {
            Tier::Raw => 0,
            Tier::Minute => 1,
            Tier::Hour => 2,
        }
    }

    pub fn from_code(code: u8) -> Option<Tier> {
        Tier::ALL.into_iter().find(|tier| tier.code() == code)
    }

    /// The tier's name in the API, in configuration and in block-file paths.
    pub fn name(self) -> &'static str {
        match self {
            Tier::Raw => "raw",
            Tier::Minute => "minute",
            Tier::Hour => "hour",
        }
    }
}

impl RollupTier {
    /// The length of one bucket, in seconds.
    pub fn bucket_secs(self) -> u64 {
        match self {
            RollupTier::Minute => 60,
            RollupTier::Hour => 3_600,
        }
    }

    /// The most buckets one chunk holds: one span's worth.
    pub fn max_buckets(self) -> usize {
        (self.tier().span_secs() / self.bucket_secs()) as usize
    }

    pub fn tier(self) -> Tier {
        match self {
            RollupTier::Minute => Tier::Minute,
            RollupTier::Hour => Tier::Hour,
        }
    }
}

impl SpanStart {
    pub fn get(self) -> u64 {
        self.0
    }

    /// The span starting at `start`, if it is a multiple of the tier's span length.
    pub fn new(tier: Tier, start: u64) -> Option<SpanStart> {
        start
            .is_multiple_of(tier.span_secs())
            .then_some(SpanStart(start))
    }

    /// The first second after the span.
    pub fn end(self, tier: Tier) -> u64 {
        self.0.saturating_add(tier.span_secs())
    }

    /// Whether the span is closed at `hub_now`: hub time past its end plus its tier's grace
    /// (§6 *Span handoff*), so no point or bucket can land in it any more.
    pub fn is_closed(self, tier: Tier, hub_now: u64) -> bool {
        hub_now > self.end(tier).saturating_add(tier.grace_secs())
    }
}

/// The sweep's interval: it closes quiet series' buckets (RFC 0010 §5).
pub const SWEEP_SECS: u64 = 60;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_are_an_hour_for_raw_and_minute_and_a_day_for_hour() {
        let cases = [
            (Tier::Raw, 3_600),
            (Tier::Minute, 3_600),
            (Tier::Hour, 86_400),
        ];
        for (tier, secs) in cases {
            assert_eq!(tier.span_secs(), secs, "{tier:?}");
        }
    }

    #[test]
    fn a_time_belongs_to_the_span_that_starts_at_or_before_it() {
        let cases = [
            ("raw at a start", Tier::Raw, 7_200, 7_200),
            ("raw one second before the next", Tier::Raw, 10_799, 7_200),
            ("raw at the next", Tier::Raw, 10_800, 10_800),
            ("minute", Tier::Minute, 3_601, 3_600),
            ("hour at the epoch", Tier::Hour, 0, 0),
            ("hour one second before a day", Tier::Hour, 86_399, 0),
            ("hour at a day", Tier::Hour, 86_400, 86_400),
        ];
        for (name, tier, ts, start) in cases {
            let span = tier.span_of(ts);
            assert_eq!(span.get(), start, "{name}");
            assert_eq!(span.end(tier), start + tier.span_secs(), "{name}: end");
        }
    }

    #[test]
    fn a_span_start_must_be_a_multiple_of_its_span_length() {
        let cases = [
            (Tier::Raw, 3_600, true),
            (Tier::Raw, 3_601, false),
            (Tier::Hour, 3_600, false),
            (Tier::Hour, 172_800, true),
            (Tier::Minute, 0, true),
        ];
        for (tier, start, valid) in cases {
            let span = SpanStart::new(tier, start);
            assert_eq!(span.is_some(), valid, "{tier:?} {start}");
            assert_eq!(span.map(SpanStart::get), valid.then_some(start));
        }
    }

    #[test]
    fn a_span_closes_one_bucket_and_one_sweep_after_its_end() {
        let cases = [(Tier::Raw, 120), (Tier::Minute, 120), (Tier::Hour, 3_660)];
        for (tier, grace) in cases {
            assert_eq!(tier.grace_secs(), grace, "{tier:?}");
        }
    }

    #[test]
    fn rollup_tiers_hold_one_span_of_buckets_per_chunk() {
        let cases = [
            (RollupTier::Minute, 60, 60, Tier::Minute),
            (RollupTier::Hour, 3_600, 24, Tier::Hour),
        ];
        for (rollup, secs, max, tier) in cases {
            assert_eq!(rollup.bucket_secs(), secs, "{rollup:?}");
            assert_eq!(rollup.max_buckets(), max, "{rollup:?}");
            assert_eq!(rollup.tier(), tier);
            assert_eq!(tier.rollup(), Some(rollup));
            assert_eq!(
                rollup.bucket_secs() * max as u64,
                tier.span_secs(),
                "{rollup:?}: one span"
            );
        }
        assert_eq!(Tier::Raw.rollup(), None);
    }

    #[test]
    fn each_tier_bounds_its_retention_from_above() {
        const DAY: u64 = 86_400;
        let cases = [
            (Tier::Raw, 30 * DAY),
            (Tier::Minute, 400 * DAY),
            (Tier::Hour, 3_650 * DAY),
        ];
        for (tier, max) in cases {
            assert_eq!(tier.max_retention_secs(), max, "{tier:?}");
        }
    }

    #[test]
    fn each_tier_bounds_its_retention_from_below() {
        const DAY: u64 = 86_400;
        let cases = [
            (Tier::Raw, 3_600),
            (Tier::Minute, DAY),
            (Tier::Hour, 7 * DAY),
        ];
        for (tier, min) in cases {
            assert_eq!(tier.min_retention_secs(), min, "{tier:?}");
        }
    }

    #[test]
    fn tiers_round_trip_their_code_and_have_their_api_names() {
        let names = [
            (Tier::Raw, "raw"),
            (Tier::Minute, "minute"),
            (Tier::Hour, "hour"),
        ];
        for (tier, name) in names {
            assert_eq!(Tier::from_code(tier.code()), Some(tier));
            assert_eq!(tier.name(), name);
        }
        let named = (0..=u8::MAX)
            .filter(|c| Tier::from_code(*c).is_some())
            .count();
        assert_eq!(named, 3);
    }

    #[test]
    fn a_span_closes_one_second_after_its_end_plus_its_tiers_grace() {
        let day = 1_800_057_600;
        let cases = [
            ("raw, at the grace", Tier::Raw, day + 3_600 + 120, false),
            ("raw, one second past", Tier::Raw, day + 3_600 + 121, true),
            (
                "minute, at the grace",
                Tier::Minute,
                day + 3_600 + 120,
                false,
            ),
            (
                "minute, one second past",
                Tier::Minute,
                day + 3_600 + 121,
                true,
            ),
            (
                "hour, at the grace",
                Tier::Hour,
                day + 86_400 + 3_660,
                false,
            ),
            (
                "hour, one second past",
                Tier::Hour,
                day + 86_400 + 3_661,
                true,
            ),
            ("inside the span", Tier::Raw, day + 10, false),
            ("before the span", Tier::Raw, 0, false),
        ];
        for (name, tier, now, closed) in cases {
            let span = SpanStart::new(tier, day).expect("grid");
            assert_eq!(span.is_closed(tier, now), closed, "{name}");
        }
        let next = SpanStart::new(Tier::Raw, day + 3_600).expect("grid");
        assert!(
            !next.is_closed(Tier::Raw, day + 7_200 + 120),
            "the next span, at its grace"
        );
        assert!(
            next.is_closed(Tier::Raw, day + 7_200 + 121),
            "the next span, one second past"
        );
        let early = SpanStart::new(Tier::Hour, 86_400).expect("grid");
        assert!(
            early.is_closed(Tier::Hour, 2 * 86_400 + 3_661),
            "an early hour span"
        );
        assert!(!early.is_closed(Tier::Hour, 2 * 86_400 + 3_660));
        let last = SpanStart::new(Tier::Raw, u64::MAX - u64::MAX % 3_600).expect("grid");
        assert!(
            !last.is_closed(Tier::Raw, u64::MAX),
            "the last span never closes"
        );
    }
}
