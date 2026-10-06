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

//! The store's tables in `hub.redb` (RFC 0010 §5) and the points log's entry format.

use redb::TableDefinition;

use crate::codec::bits::{read_varint, unzigzag, write_varint, zigzag};
use crate::series::SeriesId;

/// `&str` → bytes: the store's own keys (below) and the hub's, under `hub/`.
pub(crate) const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
/// `SeriesId` → `SeriesRecord`.
pub(crate) const SERIES: TableDefinition<u32, &[u8]> = TableDefinition::new("series");
/// Series key bytes → `SeriesId`.
pub(crate) const SERIES_KEY: TableDefinition<&[u8], u32> = TableDefinition::new("series_key");
/// Commit sequence → the points of one group commit.
pub(crate) const POINTS_LOG: TableDefinition<u64, &[u8]> = TableDefinition::new("points_log");
/// (tier, `SeriesId`) → the series' tail. The store keeps one tail per series, keyed by the
/// raw tier's code; the tail holds every tier.
pub(crate) const TAILS: TableDefinition<(u8, u32), &[u8]> = TableDefinition::new("tails");
/// (tier, span start, `SeriesId`, seq) → a sealed chunk of a span not yet handed off.
pub(crate) const CHUNKS: TableDefinition<(u8, u64, u32, u16), &[u8]> =
    TableDefinition::new("chunks");

/// The format of the store: refused at open when it differs.
pub(crate) const META_FORMAT: &str = "format";
/// The next `SeriesId` to give.
pub(crate) const META_ID_COUNTER: &str = "id_counter";
/// `last_issued`: hub time as last committed.
pub(crate) const META_CLOCK: &str = "clock";
/// The next commit sequence.
pub(crate) const META_COMMIT_SEQ: &str = "commit_seq";
/// The retention clock as the last pass persisted it.
pub(crate) const META_RETENTION_CLOCK: &str = "retention_clock";

/// The one format this version writes and reads.
pub(crate) const STORE_FORMAT: u64 = 1;

/// One point as the log keeps it until its series' tail holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LoggedPoint {
    pub id: SeriesId,
    pub ts: u64,
    pub value: i64,
}

/// Bytes that are no log entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BadLogEntry;

const LOG_V1: u8 = 1;

/// A group commit's points: a version byte, the point count and the earliest timestamp, then
/// per point its series id, its timestamp after the earliest and its zigzagged value, each a
/// varint.
pub(crate) fn encode_log_entry(points: &[LoggedPoint]) -> Vec<u8> {
    let base = points.iter().map(|p| p.ts).min().unwrap_or(0);
    let mut out = vec![LOG_V1];
    write_varint(&mut out, points.len() as u64);
    write_varint(&mut out, base);
    for p in points {
        write_varint(&mut out, u64::from(p.id.0));
        write_varint(&mut out, p.ts - base);
        write_varint(&mut out, zigzag(p.value));
    }
    out
}

pub(crate) fn decode_log_entry(bytes: &[u8]) -> Result<Vec<LoggedPoint>, BadLogEntry> {
    if bytes.first() != Some(&LOG_V1) {
        return Err(BadLogEntry);
    }
    let mut pos = 1;
    let mut next = || read_varint(bytes, &mut pos).map_err(|_| BadLogEntry);
    let count = next()?;
    let base = next()?;
    // Each point takes at least three bytes, so a count can't make this allocate past the input.
    let mut points = Vec::with_capacity(usize::try_from(count).unwrap_or(0).min(bytes.len() / 3));
    for _ in 0..count {
        let id = u32::try_from(next()?).map_err(|_| BadLogEntry)?;
        let ts = base.checked_add(next()?).ok_or(BadLogEntry)?;
        let value = unzigzag(next()?);
        points.push(LoggedPoint {
            id: SeriesId(id),
            ts,
            value,
        });
    }
    if pos != bytes.len() {
        return Err(BadLogEntry);
    }
    Ok(points)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_entry_round_trips_its_points_in_order() {
        let points = vec![
            LoggedPoint {
                id: SeriesId(7),
                ts: 1_800_000_002,
                value: 1_234,
            },
            LoggedPoint {
                id: SeriesId(u32::MAX),
                ts: 1_800_000_001,
                value: -1,
            },
            LoggedPoint {
                id: SeriesId(0),
                ts: u64::MAX,
                value: i64::MIN,
            },
            LoggedPoint {
                id: SeriesId(7),
                ts: 1_800_000_003,
                value: i64::MAX,
            },
        ];
        assert_eq!(decode_log_entry(&encode_log_entry(&points)), Ok(points));
        assert_eq!(decode_log_entry(&encode_log_entry(&[])), Ok(vec![]));
    }

    #[test]
    fn a_steady_entry_costs_a_few_bytes_a_point() {
        let points: Vec<LoggedPoint> = (0..1_000)
            .map(|i| LoggedPoint {
                id: SeriesId(i),
                ts: 1_800_000_000,
                value: 5_000,
            })
            .collect();
        let len = encode_log_entry(&points).len();
        assert!(len <= 1 + 10 + 1_000 * 6, "{len}");
    }

    #[test]
    fn a_damaged_entry_is_refused() {
        let good = encode_log_entry(&[LoggedPoint {
            id: SeriesId(1),
            ts: 10,
            value: 2,
        }]);
        let mut version = good.clone();
        version[0] = 9;
        assert_eq!(decode_log_entry(&version), Err(BadLogEntry));
        for cut in 0..good.len() {
            assert_eq!(
                decode_log_entry(&good[..cut]),
                Err(BadLogEntry),
                "cut at {cut}"
            );
        }
        let mut longer = good.clone();
        longer.push(0x80);
        assert_eq!(decode_log_entry(&longer), Err(BadLogEntry));
    }
}
