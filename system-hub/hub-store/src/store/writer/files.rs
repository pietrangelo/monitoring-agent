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

//! The writer's side of the block files (RFC 0010 §5, §6): a handoff's row joining a commit,
//! the retention pass's step in each commit, and what follows a commit: files landed become
//! known to queries, retired ones are forgotten and unlinked by the block writer.

use std::collections::BTreeMap;
use std::sync::Arc;

use redb::{ReadableTable, WriteTransaction};

use super::{Writer, open_txn};
use crate::block::name::BlockName;
use crate::block::record::BlockRecord;
use crate::name::Generation;
use crate::store::StoreError;
use crate::store::blocks::span_range;
use crate::store::handoff::{Job, Outcome, handoff_name};
use crate::store::pass::{Context, Step};
use crate::tables::{BLOCK_GENERATIONS, BLOCKS, CHUNKS, PENDING_UNLINKS};

/// Why a handoff's commit couldn't join the transaction: either fails the store stop.
#[derive(Debug)]
enum HandoffCommitError {
    /// The span already has a row: spans in flight are handed over once, so this is a defect.
    SecondRow,
    /// redb refused a read or write, as any commit's failure.
    Store,
}

impl<E: Into<redb::Error>> From<E> for HandoffCommitError {
    fn from(_: E) -> HandoffCommitError {
        HandoffCommitError::Store
    }
}

/// A span's file is durable: its row, its `block_generations` entries and the deletion of its
/// chunks join this commit (§6 *Span handoff* step 4).
fn commit_handoff(
    txn: &WriteTransaction,
    name: &BlockName,
    record: &BlockRecord,
    generations: &BTreeMap<Generation, u64>,
) -> Result<(), HandoffCommitError> {
    let key = (name.tier.code(), name.span.get());
    let mut blocks = txn.open_table(BLOCKS)?;
    if blocks.get(key)?.is_some() {
        return Err(HandoffCommitError::SecondRow);
    }
    blocks.insert(key, record.to_bytes().as_slice())?;
    let mut held = txn.open_table(BLOCK_GENERATIONS)?;
    for (generation, bytes) in generations {
        held.insert((generation.get(), key.0, key.1), *bytes)?;
    }
    txn.open_table(CHUNKS)?
        .retain_in(span_range(name.tier, name.span), |_, _| false)?;
    Ok(())
}

impl<C> Writer<C> {
    /// The block writer's outcome: a durable file's row and the deletion of its chunks join
    /// this commit; a failure leaves the span waiting for the next sweep, its cause kept. A
    /// second row for a span (a defect: spans in flight are handed over once) fails the store.
    pub(super) fn hand_off(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Written {
                name,
                record,
                summary,
                generations,
            } => {
                let shared = Arc::clone(&self.core.shared);
                let committed = open_txn(&mut self.txn, &shared.db)
                    .map_err(|_| HandoffCommitError::Store)
                    .and_then(|txn| commit_handoff(txn, &name, &record, &generations));
                match committed {
                    Ok(()) => self.landed.push((name, summary)),
                    Err(HandoffCommitError::SecondRow | HandoffCommitError::Store) => {
                        shared.lifecycle.fail()
                    }
                }
            }
            Outcome::Failed(name, cause) => {
                self.handoffs.failed((name.tier, name.span), cause);
                self.core.shared.blocks.handoff_failed(name.tier, cause);
            }
        }
    }

    /// A retired file is gone: its `pending_unlinks` entry goes with this commit. A failure
    /// to write it fails the store, as any commit's.
    pub(super) fn unlinked(&mut self, name: BlockName) {
        let shared = Arc::clone(&self.core.shared);
        let removed = open_txn(&mut self.txn, &shared.db).and_then(|txn| {
            let key = (name.tier.code(), name.span.get(), name.rewrite.0);
            let mut pending = txn
                .open_table(PENDING_UNLINKS)
                .map_err(|_| StoreError::Failed)?;
            pending.remove(key).map_err(|_| StoreError::Failed)?;
            Ok(())
        });
        if removed.is_err() {
            shared.lifecycle.fail();
        }
    }

    /// One step of the retention pass in progress, in the commit's transaction.
    pub(super) fn retention_step(&mut self, txn: &WriteTransaction) -> Result<(), redb::Error> {
        let shared = Arc::clone(&self.core.shared);
        let mut ctx = Context {
            policies: &shared.policies,
            tombstones: &shared.tombstones,
            owners: &self.core.index,
            blocks: &shared.blocks,
            handoffs: &mut self.handoffs,
        };
        match self.retention.step(txn, &mut ctx)? {
            Step::Working => {}
            Step::Retired(names) => self.retired.extend(names),
            Step::Finished => self.finishing += 1,
        }
        Ok(())
    }

    /// After a commit: the files whose rows it held become known to queries, and the spans due
    /// since the last sweep go to the block writer, whose read transaction now sees them whole.
    pub(super) fn after_handoffs(&mut self) {
        for (name, summary) in std::mem::take(&mut self.landed) {
            self.handoffs.landed((name.tier, name.span));
            self.core.shared.blocks.install(name, Ok(summary));
        }
        let Some(jobs) = &self.jobs else {
            return;
        };
        for span in self.handoffs.take_due() {
            // The block writer is gone only once this thread is stopping: nothing to retry.
            if jobs.send(Job::HandOff(handoff_name(span))).is_err() {
                return;
            }
        }
    }

    /// After a commit: the files it retired are forgotten and go to the block writer to be
    /// unlinked (their `pending_unlinks` entries are durable now), and the stats see the
    /// committed retention clock and the passes it finished.
    pub(super) fn after_retention(&mut self) {
        let shared = Arc::clone(&self.core.shared);
        for name in std::mem::take(&mut self.retired) {
            shared.blocks.evict((name.tier, name.span));
            // A file left linked keeps its entry: the next open unlinks it.
            if let Some(jobs) = &self.jobs {
                let _ = jobs.send(Job::Unlink(name));
            }
        }
        let finished = std::mem::take(&mut self.finishing);
        shared.retention.committed(self.retention.clock(), finished);
    }
}
