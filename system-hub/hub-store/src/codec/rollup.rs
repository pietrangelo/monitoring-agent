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

//! The rollup chunk: one series' buckets of one tier, at most one span's worth (60 minute
//! buckets, or 24 hour buckets).
//!
//! ```text
//! format : 0x02
//! header : bucket count (varint) · first bucket index (varint) · first avg (zigzag varint)
//! bucket : index delta of deltas (one bit when consecutive; not for the first),
//!          avg step (not for the first), avg − min, max − avg, point count (bit varint)
//! ```

use super::bits::{
    BitReader, BitWriter, read_bit_varint, read_dod, read_value, read_varint, unzigzag, varint_len,
    write_bit_varint, write_dod, write_value, write_varint, zigzag,
};
use super::{DecodeError, PushRefused};
use crate::tier::RollupTier;

pub const ROLLUP_FORMAT_V1: u8 = 0x02;

/// One closed bucket: its index (its start over the bucket length) and the average, minimum,
/// maximum and number of the points in it, in the kind's stored unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bucket {
    pub index: u64,
    pub avg: i64,
    pub min: i64,
    pub max: i64,
    pub count: u32,
}

/// An open rollup chunk, buckets pushed in index order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RollupChunk {
    tier: RollupTier,
    /// The first and last buckets, once there is one.
    ends: Option<(Bucket, Bucket)>,
    /// The last index step, which the next delta of deltas starts from.
    index_step: i64,
    count: usize,
    bits: BitWriter,
}

impl RollupChunk {
    pub fn new(tier: RollupTier) -> RollupChunk {
        RollupChunk {
            tier,
            ends: None,
            index_step: 1,
            count: 0,
            bits: BitWriter::default(),
        }
    }

    /// Appends a bucket, or refuses it and leaves the chunk as it was.
    pub fn push(&mut self, bucket: Bucket) -> Result<(), PushRefused> {
        if !(bucket.min <= bucket.avg && bucket.avg <= bucket.max && bucket.count > 0) {
            return Err(PushRefused::InvalidBucket);
        }
        let Some((first, last)) = self.ends else {
            write_spread(&mut self.bits, bucket);
            self.ends = Some((bucket, bucket));
            self.count = 1;
            return Ok(());
        };
        if bucket.index <= last.index {
            return Err(PushRefused::NotAfterLast);
        }
        if self.count >= self.tier.max_buckets() {
            return Err(PushRefused::Full);
        }
        let index_step = i64::try_from(bucket.index - last.index).map_err(|_| PushRefused::Full)?;
        if !write_dod(&mut self.bits, index_step - self.index_step) {
            return Err(PushRefused::Full);
        }
        write_value(&mut self.bits, zigzag(bucket.avg.wrapping_sub(last.avg)));
        write_spread(&mut self.bits, bucket);
        self.ends = Some((first, bucket));
        (self.index_step, self.count) = (index_step, self.count + 1);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn last(&self) -> Option<Bucket> {
        self.ends.map(|(_, last)| last)
    }

    pub fn encoded_len(&self) -> usize {
        match self.ends {
            None => 2,
            Some((first, _)) => {
                1 + varint_len(self.count as u64)
                    + varint_len(first.index)
                    + varint_len(zigzag(first.avg))
                    + self.bits.as_bytes().len()
            }
        }
    }

    /// The chunk's bytes, format byte first; an empty chunk encodes as a count of zero, which
    /// no decoder accepts.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        out.push(ROLLUP_FORMAT_V1);
        write_varint(&mut out, self.count as u64);
        if let Some((first, _)) = self.ends {
            write_varint(&mut out, first.index);
            write_varint(&mut out, zigzag(first.avg));
            out.extend_from_slice(self.bits.as_bytes());
        }
        out
    }

    pub fn rollup_tier(&self) -> RollupTier {
        self.tier
    }

    /// The chunk's buckets, in index order.
    pub fn buckets(&self) -> Vec<Bucket> {
        // What a chunk encodes always decodes; an empty chunk decodes to no bucket.
        decode_rollup(&self.encode(), self.tier).unwrap_or_default()
    }

    pub fn reopen(bytes: &[u8], tier: RollupTier) -> Result<RollupChunk, DecodeError> {
        let mut chunk = RollupChunk::new(tier);
        for bucket in decode_rollup(bytes, tier)? {
            chunk.push(bucket).map_err(|_| DecodeError::Malformed)?;
        }
        Ok(chunk)
    }
}

/// A bucket's minimum and maximum as their distances from its average, then its count.
fn write_spread(bits: &mut BitWriter, bucket: Bucket) {
    write_value(bits, bucket.avg.wrapping_sub(bucket.min) as u64);
    write_value(bits, bucket.max.wrapping_sub(bucket.avg) as u64);
    write_bit_varint(bits, u64::from(bucket.count));
}

/// The buckets of an encoded rollup chunk, in index order.
pub fn decode_rollup(bytes: &[u8], tier: RollupTier) -> Result<Vec<Bucket>, DecodeError> {
    let (count, mut index, mut avg, mut pos) = decode_header(bytes, tier)?;
    let mut reader = BitReader::new(&bytes[pos..]);
    let mut buckets = Vec::with_capacity(count);
    let mut index_step = 1i64;
    for i in 0..count {
        if i > 0 {
            index_step = index_step
                .checked_add(read_dod(&mut reader)?)
                .ok_or(DecodeError::Malformed)?;
            let step = u64::try_from(index_step)
                .ok()
                .filter(|s| *s > 0)
                .ok_or(DecodeError::Malformed)?;
            index = index.checked_add(step).ok_or(DecodeError::Malformed)?;
            avg = avg.wrapping_add(unzigzag(read_value(&mut reader)?));
        }
        buckets.push(read_spread(&mut reader, index, avg)?);
    }
    pos += reader.bytes_read();
    if pos != bytes.len() {
        return Err(DecodeError::Malformed);
    }
    Ok(buckets)
}

fn read_spread(reader: &mut BitReader<'_>, index: u64, avg: i64) -> Result<Bucket, DecodeError> {
    let below = read_value(reader)?;
    let above = read_value(reader)?;
    let count = u32::try_from(read_bit_varint(reader)?).map_err(|_| DecodeError::Malformed)?;
    let min = avg
        .checked_sub_unsigned(below)
        .ok_or(DecodeError::Malformed)?;
    let max = avg
        .checked_add_unsigned(above)
        .ok_or(DecodeError::Malformed)?;
    if count == 0 {
        return Err(DecodeError::Malformed);
    }
    Ok(Bucket {
        index,
        avg,
        min,
        max,
        count,
    })
}

/// The bucket count, the first index and average, and where the bit stream starts.
fn decode_header(bytes: &[u8], tier: RollupTier) -> Result<(usize, u64, i64, usize), DecodeError> {
    let format = *bytes.first().ok_or(DecodeError::Truncated)?;
    if format != ROLLUP_FORMAT_V1 {
        return Err(DecodeError::UnknownFormat(format));
    }
    let mut pos = 1;
    let count = match read_varint(bytes, &mut pos)? {
        0 => return Err(DecodeError::Malformed),
        n if n > tier.max_buckets() as u64 => return Err(DecodeError::TooLong),
        n => n as usize,
    };
    let index = read_varint(bytes, &mut pos)?;
    let avg = unzigzag(read_varint(bytes, &mut pos)?);
    Ok((count, index, avg, pos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn chunk_of(buckets: &[Bucket], tier: RollupTier) -> RollupChunk {
        let mut chunk = RollupChunk::new(tier);
        for b in buckets {
            chunk.push(*b).expect("fits");
        }
        chunk
    }

    fn buckets(n: usize, start: u64) -> Vec<Bucket> {
        (0..n as u64)
            .map(|i| {
                let avg = 4_000 + (i as i64 * 13) % 300;
                Bucket {
                    index: start + i,
                    avg,
                    min: avg - 50 - i as i64,
                    max: avg + 7,
                    count: 30,
                }
            })
            .collect()
    }

    #[test]
    fn one_bucket_and_a_full_chunk_round_trip_in_each_tier() {
        for tier in [RollupTier::Minute, RollupTier::Hour] {
            for n in [1, tier.max_buckets()] {
                let all = buckets(n, 28_333_320);
                let chunk = chunk_of(&all, tier);
                assert_eq!(chunk.len(), n);
                assert_eq!(chunk.last(), all.last().copied());
                let bytes = chunk.encode();
                assert_eq!(bytes[0], ROLLUP_FORMAT_V1);
                assert_eq!(bytes.len(), chunk.encoded_len());
                assert_eq!(decode_rollup(&bytes, tier), Ok(all), "{tier:?} {n}");
            }
        }
    }

    #[test]
    fn a_bucket_past_one_span_is_refused_as_full() {
        for tier in [RollupTier::Minute, RollupTier::Hour] {
            let all = buckets(tier.max_buckets() + 1, 0);
            let mut chunk = chunk_of(&all[..tier.max_buckets()], tier);
            assert_eq!(
                chunk.push(all[tier.max_buckets()]),
                Err(PushRefused::Full),
                "{tier:?}"
            );
            assert_eq!(chunk.len(), tier.max_buckets());
        }
    }

    #[test]
    fn a_bucket_not_after_the_last_is_refused() {
        let all = buckets(2, 10);
        let mut chunk = chunk_of(&all, RollupTier::Minute);
        assert_eq!(chunk.push(all[1]), Err(PushRefused::NotAfterLast));
        assert_eq!(chunk.push(all[0]), Err(PushRefused::NotAfterLast));
    }

    #[test]
    fn a_bucket_with_its_average_outside_its_range_or_no_point_is_refused() {
        let good = Bucket {
            index: 5,
            avg: 10,
            min: 8,
            max: 12,
            count: 3,
        };
        let cases = [
            ("min above avg", Bucket { min: 11, ..good }),
            ("max below avg", Bucket { max: 9, ..good }),
            ("no point", Bucket { count: 0, ..good }),
        ];
        for (name, bucket) in cases {
            let mut chunk = RollupChunk::new(RollupTier::Minute);
            assert_eq!(
                chunk.push(bucket),
                Err(PushRefused::InvalidBucket),
                "{name}"
            );
            assert!(chunk.is_empty(), "{name}");
        }
        let edge = Bucket {
            min: 10,
            max: 10,
            ..good
        };
        assert_eq!(RollupChunk::new(RollupTier::Minute).push(edge), Ok(()));
    }

    #[test]
    fn consecutive_steady_buckets_cost_twelve_bits_each() {
        let all: Vec<_> = (0..60)
            .map(|i| Bucket {
                index: 100 + i,
                avg: 500,
                min: 500,
                max: 500,
                count: 30,
            })
            .collect();
        let chunk = chunk_of(&all, RollupTier::Minute);
        // Header: format, count 60, index 100, avg 500 (zigzag 1000: two bytes).
        let header = 1 + 1 + 1 + 2;
        // The first bucket: min and max steps of one bit, a count of one group; each later one
        // adds an index bit and an avg bit.
        let bits = (1 + 1 + 8) + 59 * (1 + 1 + 1 + 1 + 8);
        assert_eq!(chunk.encoded_len(), header + (bits as usize).div_ceil(8));
    }

    #[test]
    fn the_extreme_values_round_trip() {
        let all = [
            Bucket {
                index: 0,
                avg: 0,
                min: i64::MIN,
                max: i64::MAX,
                count: u32::MAX,
            },
            Bucket {
                index: 1,
                avg: i64::MAX,
                min: i64::MIN,
                max: i64::MAX,
                count: 1,
            },
            Bucket {
                index: 23,
                avg: i64::MIN,
                min: i64::MIN,
                max: i64::MIN,
                count: 2,
            },
        ];
        let chunk = chunk_of(&all, RollupTier::Hour);
        assert_eq!(
            decode_rollup(&chunk.encode(), RollupTier::Hour),
            Ok(all.to_vec())
        );
    }

    #[test]
    fn a_reopened_chunk_takes_more_buckets_and_encodes_as_if_never_closed() {
        let all = buckets(60, 7);
        let whole = chunk_of(&all, RollupTier::Minute);
        let mut reopened = RollupChunk::reopen(
            &chunk_of(&all[..33], RollupTier::Minute).encode(),
            RollupTier::Minute,
        )
        .expect("reopens");
        for b in &all[33..] {
            reopened.push(*b).expect("fits");
        }
        assert_eq!(reopened, whole);
    }

    #[test]
    fn malformed_and_foreign_bytes_are_refused() {
        let good = chunk_of(&buckets(3, 0), RollupTier::Minute).encode();
        let mut foreign = good.clone();
        foreign[0] = super::super::RAW_FORMAT_V1;
        assert_eq!(
            decode_rollup(&foreign, RollupTier::Minute),
            Err(DecodeError::UnknownFormat(0x01))
        );
        assert_eq!(
            decode_rollup(&[], RollupTier::Minute),
            Err(DecodeError::Truncated)
        );
        assert_eq!(
            decode_rollup(&[ROLLUP_FORMAT_V1, 25, 0, 0], RollupTier::Hour),
            Err(DecodeError::TooLong)
        );
        assert_eq!(
            decode_rollup(&[ROLLUP_FORMAT_V1, 61, 0, 0], RollupTier::Minute),
            Err(DecodeError::TooLong)
        );
        assert_eq!(
            decode_rollup(&[ROLLUP_FORMAT_V1, 0, 0, 0], RollupTier::Minute),
            Err(DecodeError::Malformed)
        );
        let mut trailing = good.clone();
        trailing.push(0);
        assert_eq!(
            decode_rollup(&trailing, RollupTier::Minute),
            Err(DecodeError::Malformed)
        );
        for cut in 1..good.len() {
            assert!(
                decode_rollup(&good[..cut], RollupTier::Minute).is_err(),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn a_bucket_of_no_point_in_the_bytes_is_malformed() {
        // One bucket at index 0 with avg 0: min step `0`, max step `0`, count group 0.
        let bytes = [ROLLUP_FORMAT_V1, 1, 0, 0, 0b0000_0000, 0];
        assert_eq!(
            decode_rollup(&bytes[..5], RollupTier::Minute),
            Err(DecodeError::Truncated)
        );
        assert_eq!(
            decode_rollup(&bytes, RollupTier::Minute),
            Err(DecodeError::Malformed)
        );
    }

    fn valid_buckets(max: usize) -> impl Strategy<Value = Vec<Bucket>> {
        (
            0u64..1 << 40,
            prop::collection::vec(
                (
                    1u64..4,
                    any::<i64>(),
                    any::<u64>(),
                    any::<u64>(),
                    1u32..5_000,
                ),
                1..=max,
            ),
        )
            .prop_map(|(start, steps)| {
                let mut index = start;
                steps
                    .into_iter()
                    .map(|(gap, avg, below, above, count)| {
                        index += gap;
                        let min = avg
                            .checked_sub_unsigned(below % (1 << 40))
                            .unwrap_or(i64::MIN);
                        let max = avg
                            .checked_add_unsigned(above % (1 << 40))
                            .unwrap_or(i64::MAX);
                        Bucket {
                            index,
                            avg,
                            min,
                            max,
                            count,
                        }
                    })
                    .collect()
            })
    }

    proptest! {
        #[test]
        fn what_a_chunk_accepts_decodes_to_the_same_buckets(all in valid_buckets(24)) {
            let mut chunk = RollupChunk::new(RollupTier::Hour);
            let mut accepted = Vec::new();
            for b in all {
                if chunk.push(b).is_err() {
                    break;
                }
                accepted.push(b);
            }
            prop_assert_eq!(decode_rollup(&chunk.encode(), RollupTier::Hour), Ok(accepted));
        }

        #[test]
        fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..2_048)) {
            for tier in [RollupTier::Minute, RollupTier::Hour] {
                if let Ok(all) = decode_rollup(&bytes, tier) {
                    prop_assert!(!all.is_empty() && all.len() <= tier.max_buckets());
                    prop_assert!(all.iter().all(|b| b.min <= b.avg && b.avg <= b.max && b.count > 0));
                    prop_assert!(all.windows(2).all(|w| w[0].index < w[1].index));
                }
                let _ = RollupChunk::reopen(&bytes, tier);
            }
        }
    }
}
