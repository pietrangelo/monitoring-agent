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

//! A series' tail (RFC 0010 §5 `tails`, §6): its state in the head as persisted, so a restart
//! reopens its open chunks and buckets where they were.
//!
//! ```text
//! version 0x01 · kind code · last ts (presence, u64)
//! per tier (raw, minute, hour): span (presence, u64) · next seq u16 · chunk (u16 length, bytes)
//! per rollup tier (minute, hour): open bucket (presence, index u64, sum i128, count u32,
//!                                 min i64, max i64)
//! ```
//!
//! A tail read back is checked whole: its kind, its spans on their grid, its chunks decodable
//! under its kind's encoding, every value inside the kind's domain, its buckets consistent.

use super::{Open, RollupState, SeriesState};
use crate::bytes::{Reader, Short, put_u16_prefixed};
use crate::codec::{RawChunk, RollupChunk};
use crate::rollup::{Accumulator, AccumulatorRow};
use crate::tier::{RollupTier, SpanStart, Tier};
use crate::value::ValueKind;

const TAIL_V1: u8 = 1;

impl From<Short> for TailError {
    fn from(_: Short) -> TailError {
        TailError::Malformed
    }
}

/// Bytes that are no tail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TailError {
    UnknownVersion(u8),
    Malformed,
    /// A stored value outside the series' kind's domain.
    OutOfDomain,
}

impl std::fmt::Display for TailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TailError::UnknownVersion(v) => write!(f, "unknown tail version {v}"),
            TailError::Malformed => f.write_str("malformed tail"),
            TailError::OutOfDomain => f.write_str("tail value outside its kind's domain"),
        }
    }
}

impl std::error::Error for TailError {}

impl SeriesState {
    /// The series' tail.
    pub fn to_tail(&self) -> Vec<u8> {
        let mut out = vec![TAIL_V1, self.kind.code()];
        put_optional_u64(&mut out, self.last_ts);
        put_open(&mut out, &self.raw);
        put_open(&mut out, &self.minute.open);
        put_open(&mut out, &self.hour.open);
        for rollup in [&self.minute, &self.hour] {
            put_bucket(&mut out, rollup.bucket);
        }
        out
    }

    /// A series reopened from its tail.
    pub fn from_tail(bytes: &[u8]) -> Result<SeriesState, TailError> {
        let mut r = Reader::new(bytes);
        let version = r.u8()?;
        if version != TAIL_V1 {
            return Err(TailError::UnknownVersion(version));
        }
        let kind = ValueKind::from_code(r.u8()?).ok_or(TailError::Malformed)?;
        let last_ts = read_optional_u64(&mut r)?;
        let raw = read_open(&mut r, Tier::Raw, RawChunk::new(kind.encoding()), |b| {
            RawChunk::reopen(b, kind.encoding()).ok()
        })?;
        let minute = read_open(
            &mut r,
            Tier::Minute,
            RollupChunk::new(RollupTier::Minute),
            |b| RollupChunk::reopen(b, RollupTier::Minute).ok(),
        )?;
        let hour = read_open(
            &mut r,
            Tier::Hour,
            RollupChunk::new(RollupTier::Hour),
            |b| RollupChunk::reopen(b, RollupTier::Hour).ok(),
        )?;
        let minute = RollupState {
            tier: RollupTier::Minute,
            bucket: read_bucket(&mut r)?,
            open: minute,
        };
        let hour = RollupState {
            tier: RollupTier::Hour,
            bucket: read_bucket(&mut r)?,
            open: hour,
        };
        r.finish()?;
        let state = SeriesState {
            kind,
            last_ts,
            raw,
            minute,
            hour,
        };
        state.check_domain()?;
        state.check_coherence()?;
        Ok(state)
    }

    /// The fields against each other: every point and bucket in its chunk's span, none after
    /// the series' last time, each open bucket after its tier's closed ones.
    fn check_coherence(&self) -> Result<(), TailError> {
        let raw: Vec<u64> = self.raw.chunk.points().iter().map(|p| p.ts).collect();
        let mut times = raw.clone();
        let mut coherent = in_span(Tier::Raw, self.raw.span, &raw);
        for rollup in [&self.minute, &self.hour] {
            let len = rollup.tier.bucket_secs();
            let closed: Vec<u64> = rollup
                .open
                .chunk
                .buckets()
                .iter()
                .map(|b| b.index * len)
                .collect();
            coherent &= in_span(rollup.tier.tier(), rollup.open.span, &closed);
            if let Some(open) = rollup.bucket {
                let start = open.index() * len;
                coherent &= closed.last().is_none_or(|&c| c < start);
                times.push(start);
            }
            times.extend(closed);
        }
        coherent &= match self.last_ts {
            None => times.is_empty(),
            Some(last) => times.iter().all(|&t| t <= last),
        };
        if coherent {
            Ok(())
        } else {
            Err(TailError::Malformed)
        }
    }

    /// Every value the state holds, against its kind's domain.
    fn check_domain(&self) -> Result<(), TailError> {
        let points = self.raw.chunk.points().into_iter().map(|p| p.value);
        let buckets = [RollupTier::Minute, RollupTier::Hour]
            .into_iter()
            .flat_map(|tier| self.unsealed_buckets(tier))
            .flat_map(|b| [b.min, b.avg, b.max]);
        let open = [self.minute.bucket, self.hour.bucket]
            .into_iter()
            .flatten()
            .map(AccumulatorRow::from)
            .flat_map(|row| [row.min, row.max]);
        match points
            .chain(buckets)
            .chain(open)
            .all(|v| self.kind.check_scaled(v).is_ok())
        {
            true => Ok(()),
            false => Err(TailError::OutOfDomain),
        }
    }
}

/// Whether every time lies in the open chunk's span; an empty chunk needs none.
fn in_span(tier: Tier, span: Option<SpanStart>, times: &[u64]) -> bool {
    times.is_empty() || span.is_some_and(|s| times.iter().all(|&t| tier.span_of(t) == s))
}

fn put_optional_u64(out: &mut Vec<u8>, value: Option<u64>) {
    match value {
        None => out.push(0),
        Some(v) => {
            out.push(1);
            out.extend_from_slice(&v.to_be_bytes());
        }
    }
}

fn read_optional_u64(r: &mut Reader<'_>) -> Result<Option<u64>, TailError> {
    match r.u8()? {
        0 => Ok(None),
        1 => Ok(Some(r.u64()?)),
        _ => Err(TailError::Malformed),
    }
}

fn put_open<C: super::OpenChunk>(out: &mut Vec<u8>, open: &Open<C>) {
    put_optional_u64(out, open.span.map(SpanStart::get));
    out.extend_from_slice(&open.next_seq.to_be_bytes());
    // An open chunk is at most 1 KiB (raw) or one span of buckets, far under 64 KiB; an empty
    // one is stored as no bytes.
    let bytes = if open.chunk.is_empty() {
        Vec::new()
    } else {
        open.chunk.encode()
    };
    put_u16_prefixed(out, &bytes);
}

/// An open chunk: `empty` when the tail stores no bytes, else `reopen` of its bytes.
fn read_open<C>(
    r: &mut Reader<'_>,
    tier: Tier,
    empty: C,
    reopen: impl Fn(&[u8]) -> Option<C>,
) -> Result<Open<C>, TailError> {
    let span = match read_optional_u64(r)? {
        None => None,
        Some(start) => Some(SpanStart::new(tier, start).ok_or(TailError::Malformed)?),
    };
    let next_seq = r.u16()?;
    let bytes = r.u16_prefixed()?;
    let chunk = if bytes.is_empty() {
        empty
    } else {
        reopen(bytes).ok_or(TailError::Malformed)?
    };
    Ok(Open {
        chunk,
        span,
        next_seq,
    })
}

fn put_bucket(out: &mut Vec<u8>, bucket: Option<Accumulator>) {
    let Some(row) = bucket.map(AccumulatorRow::from) else {
        out.push(0);
        return;
    };
    out.push(1);
    out.extend_from_slice(&row.index.to_be_bytes());
    out.extend_from_slice(&row.sum.to_be_bytes());
    out.extend_from_slice(&row.count.to_be_bytes());
    out.extend_from_slice(&row.min.to_be_bytes());
    out.extend_from_slice(&row.max.to_be_bytes());
}

fn read_bucket(r: &mut Reader<'_>) -> Result<Option<Accumulator>, TailError> {
    match r.u8()? {
        0 => Ok(None),
        1 => {
            let row = AccumulatorRow {
                index: r.u64()?,
                sum: r.i128()?,
                count: r.u32()?,
                min: r.i64()?,
                max: r.i64()?,
            };
            Accumulator::try_from(row)
                .map(Some)
                .map_err(|_| TailError::Malformed)
        }
        _ => Err(TailError::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::super::Sealed;
    use super::*;
    use crate::tier::RollupTier;
    use crate::value::ValueKind;

    const T0: u64 = 1_800_057_600;

    fn fed(kind: ValueKind, points: &[(u64, i64)]) -> SeriesState {
        let mut state = SeriesState::new(kind);
        for &(ts, value) in points {
            state
                .append(ts, kind.check_scaled(value).expect("in domain"))
                .expect("after the last");
        }
        state
    }

    fn walk(n: u64, from: u64, step: u64) -> Vec<(u64, i64)> {
        (0..n)
            .map(|i| (from + i * step, ((i * 7_919) % 9_000) as i64))
            .collect()
    }

    /// States in each shape a tail must keep: empty, one point, open chunks in every tier,
    /// chunks just sealed, a swept bucket.
    fn shapes() -> Vec<(&'static str, SeriesState)> {
        let mut swept = fed(ValueKind::Percent, &walk(100, T0, 7));
        swept.sweep(T0 + 2 * 3_600 + 700);
        vec![
            ("empty", SeriesState::new(ValueKind::Rate)),
            ("one point", fed(ValueKind::Count, &[(T0 + 3, 42)])),
            (
                "open in every tier",
                fed(ValueKind::Percent, &walk(900, T0, 13)),
            ),
            ("just sealed", fed(ValueKind::Monotonic, &walk(241, T0, 1))),
            ("across days", fed(ValueKind::Load, &walk(400, T0, 397))),
            ("swept", swept),
        ]
    }

    #[test]
    fn a_tail_reopens_the_same_state() {
        for (name, state) in shapes() {
            assert_eq!(
                SeriesState::from_tail(&state.to_tail()),
                Ok(state.clone()),
                "{name}"
            );
        }
    }

    #[test]
    fn a_reopened_series_seals_what_it_would_have_sealed() {
        for (name, state) in shapes() {
            let mut kept = state.clone();
            let mut reopened = SeriesState::from_tail(&state.to_tail()).expect("reopens");
            let from = state.last_ts().map_or(T0, |t| t + 1);
            let more = walk(2_000, from, 11);
            let run = |s: &mut SeriesState| -> Vec<Sealed> {
                let kind = s.kind();
                let mut out: Vec<Sealed> = more
                    .iter()
                    .flat_map(|&(ts, v)| {
                        s.append(ts, kind.check_scaled(v).expect("in domain"))
                            .expect("after")
                    })
                    .collect();
                out.extend(s.sweep(from + 30_000));
                out
            };
            assert_eq!(run(&mut reopened), run(&mut kept), "{name}");
            assert_eq!(reopened, kept, "{name}");
        }
    }

    #[test]
    fn a_tail_of_an_unknown_version_is_refused() {
        let mut tail = fed(ValueKind::Percent, &[(T0, 1)]).to_tail();
        tail[0] = 2;
        assert_eq!(
            SeriesState::from_tail(&tail),
            Err(TailError::UnknownVersion(2))
        );
    }

    #[test]
    fn a_tail_cut_short_or_with_bytes_after_it_is_malformed() {
        let tail = fed(ValueKind::Percent, &walk(300, T0, 9)).to_tail();
        for cut in 1..tail.len() {
            assert!(
                SeriesState::from_tail(&tail[..cut]).is_err(),
                "cut at {cut}"
            );
        }
        let mut longer = tail.clone();
        longer.push(0);
        assert_eq!(SeriesState::from_tail(&longer), Err(TailError::Malformed));
        assert_eq!(SeriesState::from_tail(&[]), Err(TailError::Malformed));
    }

    #[test]
    fn a_tail_with_an_unknown_kind_or_presence_byte_is_malformed() {
        let tail = fed(ValueKind::Percent, &[(T0, 1)]).to_tail();
        let mut kind = tail.clone();
        kind[1] = 0;
        assert_eq!(SeriesState::from_tail(&kind), Err(TailError::Malformed));
        let mut presence = tail.clone();
        presence[2] = 9;
        assert_eq!(SeriesState::from_tail(&presence), Err(TailError::Malformed));
    }

    #[test]
    fn a_value_outside_the_kinds_domain_is_refused() {
        // A count of 200,000 is a valid count and 2,000 % as a percent.
        let count = fed(ValueKind::Count, &[(T0, 200_000), (T0 + 1, 3)]);
        let mut tail = count.to_tail();
        assert_eq!(tail[1], ValueKind::Count.code());
        tail[1] = ValueKind::Percent.code();
        assert_eq!(SeriesState::from_tail(&tail), Err(TailError::OutOfDomain));
    }

    #[test]
    fn a_tails_open_buckets_reopen_only_when_consistent() {
        let state = fed(ValueKind::Percent, &[(T0, 10), (T0 + 1, 30)]);
        let tail = state.to_tail();
        // The hour bucket is the tail's last field: index, sum, count, min, max.
        let max_at = tail.len() - 8;
        let min_at = max_at - 8;
        let mut swapped = tail.clone();
        swapped[min_at..max_at].copy_from_slice(&30i64.to_be_bytes());
        swapped[max_at..].copy_from_slice(&10i64.to_be_bytes());
        assert_eq!(
            SeriesState::from_tail(&swapped),
            Err(TailError::Malformed),
            "min above max"
        );
        let reopened = SeriesState::from_tail(&tail).expect("consistent");
        assert_eq!(reopened.unsealed_buckets(RollupTier::Hour), vec![]);
    }

    #[test]
    fn a_tail_whose_fields_disagree_with_each_other_is_malformed() {
        // Layout: version, kind, last ts (presence at 2, value at 3..11), raw span (presence at
        // 11, start at 12..20), ...
        let state = fed(
            ValueKind::Percent,
            &[(T0 + 10, 1), (T0 + 70, 2), (T0 + 130, 3)],
        );
        let tail = state.to_tail();
        assert_eq!(
            u64::from_be_bytes(tail[3..11].try_into().expect("8 bytes")),
            T0 + 130
        );
        assert_eq!(
            u64::from_be_bytes(tail[12..20].try_into().expect("8 bytes")),
            T0
        );
        let patch = |at: usize, value: u64| {
            let mut bytes = tail.clone();
            bytes[at..at + 8].copy_from_slice(&value.to_be_bytes());
            bytes
        };
        let cases = [
            (
                "the raw chunk's points outside its span",
                patch(12, T0 + 3_600),
            ),
            (
                "the last time before the chunk's last point",
                patch(3, T0 + 129),
            ),
            ("the last time before an open bucket", patch(3, T0 + 69)),
        ];
        for (name, bytes) in cases {
            assert_eq!(
                SeriesState::from_tail(&bytes),
                Err(TailError::Malformed),
                "{name}"
            );
        }
        let mut no_last = tail.clone();
        no_last.splice(2..11, [0u8]);
        assert_eq!(
            SeriesState::from_tail(&no_last),
            Err(TailError::Malformed),
            "data with no last time"
        );
        assert_eq!(
            SeriesState::from_tail(&tail),
            Ok(state),
            "the untouched tail reopens"
        );
    }
}
