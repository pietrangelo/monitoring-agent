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

//! Choosing each snapshot's readings (RFC 0014 §6): per reading group, from the monitored
//! cgroup or the kernel, measured, carried or unavailable, by the group's lineage.

use std::time::{Duration, Instant};

use super::cgroup::{
    Bytes, CgroupReadError, CpuCount, ResourceLimit, cpu_capacity, memory_capacity, tightest,
};
use super::usage::{CpuCounters, Percent, cpu_usage};

/// How long a cgroup group is carried past a failure before it is unavailable.
pub const CARRY_BOUND: Duration = Duration::from_secs(30);

/// Readings sourced and carried together, so their invariants hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadingGroup {
    Cpu,
    Memory,
    Swap,
}

/// The CPU group's values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuGroup {
    /// `None` when the kernel lists no CPU.
    pub usage: Option<Percent>,
    pub capacity: CpuCount,
}

/// The memory group's values. `used + available = total` always.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemoryGroup {
    pub total: Bytes,
    pub used: Bytes,
    pub free: Bytes,
    pub available: Bytes,
    /// The monitored cgroup's tightest memory limit; `None` when read from the kernel.
    pub limit: Option<ResourceLimit<Bytes>>,
}

/// The swap group's values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SwapGroup {
    pub total: Bytes,
    pub used: Bytes,
    pub free: Bytes,
    /// The monitored cgroup's tightest swap limit; `None` when read from the kernel.
    pub limit: Option<ResourceLimit<Bytes>>,
}

/// Where a group's values came from on a tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Measured from the monitored cgroup.
    Cgroup,
    /// Measured from the kernel's host-wide view.
    Kernel,
    /// The cgroup couldn't be read: the last cgroup values still stand.
    Carried,
    /// Carried past `CARRY_BOUND`: the values are the last ones, and nobody should trust them.
    Unavailable,
}

/// Where a reading came from, as the API names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadingSource {
    Cgroup,
    Kernel,
    Unavailable,
}

impl Origin {
    pub fn source(self) -> ReadingSource {
        match self {
            Self::Cgroup | Self::Carried => ReadingSource::Cgroup,
            Self::Kernel => ReadingSource::Kernel,
            Self::Unavailable => ReadingSource::Unavailable,
        }
    }
}

/// A group's values on a tick, and where they came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sourced<G> {
    pub value: G,
    pub origin: Origin,
}

/// One tick's attempt at a group's cgroup files.
#[derive(Debug, Clone, PartialEq)]
pub enum CgroupRead<T> {
    Read(T),
    /// The group's usage file isn't there: its controller isn't enabled.
    Absent,
    Failed(CgroupReadError),
}

/// The CPU group's cgroup files on one tick.
#[derive(Debug, Clone, PartialEq)]
pub struct CpuFiles {
    pub counters: CpuCounters,
    /// `cpu.max` at each level, deepest first.
    pub quotas: Vec<ResourceLimit<CpuCount>>,
    pub cpuset: ResourceLimit<CpuCount>,
}

/// The memory group's cgroup files on one tick.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryFiles {
    pub current: Bytes,
    pub inactive_file: Bytes,
    /// `memory.max` at each level, deepest first.
    pub limits: Vec<ResourceLimit<Bytes>>,
}

/// The swap group's cgroup files on one tick.
#[derive(Debug, Clone, PartialEq)]
pub struct SwapFiles {
    pub current: Bytes,
    /// `memory.swap.max` at each level, deepest first.
    pub limits: Vec<ResourceLimit<Bytes>>,
}

/// Everything read from the monitored cgroup on one tick.
#[derive(Debug, Clone, PartialEq)]
pub struct CgroupReadings {
    pub cpu: CgroupRead<CpuFiles>,
    pub memory: CgroupRead<MemoryFiles>,
    pub swap: CgroupRead<SwapFiles>,
}

/// The kernel's host-wide groups on one tick. They also give the host amounts every
/// capacity is bounded by.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KernelReadings {
    pub cpu: CpuGroup,
    pub memory: MemoryGroup,
    pub swap: SwapGroup,
}

/// Why a group's cgroup reading failed on a tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupFailure {
    File(CgroupReadError),
    /// The usage file went away after being read: a recreated cgroup, an un-delegated
    /// controller.
    Vanished,
    /// No CPU usage between this reading and the previous one (too close, or a counter went
    /// backwards).
    NoDelta,
}

/// What an operator should hear about a group, once per transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// The group's cgroup files failed before any cgroup reading: it reads the kernel.
    KernelFallback(GroupFailure),
    /// A cgroup group failed: its last values are carried.
    Carrying(GroupFailure),
    /// The carry outlasted `CARRY_BOUND`.
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcingWarning {
    pub group: ReadingGroup,
    pub warning: Warning,
}

/// A group's lineage and failure state, kept across ticks and sampler rebuilds.
#[derive(Debug, Clone, PartialEq)]
enum GroupHistory<G> {
    /// Lineage `Kernel`, its usage file never seen.
    Untouched,
    /// Lineage `Kernel`, its cgroup files failing since before any cgroup reading.
    KernelFailing,
    /// Lineage `Cgroup`: it has been read from the cgroup, and never goes back to the kernel.
    Cgroup { last: G, carry: Carry },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Carry {
    Reading,
    /// Failing since this instant.
    Since(Instant),
    Expired,
}

/// Every group's history: held by the collector, so a rebuilt sampler inherits it.
#[derive(Debug, Clone, PartialEq)]
pub struct SourcingHistory {
    cpu: GroupHistory<CpuGroup>,
    memory: GroupHistory<MemoryGroup>,
    swap: GroupHistory<SwapGroup>,
}

impl SourcingHistory {
    /// The history at the agent's start: every group's lineage is `Kernel`.
    pub fn new() -> Self {
        Self {
            cpu: GroupHistory::Untouched,
            memory: GroupHistory::Untouched,
            swap: GroupHistory::Untouched,
        }
    }
}

/// A tick's three groups.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourcedReadings {
    pub cpu: Sourced<CpuGroup>,
    pub memory: Sourced<MemoryGroup>,
    pub swap: Sourced<SwapGroup>,
}

impl SourcedReadings {
    /// Where each group came from.
    pub fn origins(&self) -> ReadingOrigins {
        ReadingOrigins {
            cpu: self.cpu.origin,
            memory: self.memory.origin,
            swap: self.swap.origin,
        }
    }
}

/// Where each of a snapshot's reading groups came from: what its alert readings are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadingOrigins {
    pub cpu: Origin,
    pub memory: Origin,
    pub swap: Origin,
}

/// What `choose_readings` decided on a tick.
#[derive(Debug, Clone, PartialEq)]
pub struct Sourcing {
    pub readings: SourcedReadings,
    pub history: SourcingHistory,
    pub warnings: Vec<SourcingWarning>,
}

/// This tick's groups (RFC 0014 §6): from `cgroup` where it was read and the group's lineage
/// allows, else the kernel, else carried. `cgroup` is `None` outside a container with a
/// readable cgroup v2 hierarchy. `prev_cpu` is the previous reading's CPU counters.
pub fn choose_readings(
    cgroup: Option<CgroupReadings>,
    kernel: KernelReadings,
    prev_cpu: Option<&CpuCounters>,
    history: &SourcingHistory,
    now: Instant,
) -> Sourcing {
    // No cgroup on a tick reads as every usage file absent: quiet for a group that never had
    // one, a failure for a group that did.
    let (cpu, memory, swap) = match cgroup {
        Some(read) => (read.cpu, read.memory, read.swap),
        None => (CgroupRead::Absent, CgroupRead::Absent, CgroupRead::Absent),
    };
    let cpu_read = attempt(cpu, |files| {
        cpu_group(&files, prev_cpu, kernel.cpu.capacity)
    });
    let memory_read = attempt(memory, |files| {
        Ok(memory_group(&files, kernel.memory.total))
    });
    let swap_read = attempt(swap, |files| Ok(swap_group(&files, kernel.swap.total)));
    let mut warnings = Vec::new();
    let mut warn = |group, warning: Option<Warning>| {
        warnings.extend(warning.map(|warning| SourcingWarning { group, warning }));
    };
    let (cpu, cpu_history, w) = source_group(&history.cpu, cpu_read, kernel.cpu, now);
    warn(ReadingGroup::Cpu, w);
    let (memory, memory_history, w) =
        source_group(&history.memory, memory_read, kernel.memory, now);
    warn(ReadingGroup::Memory, w);
    let (swap, swap_history, w) = source_group(&history.swap, swap_read, kernel.swap, now);
    warn(ReadingGroup::Swap, w);
    Sourcing {
        readings: SourcedReadings { cpu, memory, swap },
        history: SourcingHistory {
            cpu: cpu_history,
            memory: memory_history,
            swap: swap_history,
        },
        warnings,
    }
}

/// A group's cgroup attempt as `source_group` weighs it: its values, `None` for an absent
/// usage file, or why it failed.
fn attempt<T, G>(
    read: CgroupRead<T>,
    group: impl FnOnce(T) -> Result<G, GroupFailure>,
) -> Result<Option<G>, GroupFailure> {
    match read {
        CgroupRead::Read(files) => group(files).map(Some),
        CgroupRead::Absent => Ok(None),
        CgroupRead::Failed(err) => Err(GroupFailure::File(err)),
    }
}

/// The CPU group from its files: usage over capacity since the previous counters.
fn cpu_group(
    files: &CpuFiles,
    prev: Option<&CpuCounters>,
    host: CpuCount,
) -> Result<CpuGroup, GroupFailure> {
    let capacity = cpu_capacity(host, &files.quotas, files.cpuset);
    let usage = prev
        .and_then(|prev| cpu_usage(prev, &files.counters, capacity))
        .ok_or(GroupFailure::NoDelta)?;
    Ok(CpuGroup {
        usage: Some(usage),
        capacity,
    })
}

/// The memory group from its files (RFC 0014 §6): used is the working set, capped at
/// capacity, so `used + available = total` and `free ≤ available ≤ total`.
fn memory_group(files: &MemoryFiles, host: Bytes) -> MemoryGroup {
    let total = memory_capacity(host, &files.limits);
    let used = files.current.saturating_sub(files.inactive_file).min(total);
    MemoryGroup {
        total,
        used,
        free: total.saturating_sub(files.current),
        available: total.saturating_sub(used),
        limit: Some(tightest(&files.limits)),
    }
}

/// The swap group from its files, used capped at capacity.
fn swap_group(files: &SwapFiles, host: Bytes) -> SwapGroup {
    let total = memory_capacity(host, &files.limits);
    let used = files.current.min(total);
    SwapGroup {
        total,
        used,
        free: total.saturating_sub(used),
        limit: Some(tightest(&files.limits)),
    }
}

/// One group's step: this tick's cgroup attempt against the group's history.
fn source_group<G: Clone>(
    history: &GroupHistory<G>,
    read: Result<Option<G>, GroupFailure>,
    kernel: G,
    now: Instant,
) -> (Sourced<G>, GroupHistory<G>, Option<Warning>) {
    use GroupHistory::*;
    let measured = |value: G| {
        let next = Cgroup {
            last: value.clone(),
            carry: Carry::Reading,
        };
        (
            Sourced {
                value,
                origin: Origin::Cgroup,
            },
            next,
            None,
        )
    };
    let kernel_group = |next, warning| {
        let sourced = Sourced {
            value: kernel,
            origin: Origin::Kernel,
        };
        (sourced, next, warning)
    };
    match (history, read) {
        (_, Ok(Some(value))) => measured(value),
        (Untouched, Ok(None)) => kernel_group(Untouched, None),
        (Untouched, Err(failure)) => {
            kernel_group(KernelFailing, Some(Warning::KernelFallback(failure)))
        }
        (KernelFailing, Ok(None) | Err(_)) => kernel_group(KernelFailing, None),
        (Cgroup { last, carry }, Ok(None)) => carried(last, *carry, GroupFailure::Vanished, now),
        (Cgroup { last, carry }, Err(failure)) => carried(last, *carry, failure, now),
    }
}

/// A cgroup group's step on a failing tick: carried until `CARRY_BOUND` has passed since the
/// failures began, then unavailable. Warns when each begins.
fn carried<G: Clone>(
    last: &G,
    carry: Carry,
    failure: GroupFailure,
    now: Instant,
) -> (Sourced<G>, GroupHistory<G>, Option<Warning>) {
    let (carry, origin, warning) = match carry {
        Carry::Reading => (
            Carry::Since(now),
            Origin::Carried,
            Some(Warning::Carrying(failure)),
        ),
        Carry::Since(since) if now.saturating_duration_since(since) <= CARRY_BOUND => {
            (Carry::Since(since), Origin::Carried, None)
        }
        Carry::Since(_) => (
            Carry::Expired,
            Origin::Unavailable,
            Some(Warning::Unavailable),
        ),
        Carry::Expired => (Carry::Expired, Origin::Unavailable, None),
    };
    let next = GroupHistory::Cgroup {
        last: last.clone(),
        carry,
    };
    (
        Sourced {
            value: last.clone(),
            origin,
        },
        next,
        warning,
    )
}

/// Whose processes the agent lists, as uptime and process memory care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessView {
    /// Not a container: the host's, or the virtual machine's.
    Host,
    /// A container's, with no cgroup v2 hierarchy to measure it by.
    UnmeasuredContainer,
    /// The workload's: PID 1 is in the monitored cgroup.
    Workload,
    /// A container's PID namespace shared with processes outside the workload.
    SharedNamespace,
}

/// The uptime to report: the container's since its PID 1 started, when it has its own PID 1
/// and that PID 1 is visible; the kernel's otherwise.
pub fn uptime(view: ProcessView, pid1_started_at: Option<u64>, now_unix: u64, kernel: u64) -> u64 {
    match (view, pid1_started_at) {
        (ProcessView::Workload | ProcessView::UnmeasuredContainer, Some(started)) => {
            now_unix.saturating_sub(started)
        }
        (ProcessView::Workload | ProcessView::UnmeasuredContainer, None)
        | (ProcessView::Host | ProcessView::SharedNamespace, _) => kernel,
    }
}

/// The memory a process's share is taken over: the workload's capacity, or the kernel's.
pub fn process_memory_base(view: ProcessView, readings: &SourcedReadings, kernel: Bytes) -> Bytes {
    match view {
        ProcessView::Workload => readings.memory.value.total,
        ProcessView::Host | ProcessView::UnmeasuredContainer | ProcessView::SharedNamespace => {
            kernel
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::cgroup::CgroupFile;
    use ResourceLimit::{Bounded, Unbounded};

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn failure() -> GroupFailure {
        GroupFailure::File(CgroupReadError::Malformed(CgroupFile::MemoryMax))
    }

    /// (name, history, this tick, now, expected value, origin, next history, warning)
    type LineageRow = (
        &'static str,
        GroupHistory<u32>,
        Result<Option<u32>, GroupFailure>,
        Instant,
        u32,
        Origin,
        GroupHistory<u32>,
        Option<Warning>,
    );

    #[test]
    fn each_cell_of_the_lineage_table() {
        use GroupHistory::*;
        use Origin as O;
        let t0 = Instant::now();
        let cgroup = |last| Cgroup {
            last,
            carry: Carry::Reading,
        };
        let carrying = |last, since| Cgroup {
            last,
            carry: Carry::Since(since),
        };
        let expired = |last| Cgroup {
            last,
            carry: Carry::Expired,
        };
        // The kernel's value is 1, this tick's cgroup value 2, the last cgroup value 9.
        // (name, history, this tick, now, expected value, origin, next history, warning)
        let cases: Vec<LineageRow> = vec![
            (
                "kernel: first cgroup reading",
                Untouched,
                Ok(Some(2)),
                t0,
                2,
                O::Cgroup,
                cgroup(2),
                None,
            ),
            (
                "kernel: absent since start",
                Untouched,
                Ok(None),
                t0,
                1,
                O::Kernel,
                Untouched,
                None,
            ),
            (
                "kernel: failing from the start warns once",
                Untouched,
                Err(failure()),
                t0,
                1,
                O::Kernel,
                KernelFailing,
                Some(Warning::KernelFallback(failure())),
            ),
            (
                "kernel: still failing is quiet",
                KernelFailing,
                Err(failure()),
                t0,
                1,
                O::Kernel,
                KernelFailing,
                None,
            ),
            (
                "kernel: a failing file that goes away is still failing",
                KernelFailing,
                Ok(None),
                t0,
                1,
                O::Kernel,
                KernelFailing,
                None,
            ),
            (
                "kernel: a failing group recovers to the cgroup",
                KernelFailing,
                Ok(Some(2)),
                t0,
                2,
                O::Cgroup,
                cgroup(2),
                None,
            ),
            (
                "cgroup: measured",
                cgroup(9),
                Ok(Some(2)),
                t0,
                2,
                O::Cgroup,
                cgroup(2),
                None,
            ),
            (
                "cgroup: a failure carries, never the kernel",
                cgroup(9),
                Err(failure()),
                t0,
                9,
                O::Carried,
                carrying(9, t0),
                Some(Warning::Carrying(failure())),
            ),
            (
                "cgroup: a usage file gone after being read carries",
                cgroup(9),
                Ok(None),
                t0,
                9,
                O::Carried,
                carrying(9, t0),
                Some(Warning::Carrying(GroupFailure::Vanished)),
            ),
            (
                "cgroup: a no-delta tick carries",
                cgroup(9),
                Err(GroupFailure::NoDelta),
                t0,
                9,
                O::Carried,
                carrying(9, t0),
                Some(Warning::Carrying(GroupFailure::NoDelta)),
            ),
            (
                "cgroup: still carrying is quiet",
                carrying(9, t0),
                Err(failure()),
                t0 + secs(2),
                9,
                O::Carried,
                carrying(9, t0),
                None,
            ),
            (
                "cgroup: a vanish inside a running carry is quiet",
                carrying(9, t0),
                Ok(None),
                t0 + secs(2),
                9,
                O::Carried,
                carrying(9, t0),
                None,
            ),
            (
                "cgroup: exactly at the bound still carries",
                carrying(9, t0),
                Err(failure()),
                t0 + CARRY_BOUND,
                9,
                O::Carried,
                carrying(9, t0),
                None,
            ),
            (
                "cgroup: one tick past the bound is unavailable",
                carrying(9, t0),
                Err(failure()),
                t0 + CARRY_BOUND + secs(2),
                9,
                O::Unavailable,
                expired(9),
                Some(Warning::Unavailable),
            ),
            (
                "cgroup: just past the bound is unavailable",
                carrying(9, t0),
                Ok(None),
                t0 + CARRY_BOUND + Duration::from_millis(1),
                9,
                O::Unavailable,
                expired(9),
                Some(Warning::Unavailable),
            ),
            (
                "cgroup: unavailable stays so, never the kernel",
                expired(9),
                Err(failure()),
                t0 + secs(99),
                9,
                O::Unavailable,
                expired(9),
                None,
            ),
            (
                "cgroup: unavailable, its usage file gone, never the kernel",
                expired(9),
                Ok(None),
                t0 + secs(99),
                9,
                O::Unavailable,
                expired(9),
                None,
            ),
            (
                "cgroup: unavailable recovers",
                expired(9),
                Ok(Some(2)),
                t0 + secs(99),
                2,
                O::Cgroup,
                cgroup(2),
                None,
            ),
            (
                "cgroup: a carry recovers",
                carrying(9, t0),
                Ok(Some(2)),
                t0 + secs(4),
                2,
                O::Cgroup,
                cgroup(2),
                None,
            ),
        ];
        for (name, history, read, now, value, origin, next, warning) in cases {
            let (sourced, got_next, got_warning) = source_group(&history, read, 1, now);
            assert_eq!(sourced, Sourced { value, origin }, "{name}");
            assert_eq!(got_next, next, "{name}: history");
            assert_eq!(got_warning, warning, "{name}: warning");
        }
    }

    #[test]
    fn a_carry_keeps_its_start_across_a_rebuilt_sampler() {
        // The collector hands the same history to a rebuilt sampler: a carry begun at t0 ends
        // at t0 + the bound, not a bound after the rebuild.
        let t0 = Instant::now();
        let mut history = GroupHistory::Cgroup {
            last: 9,
            carry: Carry::Reading,
        };
        let ticks = [t0, t0 + secs(20), t0 + secs(32)];
        let origins: Vec<Origin> = ticks
            .iter()
            .map(|&now| {
                let (sourced, next, _) = source_group(&history, Err(failure()), 1, now);
                history = next;
                sourced.origin
            })
            .collect();
        assert_eq!(
            origins,
            [Origin::Carried, Origin::Carried, Origin::Unavailable]
        );
    }

    fn b(n: u64) -> Bytes {
        Bytes::new(n)
    }

    fn cpus(n: f64) -> CpuCount {
        CpuCount::new(n).expect("positive")
    }

    fn kernel() -> KernelReadings {
        KernelReadings {
            cpu: CpuGroup {
                usage: Percent::saturating(12.0),
                capacity: cpus(8.0),
            },
            memory: MemoryGroup {
                total: b(1000),
                used: b(400),
                free: b(300),
                available: b(600),
                limit: None,
            },
            swap: SwapGroup {
                total: b(2000),
                used: b(200),
                free: b(1800),
                limit: None,
            },
        }
    }

    fn memory_files(
        current: u64,
        inactive_file: u64,
        limits: Vec<ResourceLimit<Bytes>>,
    ) -> CgroupRead<MemoryFiles> {
        CgroupRead::Read(MemoryFiles {
            current: b(current),
            inactive_file: b(inactive_file),
            limits,
        })
    }

    fn cgroup_readings(t: Instant) -> CgroupReadings {
        CgroupReadings {
            cpu: CgroupRead::Read(CpuFiles {
                counters: CpuCounters {
                    usage: Duration::from_millis(1_500),
                    read_at: t,
                },
                quotas: vec![Bounded(cpus(1.5))],
                cpuset: Unbounded,
            }),
            memory: memory_files(600, 200, vec![Bounded(b(500))]),
            swap: CgroupRead::Read(SwapFiles {
                current: b(100),
                limits: vec![Bounded(b(1000))],
            }),
        }
    }

    #[test]
    fn outside_a_container_every_group_is_the_kernels() {
        let t = Instant::now();
        let got = choose_readings(None, kernel(), None, &SourcingHistory::new(), t);
        let k = kernel();
        assert_eq!(
            got.readings,
            SourcedReadings {
                cpu: Sourced {
                    value: k.cpu,
                    origin: Origin::Kernel
                },
                memory: Sourced {
                    value: k.memory,
                    origin: Origin::Kernel
                },
                swap: Sourced {
                    value: k.swap,
                    origin: Origin::Kernel
                },
            }
        );
        assert_eq!(got.history, SourcingHistory::new());
        assert_eq!(got.warnings, []);
    }

    #[test]
    fn a_container_reads_every_group_from_its_cgroup() {
        let t0 = Instant::now();
        let prev = CpuCounters {
            usage: Duration::ZERO,
            read_at: t0,
        };
        let t = t0 + secs(2);
        let got = choose_readings(
            Some(cgroup_readings(t)),
            kernel(),
            Some(&prev),
            &SourcingHistory::new(),
            t,
        );
        assert_eq!(
            got.readings.cpu,
            Sourced {
                value: CpuGroup {
                    usage: Percent::saturating(50.0),
                    capacity: cpus(1.5),
                },
                origin: Origin::Cgroup,
            },
            "1.5 s of cpu over 2 s of 1.5 cpus"
        );
        assert_eq!(
            got.readings.memory,
            Sourced {
                value: MemoryGroup {
                    total: b(500),
                    used: b(400),
                    free: b(0),
                    available: b(100),
                    limit: Some(Bounded(b(500))),
                },
                origin: Origin::Cgroup,
            },
            "the working set over the limit"
        );
        assert_eq!(
            got.readings.swap,
            Sourced {
                value: SwapGroup {
                    total: b(1000),
                    used: b(100),
                    free: b(900),
                    limit: Some(Bounded(b(1000))),
                },
                origin: Origin::Cgroup,
            }
        );
        assert_eq!(got.warnings, []);
    }

    #[test]
    fn memory_keeps_its_invariants_on_every_shape() {
        // (name, current, inactive_file, limits, expected total/used/free/available, limit)
        let cases = [
            (
                "unbounded, inactive cache",
                300,
                100,
                vec![Unbounded],
                (1000, 200, 700, 800),
                Unbounded,
            ),
            (
                "bounded, cache over the limit",
                600,
                200,
                vec![Bounded(b(500))],
                (500, 400, 0, 100),
                Bounded(b(500)),
            ),
            (
                "a working set over capacity is capped",
                900,
                0,
                vec![Bounded(b(500))],
                (500, 500, 0, 0),
                Bounded(b(500)),
            ),
            (
                "more cache than usage",
                100,
                300,
                vec![Bounded(b(500))],
                (500, 0, 400, 500),
                Bounded(b(500)),
            ),
            (
                "a limit above host ram",
                300,
                0,
                vec![Bounded(b(4000))],
                (1000, 300, 700, 700),
                Bounded(b(4000)),
            ),
            (
                "a parent's tighter limit",
                100,
                0,
                vec![Bounded(b(800)), Bounded(b(600))],
                (600, 100, 500, 500),
                Bounded(b(600)),
            ),
        ];
        for (name, current, inactive, limits, (total, used, free, available), limit) in cases {
            let readings = CgroupReadings {
                memory: memory_files(current, inactive, limits),
                ..cgroup_readings(Instant::now())
            };
            let got = choose_readings(
                Some(readings),
                kernel(),
                None,
                &SourcingHistory::new(),
                Instant::now(),
            );
            let m = got.readings.memory.value;
            assert_eq!(
                (m.total, m.used, m.free, m.available),
                (b(total), b(used), b(free), b(available)),
                "{name}"
            );
            assert_eq!(
                m.limit,
                Some(limit),
                "{name}: the tightest limit, not the capacity"
            );
            assert_eq!(
                m.used.get() + m.available.get(),
                m.total.get(),
                "{name}: used + available"
            );
            assert!(
                m.free <= m.available && m.available <= m.total,
                "{name}: free ≤ available ≤ total"
            );
        }
    }

    #[test]
    fn swap_capacity_is_bounded_by_host_swap() {
        // Host swap is 2000. (name, current, limits, expected total/used/free, limit)
        let cases = [
            (
                "a limit above host swap",
                100,
                vec![Bounded(b(5000))],
                (2000, 100, 1900),
                Bounded(b(5000)),
            ),
            (
                "unbounded",
                100,
                vec![Unbounded],
                (2000, 100, 1900),
                Unbounded,
            ),
            (
                "a tighter limit",
                100,
                vec![Bounded(b(1000))],
                (1000, 100, 900),
                Bounded(b(1000)),
            ),
            (
                "usage over a lowered limit is capped",
                700,
                vec![Bounded(b(500))],
                (500, 500, 0),
                Bounded(b(500)),
            ),
        ];
        for (name, current, limits, (total, used, free), limit) in cases {
            let readings = CgroupReadings {
                swap: CgroupRead::Read(SwapFiles {
                    current: b(current),
                    limits,
                }),
                ..cgroup_readings(Instant::now())
            };
            let got = choose_readings(
                Some(readings),
                kernel(),
                None,
                &SourcingHistory::new(),
                Instant::now(),
            );
            assert_eq!(
                got.readings.swap,
                Sourced {
                    value: SwapGroup {
                        total: b(total),
                        used: b(used),
                        free: b(free),
                        limit: Some(limit),
                    },
                    origin: Origin::Cgroup,
                },
                "{name}"
            );
        }
    }

    #[test]
    fn no_cgroup_on_a_tick_never_returns_a_cgroup_group_to_the_kernel() {
        let t0 = Instant::now();
        let prev = CpuCounters {
            usage: Duration::ZERO,
            read_at: t0 - secs(2),
        };
        let first = choose_readings(
            Some(cgroup_readings(t0)),
            kernel(),
            Some(&prev),
            &SourcingHistory::new(),
            t0,
        );
        let got = choose_readings(None, kernel(), None, &first.history, t0 + secs(2));
        let vanished = |group| SourcingWarning {
            group,
            warning: Warning::Carrying(GroupFailure::Vanished),
        };
        assert_eq!(
            got.warnings,
            [
                vanished(ReadingGroup::Cpu),
                vanished(ReadingGroup::Memory),
                vanished(ReadingGroup::Swap)
            ],
            "each group warns under its own name"
        );
        assert_eq!(
            got.readings.cpu,
            Sourced {
                value: first.readings.cpu.value,
                origin: Origin::Carried
            }
        );
        assert_eq!(
            got.readings.memory,
            Sourced {
                value: first.readings.memory.value,
                origin: Origin::Carried
            }
        );
        assert_eq!(
            got.readings.swap,
            Sourced {
                value: first.readings.swap.value,
                origin: Origin::Carried
            }
        );
    }

    #[test]
    fn swap_without_accounting_is_the_kernels_while_memory_stays_the_cgroups() {
        let t = Instant::now();
        let readings = CgroupReadings {
            swap: CgroupRead::Absent,
            ..cgroup_readings(t)
        };
        let got = choose_readings(Some(readings), kernel(), None, &SourcingHistory::new(), t);
        assert_eq!(
            got.readings.swap,
            Sourced {
                value: kernel().swap,
                origin: Origin::Kernel
            }
        );
        assert_eq!(got.readings.memory.origin, Origin::Cgroup);
        assert_eq!(
            got.warnings.iter().map(|w| w.group).collect::<Vec<_>>(),
            [ReadingGroup::Cpu],
            "an absent file is quiet; only the cpu's missing delta warns"
        );
    }

    #[test]
    fn a_quota_above_the_hosts_cpus_is_bounded_by_the_host() {
        let t0 = Instant::now();
        let prev = CpuCounters {
            usage: Duration::ZERO,
            read_at: t0,
        };
        let mut one_cpu = kernel();
        one_cpu.cpu.capacity = cpus(1.0);
        // 1.5 s of cpu over 2 s under a 1.5-cpu quota on a 1-cpu host.
        let t = t0 + secs(2);
        let got = choose_readings(
            Some(cgroup_readings(t)),
            one_cpu,
            Some(&prev),
            &SourcingHistory::new(),
            t,
        );
        assert_eq!(
            got.readings.cpu.value,
            CpuGroup {
                usage: Percent::saturating(75.0),
                capacity: cpus(1.0),
            }
        );
    }

    #[test]
    fn cpu_with_no_previous_counters_has_no_delta() {
        let t = Instant::now();
        let got = choose_readings(
            Some(cgroup_readings(t)),
            kernel(),
            None,
            &SourcingHistory::new(),
            t,
        );
        assert_eq!(
            got.readings.cpu,
            Sourced {
                value: kernel().cpu,
                origin: Origin::Kernel
            }
        );
        assert_eq!(
            got.warnings,
            [SourcingWarning {
                group: ReadingGroup::Cpu,
                warning: Warning::KernelFallback(GroupFailure::NoDelta),
            }]
        );
    }

    #[test]
    fn a_failing_group_is_carried_whole_after_a_lowered_capacity() {
        let t0 = Instant::now();
        let first = choose_readings(
            Some(cgroup_readings(t0)),
            kernel(),
            None,
            &SourcingHistory::new(),
            t0,
        );
        let before = first.readings.memory.value;
        // Host RAM drops under the old capacity, and memory.stat breaks on the same tick:
        // nothing of the new capacity leaks into the carried group.
        let failing = CgroupReadings {
            memory: CgroupRead::Failed(CgroupReadError::Malformed(CgroupFile::MemoryStat)),
            ..cgroup_readings(t0 + secs(2))
        };
        let mut lowered = kernel();
        lowered.memory.total = b(300);
        let got = choose_readings(Some(failing), lowered, None, &first.history, t0 + secs(2));
        assert_eq!(
            got.readings.memory,
            Sourced {
                value: before,
                origin: Origin::Carried
            }
        );
        assert_eq!(
            got.warnings
                .iter()
                .filter(|w| w.group == ReadingGroup::Memory)
                .map(|w| w.warning.clone())
                .collect::<Vec<_>>(),
            [Warning::Carrying(GroupFailure::File(
                CgroupReadError::Malformed(CgroupFile::MemoryStat)
            ))]
        );
    }

    #[test]
    fn uptime_is_the_containers_only_with_its_own_visible_pid1() {
        use ProcessView::*;
        // (name, view, pid 1's start, expected) at now = 10_000, kernel uptime 50_000.
        let cases = [
            ("host", Host, Some(9_000), 50_000),
            ("the workload", Workload, Some(9_000), 1_000),
            (
                "an unmeasured container",
                UnmeasuredContainer,
                Some(9_000),
                1_000,
            ),
            ("pid 1 hidden", Workload, None, 50_000),
            ("a shared namespace", SharedNamespace, Some(9_000), 50_000),
            ("pid 1 started after now", Workload, Some(11_000), 0),
        ];
        for (name, view, started, expected) in cases {
            assert_eq!(uptime(view, started, 10_000, 50_000), expected, "{name}");
        }
    }

    #[test]
    fn process_memory_is_over_the_workloads_capacity_only_for_the_workload() {
        use ProcessView::*;
        let t = Instant::now();
        let readings = choose_readings(
            Some(cgroup_readings(t)),
            kernel(),
            None,
            &SourcingHistory::new(),
            t,
        )
        .readings;
        let above_host = CgroupReadings {
            memory: memory_files(100, 0, vec![Bounded(b(4000))]),
            ..cgroup_readings(t)
        };
        let loose =
            choose_readings(Some(above_host), kernel(), None, &SourcingHistory::new(), t).readings;
        // (name, view, readings, expected) with 1000 of kernel memory.
        let cases = [
            ("host", Host, readings, 1000),
            ("the workload", Workload, readings, 500),
            (
                "the workload, its limit above host ram",
                Workload,
                loose,
                1000,
            ),
            (
                "an unmeasured container",
                UnmeasuredContainer,
                readings,
                1000,
            ),
            ("a shared namespace", SharedNamespace, readings, 1000),
        ];
        for (name, view, readings, expected) in cases {
            assert_eq!(
                process_memory_base(view, &readings, b(1000)),
                b(expected),
                "{name}"
            );
        }
    }
}
