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

//! The sampler: the one owner of the system's previous reading (RFC 0014 §6). Every method
//! blocks, so the collector calls them inside `spawn_blocking`.

use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use sysinfo::System;

use super::monotonic_now;
use super::system::{self, RawReadings, Workload};
use crate::environment::ExecutionEnvironment;
use crate::environment::cgroup::MonitoredCgroup;
use crate::environment::sourcing::{
    self, CgroupRead, GroupFailure, ReadingGroup, SourcingHistory, SourcingWarning, Warning,
};
use crate::environment::usage::CpuCounters;
use crate::snapshot::{CollectedSnapshot, Priming};

/// Where a sampler's readings come from. `read_at` stamps any cumulative counters it reads.
pub trait Gather: Send + 'static {
    fn gather(&mut self, read_at: Instant) -> RawReadings;
}

/// The real system: one sysinfo `System`, refreshed in place, and the files under `root`,
/// including the monitored cgroup's when there is one.
pub struct SysinfoSource {
    sys: System,
    root: PathBuf,
    cgroup: Option<MonitoredCgroup>,
}

impl SysinfoSource {
    pub fn new(root: PathBuf, cgroup: Option<MonitoredCgroup>) -> Self {
        Self {
            sys: System::new(),
            root,
            cgroup,
        }
    }
}

impl Gather for SysinfoSource {
    fn gather(&mut self, read_at: Instant) -> RawReadings {
        system::gather(&mut self.sys, &self.root, self.cgroup.as_ref(), read_at)
    }
}

pub struct Sampler<G> {
    source: G,
    /// What every reading is taken in, found once at startup.
    environment: ExecutionEnvironment,
    /// When the priming reading ended.
    primed_at: Instant,
    /// The monitored cgroup's CPU counters at the last reading that had them.
    prev_cpu: Option<CpuCounters>,
}

/// A published reading, and the sourcing history it leaves for the next one.
pub struct Sampled {
    pub snapshot: CollectedSnapshot,
    pub history: SourcingHistory,
}

impl<G: Gather> Sampler<G> {
    /// A sampler in `environment` that has taken its priming reading, which it never
    /// publishes but whose counters the first published reading is measured from.
    pub fn prime(mut source: G, environment: ExecutionEnvironment) -> Self {
        let raw = source.gather(monotonic_now());
        Self {
            source,
            environment,
            primed_at: monotonic_now(),
            prev_cpu: cpu_counters(&raw),
        }
    }

    /// Takes a reading, its groups chosen against `history` (RFC 0014 §6). It is a snapshot
    /// only once priming is done (`Priming::of`); an earlier one only refreshes the source
    /// and the counters.
    pub fn read(&mut self, history: &SourcingHistory) -> Option<Sampled> {
        let read_at = monotonic_now();
        let collected_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let raw = self.source.gather(read_at);
        let counters = cpu_counters(&raw);
        let sampled = match Priming::of(self.primed_at, read_at) {
            Priming::Done => Some(self.sampled(raw, history, read_at, collected_at)),
            Priming::Pending => None,
        };
        self.prev_cpu = counters.or(self.prev_cpu);
        sampled
    }

    /// The snapshot `raw` makes, read at `read_at` (monotonic) and `collected_at` (unix).
    fn sampled(
        &self,
        mut raw: RawReadings,
        history: &SourcingHistory,
        read_at: Instant,
        collected_at: u64,
    ) -> Sampled {
        let kernel = system::kernel_readings(&raw);
        // Only a container with a monitored cgroup is measured by it, whatever the source read.
        let cgroup = raw
            .cgroup
            .take()
            .filter(|_| self.environment.monitored_cgroup().is_some());
        let sourcing =
            sourcing::choose_readings(cgroup, kernel, self.prev_cpu.as_ref(), history, read_at);
        sourcing.warnings.iter().for_each(log_warning);
        let view = self.environment.process_view();
        let workload = Workload {
            uptime_secs: sourcing::uptime(view, raw.pid1_started_at, collected_at, raw.uptime_secs),
            process_memory_base: sourcing::process_memory_base(
                view,
                &sourcing.readings,
                kernel.memory.total,
            ),
        };
        let origins = sourcing.readings.origins();
        Sampled {
            snapshot: CollectedSnapshot {
                system: system::snapshot_from(raw, &sourcing.readings, &workload),
                environment: self.environment.clone(),
                origins,
                collected_at,
                read_at,
            },
            history: sourcing.history,
        }
    }
}

/// The monitored cgroup's CPU counters in `raw`, if it read them.
fn cpu_counters(raw: &RawReadings) -> Option<CpuCounters> {
    match raw.cgroup.as_ref().map(|cgroup| &cgroup.cpu) {
        Some(CgroupRead::Read(files)) => Some(files.counters),
        Some(CgroupRead::Absent | CgroupRead::Failed(_)) | None => None,
    }
}

/// Says a sourcing transition where operators look: once per transition, never per tick.
fn log_warning(warning: &SourcingWarning) {
    let group = match warning.group {
        ReadingGroup::Cpu => "CPU",
        ReadingGroup::Memory => "memory",
        ReadingGroup::Swap => "swap",
    };
    match &warning.warning {
        Warning::KernelFallback(why) => tracing::warn!(
            "The {group} readings come from the kernel: the cgroup gave none ({})",
            failure(why)
        ),
        Warning::Carrying(why) => tracing::warn!(
            "The {group} readings are carried: the cgroup gave none ({})",
            failure(why)
        ),
        Warning::Unavailable => tracing::warn!(
            "The {group} readings are unavailable: the cgroup has given none for {} s",
            sourcing::CARRY_BOUND.as_secs()
        ),
    }
}

fn failure(why: &GroupFailure) -> String {
    use crate::environment::cgroup::CgroupReadError;
    match why {
        GroupFailure::File(CgroupReadError::Malformed(file)) => {
            format!("{} is malformed", file.name())
        }
        GroupFailure::File(CgroupReadError::Unreadable { file, reason }) => {
            format!("{} is unreadable: {reason}", file.name())
        }
        GroupFailure::File(CgroupReadError::Missing(file)) => {
            format!("{} is missing beside its usage file", file.name())
        }
        GroupFailure::Vanished => "its usage file went away".to_owned(),
        GroupFailure::NoDelta => "no CPU usage since the previous reading".to_owned(),
    }
}

#[cfg(test)]
pub mod fakes {
    use super::*;
    use crate::collectors::system::fixtures::raw_with_cpu;

    /// What sysinfo reports on its first refresh: an average since boot, which no published
    /// snapshot may carry.
    pub const SINCE_BOOT_CPU: f32 = 99.0;

    /// A source whose first reading is `SINCE_BOOT_CPU`, and whose n-th later one is CPU n.
    /// It panics on the reading numbered `panic_on`, counting the first as 1.
    pub struct FakeSource {
        readings: u32,
        panic_on: Option<u32>,
    }

    impl FakeSource {
        pub fn new() -> Self {
            Self {
                readings: 0,
                panic_on: None,
            }
        }

        pub fn panicking_on(reading: u32) -> Self {
            Self {
                readings: 0,
                panic_on: Some(reading),
            }
        }
    }

    /// A container with a readable cgroup v2 hierarchy, its agent PID 1 at the namespace root.
    pub fn v2_container() -> ExecutionEnvironment {
        use crate::environment::cgroup::{Cgroup2Mount, CgroupEvidence, CgroupPath, cgroup_access};
        ExecutionEnvironment::Container {
            runtime: Some(crate::environment::ContainerRuntime::Docker),
            cgroup: cgroup_access(&CgroupEvidence {
                self_cgroup: Ok(CgroupPath::ROOT),
                mount: Some(Cgroup2Mount {
                    point: "/sys/fs/cgroup".into(),
                    root: CgroupPath::ROOT,
                }),
                namespace_root_typed: true,
                pid1_cgroup: Some(CgroupPath::ROOT),
            }),
        }
    }

    /// PID 1's start in a fake cgroup source: this long before each reading.
    pub const WORKLOAD_UPTIME: u64 = 300;

    /// A container's source on a 4-CPU host: readings 1, 2, 3, 4 … have used 1, 2, 4, 5 … s of
    /// CPU (not linear, so a reading measured from the wrong counters shows) under a 2-CPU
    /// quota, 600 bytes of
    /// memory (100 of them inactive cache) under a 1000-byte limit, and no swap accounting.
    /// From reading `memory_fails_from` on, memory.stat is malformed; reading `panic_on`
    /// panics. Counting the first reading as 1.
    pub struct CgroupSource {
        readings: u32,
        memory_fails_from: Option<u32>,
        panic_on: Option<u32>,
    }

    impl CgroupSource {
        pub fn new() -> Self {
            Self {
                readings: 0,
                memory_fails_from: None,
                panic_on: None,
            }
        }

        pub fn memory_failing_from(reading: u32) -> Self {
            Self {
                memory_fails_from: Some(reading),
                ..Self::new()
            }
        }

        pub fn panicking_on(reading: u32) -> Self {
            Self {
                panic_on: Some(reading),
                ..Self::new()
            }
        }
    }

    impl Gather for CgroupSource {
        fn gather(&mut self, read_at: Instant) -> RawReadings {
            use crate::environment::cgroup::{
                Bytes, CgroupFile, CgroupReadError, CpuCount, ResourceLimit,
            };
            use crate::environment::sourcing::{CgroupReadings, CpuFiles, MemoryFiles};
            use std::time::Duration;
            self.readings += 1;
            let n = self.readings;
            if self.panic_on == Some(n) {
                panic!("cgroup source: reading {n} fails");
            }
            let memory = match self.memory_fails_from {
                Some(from) if n >= from => {
                    CgroupRead::Failed(CgroupReadError::Malformed(CgroupFile::MemoryStat))
                }
                Some(_) | None => CgroupRead::Read(MemoryFiles {
                    current: Bytes::new(600),
                    inactive_file: Bytes::new(100),
                    limits: vec![ResourceLimit::Bounded(Bytes::new(1000))],
                }),
            };
            let now_unix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("after the epoch")
                .as_secs();
            RawReadings {
                pid1_started_at: Some(now_unix - WORKLOAD_UPTIME),
                cgroup: Some(CgroupReadings {
                    cpu: CgroupRead::Read(CpuFiles {
                        counters: CpuCounters {
                            usage: Duration::from_secs(match n {
                                1 | 2 => u64::from(n),
                                n => u64::from(n) + 1,
                            }),
                            read_at,
                        },
                        quotas: vec![ResourceLimit::Bounded(
                            CpuCount::new(2.0).expect("positive"),
                        )],
                        cpuset: ResourceLimit::Unbounded,
                    }),
                    memory,
                    swap: CgroupRead::Absent,
                }),
                cpus: (0..4)
                    .map(|_| crate::collectors::system::fixtures::cpu("Xeon", SINCE_BOOT_CPU, 2400))
                    .collect(),
                ..raw_with_cpu(SINCE_BOOT_CPU)
            }
        }
    }

    impl Gather for FakeSource {
        fn gather(&mut self, _read_at: Instant) -> RawReadings {
            self.readings += 1;
            if self.panic_on == Some(self.readings) {
                panic!("fake source: reading {} fails", self.readings);
            }
            match self.readings {
                1 => raw_with_cpu(SINCE_BOOT_CPU),
                n => raw_with_cpu((n - 1) as f32),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::*;
    use super::*;
    use crate::snapshot::PRIMING;
    use crate::snapshot::fixtures::ENVIRONMENT;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn a_snapshot_is_read_in_the_samplers_environment() {
        let cases = [
            ("bare metal", ExecutionEnvironment::BareMetal),
            ("a container", ENVIRONMENT),
            (
                "a vm",
                ExecutionEnvironment::VirtualMachine {
                    hypervisor: crate::environment::Hypervisor::Kvm,
                },
            ),
        ];
        for (name, environment) in cases {
            let mut sampler = Sampler::prime(FakeSource::new(), environment.clone());
            tokio::time::advance(PRIMING).await;
            let sampled = sampler
                .read(&SourcingHistory::new())
                .expect("a snapshot, past priming");
            assert_eq!(sampled.snapshot.environment, environment, "{name}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_container_sampler_reads_its_workload_from_its_cgroup() {
        use crate::environment::sourcing::{Origin, ReadingOrigins};
        let mut sampler = Sampler::prime(CgroupSource::memory_failing_from(3), v2_container());
        tokio::time::advance(PRIMING).await;
        let first = sampler.read(&SourcingHistory::new()).expect("past priming");
        let system = &first.snapshot.system;
        assert_eq!(
            system.cpu.usage_percent, 50.0,
            "1 s of cpu over 1 s of 2 cpus, from priming"
        );
        assert_eq!(system.cpu.capacity_cpus, 2.0);
        assert_eq!(system.cpu.logical_cores, 4, "the host's cores stay");
        assert_eq!(
            (system.memory.total_bytes, system.memory.used_bytes),
            (1000, 500),
            "the limit and the working set, not the host's 4096"
        );
        assert!(
            (WORKLOAD_UPTIME - 1..=WORKLOAD_UPTIME + 1).contains(&system.uptime_seconds),
            "the workload's uptime, not the kernel's: {}",
            system.uptime_seconds
        );
        assert_eq!(
            first.snapshot.origins,
            ReadingOrigins {
                cpu: Origin::Cgroup,
                memory: Origin::Cgroup,
                swap: Origin::Kernel,
            }
        );

        // The next reading breaks memory.stat: the history handed back carries the group.
        tokio::time::advance(PRIMING).await;
        let second = sampler.read(&first.history).expect("a snapshot");
        assert_eq!(second.snapshot.origins.memory, Origin::Carried);
        assert_eq!(
            second.snapshot.system.memory.total_bytes, 1000,
            "the carried group"
        );
        assert_eq!(
            second.snapshot.system.cpu.usage_percent, 100.0,
            "2 s of cpu over 1 s of 2 cpus, from the previous reading's counters"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_hosts_sampler_reads_everything_from_the_kernel() {
        use crate::environment::sourcing::{Origin, ReadingOrigins};
        // A cgroup source outside a container: its files aren't the agent's business.
        let mut sampler = Sampler::prime(CgroupSource::new(), ExecutionEnvironment::BareMetal);
        tokio::time::advance(PRIMING).await;
        let sampled = sampler.read(&SourcingHistory::new()).expect("past priming");
        assert_eq!(sampled.snapshot.system.memory.total_bytes, 4096);
        assert_eq!(
            sampled.snapshot.system.uptime_seconds,
            2 * 86400 + 3600,
            "the kernel's"
        );
        assert_eq!(
            sampled.snapshot.origins,
            ReadingOrigins {
                cpu: Origin::Kernel,
                memory: Origin::Kernel,
                swap: Origin::Kernel,
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_reading_is_a_snapshot_only_a_full_priming_interval_after_priming() {
        let cases = [
            ("right after priming", Duration::ZERO, None),
            ("just short of it", Duration::from_millis(999), None),
            ("exactly at it", PRIMING, Some(1.0)),
            ("past it", Duration::from_millis(1_500), Some(1.0)),
        ];
        for (name, wait, expected) in cases {
            let mut sampler = Sampler::prime(FakeSource::new(), ENVIRONMENT);
            tokio::time::advance(wait).await;
            let cpu = sampler
                .read(&SourcingHistory::new())
                .map(|s| s.snapshot.system.cpu.usage_percent);
            assert_eq!(cpu, expected, "case: {name}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_early_reading_still_refreshes_the_source() {
        let mut sampler = Sampler::prime(FakeSource::new(), ENVIRONMENT);
        assert!(
            sampler.read(&SourcingHistory::new()).is_none(),
            "too early to publish"
        );
        tokio::time::advance(PRIMING).await;
        let cpu = sampler
            .read(&SourcingHistory::new())
            .map(|s| s.snapshot.system.cpu.usage_percent);
        assert_eq!(cpu, Some(2.0), "the early reading was taken, not skipped");
    }

    #[tokio::test(start_paused = true)]
    async fn a_snapshot_is_stamped_when_its_reading_started() {
        let mut sampler = Sampler::prime(FakeSource::new(), ENVIRONMENT);
        tokio::time::advance(PRIMING).await;
        let unix_now = || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        };
        let (before, started) = (unix_now(), monotonic_now());
        let collected = sampler
            .read(&SourcingHistory::new())
            .expect("a snapshot, past priming")
            .snapshot;
        assert_eq!(collected.read_at, started, "on the monotonic clock");
        assert!(
            (before..=unix_now()).contains(&collected.collected_at),
            "on the agent's clock: {}",
            collected.collected_at
        );
    }
}
