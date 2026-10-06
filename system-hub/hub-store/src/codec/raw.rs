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

//! The raw chunk: one series' points, at most 240 or 1 KiB encoded.
//!
//! ```text
//! format : 0x01
//! header : point count (varint) · first ts (varint) · first value (zigzag varint)
//! points : per later point, the timestamp's delta of deltas, then the value's step
//! ```

use super::bits::{
    BitReader, BitWriter, dod_bits, read_dod, read_value, read_varint, unzigzag, value_bits,
    varint_len, write_dod, write_value, write_varint, zigzag,
};
use super::{DecodeError, PushRefused};
use crate::value::Encoding;

pub const RAW_FORMAT_V1: u8 = 0x01;
pub const MAX_RAW_POINTS: usize = 240;
pub const MAX_RAW_CHUNK_BYTES: usize = 1024;

/// One point: hub time in seconds and the value in its kind's stored unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawPoint {
    pub ts: u64,
    pub value: i64,
}

/// An open raw chunk: points are pushed in time order, and the encoded bytes are available at
/// any moment (a tail is persisted as its open chunk's bytes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawChunk {
    encoding: Encoding,
    /// The first and last points, once there is one.
    ends: Option<(RawPoint, RawPoint)>,
    /// The last timestamp step and value step, which the next deltas of deltas start from.
    ts_step: i64,
    value_step: i64,
    count: usize,
    bits: BitWriter,
}

impl RawChunk {
    pub fn new(encoding: Encoding) -> RawChunk {
        RawChunk {
            encoding,
            ends: None,
            ts_step: 0,
            value_step: 0,
            count: 0,
            bits: BitWriter::default(),
        }
    }

    /// Appends a point, or refuses it and leaves the chunk as it was.
    pub fn push(&mut self, point: RawPoint) -> Result<(), PushRefused> {
        let Some((first, last)) = self.ends else {
            self.ends = Some((point, point));
            self.count = 1;
            return Ok(());
        };
        if point.ts <= last.ts {
            return Err(PushRefused::NotAfterLast);
        }
        if self.count >= MAX_RAW_POINTS {
            return Err(PushRefused::Full);
        }
        let ts_step = i64::try_from(point.ts - last.ts).map_err(|_| PushRefused::Full)?;
        let dod = ts_step - self.ts_step;
        let ts_bits = dod_bits(dod).ok_or(PushRefused::Full)?;
        let value_step = point.value.wrapping_sub(last.value);
        let code = match self.encoding {
            Encoding::Delta => zigzag(value_step),
            Encoding::DeltaOfDelta => zigzag(value_step.wrapping_sub(self.value_step)),
        };
        let bits = self.bits.bit_len() + (ts_bits + value_bits(code)) as usize;
        if header_len(self.count + 1, first) + bits.div_ceil(8) > MAX_RAW_CHUNK_BYTES {
            return Err(PushRefused::Full);
        }
        write_dod(&mut self.bits, dod);
        write_value(&mut self.bits, code);
        self.ends = Some((first, point));
        (self.ts_step, self.value_step, self.count) = (ts_step, value_step, self.count + 1);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The last point pushed.
    pub fn last(&self) -> Option<RawPoint> {
        self.ends.map(|(_, last)| last)
    }

    /// The length of [`RawChunk::encode`]'s bytes.
    pub fn encoded_len(&self) -> usize {
        match self.ends {
            None => 2,
            Some((first, _)) => header_len(self.count, first) + self.bits.as_bytes().len(),
        }
    }

    /// The chunk's bytes, format byte first. An empty chunk encodes as a count of zero, which
    /// no decoder accepts: nothing stores one.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        out.push(RAW_FORMAT_V1);
        write_varint(&mut out, self.count as u64);
        if let Some((first, _)) = self.ends {
            write_varint(&mut out, first.ts);
            write_varint(&mut out, zigzag(first.value));
            out.extend_from_slice(self.bits.as_bytes());
        }
        out
    }

    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// The chunk's points, in time order.
    pub fn points(&self) -> Vec<RawPoint> {
        // What a chunk encodes always decodes; an empty chunk decodes to no point.
        decode_raw(&self.encode(), self.encoding).unwrap_or_default()
    }

    /// Reopens an encoded chunk to push more points into it.
    pub fn reopen(bytes: &[u8], encoding: Encoding) -> Result<RawChunk, DecodeError> {
        let mut chunk = RawChunk::new(encoding);
        for point in decode_raw(bytes, encoding)? {
            chunk.push(point).map_err(|_| DecodeError::Malformed)?;
        }
        Ok(chunk)
    }
}

fn header_len(count: usize, first: RawPoint) -> usize {
    1 + varint_len(count as u64) + varint_len(first.ts) + varint_len(zigzag(first.value))
}

/// The points of an encoded raw chunk, in time order.
pub fn decode_raw(bytes: &[u8], encoding: Encoding) -> Result<Vec<RawPoint>, DecodeError> {
    let (count, first, mut pos) = decode_header(bytes)?;
    let mut reader = BitReader::new(&bytes[pos..]);
    let mut points = Vec::with_capacity(count);
    points.push(first);
    let (mut ts_step, mut value_step) = (0i64, 0i64);
    for _ in 1..count {
        let last = points[points.len() - 1];
        ts_step = ts_step
            .checked_add(read_dod(&mut reader)?)
            .ok_or(DecodeError::Malformed)?;
        let step = u64::try_from(ts_step)
            .ok()
            .filter(|s| *s > 0)
            .ok_or(DecodeError::Malformed)?;
        let ts = last.ts.checked_add(step).ok_or(DecodeError::Malformed)?;
        let code = unzigzag(read_value(&mut reader)?);
        value_step = match encoding {
            Encoding::Delta => code,
            Encoding::DeltaOfDelta => value_step.wrapping_add(code),
        };
        points.push(RawPoint {
            ts,
            value: last.value.wrapping_add(value_step),
        });
    }
    pos += reader.bytes_read();
    if pos != bytes.len() {
        return Err(DecodeError::Malformed);
    }
    Ok(points)
}

/// The format byte, the point count and the first point; and where the bit stream starts.
fn decode_header(bytes: &[u8]) -> Result<(usize, RawPoint, usize), DecodeError> {
    let format = *bytes.first().ok_or(DecodeError::Truncated)?;
    if format != RAW_FORMAT_V1 {
        return Err(DecodeError::UnknownFormat(format));
    }
    let mut pos = 1;
    let count = match read_varint(bytes, &mut pos)? {
        0 => return Err(DecodeError::Malformed),
        n if n > MAX_RAW_POINTS as u64 => return Err(DecodeError::TooLong),
        n => n as usize,
    };
    let ts = read_varint(bytes, &mut pos)?;
    let value = unzigzag(read_varint(bytes, &mut pos)?);
    Ok((count, RawPoint { ts, value }, pos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const ENCODINGS: [Encoding; 2] = [Encoding::Delta, Encoding::DeltaOfDelta];

    fn chunk_of(points: &[RawPoint], encoding: Encoding) -> RawChunk {
        let mut chunk = RawChunk::new(encoding);
        for p in points {
            chunk.push(*p).expect("fits");
        }
        chunk
    }

    fn steady(n: usize) -> Vec<RawPoint> {
        (0..n as u64)
            .map(|i| RawPoint {
                ts: 1_700_000_000 + 2 * i,
                value: (i as i64 * 37) % 1_000,
            })
            .collect()
    }

    #[test]
    fn one_point_and_a_full_chunk_round_trip_in_both_encodings() {
        for encoding in ENCODINGS {
            for n in [1, MAX_RAW_POINTS] {
                let points = steady(n);
                let chunk = chunk_of(&points, encoding);
                assert_eq!(chunk.len(), n);
                assert_eq!(chunk.last(), points.last().copied());
                let bytes = chunk.encode();
                assert_eq!(bytes.len(), chunk.encoded_len(), "{encoding:?} {n}");
                assert_eq!(bytes[0], RAW_FORMAT_V1);
                assert_eq!(decode_raw(&bytes, encoding), Ok(points), "{encoding:?} {n}");
            }
        }
    }

    #[test]
    fn the_241st_point_is_refused_as_full_and_leaves_the_chunk_unchanged() {
        let points = steady(MAX_RAW_POINTS + 1);
        let mut chunk = chunk_of(&points[..MAX_RAW_POINTS], Encoding::Delta);
        let before = chunk.encode();
        assert_eq!(chunk.push(points[MAX_RAW_POINTS]), Err(PushRefused::Full));
        assert_eq!(chunk.encode(), before);
        assert_eq!(chunk.len(), MAX_RAW_POINTS);
    }

    #[test]
    fn a_point_that_would_pass_one_kib_is_refused_as_full() {
        // Every value step takes the 64-bit escape: 69 bits plus a 1-bit timestamp.
        let mut chunk = RawChunk::new(Encoding::Delta);
        let mut i = 0u64;
        let refused = loop {
            let value = if i.is_multiple_of(2) { 0 } else { i64::MAX };
            match chunk.push(RawPoint { ts: 10 + i, value }) {
                Ok(()) => i += 1,
                Err(refusal) => break refusal,
            }
            assert!(
                chunk.encoded_len() <= MAX_RAW_CHUNK_BYTES,
                "never past 1 KiB"
            );
            assert!(i <= MAX_RAW_POINTS as u64, "refused before the point limit");
        };
        assert_eq!(refused, PushRefused::Full);
        assert!(chunk.len() < MAX_RAW_POINTS);
        assert!(
            chunk.encoded_len() > MAX_RAW_CHUNK_BYTES - 9,
            "full: no room for one more escape"
        );
        assert_eq!(chunk.encode().len(), chunk.encoded_len());
    }

    #[test]
    fn a_point_not_after_the_last_is_refused() {
        let mut chunk = chunk_of(&[RawPoint { ts: 100, value: 1 }], Encoding::Delta);
        assert_eq!(
            chunk.push(RawPoint { ts: 100, value: 2 }),
            Err(PushRefused::NotAfterLast)
        );
        assert_eq!(
            chunk.push(RawPoint { ts: 99, value: 2 }),
            Err(PushRefused::NotAfterLast)
        );
        assert_eq!(chunk.len(), 1);
    }

    #[test]
    fn a_timestamp_step_the_code_cant_hold_is_refused_as_full() {
        let mut chunk = chunk_of(&[RawPoint { ts: 100, value: 1 }], Encoding::Delta);
        let far = RawPoint {
            ts: 100 + (1u64 << 31) + 1,
            value: 1,
        };
        assert_eq!(chunk.push(far), Err(PushRefused::Full));
        let near = RawPoint {
            ts: 100 + (1u64 << 31) - 1,
            value: 1,
        };
        assert_eq!(chunk.push(near), Ok(()));
    }

    #[test]
    fn the_extreme_values_round_trip() {
        for encoding in ENCODINGS {
            let values = [i64::MIN, i64::MAX, 0, i64::MIN, -1, i64::MAX, i64::MAX];
            let points: Vec<_> = values
                .iter()
                .enumerate()
                .map(|(i, &value)| RawPoint {
                    ts: i as u64 * 7,
                    value,
                })
                .collect();
            let chunk = chunk_of(&points, encoding);
            assert_eq!(
                decode_raw(&chunk.encode(), encoding),
                Ok(points),
                "{encoding:?}"
            );
        }
    }

    #[test]
    fn a_steady_series_costs_two_bits_a_point() {
        let points: Vec<_> = (0..200u64)
            .map(|i| RawPoint {
                ts: 1_000 + 2 * i,
                value: 500,
            })
            .collect();
        let chunk = chunk_of(&points, Encoding::Delta);
        // The first step's delta of deltas is the interval itself, then each step is `0` + `0`.
        let header = 1 + 2 + 2 + 2;
        assert!(
            chunk.encoded_len() <= header + 2 + (199usize * 2).div_ceil(8),
            "{}",
            chunk.encoded_len()
        );
    }

    #[test]
    fn a_monotonic_counter_costs_a_bit_per_value_in_delta_of_delta() {
        let points: Vec<_> = (0..200u64)
            .map(|i| RawPoint {
                ts: 1_000 + 15 * i,
                value: 15 * i as i64,
            })
            .collect();
        let dod = chunk_of(&points, Encoding::DeltaOfDelta).encoded_len();
        let delta = chunk_of(&points, Encoding::Delta).encoded_len();
        assert!(dod < delta, "delta of deltas {dod} < delta {delta}");
        assert!(
            dod <= 1 + 2 + 2 + 1 + 4 + (199usize * 2).div_ceil(8),
            "{dod}"
        );
    }

    #[test]
    fn a_reopened_chunk_takes_more_points_and_encodes_as_if_never_closed() {
        for encoding in ENCODINGS {
            let points = steady(120);
            let whole = chunk_of(&points, encoding);
            let mut reopened =
                RawChunk::reopen(&chunk_of(&points[..70], encoding).encode(), encoding)
                    .expect("reopens");
            for p in &points[70..] {
                reopened.push(*p).expect("fits");
            }
            assert_eq!(reopened.encode(), whole.encode(), "{encoding:?}");
            assert_eq!(reopened, whole, "{encoding:?}");
        }
    }

    #[test]
    fn an_unknown_format_byte_is_refused() {
        let mut bytes = chunk_of(&steady(3), Encoding::Delta).encode();
        for format in [0x00, 0x02, 0xff] {
            bytes[0] = format;
            assert_eq!(
                decode_raw(&bytes, Encoding::Delta),
                Err(DecodeError::UnknownFormat(format))
            );
        }
        assert_eq!(
            decode_raw(&[], Encoding::Delta),
            Err(DecodeError::Truncated)
        );
    }

    #[test]
    fn a_count_past_240_is_refused_before_anything_is_read() {
        let mut bytes = vec![RAW_FORMAT_V1];
        super::super::bits::write_varint(&mut bytes, 241);
        assert_eq!(
            decode_raw(&bytes, Encoding::Delta),
            Err(DecodeError::TooLong)
        );
        let mut bytes = vec![RAW_FORMAT_V1];
        super::super::bits::write_varint(&mut bytes, u64::MAX);
        assert_eq!(
            decode_raw(&bytes, Encoding::Delta),
            Err(DecodeError::TooLong)
        );
    }

    #[test]
    fn a_count_of_zero_is_malformed() {
        assert_eq!(
            decode_raw(&[RAW_FORMAT_V1, 0, 0, 0], Encoding::Delta),
            Err(DecodeError::Malformed)
        );
    }

    #[test]
    fn time_running_backwards_in_the_bytes_is_malformed() {
        // Two points from ts 100: the step `10` + `111` is a delta of -1, then a `0` value step.
        let bytes = [RAW_FORMAT_V1, 2, 100, 0, 0b1011_1000];
        assert_eq!(
            decode_raw(&bytes, Encoding::Delta),
            Err(DecodeError::Malformed)
        );
    }

    #[test]
    fn a_chunk_cut_short_is_truncated() {
        let bytes = chunk_of(&steady(50), Encoding::Delta).encode();
        for cut in [1, 2, 4, bytes.len() / 2, bytes.len() - 1] {
            assert!(
                decode_raw(&bytes[..cut], Encoding::Delta).is_err(),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn whole_bytes_after_the_last_point_are_malformed() {
        let mut bytes = chunk_of(&steady(5), Encoding::Delta).encode();
        bytes.push(0);
        assert_eq!(
            decode_raw(&bytes, Encoding::Delta),
            Err(DecodeError::Malformed)
        );
    }

    fn increasing_points() -> impl Strategy<Value = Vec<RawPoint>> {
        (
            0u64..u64::MAX / 2,
            prop::collection::vec((1u64..100_000, any::<i64>()), 1..MAX_RAW_POINTS),
        )
            .prop_map(|(start, steps)| {
                let mut ts = start;
                steps
                    .into_iter()
                    .map(|(gap, value)| {
                        ts += gap;
                        RawPoint { ts, value }
                    })
                    .collect()
            })
    }

    proptest! {
        #[test]
        fn what_a_chunk_accepts_decodes_to_the_same_points(points in increasing_points(), monotonic in any::<bool>()) {
            let encoding = if monotonic { Encoding::DeltaOfDelta } else { Encoding::Delta };
            let mut chunk = RawChunk::new(encoding);
            let mut accepted = Vec::new();
            for p in points {
                if chunk.push(p).is_err() {
                    break;
                }
                accepted.push(p);
            }
            prop_assert!(chunk.encoded_len() <= MAX_RAW_CHUNK_BYTES);
            prop_assert_eq!(decode_raw(&chunk.encode(), encoding), Ok(accepted));
        }

        #[test]
        fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..2_048), monotonic in any::<bool>()) {
            let encoding = if monotonic { Encoding::DeltaOfDelta } else { Encoding::Delta };
            if let Ok(points) = decode_raw(&bytes, encoding) {
                prop_assert!(!points.is_empty() && points.len() <= MAX_RAW_POINTS);
                prop_assert!(points.windows(2).all(|w| w[0].ts < w[1].ts));
            }
            let _ = RawChunk::reopen(&bytes, encoding);
        }

        #[test]
        fn arbitrary_bytes_after_a_format_byte_never_panic(tail in prop::collection::vec(any::<u8>(), 0..1_100)) {
            let mut bytes = vec![RAW_FORMAT_V1];
            bytes.extend(tail);
            let _ = decode_raw(&bytes, Encoding::Delta);
            let _ = decode_raw(&bytes, Encoding::DeltaOfDelta);
        }
    }
}
