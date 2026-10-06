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

//! A `blocks` row (RFC 0010 §5): `(tier, span start)` → the file's format, rewrite number,
//! length, chunk count and tombstoned bytes.
//!
//! ```text
//! version 0x01 · format u8 · rewrite u32 · length u64 · chunk count u32 · tombstoned bytes u64
//! ```

use super::name::Rewrite;
use crate::bytes::{Reader, Short};

const RECORD_V1: u8 = 1;

/// The file a span's row names, and what it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockRecord {
    pub format: u8,
    pub rewrite: Rewrite,
    pub length: u64,
    pub chunk_count: u32,
    /// The bytes of chunks of tombstoned generations still in the file.
    pub tombstoned: u64,
}

/// Bytes that are no `blocks` row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadRecord {
    UnknownVersion(u8),
    Malformed,
}

impl std::fmt::Display for BadRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BadRecord::UnknownVersion(v) => write!(f, "unknown block row version {v}"),
            BadRecord::Malformed => f.write_str("malformed block row"),
        }
    }
}

impl std::error::Error for BadRecord {}

impl BlockRecord {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = vec![RECORD_V1, self.format];
        out.extend_from_slice(&self.rewrite.0.to_be_bytes());
        out.extend_from_slice(&self.length.to_be_bytes());
        out.extend_from_slice(&self.chunk_count.to_be_bytes());
        out.extend_from_slice(&self.tombstoned.to_be_bytes());
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<BlockRecord, BadRecord> {
        let mut r = Reader::new(bytes);
        let version = r.u8().map_err(|_| BadRecord::Malformed)?;
        if version != RECORD_V1 {
            return Err(BadRecord::UnknownVersion(version));
        }
        let read = |r: &mut Reader<'_>| -> Result<BlockRecord, Short> {
            let record = BlockRecord {
                format: r.u8()?,
                rewrite: Rewrite(r.u32()?),
                length: r.u64()?,
                chunk_count: r.u32()?,
                tombstoned: r.u64()?,
            };
            Ok(record)
        };
        let record = read(&mut r).map_err(|_| BadRecord::Malformed)?;
        r.finish().map_err(|_| BadRecord::Malformed)?;
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_round_trips_at_its_bounds() {
        let cases = [
            BlockRecord {
                format: 1,
                rewrite: Rewrite(0),
                length: 46,
                chunk_count: 0,
                tombstoned: 0,
            },
            BlockRecord {
                format: 255,
                rewrite: Rewrite(u32::MAX),
                length: u64::MAX,
                chunk_count: u32::MAX,
                tombstoned: u64::MAX,
            },
            BlockRecord {
                format: 1,
                rewrite: Rewrite(3),
                length: 590_000_000,
                chunk_count: 2_000_000,
                tombstoned: 12_345,
            },
        ];
        for row in cases {
            let bytes = row.to_bytes();
            assert_eq!(bytes.len(), 1 + 1 + 4 + 8 + 4 + 8);
            assert_eq!(bytes[0], 1, "version");
            assert_eq!(BlockRecord::from_bytes(&bytes), Ok(row));
        }
    }

    #[test]
    fn a_row_is_laid_out_big_endian_in_its_documented_order() {
        // A persisted value: this layout is what rows already on disk hold.
        let row = BlockRecord {
            format: 0x01,
            rewrite: Rewrite(0x0203_0405),
            length: 0x0607_0809_0A0B_0C0D,
            chunk_count: 0x0E0F_1011,
            tombstoned: 0x1213_1415_1617_1819,
        };
        let golden: Vec<u8> = vec![
            0x01, // version
            0x01, // format
            0x02, 0x03, 0x04, 0x05, // rewrite
            0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, // length
            0x0E, 0x0F, 0x10, 0x11, // chunk count
            0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, // tombstoned bytes
        ];
        assert_eq!(row.to_bytes(), golden);
        assert_eq!(BlockRecord::from_bytes(&golden), Ok(row));
    }

    #[test]
    fn a_damaged_row_is_refused() {
        let good = BlockRecord {
            format: 1,
            rewrite: Rewrite(2),
            length: 1_000,
            chunk_count: 7,
            tombstoned: 0,
        }
        .to_bytes();
        let mut version = good.clone();
        version[0] = 2;
        assert_eq!(
            BlockRecord::from_bytes(&version),
            Err(BadRecord::UnknownVersion(2))
        );
        for cut in 0..good.len() {
            assert!(
                BlockRecord::from_bytes(&good[..cut]).is_err(),
                "cut at {cut}"
            );
        }
        let mut longer = good.clone();
        longer.push(0);
        assert_eq!(BlockRecord::from_bytes(&longer), Err(BadRecord::Malformed));
    }
}
