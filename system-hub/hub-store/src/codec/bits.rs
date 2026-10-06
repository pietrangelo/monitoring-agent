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

//! Bit streams, varints and the two variable-length codes of RFC 0010 §4: the
//! delta-of-delta code of timestamps and bucket indexes, and the code of zigzagged values.

use super::DecodeError;

/// Bits written most significant first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct BitWriter {
    bytes: Vec<u8>,
    bits: usize,
}

/// Bits read back in the order they were written; reading past the end is an error.
pub(crate) struct BitReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl BitWriter {
    /// Appends the low `width` bits of `value` (`width` ≤ 64).
    pub(crate) fn push(&mut self, value: u64, width: u32) {
        debug_assert!(width <= 64);
        for i in (0..width).rev() {
            let bit = (value >> i) & 1;
            if self.bits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if bit == 1 {
                let last = self.bytes.len() - 1;
                self.bytes[last] |= 0x80 >> (self.bits % 8);
            }
            self.bits += 1;
        }
    }

    pub(crate) fn bit_len(&self) -> usize {
        self.bits
    }

    /// The bytes written so far, the last one padded with zeros.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> BitReader<'a> {
        BitReader { bytes, pos: 0 }
    }

    /// Reads `width` bits (≤ 64).
    pub(crate) fn read(&mut self, width: u32) -> Result<u64, DecodeError> {
        let end = self.pos + width as usize;
        if end > self.bytes.len() * 8 {
            return Err(DecodeError::Truncated);
        }
        let value = (self.pos..end).fold(0u64, |acc, i| {
            let bit = (self.bytes[i / 8] >> (7 - i % 8)) & 1;
            (acc << 1) | u64::from(bit)
        });
        self.pos = end;
        Ok(value)
    }

    /// The bytes the bits read so far take, the last one counted whole.
    pub(crate) fn bytes_read(&self) -> usize {
        self.pos.div_ceil(8)
    }
}

pub(crate) fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

pub(crate) fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// LEB128, at most ten bytes.
pub(crate) fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

pub(crate) fn varint_len(value: u64) -> usize {
    (64 - value.leading_zeros() as usize).max(1).div_ceil(7)
}

pub(crate) fn read_varint(bytes: &[u8], pos: &mut usize) -> Result<u64, DecodeError> {
    let mut value = 0u64;
    for group in 0..10 {
        let byte = *bytes.get(*pos).ok_or(DecodeError::Truncated)?;
        *pos += 1;
        let bits = u64::from(byte & 0x7f);
        if group == 9 && bits > 1 {
            return Err(DecodeError::Malformed);
        }
        value |= bits << (7 * group);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(DecodeError::Malformed)
}

/// The timestamp code's steps: a prefix and the width of the two's-complement field after it.
const DOD_STEPS: [(u64, u32, u32); 4] = [
    (0b10, 2, 3),
    (0b110, 3, 7),
    (0b1110, 4, 12),
    (0b1111, 4, 32),
];

/// The cost in bits of a delta of deltas in the timestamp code, if it has one:
/// `0` | `10` + 3 bits | `110` + 7 bits | `1110` + 12 bits | `1111` + 32 bits.
pub(crate) fn dod_bits(dod: i64) -> Option<u32> {
    dod_step(dod).map(|step| step.map_or(1, |(_, prefix_len, width)| prefix_len + width))
}

/// `Some(None)` for zero, `Some(Some(step))` for the step that holds `dod`.
fn dod_step(dod: i64) -> Option<Option<(u64, u32, u32)>> {
    if dod == 0 {
        return Some(None);
    }
    DOD_STEPS
        .into_iter()
        .find(|&(_, _, width)| {
            let half = 1i64 << (width - 1);
            (-half..half).contains(&dod)
        })
        .map(Some)
}

/// Writes a delta of deltas the code holds (see [`dod_bits`]); `false` writes nothing.
pub(crate) fn write_dod(w: &mut BitWriter, dod: i64) -> bool {
    match dod_step(dod) {
        None => false,
        Some(None) => {
            w.push(0, 1);
            true
        }
        Some(Some((prefix, prefix_len, width))) => {
            w.push(prefix, prefix_len);
            w.push(dod as u64 & low_bits(width), width);
            true
        }
    }
}

pub(crate) fn read_dod(r: &mut BitReader<'_>) -> Result<i64, DecodeError> {
    let ones = read_ones(r, DOD_STEPS.len() as u32)?;
    if ones == 0 {
        return Ok(0);
    }
    let (_, _, width) = DOD_STEPS[ones as usize - 1];
    let field = r.read(width)?;
    // Sign-extend the field's two's complement.
    let shift = 64 - width;
    Ok(((field << shift) as i64) >> shift)
}

/// The value code's steps: a field width per count of leading ones (`0`, `10`, …, `11111`).
const VALUE_WIDTHS: [u32; 5] = [6, 13, 20, 32, 64];

/// The cost in bits of an unsigned value in the value code:
/// `0` | `10` + 6 bits | `110` + 13 | `1110` + 20 | `11110` + 32 | `11111` + 64.
pub(crate) fn value_bits(value: u64) -> u32 {
    match value_step(value) {
        None => 1,
        Some(step) => prefix_len(step) + VALUE_WIDTHS[step],
    }
}

/// The step that holds a non-zero value, as an index into [`VALUE_WIDTHS`].
fn value_step(value: u64) -> Option<usize> {
    (value != 0).then(|| {
        VALUE_WIDTHS
            .iter()
            .position(|&w| value <= low_bits(w))
            .unwrap_or(VALUE_WIDTHS.len() - 1)
    })
}

/// The prefix of step `step`: `step + 1` ones then a zero, except the last step's.
fn prefix_len(step: usize) -> u32 {
    if step + 1 == VALUE_WIDTHS.len() {
        step as u32 + 1
    } else {
        step as u32 + 2
    }
}

pub(crate) fn write_value(w: &mut BitWriter, value: u64) {
    match value_step(value) {
        None => w.push(0, 1),
        Some(step) => {
            let ones = step as u32 + 1;
            w.push(low_bits(ones), ones);
            if prefix_len(step) > ones {
                w.push(0, 1);
            }
            w.push(value, VALUE_WIDTHS[step]);
        }
    }
}

pub(crate) fn read_value(r: &mut BitReader<'_>) -> Result<u64, DecodeError> {
    let ones = read_ones(r, VALUE_WIDTHS.len() as u32)?;
    if ones == 0 {
        return Ok(0);
    }
    r.read(VALUE_WIDTHS[ones as usize - 1])
}

/// Reads ones up to the first zero, or up to `max` ones (the last step has no closing zero).
fn read_ones(r: &mut BitReader<'_>, max: u32) -> Result<u32, DecodeError> {
    let mut ones = 0;
    while ones < max && r.read(1)? == 1 {
        ones += 1;
    }
    Ok(ones)
}

fn low_bits(width: u32) -> u64 {
    if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    }
}

/// A varint inside a bit stream: groups of seven bits, each after a continuation bit.
pub(crate) fn write_bit_varint(w: &mut BitWriter, mut value: u64) {
    loop {
        let more = value >= 0x80;
        w.push(u64::from(more), 1);
        w.push(value & 0x7f, 7);
        value >>= 7;
        if !more {
            return;
        }
    }
}

pub(crate) fn read_bit_varint(r: &mut BitReader<'_>) -> Result<u64, DecodeError> {
    let mut value = 0u64;
    for group in 0..10 {
        let more = r.read(1)? == 1;
        let bits = r.read(7)?;
        if group == 9 && bits > 1 {
            return Err(DecodeError::Malformed);
        }
        value |= bits << (7 * group);
        if !more {
            return Ok(value);
        }
    }
    Err(DecodeError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_read_back_in_the_order_and_widths_they_were_written() {
        let fields: [(u64, u32); 7] = [
            (1, 1),
            (0b101, 3),
            (0, 5),
            (u64::MAX, 64),
            (0x3ff, 10),
            (0, 1),
            (u64::MAX >> 1, 63),
        ];
        let mut w = BitWriter::default();
        for (value, width) in fields {
            w.push(value, width);
        }
        assert_eq!(w.bit_len(), 147);
        assert_eq!(w.as_bytes().len(), 19, "147 bits in 19 bytes");
        let mut r = BitReader::new(w.as_bytes());
        for (value, width) in fields {
            assert_eq!(r.read(width), Ok(value), "width {width}");
        }
    }

    #[test]
    fn bits_are_written_most_significant_first_and_padded_with_zeros() {
        let mut w = BitWriter::default();
        w.push(0b1, 1);
        w.push(0b01, 2);
        assert_eq!(w.as_bytes(), &[0b1010_0000]);
    }

    #[test]
    fn reading_past_the_end_of_a_bit_stream_is_truncated_not_a_panic() {
        let mut r = BitReader::new(&[0xff]);
        assert_eq!(r.read(8), Ok(0xff));
        assert_eq!(r.read(1), Err(DecodeError::Truncated));
        let mut r = BitReader::new(&[]);
        assert_eq!(r.read(64), Err(DecodeError::Truncated));
        let mut r = BitReader::new(&[0; 7]);
        assert_eq!(r.read(64), Err(DecodeError::Truncated));
    }

    #[test]
    fn zigzag_maps_small_magnitudes_to_small_codes_and_round_trips_the_extremes() {
        let cases = [
            (0, 0),
            (-1, 1),
            (1, 2),
            (-2, 3),
            (2, 4),
            (i64::MAX, u64::MAX - 1),
            (i64::MIN, u64::MAX),
        ];
        for (signed, coded) in cases {
            assert_eq!(zigzag(signed), coded, "{signed}");
            assert_eq!(unzigzag(coded), signed, "{coded}");
        }
    }

    #[test]
    fn varints_round_trip_and_take_one_byte_per_seven_bits() {
        let cases = [
            (0u64, 1usize),
            (127, 1),
            (128, 2),
            (16_383, 2),
            (16_384, 3),
            (u64::MAX, 10),
        ];
        for (value, len) in cases {
            let mut out = Vec::new();
            write_varint(&mut out, value);
            assert_eq!(out.len(), len, "{value}");
            assert_eq!(varint_len(value), len, "{value}");
            let mut pos = 0;
            assert_eq!(read_varint(&out, &mut pos), Ok(value));
            assert_eq!(pos, len);
        }
    }

    #[test]
    fn a_varint_cut_short_or_longer_than_ten_bytes_is_refused() {
        let cases: [(&str, Vec<u8>); 4] = [
            ("empty", vec![]),
            ("cut short", vec![0x80]),
            (
                "eleven bytes",
                vec![0x80; 10].into_iter().chain([0x01]).collect(),
            ),
            (
                "past 64 bits",
                vec![0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02],
            ),
        ];
        for (name, bytes) in cases {
            let mut pos = 0;
            assert!(read_varint(&bytes, &mut pos).is_err(), "{name}");
        }
    }

    #[test]
    fn each_timestamp_step_is_used_up_to_its_boundary_and_round_trips() {
        let cases = [
            (0, Some(1)),
            (-4, Some(5)),
            (3, Some(5)),
            (-5, Some(10)),
            (4, Some(10)),
            (-64, Some(10)),
            (63, Some(10)),
            (-65, Some(16)),
            (64, Some(16)),
            (-2_048, Some(16)),
            (2_047, Some(16)),
            (-2_049, Some(36)),
            (2_048, Some(36)),
            (i32::MIN as i64, Some(36)),
            (i32::MAX as i64, Some(36)),
            (i32::MIN as i64 - 1, None),
            (i32::MAX as i64 + 1, None),
            (i64::MIN, None),
            (i64::MAX, None),
        ];
        for (dod, bits) in cases {
            assert_eq!(dod_bits(dod), bits, "{dod}");
            let mut w = BitWriter::default();
            assert_eq!(write_dod(&mut w, dod), bits.is_some(), "{dod}: written");
            assert_eq!(w.bit_len() as u32, bits.unwrap_or(0), "{dod}: cost");
            if bits.is_some() {
                assert_eq!(
                    read_dod(&mut BitReader::new(w.as_bytes())),
                    Ok(dod),
                    "{dod}"
                );
            }
        }
    }

    #[test]
    fn each_value_step_is_used_up_to_its_boundary_and_round_trips() {
        let cases = [
            (0u64, 1u32),
            (1, 8),
            (63, 8),
            (64, 16),
            (8_191, 16),
            (8_192, 24),
            ((1 << 20) - 1, 24),
            (1 << 20, 37),
            ((1 << 32) - 1, 37),
            (1 << 32, 69),
            (u64::MAX, 69),
        ];
        for (value, bits) in cases {
            assert_eq!(value_bits(value), bits, "{value}");
            let mut w = BitWriter::default();
            write_value(&mut w, value);
            assert_eq!(w.bit_len() as u32, bits, "{value}: cost");
            assert_eq!(
                read_value(&mut BitReader::new(w.as_bytes())),
                Ok(value),
                "{value}"
            );
        }
    }

    #[test]
    fn bit_varints_round_trip_one_group_per_seven_bits() {
        let cases = [
            (0u64, 8usize),
            (127, 8),
            (128, 16),
            (3_600, 16),
            (u64::MAX, 80),
        ];
        for (value, bits) in cases {
            let mut w = BitWriter::default();
            write_bit_varint(&mut w, value);
            assert_eq!(w.bit_len(), bits, "{value}");
            assert_eq!(
                read_bit_varint(&mut BitReader::new(w.as_bytes())),
                Ok(value),
                "{value}"
            );
        }
    }

    #[test]
    fn a_bit_varint_longer_than_ten_groups_is_refused() {
        let mut w = BitWriter::default();
        for _ in 0..11 {
            w.push(0xff, 8);
        }
        assert!(read_bit_varint(&mut BitReader::new(w.as_bytes())).is_err());
    }
}
