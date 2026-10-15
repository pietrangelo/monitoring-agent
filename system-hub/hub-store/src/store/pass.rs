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

//! The retention pass (RFC 0010 §5) as the writer runs it, one bounded batch per group commit.

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use redb::{ReadableTable, Table, WriteTransaction};

use super::blocks::{Blocks, SpanKey, span_range};
use super::handoff::Handoffs;
use super::records::{Unreadable, recorded_key};
use super::retire::{Retirement, plan_retirements, retire};
use super::{MonotonicClock, StoreStats};
use crate::block::name::BlockName;
use crate::clock::{Elapsed, retention_now};
use crate::retention::{Liveness, Owner, Policies, Tombstones, chunk_liveness};
use crate::series::{SeriesId, SeriesKey};
use crate::tables::{CHUNKS, SERIES};
use crate::tier::{SpanStart, Tier};

/// A key of `chunks`: (tier, span start, `SeriesId`, seq).
pub(super) type ChunkKey = (u8, u64, u32, u16);

/// What a pass judges a chunk in `chunks` by.
pub(super) struct World<'a> {
    pub policies: &'a Policies,
    pub tombstones: &'a Tombstones,
    pub retention_now: u64,
    /// Spans the block writer hands off: left alone until their outcome commits.
    pub in_flight: &'a BTreeSet<SpanKey>,
}

/// What the pass does to one chunk in `chunks`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChunkFate {
    Keep,
    Delete,
}

/// A chunk is deleted when it is dead, unless its span is in flight. A key off the tiers or
/// their grid is never judged dead: the pass deletes only what it can name.
pub(super) fn chunk_fate(key: ChunkKey, owner: Option<&Owner>, world: &World<'_>) -> ChunkFate {
    let Some((tier, span)) = span_of(key) else {
        return ChunkFate::Keep;
    };
    if world.in_flight.contains(&(tier, span)) {
        return ChunkFate::Keep;
    }
    let liveness = chunk_liveness(
        tier,
        span,
        owner,
        world.policies,
        world.tombstones,
        world.retention_now,
    );
    match liveness {
        Liveness::Live => ChunkFate::Keep,
        Liveness::Dead(_) => ChunkFate::Delete,
    }
}

/// The span a key of `chunks` names, if its tier and start are valid.
fn span_of((tier, start, _, _): ChunkKey) -> Option<SpanKey> {
    let tier = Tier::from_code(tier)?;
    Some((tier, SpanStart::new(tier, start)?))
}

/// How many entries one step of a pass may visit or delete: about 0.15 s of redb work (§5),
/// so the group commits keep flowing between the steps.
const BATCH: usize = 50_000;

/// Where a pass is: scanning `chunks` from a key, or retiring the files left to retire.
enum Stage {
    Chunks(ChunkKey),
    Files(VecDeque<Retirement>),
}

/// What one step of a pass did.
pub(super) enum Step {
    Working,
    /// These files' rows went in this step: unlink them once it commits.
    Retired(Vec<BlockName>),
    Finished,
}

/// What a step reads beside its transaction.
pub(super) struct Context<'a> {
    pub policies: &'a Policies,
    pub tombstones: &'a Tombstones,
    /// The head's series, by id.
    pub owners: &'a HashMap<SeriesId, SeriesKey>,
    pub blocks: &'a Blocks,
    pub handoffs: &'a mut Handoffs,
}

/// What the retention pass reports through `StoreStats`: the committed retention clock, and
/// the passes finished since the open.
pub(crate) struct PassStats {
    clock: AtomicU64,
    passes: AtomicU64,
}

impl PassStats {
    pub(super) fn new(clock: u64) -> PassStats {
        PassStats {
            clock: AtomicU64::new(clock),
            passes: AtomicU64::new(0),
        }
    }

    /// A commit landed with this retention clock, finishing `finished` passes.
    pub(super) fn committed(&self, clock: u64, finished: u64) {
        self.clock.store(clock, Ordering::SeqCst);
        self.passes.fetch_add(finished, Ordering::SeqCst);
    }

    pub(super) fn fill(&self, stats: &mut StoreStats) {
        stats.retention_clock = self.clock.load(Ordering::SeqCst);
        stats.retention_passes = self.passes.load(Ordering::SeqCst);
    }
}

/// The retention schedule (§5): the guarded retention clock (§2), when the next pass starts,
/// and the pass in progress, run one step per group commit.
pub(super) struct Retention {
    clock: u64,
    interval: Duration,
    next: Instant,
    monotonic: Arc<dyn MonotonicClock>,
    /// The monotonic clock when the last pass started; none before the first since the open.
    last_start: Option<Duration>,
    stage: Option<Stage>,
}

impl Retention {
    pub(super) fn new(
        clock: u64,
        interval: Duration,
        monotonic: Arc<dyn MonotonicClock>,
    ) -> Retention {
        Retention {
            clock,
            interval,
            next: Instant::now() + interval,
            monotonic,
            last_start: None,
            stage: None,
        }
    }

    /// The retention clock as the next commit persists it.
    pub(super) fn clock(&self) -> u64 {
        self.clock
    }

    /// Whether a pass should start: none runs, and its interval has passed.
    pub(super) fn due(&self) -> bool {
        self.stage.is_none() && Instant::now() >= self.next
    }

    /// Starts a pass: the retention clock moves (pass step 1) and `chunks` is scanned next.
    pub(super) fn begin(&mut self, hub_now: u64, system_now: u64) {
        let reading = self.monotonic.elapsed();
        let since = match self.last_start {
            None => Elapsed::SinceOpen,
            Some(last) => Elapsed::Measured(reading.saturating_sub(last)),
        };
        self.clock = retention_now(self.clock, hub_now, system_now, since);
        self.last_start = Some(reading);
        self.stage = Some(Stage::Chunks((0, 0, 0, 0)));
    }

    /// One step of the pass in progress, inside the commit's transaction.
    pub(super) fn step(
        &mut self,
        txn: &WriteTransaction,
        ctx: &mut Context<'_>,
    ) -> Result<Step, redb::Error> {
        let (next, step) = match self.stage.take() {
            None => (None, Step::Working),
            Some(Stage::Chunks(from)) => (Some(self.scan(txn, ctx, from)?), Step::Working),
            Some(Stage::Files(queue)) if queue.is_empty() => {
                self.next = Instant::now() + self.interval;
                (None, Step::Finished)
            }
            Some(Stage::Files(mut queue)) => {
                let retired = retire(txn, &mut queue, BATCH)?;
                (Some(Stage::Files(queue)), Step::Retired(retired))
            }
        };
        self.stage = next;
        Ok(step)
    }

    /// Pass step 3, one batch: dead chunks deleted from `chunks`. Once the scan is done, the
    /// spans it emptied are no longer handed off, and the files to retire are planned.
    fn scan(
        &self,
        txn: &WriteTransaction,
        ctx: &mut Context<'_>,
        from: ChunkKey,
    ) -> Result<Stage, redb::Error> {
        let scan = {
            let series = txn.open_table(SERIES)?;
            let world = self.world(ctx.policies, ctx.tombstones, ctx.handoffs.in_flight());
            // redb refusing a read fails the step after the batch, as any commit's failure.
            let failed = Cell::new(None);
            let fate = |key: ChunkKey| match owner(ctx.owners, &series, key.2) {
                Ok(owner) => chunk_fate(key, owner.as_ref(), &world),
                Err(Unreadable::Record(_)) => ChunkFate::Keep,
                Err(Unreadable::Store(e)) => {
                    failed.set(Some(e));
                    ChunkFate::Keep
                }
            };
            let scan = delete_dead_chunks(&mut txn.open_table(CHUNKS)?, from, BATCH, fate)?;
            if let Some(e) = failed.take() {
                return Err(e);
            }
            scan
        };
        if let Scan::More(next) = scan {
            return Ok(Stage::Chunks(next));
        }
        forget_emptied(txn, ctx.handoffs)?;
        let world = self.world(ctx.policies, ctx.tombstones, ctx.handoffs.in_flight());
        let queue = plan_retirements(txn, ctx.blocks, ctx.owners.values(), &world)?;
        Ok(Stage::Files(queue))
    }

    fn world<'a>(
        &self,
        policies: &'a Policies,
        tombstones: &'a Tombstones,
        in_flight: &'a BTreeSet<SpanKey>,
    ) -> World<'a> {
        World {
            policies,
            tombstones,
            retention_now: self.clock,
            in_flight,
        }
    }
}

/// Whose a series is: from the head, else from its record (a series the head doesn't hold);
/// `None` when neither names it, an error when its record doesn't decode (the chunk is kept).
fn owner(
    owners: &HashMap<SeriesId, SeriesKey>,
    series: &Table<u32, &[u8]>,
    id: u32,
) -> Result<Option<Owner>, Unreadable> {
    let to_owner = |key: &SeriesKey| Owner {
        system: key.system.clone(),
        generation: key.generation,
    };
    if let Some(key) = owners.get(&SeriesId(id)) {
        return Ok(Some(to_owner(key)));
    }
    Ok(recorded_key(series, id)?.map(|key| to_owner(&key)))
}

/// The idle spans the scan left with no chunk: nothing is left to hand off.
fn forget_emptied(txn: &WriteTransaction, handoffs: &mut Handoffs) -> Result<(), redb::Error> {
    let chunks = txn.open_table(CHUNKS)?;
    for span in handoffs.idle() {
        if chunks.range(span_range(span.0, span.1))?.next().is_none() {
            handoffs.emptied(span);
        }
    }
    Ok(())
}

/// Where a bounded scan of `chunks` stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Scan {
    /// The next key to visit.
    More(ChunkKey),
    Done,
}

/// Visits at most `limit` entries of `chunks` from `from` on and deletes those `fate` deletes.
pub(super) fn delete_dead_chunks(
    table: &mut Table<ChunkKey, &[u8]>,
    from: ChunkKey,
    limit: usize,
    fate: impl Fn(ChunkKey) -> ChunkFate,
) -> Result<Scan, redb::Error> {
    let visited = table
        .range(from..)?
        .take(limit.saturating_add(1))
        .map(|entry| entry.map(|(key, _)| key.value()))
        .collect::<Result<Vec<ChunkKey>, _>>()?;
    let (batch, rest) = visited.split_at(visited.len().min(limit));
    for key in batch.iter().filter(|k| fate(**k) == ChunkFate::Delete) {
        table.remove(*key)?;
    }
    Ok(rest.first().map_or(Scan::Done, |next| Scan::More(*next)))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use redb::{Database, ReadableDatabase, ReadableTable};

    use super::*;
    use crate::name::{Generation, SystemKey};
    use crate::retention::{RetentionChange, RetentionPolicy, TierPeriod};
    use crate::series::SeriesRecord;
    use crate::tables::CHUNKS;
    use crate::tier::{SpanStart, Tier};

    const HOUR: u64 = 3_600;
    /// A day boundary: on every tier's grid.
    const NOW: u64 = 1_800_057_600;

    fn owner(generation: u64) -> Owner {
        Owner {
            system: SystemKey::try_from(&b"s"[..]).expect("key"),
            generation: Generation::new(generation),
        }
    }

    fn span(tier: Tier, start: u64) -> SpanKey {
        (tier, SpanStart::new(tier, start).expect("grid"))
    }

    #[test]
    fn a_dead_chunk_is_deleted_unless_its_span_is_in_flight_or_its_key_is_unnamed() {
        let raw_hour = TierPeriod::new(Tier::Raw, Duration::from_secs(HOUR)).expect("bounds");
        let mut policies = Policies::new(RetentionPolicy::default().with(raw_hour));
        let raw_two_days =
            TierPeriod::new(Tier::Raw, Duration::from_secs(48 * HOUR)).expect("bounds");
        let longer = RetentionChange::new([raw_two_days]).expect("one tier");
        let long = Owner {
            system: SystemKey::try_from(&b"long"[..]).expect("key"),
            generation: Generation::new(1),
        };
        policies.change(&long.system, &longer, Duration::from_secs(600));
        let mut tombstones = Tombstones::default();
        tombstones.insert(owner(9));
        let old = NOW - 3 * HOUR;
        let in_flight = BTreeSet::from([span(Tier::Raw, NOW - 5 * HOUR)]);
        let world = World {
            policies: &policies,
            tombstones: &tombstones,
            retention_now: NOW,
            in_flight: &in_flight,
        };
        let (raw, minute) = (Tier::Raw.code(), Tier::Minute.code());
        let cases = [
            (
                "a recent chunk",
                (raw, NOW - HOUR, 1, 0),
                Some(owner(1)),
                ChunkFate::Keep,
            ),
            (
                "past its period",
                (raw, old, 1, 0),
                Some(owner(1)),
                ChunkFate::Delete,
            ),
            (
                "ending exactly at the clock minus the period",
                (raw, NOW - 2 * HOUR, 1, 0),
                Some(owner(1)),
                ChunkFate::Delete,
            ),
            (
                "a system whose override keeps it",
                (raw, old, 1, 0),
                Some(long.clone()),
                ChunkFate::Keep,
            ),
            (
                "a later seq past its period",
                (raw, old, 1, 7),
                Some(owner(1)),
                ChunkFate::Delete,
            ),
            ("unmapped", (raw, NOW - HOUR, 1, 0), None, ChunkFate::Delete),
            (
                "tombstoned",
                (raw, NOW - HOUR, 1, 0),
                Some(owner(9)),
                ChunkFate::Delete,
            ),
            (
                "past its period, in flight",
                (raw, NOW - 5 * HOUR, 1, 0),
                Some(owner(1)),
                ChunkFate::Keep,
            ),
            (
                "tombstoned, in flight",
                (raw, NOW - 5 * HOUR, 1, 0),
                Some(owner(9)),
                ChunkFate::Keep,
            ),
            (
                "unmapped, in flight",
                (raw, NOW - 5 * HOUR, 1, 0),
                None,
                ChunkFate::Keep,
            ),
            (
                "the same start in another tier is not in flight",
                (minute, NOW - 5 * HOUR, 1, 0),
                None,
                ChunkFate::Delete,
            ),
            (
                "a minute chunk within its own period",
                (minute, old, 1, 0),
                Some(owner(1)),
                ChunkFate::Keep,
            ),
            ("no tier", (9, old, 1, 0), None, ChunkFate::Keep),
            (
                "off its tier's grid",
                (raw, old + 1, 1, 0),
                None,
                ChunkFate::Keep,
            ),
        ];
        for (name, key, owner, expected) in cases {
            assert_eq!(chunk_fate(key, owner.as_ref(), &world), expected, "{name}");
        }
    }

    fn keys(db: &Database) -> Vec<ChunkKey> {
        let read = db.begin_read().expect("read");
        let table = read.open_table(CHUNKS).expect("chunks");
        table
            .iter()
            .expect("iter")
            .map(|e| e.expect("entry").0.value())
            .collect()
    }

    #[test]
    fn a_scan_visits_at_most_its_limit_and_resumes_where_it_stopped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::create(dir.path().join("t.redb")).expect("db");
        let key = |id: u32| (0u8, NOW, id, 0u16);
        let txn = db.begin_write().expect("write");
        {
            let mut table = txn.open_table(CHUNKS).expect("chunks");
            for id in 0..5 {
                table.insert(key(id), [1u8].as_slice()).expect("insert");
            }
        }
        txn.commit().expect("commit");
        let odd = |k: ChunkKey| match k.2 % 2 {
            1 => ChunkFate::Delete,
            _ => ChunkFate::Keep,
        };
        let steps = [
            (key(0), Scan::More(key(2))),
            (key(2), Scan::More(key(4))),
            (key(4), Scan::Done),
        ];
        for (from, expected) in steps {
            let txn = db.begin_write().expect("write");
            let scan = {
                let mut table = txn.open_table(CHUNKS).expect("chunks");
                delete_dead_chunks(&mut table, from, 2, odd).expect("scan")
            };
            txn.commit().expect("commit");
            assert_eq!(scan, expected, "from {from:?}");
        }
        assert_eq!(keys(&db), vec![key(0), key(2), key(4)]);
        let txn = db.begin_write().expect("write");
        let mut table = txn.open_table(CHUNKS).expect("chunks");
        assert_eq!(
            delete_dead_chunks(&mut table, key(5), 2, odd).expect("scan"),
            Scan::Done,
            "nothing past the last key"
        );
    }

    #[test]
    fn a_series_is_owned_as_the_head_or_its_record_names_it_and_unreadable_otherwise() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::create(dir.path().join("t.redb")).expect("db");
        let key = |system: &[u8], generation: u64| SeriesKey {
            system: SystemKey::try_from(system).expect("system"),
            generation: Generation::new(generation),
            metric: crate::name::MetricName::try_from("load1").expect("metric"),
        };
        let record = |key: SeriesKey| {
            SeriesRecord {
                key,
                kind: crate::value::ValueKind::Load,
                last_spans: [None; 3],
            }
            .to_bytes()
        };
        let txn = db.begin_write().expect("write");
        let mut series = txn.open_table(SERIES).expect("series");
        series.insert(1, [0xffu8, 0].as_slice()).expect("head's");
        series
            .insert(2, record(key(b"rec", 4)).as_slice())
            .expect("good");
        series.insert(3, [0xffu8, 0].as_slice()).expect("garbage");
        series.insert(4, [1u8, 0xff].as_slice()).expect("bad body");
        series
            .insert(6, record(key(b"rec", 9)).as_slice())
            .expect("stale");
        let head = HashMap::from([
            (SeriesId(1), key(b"head", 2)),
            (SeriesId(6), key(b"head", 3)),
        ]);
        let named = |system: &[u8], generation| Owner {
            system: SystemKey::try_from(system).expect("system"),
            generation: Generation::new(generation),
        };
        // `Some(owner)` names one, `None` is unreadable.
        let cases = [
            (
                "the head's, over its unreadable record",
                1,
                Some(Some(named(b"head", 2))),
            ),
            (
                "the head's, over its readable record",
                6,
                Some(Some(named(b"head", 3))),
            ),
            (
                "a record the head doesn't hold",
                2,
                Some(Some(named(b"rec", 4))),
            ),
            ("garbage", 3, None),
            ("a known version with a bad body", 4, None),
            ("no record", 5, Some(None)),
        ];
        for (name, id, expected) in cases {
            let found = match super::owner(&head, &series, id) {
                Ok(found) => Some(found),
                Err(Unreadable::Record(_)) => None,
                Err(Unreadable::Store(e)) => panic!("{name}: {e}"),
            };
            assert_eq!(found, expected, "{name}");
        }
    }
}
