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

//! The published snapshot: the one the background collector read last, and the only one any
//! route, stream or push frame reads (RFC 0014 §6).

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::environment::ExecutionEnvironment;
use crate::environment::sourcing::ReadingOrigins;
use crate::models::SystemSnapshot;

/// The collector's count of published snapshots. It only grows, across sampler rebuilds, and
/// never looks at the wall clock, so "is this the snapshot I last sent?" survives a clock step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotSeq(u64);

impl SnapshotSeq {
    /// The startup snapshot's.
    pub const FIRST: Self = Self(0);

    /// The seq of the snapshot published after this one.
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// The least time between a sampler's priming reading and the first one it publishes, so no
/// published CPU usage is sysinfo's since-boot average.
pub const PRIMING: Duration = Duration::from_secs(1);

/// Whether a reading may become a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priming {
    /// It started too soon after the priming reading: it only refreshes the source.
    Pending,
    /// It started at least `PRIMING` after the priming reading ended.
    Done,
}

impl Priming {
    /// Where a reading started at `read_at` stands, the priming reading having ended at
    /// `primed_at`.
    pub fn of(primed_at: Instant, read_at: Instant) -> Self {
        if read_at.saturating_duration_since(primed_at) >= PRIMING {
            Self::Done
        } else {
            Self::Pending
        }
    }
}

/// One reading of the system, not yet published.
pub struct CollectedSnapshot {
    pub system: SystemSnapshot,
    /// What the reading was taken in: the sampler's, found once at startup.
    pub environment: ExecutionEnvironment,
    /// Where the CPU, memory and swap groups came from on this reading.
    pub origins: ReadingOrigins,
    /// Unix seconds on the agent's clock, when the reading started.
    pub collected_at: u64,
    /// When the reading started, on the runtime's monotonic clock.
    pub read_at: Instant,
}

impl CollectedSnapshot {
    /// The published snapshot this one becomes under `seq`.
    pub fn published(self, seq: SnapshotSeq) -> PublishedSnapshot {
        PublishedSnapshot {
            system: self.system,
            environment: self.environment,
            origins: self.origins,
            collected_at: self.collected_at,
            read_at: self.read_at,
            seq,
        }
    }
}

/// The snapshot every reader shares.
pub struct PublishedSnapshot {
    pub system: SystemSnapshot,
    pub environment: ExecutionEnvironment,
    pub origins: ReadingOrigins,
    pub collected_at: u64,
    pub read_at: Instant,
    pub seq: SnapshotSeq,
}

impl PublishedSnapshot {
    /// Whether this snapshot may still be served at `now`.
    pub fn freshness(&self, now: Instant) -> SnapshotFreshness {
        SnapshotFreshness::of_age(now.saturating_duration_since(self.read_at))
    }
}

/// How long ago a snapshot may have been read and still be served: long enough for a tick
/// starved by its container's CPU quota, well inside the hub's 90 s push idle deadline.
pub const STALENESS_BOUND: Duration = Duration::from_secs(30);

/// Whether a snapshot is recent enough to serve (RFC 0014 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotFreshness {
    Fresh,
    /// Read more than `STALENESS_BOUND` ago: the collector is hung or keeps failing.
    Stale,
}

impl SnapshotFreshness {
    /// The freshness of a snapshot read `age` ago, on a monotonic clock.
    pub fn of_age(age: Duration) -> Self {
        if age > STALENESS_BOUND {
            Self::Stale
        } else {
            Self::Fresh
        }
    }
}

/// What a live stream last told its client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Live,
    Stale,
}

/// What a live stream sends on one of its ticks.
pub enum StreamEmit {
    Snapshot(Arc<PublishedSnapshot>),
    /// The snapshot just went stale: said once, then nothing until a fresh one.
    StaleNotice,
    Nothing,
}

impl StreamState {
    /// What to send on a tick at `now` that sees `snapshot`, moving to the state it leaves.
    pub fn next(&mut self, snapshot: Arc<PublishedSnapshot>, now: Instant) -> StreamEmit {
        let freshness = snapshot.freshness(now);
        let emit = match (*self, freshness) {
            (_, SnapshotFreshness::Fresh) => StreamEmit::Snapshot(snapshot),
            (Self::Live, SnapshotFreshness::Stale) => StreamEmit::StaleNotice,
            (Self::Stale, SnapshotFreshness::Stale) => StreamEmit::Nothing,
        };
        *self = match freshness {
            SnapshotFreshness::Fresh => Self::Live,
            SnapshotFreshness::Stale => Self::Stale,
        };
        emit
    }
}

#[cfg(test)]
pub mod fixtures {
    use super::*;
    use crate::collectors::system::fixtures::{kernel_snapshot, process, raw};
    use crate::collectors::system::{RawDisk, RawNetwork, RawReadings};
    use crate::collectors::{SnapshotReceiver, SnapshotSender};
    use crate::environment::sourcing::Origin;
    use tokio::sync::watch;

    /// The fixture snapshot's `collected_at`.
    pub const COLLECTED_AT: u64 = 1_790_000_000;

    /// The fixture snapshot's execution environment.
    pub const ENVIRONMENT: ExecutionEnvironment = ExecutionEnvironment::Container {
        runtime: Some(crate::environment::ContainerRuntime::Podman),
        cgroup: crate::environment::cgroup::CgroupAccess::Unreadable(
            crate::environment::cgroup::CgroupUnreadable::V1Only,
        ),
    };

    /// Every group read from the kernel.
    pub const KERNEL_ORIGINS: ReadingOrigins = ReadingOrigins {
        cpu: Origin::Kernel,
        memory: Origin::Kernel,
        swap: Origin::Kernel,
    };

    /// `system::fixtures::raw()` with a disk, a network and processes, so no real system's
    /// readings can match it.
    fn readings() -> RawReadings {
        RawReadings {
            disks: vec![RawDisk {
                mount_point: "/fixture".into(),
                filesystem: "fixturefs".into(),
                total: 1000,
                available: 400,
            }],
            networks: vec![RawNetwork {
                interface: "fixture0".into(),
                mac_address: "00:11:22:33:44:55".into(),
                ip_addresses: vec!["192.0.2.7".into()],
                received: 10,
                transmitted: 20,
            }],
            processes: vec![process(7, 1.0, 1024), process(9, 2.0, 2048)],
            ..raw()
        }
    }

    /// A snapshot read from `readings()`, published first.
    pub fn published() -> PublishedSnapshot {
        published_as(SnapshotSeq::FIRST)
    }

    /// `published()`, published under `seq`.
    pub fn published_as(seq: SnapshotSeq) -> PublishedSnapshot {
        let mut system = kernel_snapshot(readings());
        // A fractional capacity, so no route can pass the core count off as it.
        system.cpu.capacity_cpus = 1.5;
        // A steal no other field holds, so no route can pass another figure off as it.
        system.cpu.steal_percent = Some(7.5);
        CollectedSnapshot {
            system,
            environment: ENVIRONMENT,
            origins: KERNEL_ORIGINS,
            collected_at: COLLECTED_AT,
            read_at: Instant::now(),
        }
        .published(seq)
    }

    /// `published()`, read just past the staleness bound.
    pub fn stale() -> PublishedSnapshot {
        let mut snapshot = published();
        snapshot.read_at = Instant::now()
            .checked_sub(STALENESS_BOUND + Duration::from_secs(1))
            .expect("the monotonic clock is past the staleness bound");
        snapshot
    }

    /// A receiver holding `snapshot`, and the sender that can publish past it.
    pub fn channel(snapshot: PublishedSnapshot) -> (SnapshotSender, SnapshotReceiver) {
        watch::channel(Arc::new(snapshot))
    }

    /// A receiver holding `published()`. Its sender is gone, which a reader can't tell.
    pub fn receiver() -> SnapshotReceiver {
        watch::channel(Arc::new(published())).1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_seq_follows_the_one_before() {
        let seqs: Vec<SnapshotSeq> =
            std::iter::successors(Some(SnapshotSeq::FIRST), |s| Some(s.next()))
                .take(3)
                .collect();
        assert_eq!(seqs, [SnapshotSeq(0), SnapshotSeq(1), SnapshotSeq(2)]);
    }

    #[test]
    fn a_reading_is_publishable_only_a_full_priming_interval_after_priming() {
        let primed_at = Instant::now();
        let cases = [
            ("right after priming", Duration::ZERO, Priming::Pending),
            (
                "just short of it",
                Duration::from_millis(999),
                Priming::Pending,
            ),
            ("exactly at it", PRIMING, Priming::Done),
            ("past it", Duration::from_secs(5), Priming::Done),
        ];
        for (name, wait, expected) in cases {
            assert_eq!(
                Priming::of(primed_at, primed_at + wait),
                expected,
                "case: {name}"
            );
        }
        assert_eq!(
            Priming::of(primed_at + PRIMING, primed_at),
            Priming::Pending,
            "a reading before the priming ended"
        );
    }

    #[test]
    fn a_snapshot_is_stale_only_once_past_the_bound() {
        let cases = [
            ("just read", Duration::ZERO, SnapshotFreshness::Fresh),
            (
                "exactly at the bound",
                STALENESS_BOUND,
                SnapshotFreshness::Fresh,
            ),
            (
                "just past it",
                STALENESS_BOUND + Duration::from_millis(1),
                SnapshotFreshness::Stale,
            ),
            (
                "long past it",
                Duration::from_secs(3600),
                SnapshotFreshness::Stale,
            ),
        ];
        for (name, age, expected) in cases {
            assert_eq!(SnapshotFreshness::of_age(age), expected, "case: {name}");
        }
    }

    #[test]
    fn freshness_is_measured_on_the_monotonic_clock_from_the_read() {
        // The wall clock stamp says "just now"; only `read_at` counts.
        let snapshot = fixtures::published();
        let read_at = snapshot.read_at;
        assert_eq!(
            snapshot.freshness(read_at + STALENESS_BOUND),
            SnapshotFreshness::Fresh
        );
        let past = read_at + STALENESS_BOUND + Duration::from_millis(1);
        assert_eq!(snapshot.freshness(past), SnapshotFreshness::Stale);
        // A `now` before the read (never on a monotonic clock) isn't stale.
        assert_eq!(snapshot.freshness(read_at), SnapshotFreshness::Fresh);
    }

    /// What a stream sends for each tick's freshness, starting live.
    fn emits(ticks: &[SnapshotFreshness]) -> Vec<&'static str> {
        let mut state = StreamState::Live;
        ticks
            .iter()
            .map(|freshness| {
                let snapshot = match freshness {
                    SnapshotFreshness::Fresh => fixtures::published(),
                    SnapshotFreshness::Stale => fixtures::stale(),
                };
                match state.next(Arc::new(snapshot), Instant::now()) {
                    StreamEmit::Snapshot(_) => "snapshot",
                    StreamEmit::StaleNotice => "stale",
                    StreamEmit::Nothing => "nothing",
                }
            })
            .collect()
    }

    #[test]
    fn a_stream_says_stale_once_then_waits_for_a_fresh_snapshot() {
        use SnapshotFreshness::{Fresh, Stale};
        let cases: [(&str, &[SnapshotFreshness], &[&str]); 4] = [
            (
                "fresh throughout",
                &[Fresh, Fresh],
                &["snapshot", "snapshot"],
            ),
            (
                "stale from the start",
                &[Stale, Stale],
                &["stale", "nothing"],
            ),
            (
                "going stale",
                &[Fresh, Stale, Stale, Stale],
                &["snapshot", "stale", "nothing", "nothing"],
            ),
            (
                "recovering, then stale again",
                &[Stale, Fresh, Fresh, Stale],
                &["stale", "snapshot", "snapshot", "stale"],
            ),
        ];
        for (name, ticks, expected) in cases {
            assert_eq!(emits(ticks), expected, "case: {name}");
        }
    }
}
