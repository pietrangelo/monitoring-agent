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

//! Usage values and the rules that derive them.

use std::time::{Duration, Instant};

use super::cgroup::CpuCount;

/// A share of a resource capacity, from 0 to 100 inclusive.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Percent(f32);

impl Percent {
    /// `value` held to 0–100, or `None` for NaN, which has no place on the scale.
    pub fn saturating(value: f32) -> Option<Self> {
        // `+ 0.0` turns -0.0 into 0.0, which `clamp` keeps.
        (!value.is_nan()).then(|| Self(value.clamp(0.0, 100.0) + 0.0))
    }

    pub fn get(self) -> f32 {
        self.0
    }
}

/// A load average: runnable tasks averaged over a window. Not a percent, and not bounded
/// above: 150 is a valid load on a 128-core host.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoadAverage(f32);

impl LoadAverage {
    pub fn new(value: f32) -> Self {
        Self(value)
    }

    pub fn get(self) -> f32 {
        self.0
    }
}

/// The least wall time a CPU usage is measured over: a shorter interval says more about
/// scheduling jitter than about the workload.
pub const MIN_USAGE_INTERVAL: Duration = Duration::from_millis(500);

/// A cgroup's cumulative CPU time (`cpu.stat`'s `usage_usec`), and when it was read on the
/// monotonic clock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuCounters {
    pub usage: Duration,
    pub read_at: Instant,
}

/// The share of `capacity` the cgroup used between two readings (RFC 0014 §5), held to 0–100:
/// a `cpu.max.burst` can briefly run over the quota. `None` when the readings are under
/// `MIN_USAGE_INTERVAL` apart, or a counter went backwards (a cgroup recreated).
pub fn cpu_usage(prev: &CpuCounters, cur: &CpuCounters, capacity: CpuCount) -> Option<Percent> {
    let wall = cur.read_at.checked_duration_since(prev.read_at)?;
    let used = cur.usage.checked_sub(prev.usage)?;
    if wall < MIN_USAGE_INTERVAL {
        return None;
    }
    let share = used.as_secs_f64() / (wall.as_secs_f64() * capacity.get());
    Percent::saturating((share * 100.0) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_usage_is_cpu_time_over_wall_time_times_capacity() {
        let start = Instant::now();
        let at = |ms: u64, usage_ms: u64| CpuCounters {
            usage: Duration::from_millis(usage_ms),
            read_at: start + Duration::from_millis(ms),
        };
        let cap = |n: f64| CpuCount::new(n).expect("positive");
        // (name, prev, cur, capacity, expected)
        let cases = [
            (
                "half of one cpu",
                at(0, 0),
                at(2_000, 1_000),
                cap(1.0),
                Some(50.0),
            ),
            (
                "a quarter of a fractional capacity",
                at(0, 1_000),
                at(2_000, 1_750),
                cap(1.5),
                Some(25.0),
            ),
            (
                "exactly all of it",
                at(0, 0),
                at(2_000, 3_000),
                cap(1.5),
                Some(100.0),
            ),
            (
                "a burst over the quota is clamped",
                at(0, 0),
                at(2_000, 5_000),
                cap(1.5),
                Some(100.0),
            ),
            ("idle", at(0, 700), at(2_000, 700), cap(4.0), Some(0.0)),
            (
                "exactly the least interval",
                at(0, 0),
                at(500, 250),
                cap(1.0),
                Some(50.0),
            ),
            (
                "just under the least interval",
                at(0, 0),
                at(499, 250),
                cap(1.0),
                None,
            ),
            ("no time passed", at(1_000, 0), at(1_000, 0), cap(1.0), None),
            (
                "the clock order reversed",
                at(2_000, 0),
                at(0, 500),
                cap(1.0),
                None,
            ),
            (
                "the counter went backwards",
                at(0, 9_000),
                at(2_000, 100),
                cap(1.0),
                None,
            ),
        ];
        for (name, prev, cur, capacity, expected) in cases {
            assert_eq!(
                cpu_usage(&prev, &cur, capacity).map(Percent::get),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn a_percent_is_held_to_zero_through_one_hundred() {
        // (name, value, expected)
        let cases = [
            ("zero", 0.0, Some(0.0)),
            ("negative zero becomes zero", -0.0, Some(0.0)),
            (
                "smallest positive",
                f32::MIN_POSITIVE,
                Some(f32::MIN_POSITIVE),
            ),
            ("a tiny share", 1e-7, Some(1e-7)),
            ("just above zero", 0.1, Some(0.1)),
            ("full precision", 12.345_678, Some(12.345_678)),
            ("inside the scale", 33.3, Some(33.3)),
            ("just under one hundred", 99.9, Some(99.9)),
            ("exactly one hundred", 100.0, Some(100.0)),
            ("just over one hundred", 100.3, Some(100.0)),
            ("far over one hundred", 1e9, Some(100.0)),
            ("positive infinity", f32::INFINITY, Some(100.0)),
            ("just under zero", -0.1, Some(0.0)),
            ("negative infinity", f32::NEG_INFINITY, Some(0.0)),
            ("not a number", f32::NAN, None),
        ];
        for (name, value, expected) in cases {
            // Compared bit for bit, so `-0.0` can't pass for `0.0`.
            assert_eq!(
                Percent::saturating(value).map(|p| p.get().to_bits()),
                expected.map(f32::to_bits),
                "{name}"
            );
        }
    }
}
