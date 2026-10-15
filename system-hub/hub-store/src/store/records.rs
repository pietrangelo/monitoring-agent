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

//! A series' key from its `series` record, for the code that decides on data by its owner
//! (retention, handoff). A record that doesn't decode is not the same as no record: the
//! first is unreadable, never "unmapped", so nothing is deleted on its account.

use redb::ReadableTable;

use crate::series::{RecordError, SeriesKey, SeriesRecord};

/// Why a series' record couldn't name it.
#[derive(Debug)]
pub(crate) enum Unreadable {
    /// redb refused the read.
    Store(redb::Error),
    /// The record is there but doesn't decode.
    Record(RecordError),
}

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unreadable::Store(e) => write!(f, "series read failed: {e}"),
            Unreadable::Record(e) => write!(f, "series record unreadable: {e}"),
        }
    }
}

impl std::error::Error for Unreadable {}

impl<E: Into<redb::Error>> From<E> for Unreadable {
    fn from(error: E) -> Unreadable {
        Unreadable::Store(error.into())
    }
}

/// The key `series` records for `id`: `None` only when it holds no record.
pub(crate) fn recorded_key(
    series: &impl ReadableTable<u32, &'static [u8]>,
    id: u32,
) -> Result<Option<SeriesKey>, Unreadable> {
    let Some(bytes) = series.get(id)? else {
        return Ok(None);
    };
    let record = SeriesRecord::from_bytes(bytes.value()).map_err(Unreadable::Record)?;
    Ok(Some(record.key))
}

#[cfg(test)]
mod tests {
    use redb::{Database, ReadableDatabase};

    use super::*;
    use crate::name::{Generation, MetricName, SystemKey};
    use crate::series::SeriesRecord;
    use crate::tables::SERIES;
    use crate::value::ValueKind;

    fn key() -> SeriesKey {
        SeriesKey {
            system: SystemKey::try_from(&b"web01"[..]).expect("system"),
            generation: Generation::new(3),
            metric: MetricName::try_from("load1").expect("metric"),
        }
    }

    #[test]
    fn a_record_names_its_series_and_one_that_does_not_decode_is_unreadable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::create(dir.path().join("t.redb")).expect("db");
        let record = SeriesRecord {
            key: key(),
            kind: ValueKind::Load,
            last_spans: [None; 3],
        };
        let mut unknown_version = record.to_bytes();
        unknown_version[0] = 2;
        let whole = record.to_bytes();
        let truncated = &whole[..whole.len() - 1];
        let txn = db.begin_write().expect("write");
        {
            let mut series = txn.open_table(SERIES).expect("series");
            series
                .insert(1, record.to_bytes().as_slice())
                .expect("good");
            series.insert(2, [0xffu8, 0].as_slice()).expect("garbage");
            series
                .insert(3, unknown_version.as_slice())
                .expect("version");
            series.insert(4, [].as_slice()).expect("empty");
            series.insert(6, [1u8, 0xff].as_slice()).expect("bad body");
            series.insert(7, truncated).expect("truncated");
        }
        txn.commit().expect("commit");
        let read = db.begin_read().expect("read");
        let series = read.open_table(SERIES).expect("series");
        let cases = [
            ("a record", 1, Some(Some(key()))),
            ("garbage", 2, None),
            ("an unknown version", 3, None),
            ("no bytes", 4, None),
            ("a known version with a bad body", 6, None),
            ("a record cut short", 7, None),
            ("no record", 5, Some(None)),
        ];
        for (name, id, expected) in cases {
            let found = match recorded_key(&series, id) {
                Ok(found) => Some(found),
                Err(Unreadable::Record(_)) => None,
                Err(Unreadable::Store(e)) => panic!("{name}: {e}"),
            };
            assert_eq!(found, expected, "{name}");
        }
    }
}
