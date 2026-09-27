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
use super::system::{self, RawReadings};
use crate::environment::ExecutionEnvironment;
use crate::snapshot::{CollectedSnapshot, Priming};

/// Where a sampler's readings come from.
pub trait Gather: Send + 'static {
    fn gather(&mut self) -> RawReadings;
}

/// The real system: one sysinfo `System`, refreshed in place, and the files under `root`.
pub struct SysinfoSource {
    sys: System,
    root: PathBuf,
}

impl SysinfoSource {
    pub fn new(root: PathBuf) -> Self {
        Self {
            sys: System::new(),
            root,
        }
    }
}

impl Gather for SysinfoSource {
    fn gather(&mut self) -> RawReadings {
        system::gather(&mut self.sys, &self.root)
    }
}

pub struct Sampler<G> {
    source: G,
    /// What every reading is taken in, found once at startup.
    environment: ExecutionEnvironment,
    /// When the priming reading ended.
    primed_at: Instant,
}

impl<G: Gather> Sampler<G> {
    /// A sampler in `environment` that has taken its priming reading, which it never
    /// publishes.
    pub fn prime(mut source: G, environment: ExecutionEnvironment) -> Self {
        source.gather();
        Self {
            source,
            environment,
            primed_at: monotonic_now(),
        }
    }

    /// Takes a reading. It is a snapshot only once priming is done (`Priming::of`); an
    /// earlier one only refreshes the source.
    pub fn read(&mut self) -> Option<CollectedSnapshot> {
        let read_at = monotonic_now();
        let collected_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let raw = self.source.gather();
        match Priming::of(self.primed_at, read_at) {
            Priming::Done => Some(CollectedSnapshot {
                system: system::snapshot_from(raw),
                environment: self.environment.clone(),
                collected_at,
                read_at,
            }),
            Priming::Pending => None,
        }
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

    impl Gather for FakeSource {
        fn gather(&mut self) -> RawReadings {
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
            let collected = sampler.read().expect("a snapshot, past priming");
            assert_eq!(collected.environment, environment, "{name}");
        }
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
            let cpu = sampler.read().map(|s| s.system.cpu.usage_percent);
            assert_eq!(cpu, expected, "case: {name}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_early_reading_still_refreshes_the_source() {
        let mut sampler = Sampler::prime(FakeSource::new(), ENVIRONMENT);
        assert!(sampler.read().is_none(), "too early to publish");
        tokio::time::advance(PRIMING).await;
        let cpu = sampler.read().map(|s| s.system.cpu.usage_percent);
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
        let collected = sampler.read().expect("a snapshot, past priming");
        assert_eq!(collected.read_at, started, "on the monotonic clock");
        assert!(
            (before..=unix_now()).contains(&collected.collected_at),
            "on the agent's clock: {}",
            collected.collected_at
        );
    }
}
