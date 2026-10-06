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

//! One series' state in the head (RFC 0010 §5, §6): its open raw chunk, its open rollup
//! buckets and chunks, and the chunks they seal. A pure state machine over hub time; the
//! writer persists what it seals and, as the series' tail, what it holds open.

mod tail;

pub use tail::TailError;

use crate::codec::{Bucket, PushRefused, RawChunk, RawPoint, RollupChunk};
use crate::rollup::Accumulator;
use crate::tier::{RollupTier, SpanStart, Tier};
use crate::value::{Scaled, ValueKind};

/// A chunk the series sealed: the writer inserts it into `chunks` at the next commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sealed {
    pub tier: Tier,
    pub span: SpanStart,
    pub seq: u16,
    pub bytes: Vec<u8>,
}

/// A point at or before the series' last one (`Rejected::NotAfterLast` in the store).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotAfterLast;

/// The open chunk of one tier: its span and the seq its next seal takes there.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Open<C> {
    chunk: C,
    span: Option<SpanStart>,
    next_seq: u16,
}

/// A rollup tier's open bucket and open chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RollupState {
    tier: RollupTier,
    bucket: Option<Accumulator>,
    open: Open<RollupChunk>,
}

/// One series in the head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeriesState {
    kind: ValueKind,
    last_ts: Option<u64>,
    raw: Open<RawChunk>,
    minute: RollupState,
    hour: RollupState,
}

impl SeriesState {
    pub fn new(kind: ValueKind) -> SeriesState {
        SeriesState {
            kind,
            last_ts: None,
            raw: Open {
                chunk: RawChunk::new(kind.encoding()),
                span: None,
                next_seq: 0,
            },
            minute: RollupState::new(RollupTier::Minute),
            hour: RollupState::new(RollupTier::Hour),
        }
    }

    pub fn kind(&self) -> ValueKind {
        self.kind
    }

    /// The series' last point's hub time.
    pub fn last_ts(&self) -> Option<u64> {
        self.last_ts
    }

    /// Adds a point after the series' last one, returning what it sealed.
    pub fn append(&mut self, ts: u64, value: Scaled) -> Result<Vec<Sealed>, NotAfterLast> {
        if self.last_ts.is_some_and(|last| ts <= last) {
            return Err(NotAfterLast);
        }
        let point = RawPoint {
            ts,
            value: value.get(),
        };
        let mut sealed: Vec<Sealed> = self.raw.push_point(point).into_iter().collect();
        sealed.extend(self.minute.add(ts, point.value));
        sealed.extend(self.hour.add(ts, point.value));
        self.last_ts = Some(ts);
        Ok(sealed)
    }

    /// Closes the open buckets whose end is more than one bucket length before `hub_now`
    /// (RFC 0010 §5's sweep), returning what that sealed.
    pub fn sweep(&mut self, hub_now: u64) -> Vec<Sealed> {
        [self.minute.sweep(hub_now), self.hour.sweep(hub_now)]
            .into_iter()
            .flatten()
            .collect()
    }

    /// Seals the tier's open chunk if it holds part of `span` (a closing span, §6).
    pub fn seal_span(&mut self, tier: Tier, span: SpanStart) -> Option<Sealed> {
        match tier {
            Tier::Raw => self.raw.seal_if_in(span),
            Tier::Minute => self.minute.open.seal_if_in(span),
            Tier::Hour => self.hour.open.seal_if_in(span),
        }
    }

    /// The raw points not yet sealed, in time order.
    pub fn unsealed_points(&self) -> Vec<RawPoint> {
        self.raw.chunk.points()
    }

    /// The closed buckets of a rollup tier not yet sealed, in time order.
    pub fn unsealed_buckets(&self, tier: RollupTier) -> Vec<Bucket> {
        match tier {
            RollupTier::Minute => self.minute.open.chunk.buckets(),
            RollupTier::Hour => self.hour.open.chunk.buckets(),
        }
    }
}

/// What the head needs of an open chunk, raw or rollup.
trait OpenChunk: Clone {
    type Item: Copy;
    fn tier(&self) -> Tier;
    fn fresh(&self) -> Self;
    fn push(&mut self, item: Self::Item) -> Result<(), PushRefused>;
    fn is_empty(&self) -> bool;
    fn encode(&self) -> Vec<u8>;
}

impl OpenChunk for RawChunk {
    type Item = RawPoint;
    fn tier(&self) -> Tier {
        Tier::Raw
    }
    fn fresh(&self) -> Self {
        RawChunk::new(self.encoding())
    }
    fn push(&mut self, item: RawPoint) -> Result<(), PushRefused> {
        RawChunk::push(self, item)
    }
    fn is_empty(&self) -> bool {
        RawChunk::is_empty(self)
    }
    fn encode(&self) -> Vec<u8> {
        RawChunk::encode(self)
    }
}

impl OpenChunk for RollupChunk {
    type Item = Bucket;
    fn tier(&self) -> Tier {
        self.rollup_tier().tier()
    }
    fn fresh(&self) -> Self {
        RollupChunk::new(self.rollup_tier())
    }
    fn push(&mut self, item: Bucket) -> Result<(), PushRefused> {
        RollupChunk::push(self, item)
    }
    fn is_empty(&self) -> bool {
        RollupChunk::is_empty(self)
    }
    fn encode(&self) -> Vec<u8> {
        RollupChunk::encode(self)
    }
}

impl<C: OpenChunk> Open<C> {
    /// Adds an item of the span starting at `span`: a new span seals the open chunk first, and
    /// a full chunk is sealed and the item starts the next one.
    fn push_in(&mut self, span: SpanStart, item: C::Item) -> Vec<Sealed> {
        let mut sealed: Vec<Sealed> = Vec::new();
        if self.span != Some(span) {
            sealed.extend(self.seal());
            (self.span, self.next_seq) = (Some(span), 0);
        }
        if self.chunk.push(item).is_err() {
            sealed.extend(self.seal());
            // An empty chunk takes any one item of its kind.
            let _ = self.chunk.push(item);
        }
        sealed
    }

    /// Seals the open chunk, if it holds anything, at the next seq of its span.
    fn seal(&mut self) -> Option<Sealed> {
        let span = self.span?;
        if self.chunk.is_empty() {
            return None;
        }
        let sealed = Sealed {
            tier: self.chunk.tier(),
            span,
            seq: self.next_seq,
            bytes: self.chunk.encode(),
        };
        self.chunk = self.chunk.fresh();
        self.next_seq = self.next_seq.saturating_add(1);
        Some(sealed)
    }

    fn seal_if_in(&mut self, span: SpanStart) -> Option<Sealed> {
        if self.span == Some(span) {
            self.seal()
        } else {
            None
        }
    }
}

impl Open<RawChunk> {
    fn push_point(&mut self, point: RawPoint) -> Vec<Sealed> {
        self.push_in(Tier::Raw.span_of(point.ts), point)
    }
}

impl RollupState {
    fn new(tier: RollupTier) -> RollupState {
        RollupState {
            tier,
            bucket: None,
            open: Open {
                chunk: RollupChunk::new(tier),
                span: None,
                next_seq: 0,
            },
        }
    }

    /// Adds a point: a point of a later bucket closes the open one first.
    fn add(&mut self, ts: u64, value: i64) -> Vec<Sealed> {
        if let Some(open) = self.bucket.as_mut().filter(|b| b.holds(self.tier, ts)) {
            open.add(value);
            return Vec::new();
        }
        let sealed = self.close_bucket();
        self.bucket = Some(Accumulator::open(self.tier, ts, value));
        sealed
    }

    fn sweep(&mut self, hub_now: u64) -> Vec<Sealed> {
        let len = self.tier.bucket_secs();
        let past = self
            .bucket
            .is_some_and(|b| hub_now.saturating_sub((b.index() + 1) * len) > len);
        if past {
            self.close_bucket()
        } else {
            Vec::new()
        }
    }

    fn close_bucket(&mut self) -> Vec<Sealed> {
        let Some(bucket) = self.bucket.take().map(|b| b.close()) else {
            return Vec::new();
        };
        let span = self
            .tier
            .tier()
            .span_of(bucket.index * self.tier.bucket_secs());
        self.open.push_in(span, bucket)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode_raw, decode_rollup};

    const T0: u64 = 1_800_057_600; // 2027-01-16T00:00:00Z, on a day boundary
    const _: () = assert!(T0.is_multiple_of(86_400));

    fn v(kind: ValueKind, stored: i64) -> Scaled {
        kind.check_scaled(stored).expect("in domain")
    }

    fn feed(state: &mut SeriesState, points: &[(u64, i64)]) -> Vec<Sealed> {
        let kind = state.kind();
        points
            .iter()
            .flat_map(|&(ts, value)| state.append(ts, v(kind, value)).expect("after the last"))
            .collect()
    }

    fn of_tier(sealed: &[Sealed], tier: Tier) -> Vec<&Sealed> {
        sealed.iter().filter(|s| s.tier == tier).collect()
    }

    #[test]
    fn a_first_point_opens_the_series_and_seals_nothing() {
        let mut state = SeriesState::new(ValueKind::Percent);
        assert_eq!(
            state.append(T0 + 5, v(ValueKind::Percent, 1_234)),
            Ok(vec![])
        );
        assert_eq!(state.last_ts(), Some(T0 + 5));
        assert_eq!(
            state.unsealed_points(),
            vec![RawPoint {
                ts: T0 + 5,
                value: 1_234
            }]
        );
    }

    #[test]
    fn a_point_not_after_the_last_is_refused_and_changes_nothing() {
        let mut state = SeriesState::new(ValueKind::Percent);
        feed(&mut state, &[(T0, 1), (T0 + 2, 2)]);
        let before = state.clone();
        for ts in [T0 + 2, T0 + 1, T0] {
            assert_eq!(
                state.append(ts, v(ValueKind::Percent, 9)),
                Err(NotAfterLast),
                "{ts}"
            );
        }
        assert_eq!(state, before);
    }

    #[test]
    fn a_full_raw_chunk_is_sealed_and_its_successor_takes_the_next_seq() {
        let mut state = SeriesState::new(ValueKind::Count);
        let points: Vec<(u64, i64)> = (0..481).map(|i| (T0 + i, 7)).collect();
        let sealed = feed(&mut state, &points);
        let raw = of_tier(&sealed, Tier::Raw);
        assert_eq!(raw.len(), 2);
        let span = Tier::Raw.span_of(T0);
        assert_eq!(
            (raw[0].span, raw[0].seq, raw[1].span, raw[1].seq),
            (span, 0, span, 1)
        );
        let first = decode_raw(&raw[0].bytes, ValueKind::Count.encoding()).expect("decodes");
        assert_eq!(first.len(), 240);
        assert_eq!(first[0], RawPoint { ts: T0, value: 7 });
        assert_eq!(
            state.unsealed_points(),
            vec![RawPoint {
                ts: T0 + 480,
                value: 7
            }]
        );
    }

    #[test]
    fn a_point_in_a_new_raw_span_seals_the_open_chunk_and_restarts_the_seq() {
        let mut state = SeriesState::new(ValueKind::Percent);
        feed(
            &mut state,
            &(0..241).map(|i| (T0 + i, 5)).collect::<Vec<_>>(),
        );
        let sealed = feed(&mut state, &[(T0 + 3_599, 6), (T0 + 3_600, 7)]);
        let raw = of_tier(&sealed, Tier::Raw);
        assert_eq!(raw.len(), 1);
        assert_eq!((raw[0].span, raw[0].seq), (Tier::Raw.span_of(T0), 1));
        let points = decode_raw(&raw[0].bytes, ValueKind::Percent.encoding()).expect("decodes");
        assert_eq!(
            points.last(),
            Some(&RawPoint {
                ts: T0 + 3_599,
                value: 6
            })
        );
        let sealed = feed(
            &mut state,
            &(1..=240).map(|i| (T0 + 3_600 + i, 8)).collect::<Vec<_>>(),
        );
        let raw = of_tier(&sealed, Tier::Raw);
        assert_eq!(
            (raw[0].span, raw[0].seq),
            (Tier::Raw.span_of(T0 + 3_600), 0),
            "the new span starts at seq 0"
        );
    }

    #[test]
    fn a_bucket_is_closed_by_the_first_point_of_a_later_bucket() {
        let mut state = SeriesState::new(ValueKind::Percent);
        feed(&mut state, &[(T0, 10), (T0 + 30, 20), (T0 + 59, 60)]);
        assert_eq!(
            state.unsealed_buckets(RollupTier::Minute),
            vec![],
            "still open"
        );
        feed(&mut state, &[(T0 + 60, 1)]);
        let index = T0 / 60;
        assert_eq!(
            state.unsealed_buckets(RollupTier::Minute),
            vec![Bucket {
                index,
                avg: 30,
                min: 10,
                max: 60,
                count: 3
            }]
        );
        assert_eq!(state.unsealed_buckets(RollupTier::Hour), vec![]);
        feed(&mut state, &[(T0 + 3_600, 3)]);
        assert_eq!(
            state.unsealed_buckets(RollupTier::Hour),
            vec![Bucket {
                index: T0 / 3_600,
                avg: 23,
                min: 1,
                max: 60,
                count: 4
            }]
        );
    }

    #[test]
    fn the_sweep_closes_a_bucket_only_once_more_than_one_bucket_length_past_its_end() {
        // A minute bucket [T0, T0 + 60) and an hour bucket [T0, T0 + 3600).
        let cases = [
            ("one second before", RollupTier::Minute, T0 + 119, false),
            (
                "exactly one bucket past its end",
                RollupTier::Minute,
                T0 + 120,
                false,
            ),
            ("one second more", RollupTier::Minute, T0 + 121, true),
            ("hour, exactly", RollupTier::Hour, T0 + 7_200, false),
            ("hour, one second more", RollupTier::Hour, T0 + 7_201, true),
        ];
        for (name, tier, now, closed) in cases {
            let mut state = SeriesState::new(ValueKind::Percent);
            feed(&mut state, &[(T0, 10), (T0 + 1, 20)]);
            state.sweep(now);
            assert_eq!(!state.unsealed_buckets(tier).is_empty(), closed, "{name}");
        }
    }

    #[test]
    fn a_swept_bucket_is_not_reopened_and_the_next_point_opens_a_new_one() {
        let mut state = SeriesState::new(ValueKind::Percent);
        feed(&mut state, &[(T0, 10)]);
        state.sweep(T0 + 121);
        feed(&mut state, &[(T0 + 200, 30), (T0 + 260, 50)]);
        let buckets = state.unsealed_buckets(RollupTier::Minute);
        let indexes: Vec<u64> = buckets.iter().map(|b| b.index).collect();
        assert_eq!(indexes, vec![T0 / 60, T0 / 60 + 3]);
    }

    #[test]
    fn the_first_bucket_of_a_new_span_seals_the_previous_spans_rollup_chunk() {
        let mut state = SeriesState::new(ValueKind::Load);
        let points: Vec<(u64, i64)> = (0..=60).map(|m| (T0 + 60 * m, 100 + m as i64)).collect();
        let mut sealed = feed(&mut state, &points);
        sealed.extend(feed(&mut state, &[(T0 + 3_660, 1)]));
        let minute = of_tier(&sealed, Tier::Minute);
        assert_eq!(minute.len(), 1);
        assert_eq!(
            (minute[0].span, minute[0].seq),
            (Tier::Minute.span_of(T0), 0)
        );
        let buckets = decode_rollup(&minute[0].bytes, RollupTier::Minute).expect("decodes");
        assert_eq!(buckets.len(), 60);
        assert_eq!(
            buckets[59],
            Bucket {
                index: T0 / 60 + 59,
                avg: 159,
                min: 159,
                max: 159,
                count: 1
            }
        );
        assert_eq!(
            state.unsealed_buckets(RollupTier::Minute).len(),
            1,
            "the new span's first bucket"
        );
    }

    #[test]
    fn a_closing_span_seals_the_open_chunk_of_that_span_only() {
        let mut state = SeriesState::new(ValueKind::Percent);
        feed(&mut state, &[(T0 + 10, 1), (T0 + 20, 2)]);
        assert_eq!(
            state.seal_span(Tier::Raw, Tier::Raw.span_of(T0 + 3_600)),
            None,
            "another span"
        );
        let sealed = state
            .seal_span(Tier::Raw, Tier::Raw.span_of(T0))
            .expect("sealed");
        assert_eq!((sealed.tier, sealed.seq), (Tier::Raw, 0));
        assert_eq!(
            decode_raw(&sealed.bytes, ValueKind::Percent.encoding()).map(|p| p.len()),
            Ok(2)
        );
        assert_eq!(state.unsealed_points(), vec![]);
        assert_eq!(
            state.seal_span(Tier::Raw, Tier::Raw.span_of(T0)),
            None,
            "nothing left to seal"
        );
        let sealed = feed(&mut state, &[(T0 + 30, 3)]);
        assert!(of_tier(&sealed, Tier::Raw).is_empty());
        let sealed = state
            .seal_span(Tier::Raw, Tier::Raw.span_of(T0))
            .expect("sealed");
        assert_eq!(
            sealed.seq, 1,
            "a later seal in the same span takes the next seq"
        );
    }

    #[test]
    fn a_closing_span_seals_a_rollup_chunk_after_the_sweep() {
        let mut state = SeriesState::new(ValueKind::Percent);
        feed(&mut state, &[(T0 + 10, 4), (T0 + 70, 6)]);
        state.sweep(T0 + 3_600 + 121);
        let sealed = state
            .seal_span(Tier::Minute, Tier::Minute.span_of(T0))
            .expect("sealed");
        let buckets = decode_rollup(&sealed.bytes, RollupTier::Minute).expect("decodes");
        assert_eq!(
            buckets.iter().map(|b| b.avg).collect::<Vec<_>>(),
            vec![4, 6]
        );
    }

    /// Every point, rolled up directly.
    fn expected_buckets(points: &[(u64, i64)], tier: RollupTier) -> Vec<Bucket> {
        let mut out: Vec<Bucket> = Vec::new();
        let mut acc: Option<Accumulator> = None;
        for &(ts, value) in points {
            match acc.as_mut() {
                Some(a) if a.holds(tier, ts) => a.add(value),
                _ => {
                    out.extend(acc.map(|a| a.close()));
                    acc = Some(Accumulator::open(tier, ts, value));
                }
            }
        }
        out.extend(acc.map(|a| a.close()));
        out
    }

    #[test]
    fn sealed_and_unsealed_data_together_equal_a_recomputation_from_the_raw_points() {
        let kind = ValueKind::Bytes;
        let mut ts = T0 + 17;
        let mut points = Vec::new();
        for i in 0..12_000u64 {
            ts += 1 + (i * 7919) % 5;
            points.push((ts, ((i * 104_729) % 1_000_003) as i64));
        }
        let mut state = SeriesState::new(kind);
        let mut sealed = feed(&mut state, &points);
        sealed.extend(state.sweep(ts + 2 * 3_600 + 1));
        let mut raw: Vec<RawPoint> = Vec::new();
        let mut minute: Vec<Bucket> = Vec::new();
        let mut hour: Vec<Bucket> = Vec::new();
        for s in &sealed {
            match s.tier {
                Tier::Raw => raw.extend(decode_raw(&s.bytes, kind.encoding()).expect("raw")),
                Tier::Minute => {
                    minute.extend(decode_rollup(&s.bytes, RollupTier::Minute).expect("minute"))
                }
                Tier::Hour => hour.extend(decode_rollup(&s.bytes, RollupTier::Hour).expect("hour")),
            }
        }
        raw.extend(state.unsealed_points());
        minute.extend(state.unsealed_buckets(RollupTier::Minute));
        hour.extend(state.unsealed_buckets(RollupTier::Hour));
        let expected: Vec<RawPoint> = points
            .iter()
            .map(|&(ts, value)| RawPoint { ts, value })
            .collect();
        assert_eq!(raw, expected, "every point exactly once, in order");
        assert_eq!(minute, expected_buckets(&points, RollupTier::Minute));
        assert_eq!(hour, expected_buckets(&points, RollupTier::Hour));
    }
}
