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

//! What the store does with bytes it didn't write (RFC 0010 §4, §6; A08): a store of another
//! format, a damaged tail or log entry, a logged point of no series refuse the open; a damaged
//! chunk, or one holding a value outside its kind's domain, fails the query of its span.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use hub_store::codec::{RawChunk, RawPoint};
use hub_store::name::{Generation, MetricName, SystemKey};
use hub_store::series::SeriesKey;
use hub_store::store::{
    CorruptChunk, Limit, OpenError, Order, Query, Store, StoreError, StoreOptions, SystemClock,
};
use hub_store::tier::Tier;
use hub_store::value::{Encoding, ValueKind};
use redb::{Database, ReadableTable, TableDefinition};

const T0: u64 = 1_800_057_600;
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const TAILS: TableDefinition<(u8, u32), &[u8]> = TableDefinition::new("tails");
const LOG: TableDefinition<u64, &[u8]> = TableDefinition::new("points_log");
const CHUNKS: TableDefinition<(u8, u64, u32, u16), &[u8]> = TableDefinition::new("chunks");

struct Fixed(u64);

impl SystemClock for Fixed {
    fn now(&self) -> u64 {
        self.0
    }
}

fn open(dir: &Path) -> Result<Store<()>, OpenError> {
    let options = StoreOptions {
        commit_interval: Duration::from_millis(20),
        ..StoreOptions::with_clock(Arc::new(Fixed(T0 + 5_000)))
    };
    Store::open(dir, options)
}

fn key() -> SeriesKey {
    SeriesKey {
        system: SystemKey::try_from(&b"s"[..]).expect("valid"),
        generation: Generation::new(1),
        metric: MetricName::try_from("cpu").expect("valid"),
    }
}

/// A closed store holding one sealed raw chunk of `s/cpu` and an open one, then `plant` run on
/// its file through redb directly.
fn planted(plant: impl FnOnce(&Database)) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let options = StoreOptions {
            commit_interval: Duration::from_millis(20),
            ..StoreOptions::with_clock(Arc::new(Fixed(T0)))
        };
        let clock = Arc::new(std::sync::atomic::AtomicU64::new(T0));
        struct Moving(Arc<std::sync::atomic::AtomicU64>);
        impl SystemClock for Moving {
            fn now(&self) -> u64 {
                self.0.load(std::sync::atomic::Ordering::SeqCst)
            }
        }
        let options = StoreOptions {
            clock: Arc::new(Moving(Arc::clone(&clock))),
            ..options
        };
        let store: Store<()> = Store::open(dir.path(), options).expect("opens");
        for i in 0..250u64 {
            clock.store(T0 + i, std::sync::atomic::Ordering::SeqCst);
            let point = [(key().metric, ValueKind::Percent, 100)];
            store
                .append(&key().system, Generation::new(1), &point)
                .expect("append");
        }
        store.close().expect("closes");
    }
    let db = Database::open(dir.path().join("hub.redb")).expect("redb opens");
    plant(&db);
    drop(db);
    dir
}

fn write(db: &Database, f: impl FnOnce(&redb::WriteTransaction)) {
    let txn = db.begin_write().expect("write");
    f(&txn);
    txn.commit().expect("commit");
}

#[test]
fn bytes_the_store_cant_read_refuse_the_open() {
    type Plant = Box<dyn FnOnce(&Database)>;
    type Row = (&'static str, Plant, fn(&OpenError) -> bool);
    let cases: [Row; 4] = [
        (
            "another store format",
            Box::new(|db| {
                write(db, |t| {
                    t.open_table(META)
                        .expect("meta")
                        .insert("format", 2u64.to_be_bytes().as_slice())
                        .expect("insert");
                })
            }),
            |e| matches!(e, OpenError::Format(2)),
        ),
        (
            "a damaged tail",
            Box::new(|db| {
                write(db, |t| {
                    let mut tails = t.open_table(TAILS).expect("tails");
                    let first = tails
                        .first()
                        .expect("read")
                        .map(|(k, _)| k.value())
                        .expect("a tail");
                    tails.insert(first, [1u8, 1, 9].as_slice()).expect("insert");
                })
            }),
            |e| matches!(e, OpenError::Corrupt(_)),
        ),
        (
            "a damaged log entry",
            Box::new(|db| {
                write(db, |t| {
                    t.open_table(LOG)
                        .expect("log")
                        .insert(u64::MAX, [9u8, 9].as_slice())
                        .expect("insert");
                })
            }),
            |e| matches!(e, OpenError::Corrupt(_)),
        ),
        (
            "a logged point of no series",
            // Version 1, one point, base time T0, series 999, offset 0, value 0.
            Box::new(|db| {
                write(db, |t| {
                    let mut entry = vec![1u8, 1];
                    let mut base = T0;
                    while base >= 0x80 {
                        entry.push((base as u8 & 0x7f) | 0x80);
                        base >>= 7;
                    }
                    entry.push(base as u8);
                    entry.extend([0xe7, 0x07, 0, 0]);
                    t.open_table(LOG)
                        .expect("log")
                        .insert(u64::MAX, entry.as_slice())
                        .expect("insert");
                })
            }),
            |e| matches!(e, OpenError::Corrupt(_)),
        ),
    ];
    for (name, plant, expected) in cases {
        let dir = planted(plant);
        match open(dir.path()) {
            Err(e) => assert!(expected(&e), "{name}: {e}"),
            Ok(_) => panic!("{name}: opened"),
        }
    }
}

#[test]
fn a_chunk_the_store_cant_trust_fails_the_query_of_its_span() {
    let out_of_domain = {
        let mut chunk = RawChunk::new(Encoding::Delta);
        chunk
            .push(RawPoint {
                ts: T0,
                value: 200_000,
            })
            .expect("fits");
        chunk.encode()
    };
    let cases = [
        ("bytes that don't decode", vec![0x01u8, 0xff]),
        ("a percent of 2,000 %", out_of_domain),
    ];
    for (name, bytes) in cases {
        let dir = planted(|db| {
            write(db, |t| {
                let mut chunks = t.open_table(CHUNKS).expect("chunks");
                let first = chunks
                    .first()
                    .expect("read")
                    .map(|(k, _)| k.value())
                    .expect("a sealed chunk");
                chunks.insert(first, bytes.as_slice()).expect("insert");
            })
        });
        let store = open(dir.path()).expect("opens: chunks are read by queries");
        let q = Query {
            from: 0,
            until: u64::MAX,
            tier: Tier::Raw,
            order: Order::Earliest,
            limit: Limit::new(10),
        };
        let span = Tier::Raw.span_of(T0).get();
        assert_eq!(
            store.query(&key(), &q).err(),
            Some(StoreError::Corrupt(CorruptChunk {
                tier: Tier::Raw,
                span
            })),
            "{name}"
        );
    }
}
