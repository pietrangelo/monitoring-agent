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

pub mod containers;
pub mod environment;
pub mod packages;
pub mod ports;
pub mod sampler;
pub mod services;
pub mod system;

use crate::alerts::{Reading, Readings};
use crate::environment::ExecutionEnvironment;
use crate::environment::sourcing::{Origin, SourcingHistory};
use crate::environment::usage::{LoadAverage, Percent};
use crate::snapshot::{CollectedSnapshot, PRIMING, PublishedSnapshot, STALENESS_BOUND};
use crate::state::AppState;
use sampler::{Gather, Sampled, Sampler};
use std::sync::Arc;
use tokio::sync::watch;
use tokio::task::{JoinError, spawn_blocking};
use tokio::time::{Duration, Instant, Interval, MissedTickBehavior, interval_at};

/// Where the background collector publishes each snapshot, and where every reader reads it.
pub type SnapshotSender = watch::Sender<Arc<PublishedSnapshot>>;
pub type SnapshotReceiver = watch::Receiver<Arc<PublishedSnapshot>>;

/// Now, on the runtime's monotonic clock (which tests can pause and advance), as the plain
/// `std` instant the domain takes.
pub fn monotonic_now() -> std::time::Instant {
    Instant::now().into_std()
}

/// How often the background collector reads the system.
pub const COLLECT_PERIOD: Duration = Duration::from_secs(2);

/// The startup snapshot: primes a sampler in `environment` from `source`, waits out the
/// priming interval and reads, all off the runtime. `Err` if the source panicked.
pub async fn first_snapshot<G: Gather>(
    source: G,
    environment: ExecutionEnvironment,
) -> Result<(Sampler<G>, Sampled), JoinError> {
    let mut sampler = spawn_blocking(move || Sampler::prime(source, environment)).await?;
    loop {
        tokio::time::sleep(PRIMING).await;
        match read_off_runtime(sampler, SourcingHistory::new()).await? {
            (sampler, Some(sampled)) => return Ok((sampler, sampled)),
            (early, None) => sampler = early,
        }
    }
}

/// The sampler the collector reads with: none after a panic whose rebuild panicked too.
enum Slot<G> {
    Ready(Sampler<G>),
    Broken,
}

/// Background task: every `COLLECT_PERIOD`, reads the system off the runtime, publishes the
/// snapshot, records it in history and evaluates alerts on it. The first tick is a full
/// period after the startup snapshot `publisher` already holds. A panic in a tick rebuilds
/// the sampler from `rebuild`, in the startup snapshot's environment; the published snapshot
/// stays the previous one meanwhile. The sourcing `history` lives here, not in the sampler,
/// so a rebuilt sampler inherits every group's lineage and carry.
pub async fn background_collector<G, F>(
    state: Arc<AppState>,
    publisher: SnapshotSender,
    sampler: Sampler<G>,
    mut history: SourcingHistory,
    rebuild: F,
) where
    G: Gather,
    F: Fn() -> G + Send + Clone + 'static,
{
    let startup = publisher.borrow().clone();
    record(&state, &startup);
    let environment = &startup.environment;
    let mut tick = interval_at(Instant::now() + COLLECT_PERIOD, COLLECT_PERIOD);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut slot = Slot::Ready(sampler);
    loop {
        tick.tick().await;
        slot = match slot {
            Slot::Ready(sampler) => {
                let stale_at = Instant::from_std(publisher.borrow().read_at + STALENESS_BOUND);
                match watched(read_off_runtime(sampler, history.clone()), stale_at).await {
                    Ok((sampler, sampled)) => {
                        if let Some(sampled) = sampled {
                            history = sampled.history;
                            publish(&state, &publisher, sampled.snapshot);
                        }
                        Slot::Ready(sampler)
                    }
                    Err(err) => {
                        tracing::error!("The system collector panicked: {err}; rebuilding it");
                        let make = rebuild.clone();
                        rebuild_sampler(make, environment.clone(), &mut tick, Rebuild::AfterPanic)
                            .await
                    }
                }
            }
            Slot::Broken => {
                let make = rebuild.clone();
                rebuild_sampler(make, environment.clone(), &mut tick, Rebuild::Retry).await
            }
        };
    }
}

/// Awaits `read`, logging once if it is still running when the published snapshot goes
/// stale at `stale_at`. The read is never abandoned, so blocking threads can't pile up.
async fn watched<T>(read: impl Future<Output = T>, stale_at: Instant) -> T {
    tokio::pin!(read);
    if stale_at > Instant::now() {
        tokio::select! {
            out = &mut read => return out,
            () = tokio::time::sleep_until(stale_at) => tracing::error!(
                "Reading the system is taking too long; the snapshot is stale until it ends"
            ),
        }
    }
    read.await
}

/// Why the collector is building a sampler: it decides what is worth a log line.
enum Rebuild {
    /// The sampler panicked this tick.
    AfterPanic,
    /// The last rebuild panicked too.
    Retry,
}

/// Primes a new sampler off the runtime. On success the next tick is a full period away, so
/// the new sampler's first reading is taken well after its priming one. Logs a failure only
/// after a panic, and a success only after a failure: a broken source logs once, not per tick.
async fn rebuild_sampler<G, F>(
    make: F,
    environment: ExecutionEnvironment,
    tick: &mut Interval,
    why: Rebuild,
) -> Slot<G>
where
    G: Gather,
    F: FnOnce() -> G + Send + 'static,
{
    match (
        spawn_blocking(move || Sampler::prime(make(), environment)).await,
        why,
    ) {
        (Ok(sampler), Rebuild::AfterPanic) => {
            tick.reset();
            Slot::Ready(sampler)
        }
        (Ok(sampler), Rebuild::Retry) => {
            tracing::info!("The system collector is reading again");
            tick.reset();
            Slot::Ready(sampler)
        }
        (Err(err), Rebuild::AfterPanic) => {
            tracing::error!("Rebuilding the system collector failed: {err}; retrying each tick");
            Slot::Broken
        }
        (Err(_), Rebuild::Retry) => Slot::Broken,
    }
}

/// One reading off the runtime, its groups chosen against `history`.
async fn read_off_runtime<G: Gather>(
    mut sampler: Sampler<G>,
    history: SourcingHistory,
) -> Result<(Sampler<G>, Option<Sampled>), JoinError> {
    spawn_blocking(move || {
        let sampled = sampler.read(&history);
        (sampler, sampled)
    })
    .await
}

/// Publishes `collected` under the seq after the last published one, and records it.
fn publish(state: &AppState, publisher: &SnapshotSender, collected: CollectedSnapshot) {
    let seq = publisher.borrow().seq.next();
    let snapshot = Arc::new(collected.published(seq));
    record(state, &snapshot);
    publisher.send_replace(snapshot);
}

/// Adds `snapshot` to the history and evaluates the alert rules on it, at its `collected_at`.
fn record(state: &AppState, snapshot: &PublishedSnapshot) {
    let (snap, now) = (&snapshot.system, snapshot.collected_at);
    {
        let mut hist = state.history.write();
        hist.push_cpu(snap.cpu.usage_percent, now);
        hist.push_memory(snap.memory.usage_percent, now);
        hist.push_swap(snap.swap.usage_percent, now);
        hist.push_load1(snap.load_average.one as f32, now);
        hist.push_load5(snap.load_average.five as f32, now);
        hist.push_load15(snap.load_average.fifteen as f32, now);
        for disk in &snap.disks {
            hist.push_disk(&disk.mount_point, disk.usage_percent, now);
        }
    }
    let new_alerts = state
        .alert_manager
        .write()
        .evaluate(&alert_readings(snapshot), now);
    for alert in &new_alerts {
        tracing::warn!("🚨 ALERT: {}", alert.message);
    }
}

/// The readings the alert rules see in `snapshot`: each group's as its origin says.
fn alert_readings(snapshot: &PublishedSnapshot) -> Readings {
    let (snap, origins) = (&snapshot.system, &snapshot.origins);
    let load = |value: f64| LoadAverage::new(value as f32);
    Readings {
        cpu: group_reading(origins.cpu, snap.cpu.usage_percent),
        memory: group_reading(origins.memory, snap.memory.usage_percent),
        swap: group_reading(origins.swap, snap.swap.usage_percent),
        disk_usages: snap
            .disks
            .iter()
            .map(|d| (d.mount_point.clone(), d.usage_percent))
            .collect(),
        load1: load(snap.load_average.one),
        load5: load(snap.load_average.five),
        load15: load(snap.load_average.fifteen),
        cpu_cores: snap.cpu.logical_cores,
    }
}

/// A group's percentage as its origin makes it: measured when read on the tick, else
/// carried or unavailable.
fn group_reading(origin: Origin, value: f32) -> Reading<Percent> {
    match origin {
        Origin::Cgroup | Origin::Kernel => percent_reading(value),
        Origin::Carried => Reading::Carried,
        Origin::Unavailable => Reading::Unavailable,
    }
}

/// A measured percentage, or `Unavailable` when the snapshot's value isn't a number (a
/// host that lists no CPU).
fn percent_reading(value: f32) -> Reading<Percent> {
    Percent::saturating(value).map_or(Reading::Unavailable, Reading::Measured)
}

#[cfg(test)]
mod tests {
    use super::sampler::fakes::{FakeSource, SINCE_BOOT_CPU};
    use super::*;
    use crate::models::DiskInfo;
    use crate::snapshot::{SnapshotSeq, fixtures};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::sync::watch;

    /// One publish: when, relative to the collector's start, under which seq, with which CPU.
    type Publish = (Duration, SnapshotSeq, f32);

    /// Starts the collector with `sampler`, and returns every publish until `until`.
    async fn run<F>(sampler: Sampler<FakeSource>, rebuild: F, until: Duration) -> Vec<Publish>
    where
        F: Fn() -> FakeSource + Send + Clone + 'static,
    {
        let (publisher, mut snapshots): (SnapshotSender, SnapshotReceiver) =
            watch::channel(Arc::new(fixtures::published()));
        let state = AppState::new(snapshots.clone());
        let start = Instant::now();
        let collector = tokio::spawn(background_collector(
            state,
            publisher,
            sampler,
            SourcingHistory::new(),
            rebuild,
        ));
        let mut publishes = Vec::new();
        while let Ok(Ok(())) = tokio::time::timeout_at(start + until, snapshots.changed()).await {
            let snap = snapshots.borrow_and_update().clone();
            publishes.push((start.elapsed(), snap.seq, snap.system.cpu.usage_percent));
        }
        collector.abort();
        publishes
    }

    /// A sampler primed as at startup: a full priming interval ago.
    async fn primed(source: FakeSource) -> Sampler<FakeSource> {
        let sampler = Sampler::prime(source, fixtures::ENVIRONMENT);
        tokio::time::advance(PRIMING).await;
        sampler
    }

    #[test]
    fn alert_readings_carry_each_snapshot_value_to_its_metric() {
        let mut published = fixtures::published();
        let snap = &mut published.system;
        snap.cpu.usage_percent = 11.5;
        snap.memory.usage_percent = 22.5;
        snap.swap.usage_percent = 33.5;
        snap.load_average.one = 1.25;
        snap.load_average.five = 150.0;
        snap.load_average.fifteen = 3.75;
        snap.cpu.logical_cores = 6;
        snap.disks = [("/", 44.5), ("/data", 55.5)]
            .map(|(mount_point, usage_percent)| DiskInfo {
                mount_point: mount_point.into(),
                filesystem: "fixturefs".into(),
                total_bytes: 0,
                used_bytes: 0,
                free_bytes: 0,
                total_display: String::new(),
                used_display: String::new(),
                usage_percent,
            })
            .into();

        let readings = alert_readings(&published);
        let percent = |v| Reading::Measured(Percent::saturating(v).expect("a number"));
        assert_eq!(readings.cpu, percent(11.5), "cpu");
        assert_eq!(readings.memory, percent(22.5), "memory");
        assert_eq!(readings.swap, percent(33.5), "swap");
        assert_eq!(readings.load1, LoadAverage::new(1.25), "load1");
        assert_eq!(readings.load5, LoadAverage::new(150.0), "load5");
        assert_eq!(readings.load15, LoadAverage::new(3.75), "load15");
        assert_eq!(readings.cpu_cores, 6, "cores");
        let disks = HashMap::from([("/".to_string(), 44.5), ("/data".to_string(), 55.5)]);
        assert_eq!(readings.disk_usages, disks, "each mount point's usage");
    }

    #[test]
    fn a_snapshot_percentage_becomes_a_reading() {
        // (name, snapshot value, expected)
        let cases = [
            (
                "inside the scale",
                42.5,
                Percent::saturating(42.5).map(Reading::Measured),
            ),
            (
                "over the scale",
                100.4,
                Percent::saturating(100.0).map(Reading::Measured),
            ),
            (
                "under the scale",
                -5.0,
                Percent::saturating(0.0).map(Reading::Measured),
            ),
            (
                "infinite",
                f32::INFINITY,
                Percent::saturating(100.0).map(Reading::Measured),
            ),
            (
                "not a number, as with no CPU listed",
                f32::NAN,
                Some(Reading::Unavailable),
            ),
        ];
        for (name, value, expected) in cases {
            assert_eq!(Some(percent_reading(value)), expected, "{name}");
        }
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn seq(n: u32) -> SnapshotSeq {
        (0..n).fold(SnapshotSeq::FIRST, |s, _| s.next())
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_tick_is_a_full_period_after_startup() {
        let sampler = primed(FakeSource::new()).await;
        let publishes = run(sampler, FakeSource::new, secs(5)).await;
        assert_eq!(
            publishes,
            [(secs(2), seq(1), 1.0), (secs(4), seq(2), 2.0)],
            "a tick every period from startup, none at once"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_panicking_tick_rebuilds_the_sampler_which_never_publishes_its_priming() {
        // The first sampler's third reading (the t=4 s tick) panics.
        let sampler = primed(FakeSource::panicking_on(3)).await;
        let publishes = run(sampler, FakeSource::new, secs(9)).await;
        assert_eq!(
            publishes,
            [
                (secs(2), seq(1), 1.0),
                (secs(6), seq(2), 1.0),
                (secs(8), seq(3), 2.0),
            ],
            "nothing at the panic, then the rebuilt sampler's readings, seqs running on"
        );
        assert!(
            publishes.iter().all(|p| p.2 != SINCE_BOOT_CPU),
            "no priming reading is published"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_rebuilt_sampler_reads_in_the_startup_environment() {
        let (publisher, mut snapshots) = fixtures::channel(fixtures::published());
        let state = AppState::new(snapshots.clone());
        let sampler = primed(FakeSource::panicking_on(3)).await;
        let collector = tokio::spawn(background_collector(
            state,
            publisher,
            sampler,
            SourcingHistory::new(),
            FakeSource::new,
        ));
        let mut environments = Vec::new();
        let until = Instant::now() + secs(9);
        while let Ok(Ok(())) = tokio::time::timeout_at(until, snapshots.changed()).await {
            let snap = snapshots.borrow_and_update().clone();
            environments.push((snap.seq, snap.environment.clone()));
        }
        collector.abort();
        assert_eq!(
            environments,
            [1, 2, 3].map(|n| (seq(n), fixtures::ENVIRONMENT)),
            "before the panic and after the rebuild"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_rebuilt_sampler_inherits_every_groups_lineage() {
        use super::sampler::fakes::{CgroupSource, v2_container};
        let (publisher, mut snapshots) = fixtures::channel(fixtures::published());
        let state = AppState::new(snapshots.clone());
        let sampler = Sampler::prime(CgroupSource::panicking_on(3), v2_container());
        tokio::time::advance(PRIMING).await;
        // The rebuilt sampler's memory.stat is malformed from its first reading.
        let rebuild = || CgroupSource::memory_failing_from(1);
        let collector = tokio::spawn(background_collector(
            state,
            publisher,
            sampler,
            SourcingHistory::new(),
            rebuild,
        ));
        let mut memory = Vec::new();
        let until = Instant::now() + secs(9);
        while let Ok(Ok(())) = tokio::time::timeout_at(until, snapshots.changed()).await {
            let snap = snapshots.borrow_and_update().clone();
            memory.push((
                snap.seq,
                snap.origins.memory,
                snap.system.memory.total_bytes,
            ));
        }
        collector.abort();
        assert_eq!(
            memory,
            [
                (seq(1), Origin::Cgroup, 1000),
                (seq(2), Origin::Carried, 1000),
                (seq(3), Origin::Carried, 1000),
            ],
            "carried after the rebuild, never the kernel's 4096"
        );
    }

    #[test]
    fn alert_readings_follow_each_groups_origin() {
        use crate::environment::sourcing::ReadingOrigins;
        let mut published = fixtures::published();
        published.system.cpu.usage_percent = 11.5;
        published.system.memory.usage_percent = 22.5;
        published.system.swap.usage_percent = 33.5;
        let measured = |v| Reading::Measured(Percent::saturating(v).expect("a number"));
        // (name, origin, expected cpu reading)
        let cases = [
            ("from the cgroup", Origin::Cgroup, measured(11.5)),
            ("from the kernel", Origin::Kernel, measured(11.5)),
            ("carried", Origin::Carried, Reading::Carried),
            ("unavailable", Origin::Unavailable, Reading::Unavailable),
        ];
        for (name, origin, expected) in cases {
            published.origins = ReadingOrigins {
                cpu: origin,
                memory: origin,
                swap: origin,
            };
            let readings = alert_readings(&published);
            let expect = |v| match expected {
                Reading::Measured(_) => measured(v),
                Reading::Carried => Reading::Carried,
                Reading::Unavailable => Reading::Unavailable,
            };
            assert_eq!(readings.cpu, expect(11.5), "{name}: cpu");
            assert_eq!(readings.memory, expect(22.5), "{name}: memory");
            assert_eq!(readings.swap, expect(33.5), "{name}: swap");
        }
        published.origins.memory = Origin::Carried;
        published.origins.cpu = Origin::Cgroup;
        let mixed = alert_readings(&published);
        assert_eq!(
            (mixed.cpu, mixed.memory),
            (measured(11.5), Reading::Carried),
            "per group"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_rebuild_is_retried_once_a_tick() {
        let builds = Arc::new(AtomicU32::new(0));
        let counted = builds.clone();
        // The first three rebuilds panic while priming; the fourth works.
        let rebuild = move || match counted.fetch_add(1, Ordering::SeqCst) + 1 {
            1..=3 => FakeSource::panicking_on(1),
            _ => FakeSource::new(),
        };
        let sampler = primed(FakeSource::panicking_on(3)).await;
        let publishes = run(sampler, rebuild, secs(13)).await;
        assert_eq!(
            publishes,
            [(secs(2), seq(1), 1.0), (secs(12), seq(2), 1.0)],
            "rebuilds at 4, 6, 8 and 10 s; the one at 10 s publishes a period later"
        );
        assert_eq!(
            builds.load(Ordering::SeqCst),
            4,
            "one rebuild a tick, no loop"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_outlasting_the_staleness_bound_is_waited_for_not_abandoned() {
        let start = Instant::now();
        let hung = async {
            tokio::time::sleep(STALENESS_BOUND * 2).await;
            "the reading"
        };
        assert_eq!(watched(hung, start + STALENESS_BOUND).await, "the reading");
        assert_eq!(start.elapsed(), STALENESS_BOUND * 2, "awaited to its end");
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_snapshot_is_read_a_priming_interval_after_priming() {
        let start = monotonic_now();
        let (
            _,
            Sampled {
                snapshot: first, ..
            },
        ) = first_snapshot(FakeSource::new(), fixtures::ENVIRONMENT)
            .await
            .expect("no panic");
        assert_eq!(
            first.system.cpu.usage_percent, 1.0,
            "not the priming reading"
        );
        assert_eq!(first.read_at - start, PRIMING);
    }
}
