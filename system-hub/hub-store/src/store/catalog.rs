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

//! Catalog transactions (RFC 0010 §6, §9; RFC 0011 §1): `f` runs on the writer thread inside
//! the group commit's redb transaction. It may refuse only before its first write; a redb
//! error inside it is recorded and fails the store once `f` returns, so `f` sees plain values.

use std::sync::mpsc::SyncSender;

use redb::{
    ReadOnlyTable, ReadTransaction, ReadableTable, TableDefinition, TableError, WriteTransaction,
};

use super::writer::Core;
use super::{Abort, Answer, AppendReport, CatalogTable, StoreError, TransactError};
use crate::name::{Generation, MetricName, SystemKey};
use crate::value::ValueKind;

/// One catalog entry: its key and value bytes.
pub type CatalogEntry = (Vec<u8>, Vec<u8>);

/// A catalog table opened for reading.
type ReadTable = ReadOnlyTable<&'static [u8], &'static [u8]>;

/// How a settled transaction is answered once its commit landed, or failed.
pub(super) type Deliver = Box<dyn FnOnce(Result<(), StoreError>) + Send>;

fn definition(table: CatalogTable) -> TableDefinition<'static, &'static [u8], &'static [u8]> {
    TableDefinition::new(table.name())
}

/// Inside `transact`: the catalog tables, read and written in the writer's transaction.
pub struct CatalogTxn<'t, C> {
    txn: &'t WriteTransaction,
    core: &'t mut Core<C>,
    now: u64,
    wrote: bool,
    error: Option<StoreError>,
    changes: Vec<C>,
}

impl<'t, C> CatalogTxn<'t, C> {
    pub(super) fn new(
        txn: &'t WriteTransaction,
        core: &'t mut Core<C>,
        now: u64,
    ) -> CatalogTxn<'t, C> {
        CatalogTxn {
            txn,
            core,
            now,
            wrote: false,
            error: None,
            changes: Vec::new(),
        }
    }

    /// The hub time of this transaction: committed with it, so no stored hub time is later
    /// than the committed clock.
    pub fn now(&self) -> u64 {
        self.now
    }

    /// Whether the transaction has written anything: a catalog write that changed a value, or
    /// a staged point. A write equal to the stored value isn't one.
    pub fn wrote(&self) -> bool {
        self.wrote
    }

    fn failed(&mut self) {
        self.error = Some(StoreError::Failed);
    }

    pub fn get(&mut self, table: CatalogTable, key: &[u8]) -> Option<Vec<u8>> {
        let found = self
            .txn
            .open_table(definition(table))
            .and_then(|t| Ok(t.get(key)?.map(|v| v.value().to_vec())));
        found.unwrap_or_else(|_| {
            self.failed();
            None
        })
    }

    /// Up to `limit` entries whose key starts with `prefix`, in key order.
    pub fn prefix(
        &mut self,
        table: CatalogTable,
        prefix: &[u8],
        limit: usize,
    ) -> Vec<CatalogEntry> {
        let found = self
            .txn
            .open_table(definition(table))
            .map_err(redb::Error::from)
            .and_then(|t| scan(&t, prefix, limit));
        found.unwrap_or_else(|_| {
            self.failed();
            Vec::new()
        })
    }

    /// Writes `value` at `key`, unless it is already there.
    pub fn insert(&mut self, table: CatalogTable, key: &[u8], value: &[u8]) {
        let written = self
            .txn
            .open_table(definition(table))
            .map_err(redb::Error::from)
            .and_then(|mut t| {
                if t.get(key)?.is_some_and(|v| v.value() == value) {
                    return Ok(false);
                }
                t.insert(key, value)?;
                Ok(true)
            });
        self.record_write(written);
    }

    /// Removes an entry; whether there was one.
    pub fn remove(&mut self, table: CatalogTable, key: &[u8]) -> bool {
        let removed = self
            .txn
            .open_table(definition(table))
            .map_err(redb::Error::from)
            .and_then(|mut t| Ok(t.remove(key)?.is_some()));
        self.record_write(removed)
    }

    fn record_write(&mut self, outcome: Result<bool, redb::Error>) -> bool {
        match outcome {
            Ok(true) => {
                self.wrote = true;
                self.core.catalog_written();
                true
            }
            Ok(false) => false,
            Err(_) => {
                self.failed();
                false
            }
        }
    }

    /// A change for the commit hook, delivered with the commit holding this transaction;
    /// dropped if `f` aborts.
    pub fn notify(&mut self, change: C) {
        self.changes.push(change);
    }

    /// Stages points in this transaction, stamped at its hub time and committed with it. A
    /// staged point counts as a write: `f` may no longer abort.
    pub fn append(
        &mut self,
        system: &SystemKey,
        generation: Generation,
        points: &[(MetricName, ValueKind, i64)],
    ) -> AppendReport {
        match self
            .core
            .append_points(self.txn, system, generation, points, self.now)
        {
            Ok(report) => {
                self.wrote |= report.accepted > 0;
                report
            }
            Err(e) => {
                self.error = Some(e);
                AppendReport {
                    at: self.now,
                    accepted: 0,
                    rejected: Vec::new(),
                }
            }
        }
    }
}

/// Up to `limit` entries of `table` whose key starts with `prefix`.
fn scan<T: ReadableTable<&'static [u8], &'static [u8]>>(
    t: &T,
    prefix: &[u8],
    limit: usize,
) -> Result<Vec<CatalogEntry>, redb::Error> {
    let mut out = Vec::new();
    for entry in t.range::<&[u8]>(prefix..)? {
        let (k, v) = entry?;
        if out.len() >= limit || !k.value().starts_with(prefix) {
            break;
        }
        out.push((k.value().to_vec(), v.value().to_vec()));
    }
    Ok(out)
}

/// What the writer does with a transaction once `f` returned.
pub(super) enum Settled {
    /// `f` refused before writing: answered at once.
    Now(Box<dyn FnOnce() + Send>),
    /// `f` returned a value: answered after the commit holding it, or at once when it wrote
    /// nothing and nothing uncommitted lies beneath it (`AfterCommit`), or at once (`AtOnce`).
    Wait {
        wrote: bool,
        answer: Answer,
        deliver: Deliver,
    },
    /// `f` refused after a write, or redb failed inside it: the store fails stop.
    FailStop(Box<dyn FnOnce() + Send>),
}

/// The changes of a transaction that settled with a value, and how it settled.
pub(super) type Run<C> = Box<dyn for<'t> FnOnce(&mut CatalogTxn<'t, C>) -> Settled + Send>;

/// `f` boxed for the writer, answering `reply` as it settles.
pub(super) fn boxed<C, T, A, F>(f: F, reply: SyncSender<Result<T, TransactError<A>>>) -> Run<C>
where
    T: Send + 'static,
    A: Send + 'static,
    F: for<'t> FnOnce(&mut CatalogTxn<'t, C>) -> Result<(T, Answer), Abort<A>> + Send + 'static,
{
    Box::new(move |txn: &mut CatalogTxn<'_, C>| {
        let outcome = f(txn);
        if txn.error.is_some() {
            return Settled::FailStop(Box::new(move || {
                let _ = reply.send(Err(TransactError::Store(StoreError::Failed)));
            }));
        }
        match outcome {
            Err(abort) if !txn.wrote => Settled::Now(Box::new(move || {
                let _ = reply.send(Err(TransactError::Aborted(abort)));
            })),
            Err(_) => Settled::FailStop(Box::new(move || {
                let _ = reply.send(Err(TransactError::Store(StoreError::Failed)));
            })),
            Ok((value, answer)) => {
                txn.core.take_changes(&mut txn.changes);
                Settled::Wait {
                    wrote: txn.wrote,
                    answer,
                    deliver: Box::new(move |committed| {
                        let _ = reply.send(committed.map(|()| value).map_err(TransactError::Store));
                    }),
                }
            }
        }
    })
}

/// A read transaction over the catalog tables (MVCC: it never waits for the writer).
pub struct CatalogRead {
    txn: ReadTransaction,
}

impl CatalogRead {
    pub(super) fn new(txn: ReadTransaction) -> CatalogRead {
        CatalogRead { txn }
    }

    /// The table, or `None` while nothing was ever written to it.
    fn table(&self, table: CatalogTable) -> Result<Option<ReadTable>, StoreError> {
        match self.txn.open_table(definition(table)) {
            Ok(t) => Ok(Some(t)),
            Err(TableError::TableDoesNotExist(_)) => Ok(None),
            Err(_) => Err(StoreError::Io),
        }
    }

    pub fn get(&self, table: CatalogTable, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        let Some(t) = self.table(table)? else {
            return Ok(None);
        };
        let found = t.get(key).map_err(|_| StoreError::Io)?;
        Ok(found.map(|v| v.value().to_vec()))
    }

    /// Up to `limit` entries whose key starts with `prefix`, in key order.
    pub fn prefix(
        &self,
        table: CatalogTable,
        prefix: &[u8],
        limit: usize,
    ) -> Result<Vec<CatalogEntry>, StoreError> {
        let Some(t) = self.table(table)? else {
            return Ok(Vec::new());
        };
        scan(&t, prefix, limit).map_err(|_| StoreError::Io)
    }
}
