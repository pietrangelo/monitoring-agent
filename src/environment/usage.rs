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

/// Where steal sits among `/proc/stat`'s `cpu` columns: user, nice, system, idle, iowait,
/// irq, softirq, steal.
const STEAL_COLUMN: usize = 7;

/// The host kernel's cumulative CPU time (`/proc/stat`'s `cpu` line, in jiffies): how much of
/// it the hypervisor stole, out of all of it. Built only from that line, so `steal` is always
/// part of `total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StealCounters {
    steal: u64,
    total: u64,
}

impl StealCounters {
    /// The counters on `/proc/stat`'s aggregate `cpu` line. `None` when there is no such
    /// line, it has no steal column (kernels before 2.6.11), a column isn't a number, or the
    /// total overflows. `guest` and `guest_nice` are left out: the kernel already counts them
    /// in `user` and `nice`.
    pub fn from_proc_stat(content: &str) -> Option<Self> {
        let columns = content.lines().find_map(|line| line.strip_prefix("cpu "))?;
        let user_to_steal = columns
            .split_whitespace()
            .take(STEAL_COLUMN + 1)
            .map(|column| column.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()?;
        let steal = *user_to_steal.get(STEAL_COLUMN)?;
        let total = user_to_steal
            .iter()
            .try_fold(0u64, |sum, &jiffies| sum.checked_add(jiffies))?;
        Some(Self { steal, total })
    }
}

/// The share of CPU time the hypervisor withheld between two readings (RFC 0014 §5): Δsteal
/// over Δtotal. `None` when no time passed or a counter went backwards. On bare metal the
/// kernel counts no steal, so it is a real 0.
pub fn steal_share(prev: &StealCounters, cur: &StealCounters) -> Option<Percent> {
    let stolen = cur.steal.checked_sub(prev.steal)?;
    let elapsed = cur.total.checked_sub(prev.total).filter(|&t| t > 0)?;
    Percent::saturating((stolen as f64 * 100.0 / elapsed as f64) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `/proc/stat` whose `cpu` line reads user, nice, system, idle, iowait, irq, softirq
    /// and steal as given, then guest and guest_nice.
    fn proc_stat(user_to_steal: [u64; 8]) -> String {
        let columns = user_to_steal.map(|n| n.to_string()).join(" ");
        format!("cpu  {columns} 40 50\ncpu0 1 2 3 4 5 6 7 8 0 0\nintr 12345\nctxt 99\n")
    }

    #[test]
    fn steal_counters_are_read_from_the_aggregate_cpu_line() {
        let counters = |steal, total| Some(StealCounters { steal, total });
        // (name, content, expected)
        let cases = [
            (
                "every column, guest left out of the total",
                proc_stat([100, 20, 30, 800, 10, 5, 5, 30]),
                counters(30, 1000),
            ),
            (
                "bare metal: no steal",
                proc_stat([100, 0, 50, 850, 0, 0, 0, 0]),
                counters(0, 1000),
            ),
            (
                "a kernel with no guest columns",
                "cpu  1 2 3 4 5 6 7 8\n".to_owned(),
                counters(8, 36),
            ),
            (
                "a kernel with no steal column",
                "cpu  1 2 3 4 5 6 7\ncpu0 1 2 3 4 5 6 7\n".to_owned(),
                None,
            ),
            (
                "only per-cpu lines",
                "cpu0 1 2 3 4 5 6 7 8 0 0\n".to_owned(),
                None,
            ),
            (
                "a cpu0 line first is not the aggregate",
                "cpu0 9 9 9 9 9 9 9 9 0 0\ncpu  1 2 3 4 5 6 7 8 0 0\n".to_owned(),
                counters(8, 36),
            ),
            (
                "a column that isn't a number",
                "cpu  1 2 3 x 5 6 7 8 0 0\n".to_owned(),
                None,
            ),
            (
                "a negative column",
                "cpu  1 2 3 -4 5 6 7 8 0 0\n".to_owned(),
                None,
            ),
            (
                "a total past u64",
                format!("cpu  {} 1 0 0 0 0 0 0 0 0\n", u64::MAX),
                None,
            ),
            ("empty", String::new(), None),
        ];
        for (name, content, expected) in cases {
            assert_eq!(StealCounters::from_proc_stat(&content), expected, "{name}");
        }
    }

    #[test]
    fn steal_share_is_stolen_time_over_all_cpu_time() {
        let at = |steal, total| StealCounters { steal, total };
        // (name, prev, cur, expected)
        let cases = [
            ("a starved vm", at(100, 1_000), at(400, 2_000), Some(30.0)),
            ("bare metal", at(0, 1_000), at(0, 3_000), Some(0.0)),
            ("all of it stolen", at(0, 0), at(500, 500), Some(100.0)),
            ("a sliver", at(0, 0), at(1, 1_000), Some(0.1)),
            ("no time passed", at(50, 1_000), at(50, 1_000), None),
            (
                "the total went backwards",
                at(0, 2_000),
                at(10, 1_000),
                None,
            ),
            (
                "the steal went backwards",
                at(90, 1_000),
                at(10, 2_000),
                None,
            ),
        ];
        for (name, prev, cur, expected) in cases {
            assert_eq!(
                steal_share(&prev, &cur).map(Percent::get),
                expected,
                "{name}"
            );
        }
    }

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
