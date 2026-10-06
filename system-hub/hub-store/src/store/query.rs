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

//! Queries (RFC 0010 §9): committed chunks from a redb read transaction, then the chunks
//! sealed since the last commit and the unsealed points or buckets, copied from the head under
//! the series' shard lock. A chunk sealed while the query runs may be in both; it is read once,
//! by its key. Every value read is checked against the series' kind.

use std::collections::BTreeMap;

use redb::{ReadOnlyTable, ReadTransaction, ReadableDatabase};

use super::blocks::Blocks;
use super::{CorruptChunk, Order, Query, Samples, Series, Shared, StoreError};
use crate::block::name::BlockName;
use crate::block::record::BlockRecord;
use crate::codec::{Bucket, RawPoint, decode_raw, decode_rollup};
use crate::series::{SeriesId, SeriesKey, SeriesRecord};
use crate::state::Sealed;
use crate::tables::{BLOCKS, CHUNKS, SERIES, SERIES_KEY};
use crate::tier::{RollupTier, SpanStart, Tier};
use crate::value::ValueKind;

/// What a query copies out of the head.
struct HeadView {
    id: SeriesId,
    kind: ValueKind,
    last_span: Option<SpanStart>,
    pending: Vec<Sealed>,
    unsealed: Samples,
}

pub(super) fn run<C>(
    shared: &Shared<C>,
    key: &SeriesKey,
    q: &Query,
) -> Result<Option<Series>, StoreError> {
    let view = head_view(shared, key, q.tier);
    let read = shared.db.begin_read().map_err(|_| StoreError::Io)?;
    let view = match view {
        Some(view) => view,
        None => match committed(&read, key)? {
            Some((id, record)) => HeadView {
                id,
                kind: record.kind,
                last_span: record.last_span(q.tier),
                pending: Vec::new(),
                unsealed: empty(q.tier),
            },
            None => return Ok(None),
        },
    };
    let mut chunks = read_chunks(&shared.blocks, &read, &view, q)?;
    for chunk in view.pending.iter().filter(|c| c.tier == q.tier) {
        chunks
            .entry((chunk.span.get(), chunk.seq))
            .or_insert_with(|| chunk.bytes.clone());
    }
    let samples = decode_all(&chunks, view.kind, q.tier, view.unsealed)?;
    Ok(Some(Series {
        kind: view.kind,
        samples: select(samples, q),
    }))
}

fn empty(tier: Tier) -> Samples {
    match tier {
        Tier::Raw => Samples::Raw(Vec::new()),
        Tier::Minute | Tier::Hour => Samples::Rollup(Vec::new()),
    }
}

fn head_view<C>(shared: &Shared<C>, key: &SeriesKey, tier: Tier) -> Option<HeadView> {
    let shard = shared.head.shard(key);
    let entry = shard.get(key)?;
    let unsealed = match tier.rollup() {
        None => Samples::Raw(entry.state.unsealed_points()),
        Some(rollup) => Samples::Rollup(entry.state.unsealed_buckets(rollup)),
    };
    Some(HeadView {
        id: entry.id,
        kind: entry.state.kind(),
        last_span: entry.last_spans[usize::from(tier.code())],
        pending: entry.pending.clone(),
        unsealed,
    })
}

fn committed(
    read: &ReadTransaction,
    key: &SeriesKey,
) -> Result<Option<(SeriesId, SeriesRecord)>, StoreError> {
    let found = (|| -> Result<_, redb::Error> {
        let Some(id) = read
            .open_table(SERIES_KEY)?
            .get(key.to_bytes().as_slice())?
            .map(|v| v.value())
        else {
            return Ok(None);
        };
        Ok(read
            .open_table(SERIES)?
            .get(id)?
            .map(|v| (id, v.value().to_vec())))
    })()
    .map_err(|_| StoreError::Io)?;
    let Some((id, bytes)) = found else {
        return Ok(None);
    };
    let record = SeriesRecord::from_bytes(&bytes).map_err(|_| StoreError::Io)?;
    Ok(Some((SeriesId(id), record)))
}

/// The committed chunks of the series in the query's spans, by (span, seq): from the span's
/// block file when the read transaction holds its `blocks` row (the handoff deleted its chunks
/// from `chunks` in that same commit), from `chunks` otherwise. No live chunk is older than the
/// tier's longest retention before the series' last span.
fn read_chunks(
    blocks: &Blocks,
    read: &ReadTransaction,
    view: &HeadView,
    q: &Query,
) -> Result<BTreeMap<(u64, u16), Vec<u8>>, StoreError> {
    let mut out = BTreeMap::new();
    let Some(last) = view.last_span else {
        return Ok(out);
    };
    let tier = q.tier;
    let first = last
        .get()
        .saturating_sub(tier.max_retention_secs())
        .max(tier.span_of(q.from).get());
    let last = last.get().min(tier.span_of(q.until).get());
    let (chunks, rows) = (
        read.open_table(CHUNKS).map_err(io)?,
        read.open_table(BLOCKS).map_err(io)?,
    );
    let mut span = first;
    while span <= last {
        let held = match rows.get((tier.code(), span)).map_err(io)? {
            Some(row) => from_file(blocks, tier, span, row.value(), view.id)?,
            None => from_chunks(&chunks, tier, span, view.id)?,
        };
        out.extend(held.into_iter().map(|(seq, bytes)| ((span, seq), bytes)));
        span += tier.span_secs();
    }
    Ok(out)
}

/// A series' chunks of a span in its block file, as its `blocks` row names it. A row that
/// doesn't decode, or a span off its grid, is a corrupt span.
fn from_file(
    blocks: &Blocks,
    tier: Tier,
    span: u64,
    row: &[u8],
    id: SeriesId,
) -> Result<Vec<(u16, Vec<u8>)>, StoreError> {
    let corrupt = StoreError::Corrupt(CorruptChunk { tier, span });
    let record = BlockRecord::from_bytes(row).map_err(|_| corrupt)?;
    let span = SpanStart::new(tier, span).ok_or(corrupt)?;
    let name = BlockName {
        tier,
        span,
        rewrite: record.rewrite,
    };
    blocks.series_chunks(name, id)
}

/// A series' chunks of a span still in `chunks`.
fn from_chunks(
    chunks: &ReadOnlyTable<(u8, u64, u32, u16), &[u8]>,
    tier: Tier,
    span: u64,
    id: SeriesId,
) -> Result<Vec<(u16, Vec<u8>)>, StoreError> {
    let range = (tier.code(), span, id.0, 0)..=(tier.code(), span, id.0, u16::MAX);
    chunks
        .range(range)
        .map_err(io)?
        .map(|entry| {
            let (k, v) = entry.map_err(io)?;
            Ok((k.value().3, v.value().to_vec()))
        })
        .collect()
}

/// A redb error on a read, whichever its type.
fn io<E>(_: E) -> StoreError {
    StoreError::Io
}

fn decode_all(
    chunks: &BTreeMap<(u64, u16), Vec<u8>>,
    kind: ValueKind,
    tier: Tier,
    unsealed: Samples,
) -> Result<Samples, StoreError> {
    let corrupt = |span: u64| StoreError::Corrupt(CorruptChunk { tier, span });
    match (tier.rollup(), unsealed) {
        (None, Samples::Raw(open)) => {
            let mut points: Vec<RawPoint> = Vec::new();
            for (&(span, _), bytes) in chunks {
                let decoded = decode_raw(bytes, kind.encoding()).map_err(|_| corrupt(span))?;
                if decoded.iter().any(|p| kind.check_scaled(p.value).is_err()) {
                    return Err(corrupt(span));
                }
                points.extend(decoded);
            }
            Ok(Samples::Raw(append_newer(points, open, |p| p.ts)))
        }
        (Some(rollup), Samples::Rollup(open)) => {
            let mut buckets: Vec<Bucket> = Vec::new();
            for (&(span, _), bytes) in chunks {
                let decoded = decode_rollup(bytes, rollup).map_err(|_| corrupt(span))?;
                if decoded.iter().any(|b| {
                    [b.min, b.avg, b.max]
                        .iter()
                        .any(|v| kind.check_scaled(*v).is_err())
                }) {
                    return Err(corrupt(span));
                }
                buckets.extend(decoded);
            }
            Ok(Samples::Rollup(append_newer(buckets, open, |b| b.index)))
        }
        (None, Samples::Rollup(_)) | (Some(_), Samples::Raw(_)) => Err(StoreError::Io),
    }
}

/// The chunks' items, then the unsealed ones after the last of them. Hub time only runs forward
/// within a series, so an unsealed item at or before the chunks' last was sealed and committed
/// between the head copy and the read transaction: the chunks already hold it.
fn append_newer<T>(mut stored: Vec<T>, unsealed: Vec<T>, at: impl Fn(&T) -> u64) -> Vec<T> {
    let last = stored.last().map(&at);
    stored.extend(
        unsealed
            .into_iter()
            .filter(|item| last.is_none_or(|l| at(item) > l)),
    );
    stored
}

/// The query's range, order and limit, ascending.
fn select(samples: Samples, q: &Query) -> Samples {
    let limit = usize::from(q.limit.get());
    let keep = |len: usize| match q.order {
        Order::Earliest => 0..len.min(limit),
        Order::Latest => len.saturating_sub(limit)..len,
    };
    match samples {
        Samples::Raw(points) => {
            let inside: Vec<RawPoint> = points
                .into_iter()
                .filter(|p| (q.from..=q.until).contains(&p.ts))
                .collect();
            Samples::Raw(inside[keep(inside.len())].to_vec())
        }
        Samples::Rollup(buckets) => {
            let secs = q.tier.rollup().map_or(1, RollupTier::bucket_secs);
            let inside: Vec<Bucket> = buckets
                .into_iter()
                .filter(|b| (q.from..=q.until).contains(&(b.index * secs)))
                .collect();
            Samples::Rollup(inside[keep(inside.len())].to_vec())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsealed_items_the_chunks_already_hold_are_read_once() {
        let at = |x: &u64| *x;
        type Row = (&'static str, Vec<u64>, Vec<u64>, Vec<u64>);
        let cases: [Row; 5] = [
            ("no overlap", vec![1, 2], vec![3, 4], vec![1, 2, 3, 4]),
            (
                "sealed and committed meanwhile",
                vec![1, 2, 3, 4],
                vec![3, 4],
                vec![1, 2, 3, 4],
            ),
            (
                "partly committed",
                vec![1, 2, 3],
                vec![3, 4, 5],
                vec![1, 2, 3, 4, 5],
            ),
            ("nothing stored", vec![], vec![7, 8], vec![7, 8]),
            ("nothing unsealed", vec![1], vec![], vec![1]),
        ];
        for (name, stored, unsealed, expected) in cases {
            assert_eq!(append_newer(stored, unsealed, at), expected, "{name}");
        }
    }
}
