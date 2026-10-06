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

//! Opening the store (RFC 0010 §6 *Recovery*): redb opens the file (repairing it after a crash),
//! then memory is re-derived: the head from the tails, then the points log replayed in order,
//! each point applied only to a series whose last point is earlier, then a sweep at hub now.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use redb::{Database, Durability, ReadableDatabase, ReadableTable, WriteTransaction};

use super::blocks::{Blocks, reconcile_at_open};
use super::handoff::Handoffs;
use super::head::HeadEntry;
use super::writer::{Core, Writer};
use super::{Head, OpenError, Shared, StoreOptions};
use crate::block::file::BlockDir;
use crate::clock::{HubClock, first_retention_clock};
use crate::series::{SeriesId, SeriesRecord};
use crate::state::SeriesState;
use crate::tables::{
    BLOCKS, CHUNKS, META, META_CLOCK, META_COMMIT_SEQ, META_FORMAT, META_ID_COUNTER,
    META_RETENTION_CLOCK, PENDING_UNLINKS, POINTS_LOG, SERIES, SERIES_KEY, STORE_FORMAT, TAILS,
    decode_log_entry,
};

/// The file's name inside the data directory.
pub(crate) const STORE_FILE: &str = "hub.redb";

/// The persisted clocks and counters.
struct Meta {
    clock: u64,
    next_id: u32,
    commit_seq: u64,
}

fn database(e: impl std::fmt::Display) -> OpenError {
    OpenError::Database(e.to_string())
}

pub(super) fn open<C: Send + 'static>(
    data_dir: &Path,
    options: StoreOptions,
) -> Result<(Arc<Shared<C>>, Writer<C>), OpenError> {
    // Temporaries go before redb opens: a crashed raw handoff's can be half a gigabyte (§8).
    let dir = BlockDir::create(data_dir).map_err(database)?;
    dir.remove_temporaries().map_err(database)?;
    let db = Database::builder()
        .set_cache_size(options.cache_bytes)
        .create(data_dir.join(STORE_FILE))
        .map_err(database)?;
    let meta = init_meta(&db, options.clock.now())?;
    let shared = Arc::new(Shared {
        db,
        head: Head::new(),
        blocks: Blocks::new(dir),
        lifecycle: super::lifecycle::Lifecycle::new(),
        fail_cause: Mutex::new(None),
        hook: Mutex::new(None),
    });
    let mut core = Core::new(
        Arc::clone(&shared),
        HubClock::resume(meta.clock),
        Arc::clone(&options.clock),
        meta.next_id,
    );
    let records = load_head(&shared, &mut core)?;
    // Rule (a) asks the tails as persisted and the clock as last committed (§6 step 2a).
    let waiting = reconcile_at_open(&shared.db, &shared.head, &shared.blocks, meta.clock)?;
    replay(&shared, &mut core, &records)?;
    let now = core.hub_now();
    core.sweep(now);
    let handoffs = Handoffs::new(waiting);
    Ok((
        shared,
        Writer::new(core, &options, meta.commit_seq, handoffs),
    ))
}

fn read_u64(bytes: Option<&[u8]>) -> Result<Option<u64>, OpenError> {
    bytes
        .map(|b| {
            <[u8; 8]>::try_from(b)
                .map(u64::from_be_bytes)
                .map_err(|_| OpenError::Corrupt("a meta value".into()))
        })
        .transpose()
}

/// Reads the meta keys, creating them in a new file.
fn init_meta(db: &Database, system_now: u64) -> Result<Meta, OpenError> {
    let mut txn = db.begin_write().map_err(database)?;
    txn.set_durability(Durability::Immediate)
        .map_err(database)?;
    txn.set_two_phase_commit(true);
    txn.set_quick_repair(true);
    let meta = read_meta(&txn, system_now)?;
    create_store_tables(&txn)?;
    txn.commit().map_err(database)?;
    Ok(meta)
}

/// The meta keys; in a new file, the format and the retention clock are written first.
fn read_meta(txn: &WriteTransaction, system_now: u64) -> Result<Meta, OpenError> {
    let mut table = txn.open_table(META).map_err(database)?;
    let get = |key: &str| -> Result<Option<u64>, OpenError> {
        read_u64(
            table
                .get(key)
                .map_err(database)?
                .as_ref()
                .map(|v| v.value()),
        )
    };
    let (format, clock, id_counter, commit_seq) = (
        get(META_FORMAT)?,
        get(META_CLOCK)?,
        get(META_ID_COUNTER)?,
        get(META_COMMIT_SEQ)?,
    );
    match format {
        Some(STORE_FORMAT) => {}
        Some(other) => return Err(OpenError::Format(other)),
        None => {
            let retention = first_retention_clock(system_now, system_now);
            for (key, value) in [
                (META_FORMAT, STORE_FORMAT),
                (META_RETENTION_CLOCK, retention),
            ] {
                table
                    .insert(key, value.to_be_bytes().as_slice())
                    .map_err(database)?;
            }
        }
    }
    let next_id = u32::try_from(id_counter.unwrap_or(0))
        .map_err(|_| OpenError::Corrupt("the id counter".into()))?;
    Ok(Meta {
        clock: clock.unwrap_or(0),
        next_id,
        commit_seq: commit_seq.unwrap_or(0),
    })
}

/// Every store table exists from the first open, so readers never meet a missing one.
fn create_store_tables(txn: &WriteTransaction) -> Result<(), OpenError> {
    txn.open_table(SERIES).map_err(database)?;
    txn.open_table(SERIES_KEY).map_err(database)?;
    txn.open_table(POINTS_LOG).map_err(database)?;
    txn.open_table(TAILS).map_err(database)?;
    txn.open_table(CHUNKS).map_err(database)?;
    txn.open_table(BLOCKS).map_err(database)?;
    txn.open_table(PENDING_UNLINKS).map_err(database)?;
    Ok(())
}

/// Every series record, and the head: each series with a tail, reopened from it.
fn load_head<C>(
    shared: &Shared<C>,
    core: &mut Core<C>,
) -> Result<HashMap<SeriesId, SeriesRecord>, OpenError> {
    let read = shared.db.begin_read().map_err(database)?;
    let mut records = HashMap::new();
    for entry in read
        .open_table(SERIES)
        .map_err(database)?
        .iter()
        .map_err(database)?
    {
        let (id, bytes) = entry.map_err(database)?;
        let record = SeriesRecord::from_bytes(bytes.value())
            .map_err(|e| OpenError::Corrupt(format!("series {}: {e}", id.value())))?;
        records.insert(SeriesId(id.value()), record);
    }
    for entry in read
        .open_table(TAILS)
        .map_err(database)?
        .iter()
        .map_err(database)?
    {
        let (key, bytes) = entry.map_err(database)?;
        let id = SeriesId(key.value().1);
        let record = records
            .get(&id)
            .ok_or_else(|| OpenError::Corrupt(format!("a tail of no series ({})", id.0)))?;
        let state = SeriesState::from_tail(bytes.value())
            .map_err(|e| OpenError::Corrupt(format!("tail {}: {e}", id.0)))?;
        install(core, id, record, state);
    }
    Ok(records)
}

fn install<C>(core: &mut Core<C>, id: SeriesId, record: &SeriesRecord, state: SeriesState) {
    let entry = HeadEntry {
        id,
        state,
        last_spans: record.last_spans,
        dirty: false,
        pending: Vec::new(),
    };
    core.index.insert(id, record.key.clone());
    core.shared
        .head
        .shard(&record.key)
        .insert(record.key.clone(), entry);
}

/// Applies the points log in commit order: each point only to a series whose last point is
/// earlier, so a point already in a tail or a sealed chunk is never applied twice.
fn replay<C>(
    shared: &Shared<C>,
    core: &mut Core<C>,
    records: &HashMap<SeriesId, SeriesRecord>,
) -> Result<(), OpenError> {
    let read = shared.db.begin_read().map_err(database)?;
    for entry in read
        .open_table(POINTS_LOG)
        .map_err(database)?
        .iter()
        .map_err(database)?
    {
        let (seq, bytes) = entry.map_err(database)?;
        let points = decode_log_entry(bytes.value())
            .map_err(|_| OpenError::Corrupt(format!("log entry {}", seq.value())))?;
        for point in points {
            let record = records.get(&point.id).ok_or_else(|| {
                OpenError::Corrupt(format!("a logged point of no series ({})", point.id.0))
            })?;
            if !core.index.contains_key(&point.id) {
                install(core, point.id, record, SeriesState::new(record.kind));
            }
            let value = record
                .kind
                .check_scaled(point.value)
                .map_err(|_| OpenError::Corrupt(format!("log entry {}", seq.value())))?;
            let mut shard = shared.head.shard(&record.key);
            let Some(entry) = shard.get_mut(&record.key) else {
                continue;
            };
            if let Ok(sealed) = entry.state.append(point.ts, value) {
                entry.dirty = true;
                core.keep_sealed(entry, sealed);
            }
        }
    }
    Ok(())
}
