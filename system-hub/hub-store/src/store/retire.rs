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

//! Retention pass step 4 (RFC 0010 §5), the redb side: each block file's facts read from its
//! `blocks` row and `block_generations` entries, planned by the pure `plan_files`, and the
//! files to retire removed in bounded batches. The files are unlinked after the commit.

use std::collections::{BTreeMap, HashMap, VecDeque};

use redb::{ReadableTable, WriteTransaction};

use super::blocks::{Blocks, SpanKey};
use super::pass::World;
use super::records::Unreadable;
use crate::block::name::BlockName;
use crate::block::record::BlockRecord;
use crate::name::{Generation, SystemKey};
use crate::retention::{FileCondition, FileFacts, Holder, Owner, plan_files};
use crate::series::{SeriesKey, SeriesRecord};
use crate::tables::{BLOCK_GENERATIONS, BLOCKS, PENDING_UNLINKS, SERIES};
use crate::tier::{SpanStart, Tier};

/// A file to retire: its name and the generations its `block_generations` entries name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Retirement {
    pub name: BlockName,
    generations: Vec<Generation>,
}

impl Retirement {
    /// The entries its transaction removes or inserts: the row, the pending unlink, and one
    /// per generation.
    fn cost(&self) -> usize {
        2 + self.generations.len()
    }
}

/// Every file's row and holders, as the pass reads them.
struct Inventory {
    rows: BTreeMap<SpanKey, BlockRecord>,
    holders: BTreeMap<SpanKey, Vec<(Generation, u64)>>,
}

/// The files whose every chunk is dead at the world's retention clock. `owners` is the head's
/// series; a generation it doesn't name is looked up in `series`.
pub(super) fn plan_retirements<'a>(
    txn: &WriteTransaction,
    blocks: &Blocks,
    owners: impl Iterator<Item = &'a SeriesKey>,
    world: &World<'_>,
) -> Result<VecDeque<Retirement>, redb::Error> {
    let inventory = read_inventory(txn)?;
    let systems = match systems_of(txn, owners, &inventory) {
        Ok(systems) => systems,
        Err(Unreadable::Store(e)) => return Err(e),
        // A holder the pass can't name might be live: no file is retired on its account.
        Err(Unreadable::Record(_)) => return Ok(VecDeque::new()),
    };
    let facts: Vec<FileFacts> = inventory
        .rows
        .iter()
        .map(|(span, record)| facts(*span, record, &inventory, &systems, blocks, world))
        .collect();
    let plan = plan_files(
        &facts,
        world.policies,
        world.tombstones,
        world.retention_now,
    );
    Ok(plan
        .retire
        .into_iter()
        .filter_map(|span| retirement(span, &inventory))
        .collect())
}

fn retirement(span: SpanKey, inventory: &Inventory) -> Option<Retirement> {
    let record = inventory.rows.get(&span)?;
    let generations = inventory
        .holders
        .get(&span)
        .map(|h| h.iter().map(|(g, _)| *g).collect())
        .unwrap_or_default();
    Some(Retirement {
        name: BlockName {
            tier: span.0,
            span: span.1,
            rewrite: record.rewrite,
        },
        generations,
    })
}

fn facts(
    span: SpanKey,
    record: &BlockRecord,
    inventory: &Inventory,
    systems: &HashMap<Generation, SystemKey>,
    blocks: &Blocks,
    world: &World<'_>,
) -> FileFacts {
    let holders = inventory.holders.get(&span).map_or(Vec::new(), |held| {
        held.iter()
            .map(|(generation, bytes)| Holder {
                owner: systems.get(generation).map(|system| Owner {
                    system: system.clone(),
                    generation: *generation,
                }),
                bytes: *bytes,
            })
            .collect()
    });
    let condition = match world.in_flight.contains(&span) {
        true => FileCondition::InFlight,
        false => blocks.condition(span),
    };
    FileFacts {
        tier: span.0,
        span: span.1,
        length: record.length,
        condition,
        holders,
    }
}

/// A stored (tier, span start) pair, if it names a span.
fn span_key(tier: u8, start: u64) -> Option<SpanKey> {
    let tier = Tier::from_code(tier)?;
    Some((tier, SpanStart::new(tier, start)?))
}

/// The `blocks` rows and `block_generations` entries. A row or entry that doesn't decode, or
/// a row whose holders are unrecorded, is left out: the pass retires only what it can name.
fn read_inventory(txn: &WriteTransaction) -> Result<Inventory, redb::Error> {
    let mut rows = BTreeMap::new();
    for entry in txn.open_table(BLOCKS)?.iter()? {
        let (key, value) = entry?;
        let (tier, start) = key.value();
        let record = BlockRecord::from_bytes(value.value()).ok();
        if let (Some(span), Some(record)) = (span_key(tier, start), record) {
            rows.insert(span, record);
        }
    }
    let mut holders: BTreeMap<SpanKey, Vec<(Generation, u64)>> = BTreeMap::new();
    for entry in txn.open_table(BLOCK_GENERATIONS)?.iter()? {
        let (key, bytes) = entry?;
        let (generation, tier, start) = key.value();
        if let Some(span) = span_key(tier, start) {
            let held = (Generation::new(generation), bytes.value());
            holders.entry(span).or_default().push(held);
        }
    }
    // A row counting chunks with no entries naming their holders is incomplete: no facts.
    rows.retain(|span, record| record.chunk_count == 0 || holders.contains_key(span));
    Ok(Inventory { rows, holders })
}

/// Each generation the files hold, to its system: from the head, then from `series` for a
/// generation the head doesn't name. One no series names stays unmapped; a record that
/// doesn't decode is unreadable, never unmapped.
fn systems_of<'a>(
    txn: &WriteTransaction,
    owners: impl Iterator<Item = &'a SeriesKey>,
    inventory: &Inventory,
) -> Result<HashMap<Generation, SystemKey>, Unreadable> {
    let mut systems: HashMap<Generation, SystemKey> = owners
        .map(|key| (key.generation, key.system.clone()))
        .collect();
    let unnamed = inventory
        .holders
        .values()
        .flatten()
        .any(|(generation, _)| !systems.contains_key(generation));
    if unnamed {
        for entry in txn.open_table(SERIES)?.iter()? {
            let record = SeriesRecord::from_bytes(entry?.1.value()).map_err(Unreadable::Record)?;
            systems
                .entry(record.key.generation)
                .or_insert(record.key.system);
        }
    }
    Ok(systems)
}

/// Retires files from the front of `queue` while `limit` entries allow, at least one: each
/// file's row and `block_generations` entries go, and it joins `pending_unlinks`.
pub(super) fn retire(
    txn: &WriteTransaction,
    queue: &mut VecDeque<Retirement>,
    limit: usize,
) -> Result<Vec<BlockName>, redb::Error> {
    let (mut rows, mut generations, mut pending) = (
        txn.open_table(BLOCKS)?,
        txn.open_table(BLOCK_GENERATIONS)?,
        txn.open_table(PENDING_UNLINKS)?,
    );
    let mut retired = Vec::new();
    let mut spent = 0;
    while let Some(next) = queue.front() {
        if !retired.is_empty() && spent + next.cost() > limit {
            break;
        }
        let Some(file) = queue.pop_front() else { break };
        spent += file.cost();
        let (tier, span) = (file.name.tier.code(), file.name.span.get());
        rows.remove((tier, span))?;
        for generation in &file.generations {
            generations.remove((generation.get(), tier, span))?;
        }
        pending.insert((tier, span, file.name.rewrite.0), ())?;
        retired.push(file.name);
    }
    Ok(retired)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use redb::{Database, ReadableDatabase, ReadableTableMetadata};

    use super::*;
    use crate::block::file::BlockDir;
    use crate::block::format::BLOCK_FORMAT_V1;
    use crate::block::name::Rewrite;
    use crate::name::MetricName;
    use crate::retention::{Policies, RetentionChange, RetentionPolicy, TierPeriod, Tombstones};
    use crate::value::ValueKind;

    const HOUR: u64 = 3_600;
    /// A day boundary: on every tier's grid.
    const NOW: u64 = 1_800_057_600;

    fn raw(start: u64) -> SpanKey {
        (Tier::Raw, SpanStart::new(Tier::Raw, start).expect("grid"))
    }

    fn series_key(system: &str, generation: u64) -> SeriesKey {
        SeriesKey {
            system: SystemKey::try_from(system.as_bytes()).expect("system"),
            generation: Generation::new(generation),
            metric: MetricName::try_from("load1").expect("metric"),
        }
    }

    fn record(system: &str, generation: u64) -> Vec<u8> {
        SeriesRecord {
            key: series_key(system, generation),
            kind: ValueKind::Load,
            last_spans: [None; 3],
        }
        .to_bytes()
    }

    fn row(chunk_count: u32) -> Vec<u8> {
        BlockRecord {
            format: BLOCK_FORMAT_V1,
            rewrite: Rewrite(0),
            length: 100,
            chunk_count,
            tombstoned: 0,
        }
        .to_bytes()
    }

    /// One row of the planning table: a raw file, what redb holds about it, and the head.
    struct Case<'a> {
        name: &'a str,
        start: u64,
        chunks: u32,
        holders: &'a [u64],
        series: Vec<(u32, Vec<u8>)>,
        head: &'a [(&'a str, u64)],
        in_flight: bool,
        retired: bool,
    }

    /// A store holding the case's file, its `block_generations` entries and `series` records.
    fn store_of(dir: &std::path::Path, case: &Case<'_>) -> Database {
        let db = Database::create(dir.join("t.redb")).expect("db");
        let (tier, start, holders, series) =
            (Tier::Raw.code(), case.start, case.holders, &case.series);
        let txn = db.begin_write().expect("write");
        {
            let mut rows = txn.open_table(BLOCKS).expect("blocks");
            rows.insert((tier, start), row(case.chunks).as_slice())
                .expect("row");
            let mut held = txn.open_table(BLOCK_GENERATIONS).expect("generations");
            for generation in holders {
                held.insert((*generation, tier, start), 100)
                    .expect("holder");
            }
            let mut table = txn.open_table(SERIES).expect("series");
            for (id, bytes) in series {
                table.insert(*id, bytes.as_slice()).expect("series");
            }
        }
        txn.commit().expect("commit");
        db
    }

    #[test]
    fn a_file_is_retired_only_when_every_holder_is_named_and_dead() {
        let raw_hour = TierPeriod::new(Tier::Raw, Duration::from_secs(HOUR)).expect("bounds");
        let mut policies = Policies::new(RetentionPolicy::default().with(raw_hour));
        let raw_two_days =
            TierPeriod::new(Tier::Raw, Duration::from_secs(48 * HOUR)).expect("bounds");
        let longer = RetentionChange::new([raw_two_days]).expect("one tier");
        let long = SystemKey::try_from(&b"long"[..]).expect("system");
        policies.change(&long, &longer, Duration::from_secs(600));
        let tombstones = Tombstones::default();
        let (old, young) = (NOW - 3 * HOUR, NOW - HOUR);
        let garbage = || vec![0xff, 0];
        let case = |name, start, holders, series, head, retired| Case {
            name,
            start,
            chunks: 1,
            holders,
            series,
            head,
            in_flight: false,
            retired,
        };
        let cases = [
            case(
                "expired, named by its record",
                old,
                &[1],
                vec![(1, record("s", 1))],
                &[],
                true,
            ),
            case("expired, named by no series", old, &[1], vec![], &[], true),
            case(
                "live, named by its record",
                young,
                &[1],
                vec![(1, record("s", 1))],
                &[],
                false,
            ),
            case(
                "live, named by the head",
                young,
                &[1],
                vec![],
                &[("s", 1)],
                false,
            ),
            case("live, named by no series", young, &[1], vec![], &[], true),
            case(
                "expired, chunks but no holder recorded",
                old,
                &[],
                vec![],
                &[],
                false,
            ),
            case(
                "live, chunks but no holder recorded",
                young,
                &[],
                vec![],
                &[],
                false,
            ),
            Case {
                chunks: 0,
                ..case("expired, no chunk at all", old, &[], vec![], &[], true)
            },
            Case {
                in_flight: true,
                ..case(
                    "expired, in flight",
                    old,
                    &[1],
                    vec![(1, record("s", 1))],
                    &[],
                    false,
                )
            },
            case(
                "its record doesn't decode",
                old,
                &[1],
                vec![(1, garbage())],
                &[],
                false,
            ),
            case(
                "another record doesn't decode, scanned",
                old,
                &[1],
                vec![(1, record("s", 1)), (2, garbage())],
                &[],
                false,
            ),
            case(
                "another record doesn't decode, the head names all",
                old,
                &[1],
                vec![(2, garbage())],
                &[("s", 1)],
                true,
            ),
            case(
                "the head names another generation, the holder's record doesn't decode",
                old,
                &[2],
                vec![(1, garbage())],
                &[("s", 1)],
                false,
            ),
            case(
                "the head names another generation, the holder's record names it live",
                young,
                &[2],
                vec![(1, record("s", 2))],
                &[("s", 1)],
                false,
            ),
            case(
                "the head's system wins over its record's",
                old,
                &[1],
                vec![(1, record("s", 1))],
                &[("long", 1)],
                false,
            ),
            case(
                "its record's system",
                old,
                &[1],
                vec![(1, record("long", 1))],
                &[],
                false,
            ),
            case(
                "the head's system wins while another holder forces the scan",
                old,
                &[1, 2],
                vec![(1, record("s", 1)), (2, record("s", 2))],
                &[("long", 1)],
                false,
            ),
            case(
                "an unnamed generation takes no other's system",
                old,
                &[2],
                vec![],
                &[("long", 1)],
                true,
            ),
        ];
        for case in cases {
            let dir = tempfile::tempdir().expect("tempdir");
            let db = store_of(dir.path(), &case);
            let blocks = Blocks::new(BlockDir::create(dir.path()).expect("dir"));
            let in_flight: BTreeSet<SpanKey> = match case.in_flight {
                true => BTreeSet::from([raw(case.start)]),
                false => BTreeSet::new(),
            };
            let world = World {
                policies: &policies,
                tombstones: &tombstones,
                retention_now: NOW,
                in_flight: &in_flight,
            };
            let head: Vec<SeriesKey> = case.head.iter().map(|(s, g)| series_key(s, *g)).collect();
            let txn = db.begin_write().expect("write");
            let plan = plan_retirements(&txn, &blocks, head.iter(), &world).expect("plan");
            let file = Retirement {
                name: BlockName {
                    tier: Tier::Raw,
                    span: raw(case.start).1,
                    rewrite: Rewrite(0),
                },
                generations: case.holders.iter().map(|g| Generation::new(*g)).collect(),
            };
            let wanted: Vec<Retirement> = [file].into_iter().filter(|_| case.retired).collect();
            assert_eq!(Vec::from(plan), wanted, "{}", case.name);
        }
    }

    /// Three files, each with its own rewrite and generations. Each costs its row, its pending
    /// unlink and one entry per generation: 5, 3 and 5 entries, so no constant cost fits.
    fn queued() -> Vec<Retirement> {
        let files: [(u64, u32, &[u64]); 3] = [
            (NOW - 3 * HOUR, 0, &[1, 2, 3]),
            (NOW - 2 * HOUR, 2, &[4]),
            (NOW - HOUR, 1, &[5, 6, 7]),
        ];
        files
            .iter()
            .map(|(start, rewrite, generations)| Retirement {
                name: BlockName {
                    tier: Tier::Raw,
                    span: raw(*start).1,
                    rewrite: Rewrite(*rewrite),
                },
                generations: generations.iter().map(|g| Generation::new(*g)).collect(),
            })
            .collect()
    }

    #[test]
    fn a_retire_step_takes_files_while_its_limit_allows_and_always_one() {
        let files = queued();
        let cases = [
            ("one short of two", 7, 1),
            ("exactly two", 8, 2),
            ("one short of three", 12, 2),
            ("exactly three", 13, 3),
            ("less than one file", 1, 1),
            ("far more than all", 100, 3),
        ];
        for (name, limit, expected) in cases {
            let dir = tempfile::tempdir().expect("tempdir");
            let db = Database::create(dir.path().join("t.redb")).expect("db");
            let mut queue: VecDeque<Retirement> = files.iter().cloned().collect();
            let txn = db.begin_write().expect("write");
            for file in &files {
                let (tier, span) = (file.name.tier.code(), file.name.span.get());
                let mut rows = txn.open_table(BLOCKS).expect("blocks");
                rows.insert((tier, span), row(1).as_slice()).expect("row");
                let mut held = txn.open_table(BLOCK_GENERATIONS).expect("generations");
                for generation in &file.generations {
                    held.insert((generation.get(), tier, span), 100)
                        .expect("holder");
                }
            }
            let retired = retire(&txn, &mut queue, limit).expect("retire");
            txn.commit().expect("commit");
            let names: Vec<BlockName> = files[..expected].iter().map(|f| f.name).collect();
            assert_eq!(retired, names, "{name}");
            assert_eq!(
                Vec::from(queue),
                files[expected..].to_vec(),
                "{name}: left queued"
            );
            let read = db.begin_read().expect("read");
            let rows = read.open_table(BLOCKS).expect("blocks");
            let held = read.open_table(BLOCK_GENERATIONS).expect("generations");
            let pending = read.open_table(PENDING_UNLINKS).expect("pending");
            for (i, file) in files.iter().enumerate() {
                let gone = i < expected;
                let (tier, span) = (file.name.tier.code(), file.name.span.get());
                let row_gone = rows.get((tier, span)).expect("get").is_none();
                assert_eq!(row_gone, gone, "{name}: row {i}");
                for generation in &file.generations {
                    let entry = held.get((generation.get(), tier, span)).expect("get");
                    assert_eq!(entry.is_none(), gone, "{name}: file {i} {generation:?}");
                }
                let unlink = pending.get((tier, span, file.name.rewrite.0)).expect("get");
                assert_eq!(unlink.is_some(), gone, "{name}: pending {i}");
            }
            assert_eq!(
                pending.len().expect("len"),
                expected as u64,
                "{name}: no other"
            );
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::create(dir.path().join("t.redb")).expect("db");
        let txn = db.begin_write().expect("write");
        let retired = retire(&txn, &mut VecDeque::new(), 6).expect("retire");
        assert!(retired.is_empty(), "an empty queue retires nothing");
    }
}
