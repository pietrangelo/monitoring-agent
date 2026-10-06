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

//! The store's block files (RFC 0010 §5, §6): what memory keeps of each (its summary, or that
//! it is unreadable), the reads a query makes of them, and the open's reconciliation of the
//! files with the `blocks` rows (§6 step 2a).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use redb::{Database, ReadTransaction, ReadableDatabase, ReadableTable};

use super::head::Head;
use super::{CorruptChunk, HandoffFailure, OpenError, StoreError, StoreStats};
use crate::block::file::{BlockDir, FileError};
use crate::block::format::BlockSummary;
use crate::block::name::{BlockName, Rewrite};
use crate::block::reconcile::{
    Catalogued, ChunksInRedb, Refusal, SpanEvidence, TailsInSpan, reconcile,
};
use crate::block::record::BlockRecord;
use crate::series::SeriesId;
use crate::tables::{BLOCKS, CHUNKS, PENDING_UNLINKS};
use crate::tier::{SpanStart, Tier};

/// A span whose chunks are in a block file, or still in `chunks`.
pub(crate) type SpanKey = (Tier, SpanStart);

/// What memory keeps of one span's block file.
#[derive(Clone, Debug)]
enum Known {
    Readable {
        rewrite: Rewrite,
        summary: Arc<BlockSummary>,
    },
    /// Missing, or its header or summary failed its check: every query of the span is
    /// `Corrupt` until retention or the cap retires it.
    Unreadable { rewrite: Rewrite },
}

impl Known {
    fn rewrite(&self) -> Rewrite {
        match self {
            Known::Readable { rewrite, .. } | Known::Unreadable { rewrite } => *rewrite,
        }
    }
}

/// The block files as the store knows them: one entry per committed `blocks` row.
pub(crate) struct Blocks {
    dir: BlockDir,
    known: Mutex<HashMap<SpanKey, Known>>,
    failures: AtomicU64,
    /// Per tier, why its last handoff failed.
    last_failures: Mutex<[Option<HandoffFailure>; 3]>,
}

impl Blocks {
    pub(crate) fn new(dir: BlockDir) -> Blocks {
        Blocks {
            dir,
            known: Mutex::new(HashMap::new()),
            failures: AtomicU64::new(0),
            last_failures: Mutex::new([None; 3]),
        }
    }

    pub(crate) fn dir(&self) -> &BlockDir {
        &self.dir
    }

    fn known(&self) -> std::sync::MutexGuard<'_, HashMap<SpanKey, Known>> {
        self.known.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records a committed row's file: readable with its summary, or unreadable.
    pub(crate) fn install(&self, name: BlockName, summary: Result<BlockSummary, FileError>) {
        let known = match summary {
            Ok(summary) => Known::Readable {
                rewrite: name.rewrite,
                summary: Arc::new(summary),
            },
            Err(_) => Known::Unreadable {
                rewrite: name.rewrite,
            },
        };
        self.known().insert((name.tier, name.span), known);
    }

    pub(crate) fn handoff_failed(&self, tier: Tier, cause: HandoffFailure) {
        self.last_failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)[usize::from(tier.code())] = Some(cause);
        self.failures.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn stats(&self) -> StoreStats {
        let mut stats = StoreStats {
            handoff_failures: self.failures.load(Ordering::SeqCst),
            last_failures: *self
                .last_failures
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            ..StoreStats::default()
        };
        for (tier, _) in self.known().keys() {
            stats.block_files[usize::from(tier.code())] += 1;
        }
        stats
    }

    /// A series' chunks in the file a row names, by seq. A file memory doesn't know yet (its
    /// row committed after the query's copy of memory) is read and kept.
    pub(crate) fn series_chunks(
        &self,
        name: BlockName,
        id: SeriesId,
    ) -> Result<Vec<(u16, Vec<u8>)>, StoreError> {
        let corrupt = StoreError::Corrupt(CorruptChunk {
            tier: name.tier,
            span: name.span.get(),
        });
        let current = self.known().get(&(name.tier, name.span)).cloned();
        let known = match current {
            Some(known) if known.rewrite() == name.rewrite => known,
            _ => {
                self.install(name, self.dir.read_summary(&name));
                self.known()
                    .get(&(name.tier, name.span))
                    .cloned()
                    .ok_or(StoreError::Io)?
            }
        };
        let Known::Readable { summary, .. } = known else {
            return Err(corrupt);
        };
        self.dir
            .read_series(&name, &summary, id)
            .map_err(|e| match e {
                FileError::Missing | FileError::Damaged(_) => corrupt,
                FileError::Io => StoreError::Io,
            })
    }
}

fn database(e: impl std::fmt::Display) -> OpenError {
    OpenError::Database(e.to_string())
}

/// The open's reconciliation (§6 step 2a), after the tails are loaded: files deleted or the open
/// refused by the pure plan, `pending_unlinks` cleared, every row's file read into memory.
/// `hub_now` is the persisted clock. Answers the spans that still have chunks in redb.
pub(super) fn reconcile_at_open(
    db: &Database,
    head: &Head,
    blocks: &Blocks,
    hub_now: u64,
) -> Result<BTreeSet<SpanKey>, OpenError> {
    let scan = blocks.dir.scan().map_err(database)?;
    let read = db.begin_read().map_err(database)?;
    let rows = read_rows(&read)?;
    let pending = read_pending(&read)?;
    let catalogued = Catalogued {
        rows: &rows,
        pending: &pending,
        hub_now,
    };
    let plan = reconcile(&scan.blocks, &catalogued, |tier, span| {
        evidence(&read, head, tier, span)
    })
    .map_err(|refusal| match refusal {
        Refusal::Unexplained(name) => {
            OpenError::UnexplainedBlock(blocks.dir.path(&name).display().to_string())
        }
        Refusal::Evidence(e) => database(e),
    })?;
    for name in &plan.unlink {
        blocks.dir.unlink(name).map_err(database)?;
    }
    clear_pending(db, &plan.clear_pending)?;
    for (&(tier, span), &rewrite) in &rows {
        let name = BlockName {
            tier,
            span,
            rewrite,
        };
        let summary = match plan.missing.contains(&name) {
            true => Err(FileError::Missing),
            false => blocks.dir.read_summary(&name),
        };
        blocks.install(name, summary);
    }
    spans_in_redb(&read)
}

fn span_key(tier: u8, span: u64) -> Result<SpanKey, OpenError> {
    let tier =
        Tier::from_code(tier).ok_or_else(|| OpenError::Corrupt(format!("a tier code {tier}")))?;
    let span = SpanStart::new(tier, span)
        .ok_or_else(|| OpenError::Corrupt(format!("a {} span off its grid", tier.name())))?;
    Ok((tier, span))
}

fn read_rows(read: &ReadTransaction) -> Result<BTreeMap<SpanKey, Rewrite>, OpenError> {
    let mut rows = BTreeMap::new();
    for entry in read
        .open_table(BLOCKS)
        .map_err(database)?
        .iter()
        .map_err(database)?
    {
        let (key, bytes) = entry.map_err(database)?;
        let (tier, span) = key.value();
        let record = BlockRecord::from_bytes(bytes.value())
            .map_err(|e| OpenError::Corrupt(format!("a blocks row: {e}")))?;
        rows.insert(span_key(tier, span)?, record.rewrite);
    }
    Ok(rows)
}

fn read_pending(read: &ReadTransaction) -> Result<BTreeSet<BlockName>, OpenError> {
    let mut pending = BTreeSet::new();
    for entry in read
        .open_table(PENDING_UNLINKS)
        .map_err(database)?
        .iter()
        .map_err(database)?
    {
        let (tier, span, rewrite) = entry.map_err(database)?.0.value();
        let (tier, span) = span_key(tier, span)?;
        pending.insert(BlockName {
            tier,
            span,
            rewrite: Rewrite(rewrite),
        });
    }
    Ok(pending)
}

/// Rule (a)'s evidence for a span with a row-less file: whether a loaded tail still holds the
/// span open, and whether `chunks` holds any of it.
fn evidence(
    read: &ReadTransaction,
    head: &Head,
    tier: Tier,
    span: SpanStart,
) -> Result<SpanEvidence, redb::Error> {
    let mut open = false;
    head.each_shard(|shard| {
        open = open || shard.values().any(|e| e.state.holds_open(tier, span));
    });
    let present = read
        .open_table(CHUNKS)?
        .range(span_range(tier, span))?
        .next()
        .is_some();
    Ok(SpanEvidence {
        tails: if open {
            TailsInSpan::Open
        } else {
            TailsInSpan::AllSealed
        },
        chunks: if present {
            ChunksInRedb::Present
        } else {
            ChunksInRedb::Absent
        },
    })
}

/// Every key of a span in `chunks`.
pub(crate) fn span_range(
    tier: Tier,
    span: SpanStart,
) -> std::ops::RangeInclusive<(u8, u64, u32, u16)> {
    let (t, s) = (tier.code(), span.get());
    (t, s, 0, 0)..=(t, s, u32::MAX, u16::MAX)
}

fn clear_pending(db: &Database, names: &[BlockName]) -> Result<(), OpenError> {
    if names.is_empty() {
        return Ok(());
    }
    let txn = db.begin_write().map_err(database)?;
    {
        let mut table = txn.open_table(PENDING_UNLINKS).map_err(database)?;
        for name in names {
            table
                .remove((name.tier.code(), name.span.get(), name.rewrite.0))
                .map_err(database)?;
        }
    }
    txn.commit().map_err(database)
}

/// The spans with chunks in `chunks`, one seek per span.
fn spans_in_redb(read: &ReadTransaction) -> Result<BTreeSet<SpanKey>, OpenError> {
    let table = read.open_table(CHUNKS).map_err(database)?;
    let mut spans = BTreeSet::new();
    let mut from = (0u8, 0u64, 0u32, 0u16);
    while let Some(entry) = table.range(from..).map_err(database)?.next() {
        let (tier, span, _, _) = entry.map_err(database)?.0.value();
        spans.insert(span_key(tier, span)?);
        match span.checked_add(1) {
            Some(next) => from = (tier, next, 0, 0),
            None => match tier.checked_add(1) {
                Some(next_tier) => from = (next_tier, 0, 0, 0),
                None => break,
            },
        }
    }
    Ok(spans)
}
