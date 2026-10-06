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

//! Value kinds: each metric kind's declared resolution, domain range and encoding
//! (RFC 0010 §3). A value is stored as `round(v / scale)`, exact down to the scale.

/// How a metric's values are stored. A series' kind is fixed at its first point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ValueKind {
    /// 0.01 %, from 0 to 1,000 %.
    Percent,
    /// 0.01, from 0 to 100,000.
    Load,
    /// 1, from 0 to 2⁴⁰.
    Count,
    /// 1, from 0 to 2⁴⁰, encoded as a delta of deltas (a counter such as an uptime).
    Monotonic,
    /// 1 KiB, from 0 to 2⁶⁰ bytes.
    Bytes,
    /// 0.001 per second, from 0 to 10⁹ per second.
    Rate,
    /// 0.01 ms, from 0 to 10⁹ ms.
    Millis,
}

/// How consecutive values of a chunk are coded (RFC 0010 §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    /// The difference from the previous value.
    Delta,
    /// The difference between consecutive differences.
    DeltaOfDelta,
}

/// A value in its kind's stored unit, inside the kind's domain: unconstructible otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Scaled(i64);

/// A value outside its kind's domain range, or not finite (RFC 0010 §3: refused as out of
/// domain at the hub edge and in the store).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutOfDomain;

impl std::fmt::Display for OutOfDomain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("value outside its kind's domain range, or not finite")
    }
}

impl std::error::Error for OutOfDomain {}

/// How a kind maps a natural value to its stored unit: multiplied, or divided, by a power
/// that is exact in binary or decimal, so the scale itself adds no rounding error beyond f64's.
#[derive(Clone, Copy)]
enum Factor {
    Times(f64),
    Over(f64),
}

impl ValueKind {
    /// Every kind, in the order of its persisted code.
    pub const ALL: [ValueKind; 7] = [
        ValueKind::Percent,
        ValueKind::Load,
        ValueKind::Count,
        ValueKind::Monotonic,
        ValueKind::Bytes,
        ValueKind::Rate,
        ValueKind::Millis,
    ];

    /// The natural value one stored unit stands for.
    pub fn scale(self) -> f64 {
        match self.factor() {
            Factor::Times(f) => 1.0 / f,
            Factor::Over(d) => d,
        }
    }

    fn factor(self) -> Factor {
        match self {
            ValueKind::Percent | ValueKind::Load | ValueKind::Millis => Factor::Times(100.0),
            ValueKind::Count | ValueKind::Monotonic => Factor::Times(1.0),
            ValueKind::Bytes => Factor::Over(1024.0),
            ValueKind::Rate => Factor::Times(1000.0),
        }
    }

    /// The top of the kind's domain range, in its natural unit; the bottom is 0.
    pub fn max_natural(self) -> f64 {
        match self {
            ValueKind::Percent => 1_000.0,
            ValueKind::Load => 100_000.0,
            ValueKind::Count | ValueKind::Monotonic => (1u64 << 40) as f64,
            ValueKind::Bytes => (1u64 << 60) as f64,
            ValueKind::Rate | ValueKind::Millis => 1e9,
        }
    }

    /// The top of the kind's domain range, in its stored unit.
    pub fn max_scaled(self) -> i64 {
        self.to_stored(self.max_natural()) as i64
    }

    /// How the kind's consecutive values are coded.
    pub fn encoding(self) -> Encoding {
        match self {
            ValueKind::Monotonic => Encoding::DeltaOfDelta,
            ValueKind::Percent
            | ValueKind::Load
            | ValueKind::Count
            | ValueKind::Bytes
            | ValueKind::Rate
            | ValueKind::Millis => Encoding::Delta,
        }
    }

    /// The stored form of a natural value: `round(v / scale)`, refused outside the domain.
    pub fn scale_value(self, natural: f64) -> Result<Scaled, OutOfDomain> {
        // `contains` is false for NaN, and the bounds are finite, so this also refuses ±∞.
        if !(0.0..=self.max_natural()).contains(&natural) {
            return Err(OutOfDomain);
        }
        Ok(Scaled(self.to_stored(natural) as i64))
    }

    /// A stored value read back (a chunk, a tail): refused outside the domain.
    pub fn check_scaled(self, stored: i64) -> Result<Scaled, OutOfDomain> {
        if (0..=self.max_scaled()).contains(&stored) {
            Ok(Scaled(stored))
        } else {
            Err(OutOfDomain)
        }
    }

    /// The natural value of a stored one.
    pub fn natural(self, value: Scaled) -> f64 {
        match self.factor() {
            Factor::Times(f) => value.0 as f64 / f,
            Factor::Over(d) => value.0 as f64 * d,
        }
    }

    fn to_stored(self, natural: f64) -> f64 {
        match self.factor() {
            Factor::Times(f) => (natural * f).round(),
            Factor::Over(d) => (natural / d).round(),
        }
    }

    /// The kind's persisted code.
    pub fn code(self) -> u8 {
        match self {
            ValueKind::Percent => 1,
            ValueKind::Load => 2,
            ValueKind::Count => 3,
            ValueKind::Monotonic => 4,
            ValueKind::Bytes => 5,
            ValueKind::Rate => 6,
            ValueKind::Millis => 7,
        }
    }

    /// The kind a persisted code names, if any.
    pub fn from_code(code: u8) -> Option<ValueKind> {
        ValueKind::ALL.into_iter().find(|kind| kind.code() == code)
    }
}

impl Scaled {
    /// The stored integer.
    pub fn get(self) -> i64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_stores_one_unit_of_its_scale_as_one() {
        let cases = [
            ("percent", ValueKind::Percent, 0.01, 1),
            ("load", ValueKind::Load, 0.01, 1),
            ("count", ValueKind::Count, 1.0, 1),
            ("monotonic", ValueKind::Monotonic, 1.0, 1),
            ("bytes", ValueKind::Bytes, 1024.0, 1),
            ("rate", ValueKind::Rate, 0.001, 1),
            ("millis", ValueKind::Millis, 0.01, 1),
        ];
        for (name, kind, natural, stored) in cases {
            assert_eq!(kind.scale(), natural, "{name}: scale");
            assert_eq!(
                kind.scale_value(natural).map(Scaled::get),
                Ok(stored),
                "{name}"
            );
        }
    }

    #[test]
    fn values_round_to_the_nearest_unit_of_their_scale() {
        let cases = [
            ("percent below half", ValueKind::Percent, 12.344, 1234),
            ("percent above half", ValueKind::Percent, 12.346, 1235),
            ("percent exact", ValueKind::Percent, 99.5, 9950),
            (
                "bytes below half a KiB",
                ValueKind::Bytes,
                1024.0 + 511.0,
                1,
            ),
            (
                "bytes above half a KiB",
                ValueKind::Bytes,
                1024.0 + 513.0,
                2,
            ),
            ("rate", ValueKind::Rate, 2.0004, 2000),
            ("rate up", ValueKind::Rate, 2.0006, 2001),
            ("count", ValueKind::Count, 7.6, 8),
            ("millis", ValueKind::Millis, 3.139, 314),
            ("load", ValueKind::Load, 0.004, 0),
            ("zero", ValueKind::Monotonic, 0.0, 0),
        ];
        for (name, kind, natural, stored) in cases {
            assert_eq!(
                kind.scale_value(natural).map(Scaled::get),
                Ok(stored),
                "{name}"
            );
        }
    }

    #[test]
    fn the_domain_edges_are_kept_and_one_step_past_them_is_refused() {
        let cases = [
            ("percent top", ValueKind::Percent, 1_000.0, Ok(100_000)),
            (
                "percent past top",
                ValueKind::Percent,
                1_000.01,
                Err(OutOfDomain),
            ),
            ("load top", ValueKind::Load, 100_000.0, Ok(10_000_000)),
            (
                "load past top",
                ValueKind::Load,
                100_000.01,
                Err(OutOfDomain),
            ),
            (
                "count top",
                ValueKind::Count,
                (1u64 << 40) as f64,
                Ok(1 << 40),
            ),
            (
                "count past top",
                ValueKind::Count,
                ((1u64 << 40) + 1) as f64,
                Err(OutOfDomain),
            ),
            (
                "monotonic top",
                ValueKind::Monotonic,
                (1u64 << 40) as f64,
                Ok(1 << 40),
            ),
            (
                "bytes top",
                ValueKind::Bytes,
                (1u64 << 60) as f64,
                Ok(1 << 50),
            ),
            (
                "bytes past top",
                ValueKind::Bytes,
                ((1u64 << 60) + 1024) as f64,
                Err(OutOfDomain),
            ),
            ("rate top", ValueKind::Rate, 1e9, Ok(1_000_000_000_000)),
            (
                "rate past top",
                ValueKind::Rate,
                1e9 + 0.001,
                Err(OutOfDomain),
            ),
            ("millis top", ValueKind::Millis, 1e9, Ok(100_000_000_000)),
            (
                "millis past top",
                ValueKind::Millis,
                1e9 + 0.01,
                Err(OutOfDomain),
            ),
            ("bottom", ValueKind::Percent, 0.0, Ok(0)),
            ("negative zero", ValueKind::Percent, -0.0, Ok(0)),
            ("below bottom", ValueKind::Percent, -0.01, Err(OutOfDomain)),
            ("tiny negative", ValueKind::Count, -1e-9, Err(OutOfDomain)),
        ];
        for (name, kind, natural, expected) in cases {
            assert_eq!(
                kind.scale_value(natural).map(Scaled::get),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn values_that_are_not_finite_are_refused_by_every_kind() {
        for kind in ValueKind::ALL {
            for natural in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                assert_eq!(
                    kind.scale_value(natural),
                    Err(OutOfDomain),
                    "{kind:?} {natural}"
                );
            }
        }
    }

    #[test]
    fn the_stored_top_of_each_kind_is_its_natural_top_over_its_scale() {
        let cases = [
            (ValueKind::Percent, 100_000),
            (ValueKind::Load, 10_000_000),
            (ValueKind::Count, 1 << 40),
            (ValueKind::Monotonic, 1 << 40),
            (ValueKind::Bytes, 1 << 50),
            (ValueKind::Rate, 1_000_000_000_000),
            (ValueKind::Millis, 100_000_000_000),
        ];
        for (kind, top) in cases {
            assert_eq!(kind.max_scaled(), top, "{kind:?}");
        }
    }

    #[test]
    fn a_stored_value_read_back_is_checked_against_the_domain() {
        for kind in ValueKind::ALL {
            let top = kind.max_scaled();
            assert_eq!(
                kind.check_scaled(0).map(Scaled::get),
                Ok(0),
                "{kind:?} bottom"
            );
            assert_eq!(
                kind.check_scaled(top).map(Scaled::get),
                Ok(top),
                "{kind:?} top"
            );
            assert_eq!(
                kind.check_scaled(top + 1),
                Err(OutOfDomain),
                "{kind:?} past top"
            );
            assert_eq!(kind.check_scaled(-1), Err(OutOfDomain), "{kind:?} below");
            assert_eq!(
                kind.check_scaled(i64::MIN),
                Err(OutOfDomain),
                "{kind:?} min"
            );
        }
    }

    #[test]
    fn a_stored_value_reads_back_in_its_natural_unit() {
        let cases = [
            (ValueKind::Percent, 1234, 12.34),
            (ValueKind::Bytes, 3, 3072.0),
            (ValueKind::Rate, 2001, 2.001),
            (ValueKind::Count, 42, 42.0),
            (ValueKind::Millis, 315, 3.15),
            (ValueKind::Load, 150, 1.5),
            (ValueKind::Monotonic, 9, 9.0),
        ];
        for (kind, stored, natural) in cases {
            let value = kind.check_scaled(stored).expect("in domain");
            assert!((kind.natural(value) - natural).abs() < 1e-9, "{kind:?}");
        }
    }

    #[test]
    fn only_the_monotonic_kind_codes_a_delta_of_deltas() {
        for kind in ValueKind::ALL {
            let expected = match kind {
                ValueKind::Monotonic => Encoding::DeltaOfDelta,
                ValueKind::Percent
                | ValueKind::Load
                | ValueKind::Count
                | ValueKind::Bytes
                | ValueKind::Rate
                | ValueKind::Millis => Encoding::Delta,
            };
            assert_eq!(kind.encoding(), expected, "{kind:?}");
        }
    }

    #[test]
    fn every_kind_round_trips_its_persisted_code_and_no_other_code_names_one() {
        for kind in ValueKind::ALL {
            assert_eq!(ValueKind::from_code(kind.code()), Some(kind), "{kind:?}");
        }
        let codes: std::collections::BTreeSet<u8> =
            ValueKind::ALL.iter().map(|k| k.code()).collect();
        assert_eq!(codes.len(), ValueKind::ALL.len(), "codes are distinct");
        let named = (0..=u8::MAX)
            .filter(|c| ValueKind::from_code(*c).is_some())
            .count();
        assert_eq!(named, ValueKind::ALL.len());
    }
}
