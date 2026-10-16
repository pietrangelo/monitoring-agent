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

//! The writer thread (RFC 0010 §6): it owns every redb write transaction, stamps every point
//! with hub time, and commits once per commit interval (or at once for a durable catalog
//! transaction) one transaction holding the interval's points log entry, the chunks sealed in
//! it, the tails due, the catalog writes and the clock.

mod files;

use std::collections::BTreeSet;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use redb::{Database, Durability, WriteTransaction};

use super::catalog::{CatalogTxn, Deliver, Run, Settled};
use super::handoff::{Backlog, Handoffs, Job, Outcome};
pub(crate) use super::ingest::Core;
use super::pass::Retention;
use super::rotation::Rotation;
use super::{AppendReport, Commit, FailCause, Shared, StoreError, StoreOptions};
use crate::block::format::BlockSummary;
use crate::block::name::BlockName;
use crate::name::{Generation, MetricName, SystemKey};
use crate::series::{SeriesId, SeriesKey, SeriesRecord};
use crate::tables::{
    CHUNKS, META, META_CLOCK, META_COMMIT_SEQ, META_ID_COUNTER, META_RETENTION_CLOCK, POINTS_LOG,
    SERIES, SERIES_KEY, TAILS, encode_log_entry,
};
use crate::tier::Tier;
use crate::value::ValueKind;

/// What callers ask of the writer.
pub(crate) enum Request<C> {
    Append {
        system: SystemKey,
        generation: Generation,
        points: Vec<(MetricName, ValueKind, i64)>,
        reply: SyncSender<Result<AppendReport, StoreError>>,
    },
    Transact {
        class: Commit,
        run: Run<C>,
    },
    /// Flush every tail, commit, stop.
    Close {
        reply: SyncSender<Result<(), StoreError>>,
    },
    /// The store was dropped without `close`: stop, committing nothing more.
    Abandon,
    /// The block writer's outcome for one span (§6 *Span handoff*).
    HandOff(Outcome),
    /// The block writer unlinked a retired file: its `pending_unlinks` entry goes.
    Unlinked(BlockName),
}

/// Which tails a commit writes beside the ones that sealed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flush {
    /// The rotation's slice due since the previous commit.
    Rotation,
    /// Every dirty tail (a close).
    All,
}

/// The writer: its core, the open redb transaction, and what waits for the next commit.
pub(crate) struct Writer<C> {
    core: Core<C>,
    txn: Option<WriteTransaction>,
    waiters: Vec<Deliver>,
    commit_seq: u64,
    /// Log entries below this sequence are no longer needed: every tail was written since.
    log_bound: u64,
    rotation: Rotation,
    /// Series whose sealed chunks the commit in progress holds: their pending copies in the
    /// head are dropped once it lands.
    committing: BTreeSet<SeriesId>,
    commit_interval: Duration,
    tail_rotation: Duration,
    sweep_interval: Duration,
    handoffs: Handoffs,
    /// The block writer's queue; `None` until the store connects it.
    jobs: Option<Sender<Job>>,
    /// Files whose rows the commit in progress holds: known to queries once it lands.
    landed: Vec<(BlockName, BlockSummary)>,
    retention: Retention,
    /// Files whose retirement the commit in progress holds: unlinked once it lands.
    retired: Vec<BlockName>,
    /// Passes the commit in progress finishes.
    finishing: u64,
}

/// The interval's transaction, begun when first needed: durable when committed, with 2-phase
/// commit and quick-repair (RFC 0010 §1).
fn open_txn<'a>(
    slot: &'a mut Option<WriteTransaction>,
    db: &Database,
) -> Result<&'a WriteTransaction, StoreError> {
    if slot.is_none() {
        let mut txn = db.begin_write().map_err(|_| StoreError::Failed)?;
        txn.set_durability(Durability::Immediate)
            .map_err(|_| StoreError::Failed)?;
        txn.set_two_phase_commit(true);
        txn.set_quick_repair(true);
        *slot = Some(txn);
    }
    slot.as_ref().ok_or(StoreError::Failed)
}

/// Marks the store failed if the writer thread unwinds.
struct FailOnPanic<C>(Arc<Shared<C>>);

impl<C> Drop for FailOnPanic<C> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.lifecycle.fail();
        }
    }
}

impl<C> Writer<C> {
    pub(super) fn new(
        core: Core<C>,
        options: &StoreOptions,
        commit_seq: u64,
        handoffs: Handoffs,
        retention: Retention,
    ) -> Writer<C> {
        let rotation = Rotation::start(commit_seq, core.index.keys().copied());
        Writer {
            core,
            txn: None,
            waiters: Vec::new(),
            commit_seq,
            log_bound: 0,
            rotation,
            committing: BTreeSet::new(),
            commit_interval: options.commit_interval,
            tail_rotation: options.tail_rotation,
            sweep_interval: options.sweep_interval,
            handoffs,
            jobs: None,
            landed: Vec::new(),
            retention,
            retired: Vec::new(),
            finishing: 0,
        }
    }

    /// Connects the block writer's queue: closed spans are handed to it from now on.
    pub(super) fn connect(&mut self, jobs: Sender<Job>) {
        self.jobs = Some(jobs);
    }

    pub(super) fn run(mut self, inbox: Receiver<Request<C>>) {
        let _guard = FailOnPanic(Arc::clone(&self.core.shared));
        let sweep_every = self.sweep_interval;
        let (mut next_commit, mut next_sweep) = (
            Instant::now() + self.commit_interval,
            Instant::now() + sweep_every,
        );
        loop {
            let wait = next_commit.saturating_duration_since(Instant::now());
            let force = match inbox.recv_timeout(wait) {
                Ok(Request::Close { reply }) => return self.finish(reply),
                Ok(Request::Abandon) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(request) => self.handle(request),
                Err(RecvTimeoutError::Timeout) => false,
            };
            if self.core.shared.check().is_err() {
                return self.fail_stop();
            }
            if Instant::now() >= next_sweep {
                let now = self.core.hub_now();
                self.core.sweep(now);
                if let Err(Backlog { tier, last }) = self.handoffs.choose(now) {
                    *self
                        .core
                        .shared
                        .fail_cause
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) =
                        Some(FailCause::HandoffBacklog { tier, last });
                    return self.fail_stop();
                }
                next_sweep = Instant::now() + sweep_every;
            }
            if self.retention.due() {
                let (hub_now, system_now) = self.core.clocks();
                self.retention.begin(hub_now, system_now);
            }
            if force || Instant::now() >= next_commit {
                if self.commit(Flush::Rotation).is_err() {
                    return;
                }
                next_commit = Instant::now() + self.commit_interval;
            }
        }
    }

    /// Handles one request; whether it asks for a commit at once.
    fn handle(&mut self, request: Request<C>) -> bool {
        let request = match request {
            Request::HandOff(outcome) => {
                self.hand_off(outcome);
                return false;
            }
            Request::Unlinked(name) => {
                self.unlinked(name);
                return false;
            }
            other => other,
        };
        let shared = Arc::clone(&self.core.shared);
        let Ok(txn) = open_txn(&mut self.txn, &shared.db) else {
            self.core.shared.lifecycle.fail();
            return false;
        };
        match request {
            Request::Append {
                system,
                generation,
                points,
                reply,
            } => {
                let at = self.core.hub_now();
                let report = self
                    .core
                    .append_points(txn, &system, generation, &points, at);
                if report.is_err() {
                    self.core.shared.lifecycle.fail();
                }
                let _ = reply.send(report);
                false
            }
            Request::Transact { class, run } => {
                let now = self.core.hub_now();
                let settled = run(&mut CatalogTxn::new(txn, &mut self.core, now));
                self.settle(class, settled)
            }
            Request::Close { .. }
            | Request::Abandon
            | Request::HandOff(_)
            | Request::Unlinked(_) => false,
        }
    }

    fn settle(&mut self, class: Commit, settled: Settled) -> bool {
        match settled {
            Settled::Now(answer) => answer(),
            Settled::FailStop(answer) => {
                self.core.shared.lifecycle.fail();
                answer();
            }
            Settled::Wait {
                wrote: true,
                deliver,
                ..
            } => {
                self.waiters.push(deliver);
                return class == Commit::Durable;
            }
            Settled::Wait {
                wrote: false,
                answer,
                deliver,
            } => match answer {
                super::Answer::AfterCommit if self.core.catalog_dirty => self.waiters.push(deliver),
                super::Answer::AfterCommit | super::Answer::AtOnce => deliver(Ok(())),
            },
        }
        false
    }

    /// Commits the interval; a failure fails the store stop.
    fn commit(&mut self, flush: Flush) -> Result<(), StoreError> {
        let committed = self.try_commit(flush);
        match committed {
            Ok(()) => self.after_commit(),
            Err(_) => self.fail_stop(),
        }
        committed
    }

    fn try_commit(&mut self, flush: Flush) -> Result<(), StoreError> {
        let shared = Arc::clone(&self.core.shared);
        open_txn(&mut self.txn, &shared.db)?;
        let txn = self.txn.take().ok_or(StoreError::Failed)?;
        self.write_interval(&txn, flush)
            .map_err(|_| StoreError::Failed)?;
        txn.commit().map_err(|_| StoreError::Failed)
    }

    /// Everything a commit holds beside the catalog writes already in `txn`.
    fn write_interval(&mut self, txn: &WriteTransaction, flush: Flush) -> Result<(), redb::Error> {
        let seq = self.commit_seq;
        let points = std::mem::take(&mut self.core.points);
        if !points.is_empty() {
            txn.open_table(POINTS_LOG)?
                .insert(seq, encode_log_entry(&points).as_slice())?;
        }
        let mut chunks = txn.open_table(CHUNKS)?;
        for (id, chunk) in std::mem::take(&mut self.core.sealed) {
            chunks.insert(
                (chunk.tier.code(), chunk.span.get(), id.0, chunk.seq),
                chunk.bytes.as_slice(),
            )?;
            self.handoffs.sealed((chunk.tier, chunk.span));
            self.committing.insert(id);
        }
        drop(chunks);
        self.write_tails(txn, flush)?;
        if self.log_bound > 0 {
            txn.open_table(POINTS_LOG)?
                .retain_in(..self.log_bound, |_, _| false)?;
        }
        let mut meta = txn.open_table(META)?;
        meta.insert(
            META_CLOCK,
            self.core.clock.last_issued().to_be_bytes().as_slice(),
        )?;
        meta.insert(
            META_ID_COUNTER,
            u64::from(self.core.next_id).to_be_bytes().as_slice(),
        )?;
        meta.insert(META_COMMIT_SEQ, (seq + 1).to_be_bytes().as_slice())?;
        meta.insert(
            META_RETENTION_CLOCK,
            self.retention.clock().to_be_bytes().as_slice(),
        )?;
        drop(meta);
        // After the chunks this commit seals, so a span the pass empties is seen whole.
        self.retention_step(txn)
    }

    /// The tails and records of series that sealed or are new, and the tails due.
    fn write_tails(&mut self, txn: &WriteTransaction, flush: Flush) -> Result<(), redb::Error> {
        let touched: BTreeSet<SeriesId> = std::mem::take(&mut self.core.touched);
        let mut ids = touched.clone();
        match flush {
            Flush::Rotation => ids.extend(self.rotation.due(self.tail_rotation)),
            Flush::All => ids.extend(self.core.index.keys().copied()),
        }
        let new_series = std::mem::take(&mut self.core.new_series);
        let (mut tails, mut series, mut keys) = (
            txn.open_table(TAILS)?,
            txn.open_table(SERIES)?,
            txn.open_table(SERIES_KEY)?,
        );
        for id in ids {
            let Some(key) = self.core.index.get(&id) else {
                continue;
            };
            let Some((tail, record)) = self.take_tail(key, touched.contains(&id)) else {
                continue;
            };
            tails.insert((Tier::Raw.code(), id.0), tail.as_slice())?;
            series.insert(id.0, record.to_bytes().as_slice())?;
            if new_series.contains(&id) {
                keys.insert(key.to_bytes().as_slice(), id.0)?;
            }
        }
        Ok(())
    }

    /// A series' tail and record, if it has something to write, copied out under its shard's
    /// lock: no redb call is made while the lock is held. The series is then clean, which is
    /// exact because only this thread appends.
    fn take_tail(&self, key: &SeriesKey, touched: bool) -> Option<(Vec<u8>, SeriesRecord)> {
        let mut shard = self.core.shared.head.shard(key);
        let entry = shard.get_mut(key).filter(|e| e.dirty || touched)?;
        entry.dirty = false;
        let record = SeriesRecord {
            key: key.clone(),
            kind: entry.state.kind(),
            last_spans: entry.last_spans,
        };
        Some((entry.state.to_tail(), record))
    }

    fn after_commit(&mut self) {
        self.commit_seq += 1;
        let shared = Arc::clone(&self.core.shared);
        for id in std::mem::take(&mut self.committing) {
            let Some(key) = self.core.index.get(&id) else {
                continue;
            };
            if let Some(entry) = shared.head.shard(key).get_mut(key) {
                entry.pending.clear();
            }
        }
        self.after_handoffs();
        self.after_retention();
        if self.rotation.finished() {
            self.log_bound = self.rotation.started_seq;
            self.rotation = Rotation::start(self.commit_seq, self.core.index.keys().copied());
        }
        let changes = std::mem::take(&mut self.core.changes);
        // Called outside the hook's own lock, so the hook may be replaced meanwhile.
        let hook = shared
            .hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            hook(&changes);
        }
        self.core.catalog_dirty = false;
        for deliver in std::mem::take(&mut self.waiters) {
            deliver(Ok(()));
        }
    }

    /// The close: flushes every tail and commits; marks the store closed before answering
    /// and before the inbox drops, so a call queued behind the close finds a clean stop. A
    /// failed commit stays a failure.
    fn finish(&mut self, reply: SyncSender<Result<(), StoreError>>) {
        let committed = self.commit(Flush::All);
        if committed.is_ok() {
            self.core.shared.lifecycle.close();
        }
        let _ = reply.send(committed);
    }

    /// Marks the store failed and answers every waiting caller.
    fn fail_stop(&mut self) {
        self.core.shared.lifecycle.fail();
        for deliver in std::mem::take(&mut self.waiters) {
            deliver(Err(StoreError::Failed));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::store::lifecycle::State;
    use crate::store::{WallClock, open};

    /// RFC 0010 §6 Shutdown: a request queued behind `Close` is dropped unanswered when the
    /// writer stops, and its caller finds the store closed, not failed.
    #[test]
    fn a_request_queued_behind_close_finds_the_store_closed_not_failed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let options = StoreOptions::with_clock(Arc::new(WallClock));
        let (shared, writer) = open::open::<()>(dir.path(), options).expect("opens");
        let (requests, inbox) = mpsc::sync_channel(4);
        let (close_reply, closed) = mpsc::sync_channel(1);
        let (append_reply, appended) = mpsc::sync_channel(1);
        requests
            .send(Request::Close { reply: close_reply })
            .expect("queued");
        requests
            .send(Request::Append {
                system: SystemKey::try_from(&b"a"[..]).expect("key"),
                generation: Generation::new(1),
                points: Vec::new(),
                reply: append_reply,
            })
            .expect("queued");
        drop(requests);
        writer.run(inbox);
        assert_eq!(closed.recv(), Ok(Ok(())), "the close is answered");
        assert!(appended.recv().is_err(), "the request behind it is not");
        assert_eq!(shared.lifecycle.state(), State::Closed);
        assert_eq!(shared.lifecycle.writer_gone(), StoreError::Closed);
        assert_eq!(shared.lifecycle.check(), Err(StoreError::Closed));
    }
}
