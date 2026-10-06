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

//! Rollup accumulators (RFC 0010 §5): the open bucket of a series in a rollup tier, summed in
//! `i128` so no domain can overflow it.

use crate::codec::Bucket;
use crate::tier::RollupTier;

/// The points of one open bucket so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Accumulator {
    index: u64,
    sum: i128,
    count: u32,
    min: i64,
    max: i64,
}

impl Accumulator {
    /// A bucket opened by its first point, at hub time `ts`.
    pub fn open(tier: RollupTier, ts: u64, value: i64) -> Accumulator {
        Accumulator {
            index: ts / tier.bucket_secs(),
            sum: i128::from(value),
            count: 1,
            min: value,
            max: value,
        }
    }

    /// The bucket's index: its start over the bucket length.
    pub fn index(&self) -> u64 {
        self.index
    }

    /// Whether a point at `ts` belongs to this bucket.
    pub fn holds(&self, tier: RollupTier, ts: u64) -> bool {
        ts / tier.bucket_secs() == self.index
    }

    /// Adds a point of this bucket. The store adds at most one point per second per series, so
    /// a bucket never nears `u32::MAX` points; the count saturates rather than wrapping, so
    /// `close` can never divide by zero.
    pub fn add(&mut self, value: i64) {
        self.sum += i128::from(value);
        self.count = self.count.saturating_add(1);
        self.min = self.min.min(value);
        self.max = self.max.max(value);
    }

    /// The closed bucket: the average rounded half away from zero, the minimum, the maximum
    /// and the count.
    pub fn close(&self) -> Bucket {
        let count = i128::from(self.count);
        let half = count / 2 + count % 2; // ⌈count / 2⌉: exact halves round away from zero
        let rounded = if self.sum >= 0 {
            (self.sum + count - half) / count
        } else {
            (self.sum - count + half) / count
        };
        // The average of values in [min, max] stays there, so the clamp only states it.
        let avg = rounded.clamp(i128::from(self.min), i128::from(self.max)) as i64;
        Bucket {
            index: self.index,
            avg,
            min: self.min,
            max: self.max,
            count: self.count,
        }
    }

    /// The accumulator as persisted in a tail: index, sum, count, minimum, maximum. Crate
    /// code reads and writes it through [`AccumulatorRow`]'s named fields.
    pub(crate) fn parts(&self) -> (u64, i128, u32, i64, i64) {
        (self.index, self.sum, self.count, self.min, self.max)
    }

    /// An accumulator read back from a tail, if its parts are consistent.
    pub(crate) fn from_parts(
        index: u64,
        sum: i128,
        count: u32,
        min: i64,
        max: i64,
    ) -> Option<Accumulator> {
        let n = i128::from(count);
        let consistent =
            count > 0 && min <= max && n * i128::from(min) <= sum && sum <= n * i128::from(max);
        consistent.then_some(Accumulator {
            index,
            sum,
            count,
            min,
            max,
        })
    }
}

/// An open bucket as a tail persists it: every field named, so none can be swapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccumulatorRow {
    pub index: u64,
    pub sum: i128,
    pub count: u32,
    pub min: i64,
    pub max: i64,
}

/// A row whose sum, count, minimum and maximum can't belong to one bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InconsistentBucket;

impl From<Accumulator> for AccumulatorRow {
    fn from(acc: Accumulator) -> AccumulatorRow {
        let (index, sum, count, min, max) = acc.parts();
        AccumulatorRow {
            index,
            sum,
            count,
            min,
            max,
        }
    }
}

impl TryFrom<AccumulatorRow> for Accumulator {
    type Error = InconsistentBucket;

    fn try_from(row: AccumulatorRow) -> Result<Accumulator, InconsistentBucket> {
        let AccumulatorRow {
            index,
            sum,
            count,
            min,
            max,
        } = row;
        Accumulator::from_parts(index, sum, count, min, max).ok_or(InconsistentBucket)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket_of(tier: RollupTier, points: &[(u64, i64)]) -> Bucket {
        let (ts, value) = points[0];
        let mut acc = Accumulator::open(tier, ts, value);
        for &(ts, value) in &points[1..] {
            assert!(acc.holds(tier, ts), "{ts} is in the bucket");
            acc.add(value);
        }
        acc.close()
    }

    #[test]
    fn a_bucket_holds_the_average_minimum_maximum_and_count_of_its_points() {
        let cases = [
            (
                "one point",
                vec![(120, 7)],
                Bucket {
                    index: 2,
                    avg: 7,
                    min: 7,
                    max: 7,
                    count: 1,
                },
            ),
            (
                "three",
                vec![(60, 1), (70, 5), (119, 3)],
                Bucket {
                    index: 1,
                    avg: 3,
                    min: 1,
                    max: 5,
                    count: 3,
                },
            ),
            (
                "half rounds up",
                vec![(0, 1), (1, 2)],
                Bucket {
                    index: 0,
                    avg: 2,
                    min: 1,
                    max: 2,
                    count: 2,
                },
            ),
            (
                "below half rounds down",
                vec![(0, 1), (1, 1), (2, 2)],
                Bucket {
                    index: 0,
                    avg: 1,
                    min: 1,
                    max: 2,
                    count: 3,
                },
            ),
            (
                "above half rounds up",
                vec![(0, 1), (1, 2), (2, 2)],
                Bucket {
                    index: 0,
                    avg: 2,
                    min: 1,
                    max: 2,
                    count: 3,
                },
            ),
            (
                "negative half rounds away",
                vec![(0, -1), (1, -2)],
                Bucket {
                    index: 0,
                    avg: -2,
                    min: -2,
                    max: -1,
                    count: 2,
                },
            ),
        ];
        for (name, points, expected) in cases {
            assert_eq!(bucket_of(RollupTier::Minute, &points), expected, "{name}");
        }
    }

    #[test]
    fn an_hour_bucket_is_indexed_by_the_hour() {
        let bucket = bucket_of(RollupTier::Hour, &[(7_200, 10), (10_799, 20)]);
        assert_eq!((bucket.index, bucket.avg, bucket.count), (2, 15, 2));
    }

    #[test]
    fn a_bucket_holds_exactly_the_points_of_its_interval() {
        let acc = Accumulator::open(RollupTier::Minute, 125, 0);
        let cases = [(119, false), (120, true), (179, true), (180, false)];
        for (ts, held) in cases {
            assert_eq!(acc.holds(RollupTier::Minute, ts), held, "{ts}");
        }
        let hour = Accumulator::open(RollupTier::Hour, 3_600, 0);
        assert!(hour.holds(RollupTier::Hour, 7_199));
        assert!(!hour.holds(RollupTier::Hour, 7_200));
    }

    #[test]
    fn sums_at_the_domains_top_never_overflow() {
        let top = 1i64 << 60;
        let mut acc = Accumulator::open(RollupTier::Hour, 0, top);
        for _ in 1..3_600 * 8 {
            acc.add(top);
        }
        assert_eq!(
            acc.close(),
            Bucket {
                index: 0,
                avg: top,
                min: top,
                max: top,
                count: 3_600 * 8
            }
        );
        let mut acc = Accumulator::open(RollupTier::Hour, 0, i64::MAX);
        acc.add(i64::MAX);
        acc.add(i64::MAX - 1);
        assert_eq!(acc.close().avg, i64::MAX);
    }

    #[test]
    fn an_accumulator_round_trips_its_parts_and_refuses_inconsistent_ones() {
        let mut acc = Accumulator::open(RollupTier::Minute, 61, 4);
        acc.add(9);
        let (index, sum, count, min, max) = acc.parts();
        assert_eq!((index, sum, count, min, max), (1, 13, 2, 4, 9));
        assert_eq!(
            Accumulator::from_parts(index, sum, count, min, max),
            Some(acc)
        );
        let cases = [
            ("no point", (1, 0, 0, 0, 0)),
            ("min above max", (1, 13, 2, 9, 4)),
            ("sum below count × min", (1, 7, 2, 4, 9)),
            ("sum above count × max", (1, 19, 2, 4, 9)),
        ];
        for (name, (index, sum, count, min, max)) in cases {
            assert_eq!(
                Accumulator::from_parts(index, sum, count, min, max),
                None,
                "{name}"
            );
        }
    }
}
