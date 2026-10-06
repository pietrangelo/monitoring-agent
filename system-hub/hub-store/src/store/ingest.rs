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

//! Ingestion on the writer thread (RFC 0010 §2, §6): points stamped at hub time, checked
//! against their series and applied to the head; series interned, or brought back into the
//! head from their tail; and what the next commit must write because of it.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use redb::{ReadableTable, WriteTransaction};

use super::head::HeadEntry;
use super::{AppendReport, Rejected, Shared, StoreError, SystemClock};
use crate::clock::HubClock;
use crate::name::{Generation, MetricName, SystemKey};
use crate::series::{SeriesId, SeriesKey, SeriesRecord};
use crate::state::{Sealed, SeriesState};
use crate::tables::{LoggedPoint, SERIES, SERIES_KEY, TAILS};
use crate::tier::Tier;
use crate::value::ValueKind;

/// The writer's state between commits: everything a commit writes beside the catalog.
pub(crate) struct Core<C> {
    pub(super) shared: Arc<Shared<C>>,
    pub(super) clock: HubClock,
    system_clock: Arc<dyn SystemClock>,
    /// The next `SeriesId` to give.
    pub(super) next_id: u32,
    /// Every series in the head, by id.
    pub(super) index: HashMap<SeriesId, SeriesKey>,
    pub(super) points: Vec<LoggedPoint>,
    /// Series interned since the last commit: their rows are written with their first points.
    pub(super) new_series: BTreeSet<SeriesId>,
    pub(super) sealed: Vec<(SeriesId, Sealed)>,
    /// Series whose tail and record the next commit rewrites (they sealed, or are new).
    pub(super) touched: BTreeSet<SeriesId>,
    pub(super) changes: Vec<C>,
    /// A catalog write by a transaction not yet committed.
    pub(super) catalog_dirty: bool,
}

impl<C> Core<C> {
    pub(super) fn new(
        shared: Arc<Shared<C>>,
        clock: HubClock,
        system_clock: Arc<dyn SystemClock>,
        next_id: u32,
    ) -> Core<C> {
        Core {
            shared,
            clock,
            system_clock,
            next_id,
            index: HashMap::new(),
            points: Vec::new(),
            new_series: BTreeSet::new(),
            sealed: Vec::new(),
            touched: BTreeSet::new(),
            changes: Vec::new(),
            catalog_dirty: false,
        }
    }

    /// Hub time now.
    pub(super) fn hub_now(&mut self) -> u64 {
        self.clock.now(self.system_clock.now())
    }

    pub(super) fn catalog_written(&mut self) {
        self.catalog_dirty = true;
    }

    pub(super) fn take_changes(&mut self, changes: &mut Vec<C>) {
        self.changes.append(changes);
    }

    /// Stamps points at `at` and applies them to the head.
    pub(super) fn append_points(
        &mut self,
        txn: &WriteTransaction,
        system: &SystemKey,
        generation: Generation,
        points: &[(MetricName, ValueKind, i64)],
        at: u64,
    ) -> Result<AppendReport, StoreError> {
        let mut report = AppendReport {
            at,
            accepted: 0,
            rejected: Vec::new(),
        };
        for (metric, kind, value) in points {
            let key = SeriesKey {
                system: system.clone(),
                generation,
                metric: metric.clone(),
            };
            match self.append_point(txn, key, *kind, *value, at)? {
                Ok(()) => report.accepted = report.accepted.saturating_add(1),
                Err(refusal) => report.rejected.push((metric.clone(), refusal)),
            }
        }
        Ok(report)
    }

    fn append_point(
        &mut self,
        txn: &WriteTransaction,
        key: SeriesKey,
        kind: ValueKind,
        value: i64,
        at: u64,
    ) -> Result<Result<(), Rejected>, StoreError> {
        let Ok(scaled) = kind.check_scaled(value) else {
            return Ok(Err(Rejected::OutOfDomain));
        };
        if !self.shared.head.shard(&key).contains_key(&key)
            && let Err(refusal) = self.activate(txn, &key, kind)?
        {
            return Ok(Err(refusal));
        }
        let shared = Arc::clone(&self.shared);
        let mut shard = shared.head.shard(&key);
        let Some(entry) = shard.get_mut(&key) else {
            return Err(StoreError::Failed);
        };
        if entry.state.kind() != kind {
            return Ok(Err(Rejected::KindMismatch));
        }
        let Ok(sealed) = entry.state.append(at, scaled) else {
            return Ok(Err(Rejected::NotAfterLast));
        };
        entry.dirty = true;
        let id = entry.id;
        self.keep_sealed(entry, sealed);
        self.points.push(LoggedPoint { id, ts: at, value });
        Ok(Ok(()))
    }

    /// Records chunks a series sealed: kept in its entry for queries until they commit.
    pub(super) fn keep_sealed(&mut self, entry: &mut HeadEntry, sealed: Vec<Sealed>) {
        for chunk in sealed {
            let slot = &mut entry.last_spans[usize::from(chunk.tier.code())];
            *slot = (*slot).max(Some(chunk.span));
            entry.pending.push(chunk.clone());
            self.sealed.push((entry.id, chunk));
            self.touched.insert(entry.id);
        }
    }

    /// Brings a series into the head: interned and reopened from its tail, or new.
    fn activate(
        &mut self,
        txn: &WriteTransaction,
        key: &SeriesKey,
        kind: ValueKind,
    ) -> Result<Result<(), Rejected>, StoreError> {
        let entry = match interned(txn, key).map_err(|_| StoreError::Failed)? {
            Some(Interned { id, record, tail }) => {
                let state = match tail {
                    Some(bytes) => {
                        SeriesState::from_tail(&bytes).map_err(|_| StoreError::Failed)?
                    }
                    None => SeriesState::new(record.kind),
                };
                HeadEntry {
                    id,
                    state,
                    last_spans: record.last_spans,
                    dirty: false,
                    pending: Vec::new(),
                }
            }
            None => {
                let Some(next) = self.next_id.checked_add(1) else {
                    return Ok(Err(Rejected::SeriesCapReached));
                };
                let id = SeriesId(std::mem::replace(&mut self.next_id, next));
                self.new_series.insert(id);
                self.touched.insert(id);
                HeadEntry {
                    id,
                    state: SeriesState::new(kind),
                    last_spans: [None; 3],
                    dirty: true,
                    pending: Vec::new(),
                }
            }
        };
        self.index.insert(entry.id, key.clone());
        self.shared.head.shard(key).insert(key.clone(), entry);
        Ok(Ok(()))
    }

    /// Closes the buckets of quiet series (RFC 0010 §5's sweep).
    pub(super) fn sweep(&mut self, hub_now: u64) {
        let shared = Arc::clone(&self.shared);
        shared.head.each_shard(|shard| {
            for entry in shard.values_mut() {
                let sealed = entry.state.sweep(hub_now);
                if !sealed.is_empty() {
                    self.keep_sealed(entry, sealed);
                }
            }
        });
    }
}

/// A series the table already holds.
struct Interned {
    id: SeriesId,
    record: SeriesRecord,
    /// Its tail, if it was ever written.
    tail: Option<Vec<u8>>,
}

/// The series of `key`, if the table holds it.
fn interned(txn: &WriteTransaction, key: &SeriesKey) -> Result<Option<Interned>, redb::Error> {
    let Some(id) = txn
        .open_table(SERIES_KEY)?
        .get(key.to_bytes().as_slice())?
        .map(|v| v.value())
    else {
        return Ok(None);
    };
    let Some(record) = txn.open_table(SERIES)?.get(id)?.map(|v| v.value().to_vec()) else {
        return Ok(None);
    };
    let record =
        SeriesRecord::from_bytes(&record).map_err(|e| redb::Error::Corrupted(e.to_string()))?;
    let tail = txn
        .open_table(TAILS)?
        .get((Tier::Raw.code(), id))?
        .map(|v| v.value().to_vec());
    Ok(Some(Interned {
        id: SeriesId(id),
        record,
        tail,
    }))
}
