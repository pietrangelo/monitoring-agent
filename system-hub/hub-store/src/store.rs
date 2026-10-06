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

//! The store's API (RFC 0010 §9): appends and queries of series, catalog transactions run on
//! the writer thread inside its group commit, the commit hook, close.

mod blocks;
mod catalog;
mod handoff;
mod head;
mod ingest;
mod open;
mod query;
mod writer;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use redb::{Database, ReadableDatabase};

pub use catalog::{CatalogRead, CatalogTxn};
pub(crate) use head::Head;
use writer::Request;

use crate::codec::{Bucket, RawPoint};
use crate::name::{Generation, MetricName, SystemKey};
use crate::series::SeriesKey;
use crate::tier::Tier;
use crate::value::ValueKind;

/// The system clock, in Unix seconds: read only by the writer thread, injected for tests.
pub trait SystemClock: Send + Sync + 'static {
    fn now(&self) -> u64;
}

/// The host's clock.
pub struct WallClock;

impl SystemClock for WallClock {
    fn now(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }
}

/// How the store runs.
pub struct StoreOptions {
    /// The group commit: the durability window (`HUB_COMMIT_INTERVAL`).
    pub commit_interval: Duration,
    /// How long one rotation of the tail flush takes: every series' tail is written once in it.
    pub tail_rotation: Duration,
    /// redb's page cache (`HUB_STORE_CACHE`).
    pub cache_bytes: usize,
    /// How often the sweep closes quiet buckets and closed spans are handed off (§5, §6).
    pub sweep_interval: Duration,
    pub clock: Arc<dyn SystemClock>,
}

impl StoreOptions {
    /// The defaults of RFC 0010 §8 over a given clock.
    pub fn with_clock(clock: Arc<dyn SystemClock>) -> StoreOptions {
        StoreOptions {
            commit_interval: Duration::from_secs(1),
            tail_rotation: Duration::from_secs(15 * 60),
            cache_bytes: 256 << 20,
            sweep_interval: Duration::from_secs(crate::tier::SWEEP_SECS),
            clock,
        }
    }
}

/// A catalog table of the hub's (RFC 0011 §1): bytes to the store. Its name carries the
/// `catalog.` prefix, so it can't be one of the store's own tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogTable {
    name: &'static str,
}

impl CatalogTable {
    pub const fn new(name: &'static str) -> CatalogTable {
        let prefix = b"catalog.";
        let bytes = name.as_bytes();
        assert!(
            bytes.len() > prefix.len(),
            "a catalog table is named catalog.<name>"
        );
        let mut i = 0;
        while i < prefix.len() {
            assert!(
                bytes[i] == prefix[i],
                "a catalog table is named catalog.<name>"
            );
            i += 1;
        }
        CatalogTable { name }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }
}

/// When a catalog transaction is committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Commit {
    /// At once: the call returns after its commit.
    Durable,
    /// With the next group commit, within one commit interval.
    Batched,
}

/// When a write-free transaction is answered: after the commit holding the transaction it read
/// (at once if no uncommitted catalog write lies beneath it), or at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    AfterCommit,
    AtOnce,
}

/// A transaction's own refusal, before its first write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Abort<A>(pub A);

/// What `transact` answers when it doesn't return `T`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactError<A> {
    Aborted(Abort<A>),
    Store(StoreError),
}

/// What a damaged chunk belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CorruptChunk {
    pub tier: Tier,
    pub span: u64,
}

/// Why the store can't answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreError {
    /// `close` was called.
    Closed,
    /// The writer thread failed (a panic, an I/O error on commit): the hub must exit.
    Failed,
    /// An I/O error on a read.
    Io,
    /// A chunk that doesn't decode, or holds a value outside its kind's domain.
    Corrupt(CorruptChunk),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Closed => f.write_str("the store is closed"),
            StoreError::Failed => f.write_str("the store failed"),
            StoreError::Io => f.write_str("an I/O error reading the store"),
            StoreError::Corrupt(c) => write!(
                f,
                "a corrupt chunk in the {} span at {}",
                c.tier.name(),
                c.span
            ),
        }
    }
}

impl std::error::Error for StoreError {}

/// Why the store didn't open.
#[derive(Debug)]
pub enum OpenError {
    /// redb refused the file, or an I/O error.
    Database(String),
    /// The file is of another store format.
    Format(u64),
    /// The file holds bytes this version can't read.
    Corrupt(String),
    /// A block file no `blocks` row names and no rule explains (§6 step 2a): `hub.redb` is
    /// older than `blocks/`. Its path.
    UnexplainedBlock(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Database(e) => write!(f, "can't open the store: {e}"),
            OpenError::Format(v) => write!(
                f,
                "the store is of format {v}, which this version doesn't read"
            ),
            OpenError::Corrupt(e) => write!(f, "the store is corrupt: {e}"),
            OpenError::UnexplainedBlock(path) => write!(
                f,
                "{path} is a block file the store doesn't know: hub.redb is older than \
                 blocks/ (restore both together, or move the file aside)"
            ),
        }
    }
}

impl std::error::Error for OpenError {}

/// Why one point of an append was refused. (RFC 0010 §9's `SystemGone` arrives with
/// tombstones; its `InvalidName` is the hub edge's, since a `MetricName` can't be invalid.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    NotAfterLast,
    /// No series id is left to give (2³² ids, never reused); the active-series caps join it.
    SeriesCapReached,
    KindMismatch,
    OutOfDomain,
}

/// What an append did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppendReport {
    /// The hub time the points were stamped with.
    pub at: u64,
    pub accepted: u16,
    pub rejected: Vec<(MetricName, Rejected)>,
}

/// Which end of a query's range its limit keeps; results are always ascending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    Earliest,
    Latest,
}

/// How many points or buckets a query answers: 0 to 10,000, a larger value clamped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit(u16);

impl Limit {
    pub const MAX: u16 = 10_000;

    pub fn new(limit: u16) -> Limit {
        Limit(limit.min(Limit::MAX))
    }

    pub fn get(self) -> u16 {
        self.0
    }
}

/// A query of one series: an inclusive range of hub time, a tier, an order and a limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Query {
    pub from: u64,
    pub until: u64,
    pub tier: Tier,
    pub order: Order,
    pub limit: Limit,
}

/// A query's answer: raw points, or the closed buckets of a rollup tier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Samples {
    Raw(Vec<RawPoint>),
    Rollup(Vec<Bucket>),
}

/// A series' samples and its kind, which turns them back into natural values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Series {
    pub kind: ValueKind,
    pub samples: Samples,
}

/// What the store reports of itself (RFC 0010 §9's `StoreStats`, the block-file part).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreStats {
    block_files: [u64; 3],
    handoff_failures: u64,
    last_failures: [Option<HandoffFailure>; 3],
}

impl StoreStats {
    /// The tier's block files: one per `blocks` row committed.
    pub fn block_files(&self, tier: Tier) -> u64 {
        self.block_files[usize::from(tier.code())]
    }

    /// Handoffs that failed since the open (each is retried at the next sweep).
    pub fn handoff_failures(&self) -> u64 {
        self.handoff_failures
    }

    /// Why the tier's last failed handoff failed, if one did since the open.
    pub fn last_handoff_failure(&self, tier: Tier) -> Option<HandoffFailure> {
        self.last_failures[usize::from(tier.code())]
    }
}

/// Why a span's handoff failed (RFC 0010 §6): the span stays in redb and is retried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandoffFailure {
    /// Its chunks couldn't be read from redb.
    Read,
    /// Its chunks couldn't be encoded into a file (a defect: they come in key order).
    Encode,
    /// Writing, syncing or linking the file failed, with this I/O error.
    Write(std::io::ErrorKind),
    /// A file of that name already exists: it is never replaced.
    Exists,
}

/// Why the store failed stop, when it was the store's own decision rather than a panic or a
/// failed commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailCause {
    /// More than three spans of a tier waited on failing handoffs; the last one failed so.
    HandoffBacklog { tier: Tier, last: HandoffFailure },
}

impl std::fmt::Display for HandoffFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandoffFailure::Read => f.write_str("its chunks couldn't be read"),
            HandoffFailure::Encode => f.write_str("its chunks couldn't be encoded"),
            HandoffFailure::Write(kind) => write!(f, "writing its file failed: {kind}"),
            HandoffFailure::Exists => f.write_str("a file of its name already exists"),
        }
    }
}

impl std::fmt::Display for FailCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FailCause::HandoffBacklog { tier, last } => write!(
                f,
                "more than three {} spans wait on failing handoffs; the last failed because {last}",
                tier.name()
            ),
        }
    }
}

/// The hook the writer calls after each commit, in commit order, on the writer thread. It must
/// not call back into the store (`append`, `transact`, `close` would wait on the writer that
/// is running it): it updates in-memory state, or hands the changes to another thread.
pub type CommitHook<C> = Box<dyn Fn(&[C]) + Send + Sync>;

/// The hook as the writer holds it: cloned out of its lock before each call.
pub(crate) type SharedHook<C> = Arc<dyn Fn(&[C]) + Send + Sync>;

/// What the writer and the readers share.
pub(crate) struct Shared<C> {
    pub db: Database,
    pub head: Head,
    pub blocks: blocks::Blocks,
    pub failed: AtomicBool,
    /// Why the store failed stop, when it decided to.
    pub fail_cause: Mutex<Option<FailCause>>,
    pub hook: Mutex<Option<SharedHook<C>>>,
}

impl<C> Shared<C> {
    /// `Failed` once the writer failed; the store answers nothing after that.
    pub(crate) fn check(&self) -> Result<(), StoreError> {
        if self.failed.load(Ordering::SeqCst) {
            Err(StoreError::Failed)
        } else {
            Ok(())
        }
    }
}

/// The store: one redb file and its block files; one writer thread, one block writer.
pub struct Store<C: Send + 'static> {
    shared: Arc<Shared<C>>,
    requests: SyncSender<Request<C>>,
    /// The writer, then the block writer: joined in that order, since the block writer ends
    /// when the writer drops its queue.
    threads: Mutex<Vec<JoinHandle<()>>>,
    closed: AtomicBool,
}

/// How many requests may wait for the writer: a full channel makes callers wait.
const REQUEST_QUEUE: usize = 1_024;

impl<C: Send + 'static> Store<C> {
    /// Opens (or creates) `hub.redb` in `data_dir`, re-derives memory and starts the writer.
    pub fn open(data_dir: &Path, options: StoreOptions) -> Result<Store<C>, OpenError> {
        let (shared, mut writer) = open::open(data_dir, options)?;
        let thread = |e: std::io::Error| OpenError::Database(e.to_string());
        let (requests, inbox) = mpsc::sync_channel(REQUEST_QUEUE);
        let (jobs, block_writer) =
            handoff::spawn(Arc::clone(&shared), requests.clone()).map_err(thread)?;
        writer.connect(jobs);
        let handle = std::thread::Builder::new()
            .name("hub-store-writer".into())
            .spawn(move || writer.run(inbox))
            .map_err(thread)?;
        Ok(Store {
            shared,
            requests,
            threads: Mutex::new(vec![handle, block_writer]),
            closed: AtomicBool::new(false),
        })
    }

    /// Joins the writer, then the block writer.
    fn join(&self) {
        let threads =
            std::mem::take(&mut *self.threads.lock().unwrap_or_else(PoisonError::into_inner));
        for handle in threads {
            let _ = handle.join();
        }
    }

    fn check(&self) -> Result<(), StoreError> {
        self.shared.check()?;
        if self.closed.load(Ordering::SeqCst) {
            Err(StoreError::Closed)
        } else {
            Ok(())
        }
    }

    /// Sends a request and waits for its answer; a writer gone answers `Failed`.
    fn ask<R>(&self, request: impl FnOnce(SyncSender<R>) -> Request<C>) -> Result<R, StoreError> {
        self.check()?;
        let (reply, answer) = mpsc::sync_channel(1);
        self.requests
            .send(request(reply))
            .map_err(|_| StoreError::Failed)?;
        answer
            .recv()
            .map_err(|_| self.shared.check().err().unwrap_or(StoreError::Failed))
    }

    pub fn append(
        &self,
        system: &SystemKey,
        generation: Generation,
        points: &[(MetricName, ValueKind, i64)],
    ) -> Result<AppendReport, StoreError> {
        let (system, points) = (system.clone(), points.to_vec());
        self.ask(|reply| Request::Append {
            system,
            generation,
            points,
            reply,
        })?
    }

    /// A series' samples, or `None` when the store has no such series.
    pub fn query(&self, series: &SeriesKey, query: &Query) -> Result<Option<Series>, StoreError> {
        self.check()?;
        query::run(&self.shared, series, query)
    }

    /// What the store reports of itself.
    pub fn stats(&self) -> Result<StoreStats, StoreError> {
        self.check()?;
        Ok(self.shared.blocks.stats())
    }

    /// Why the store failed stop, when it decided to (readable after the failure, for the hub
    /// to log); `None` while it runs, or after a panic or a failed commit.
    pub fn fail_cause(&self) -> Option<FailCause> {
        *self
            .shared
            .fail_cause
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `f` inside the writer's transaction.
    pub fn transact<T, A, F>(&self, class: Commit, f: F) -> Result<T, TransactError<A>>
    where
        T: Send + 'static,
        A: Send + 'static,
        F: for<'t> FnOnce(&mut CatalogTxn<'t, C>) -> Result<(T, Answer), Abort<A>> + Send + 'static,
    {
        let answer = self.ask(|reply| Request::Transact {
            class,
            run: catalog::boxed(f, reply),
        });
        match answer {
            Ok(result) => result,
            Err(e) => Err(TransactError::Store(e)),
        }
    }

    /// A read transaction over the catalog tables.
    pub fn read_catalog<T>(
        &self,
        f: impl FnOnce(&CatalogRead) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        self.check()?;
        let txn = self.shared.db.begin_read().map_err(|_| StoreError::Io)?;
        f(&CatalogRead::new(txn))
    }

    pub fn on_commit(&self, hook: CommitHook<C>) {
        *self
            .shared
            .hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::from(hook));
    }

    /// Flushes every tail and commits; later calls answer `Closed`. Idempotent.
    pub fn close(&self) -> Result<(), StoreError> {
        if self.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        let flushed = self.ask(|reply| Request::Close { reply });
        self.closed.store(true, Ordering::SeqCst);
        self.join();
        flushed?
    }
}

impl<C: Send + 'static> Drop for Store<C> {
    /// A store dropped without `close` commits nothing more: what the last group commit holds
    /// is what reopens, as after a crash (a file the block writer was writing has no row, and
    /// the next open deletes it by rule (a)).
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Abandon);
        // The writer holds the database: once joined, the file is free for the next open.
        self.join();
    }
}
