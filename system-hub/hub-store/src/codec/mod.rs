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

//! The store's own chunk codec (RFC 0010 §4): versioned by a leading format byte, total on
//! any input, and bounded in what it allocates.

pub(crate) mod bits;
mod raw;
mod rollup;

pub use raw::{MAX_RAW_CHUNK_BYTES, MAX_RAW_POINTS, RAW_FORMAT_V1, RawChunk, RawPoint, decode_raw};
pub use rollup::{Bucket, ROLLUP_FORMAT_V1, RollupChunk, decode_rollup};

/// Why bytes are not a chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The bytes end before the chunk does.
    Truncated,
    /// A format byte this version doesn't know.
    UnknownFormat(u8),
    /// More points or buckets than a chunk may hold.
    TooLong,
    /// The bytes decode to something no encoder writes (time running backwards, an empty
    /// bucket, bytes after the end).
    Malformed,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Truncated => f.write_str("chunk truncated"),
            DecodeError::UnknownFormat(format) => write!(f, "unknown chunk format {format:#04x}"),
            DecodeError::TooLong => f.write_str("chunk holds more than its limit"),
            DecodeError::Malformed => f.write_str("malformed chunk"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Why a chunk refuses another point or bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushRefused {
    /// The chunk is full, or the step can't be coded: seal it and start the next one.
    Full,
    /// Not after the chunk's last time.
    NotAfterLast,
    /// A bucket whose average isn't between its minimum and maximum, or with no point.
    InvalidBucket,
}

impl std::fmt::Display for PushRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PushRefused::Full => "chunk full",
            PushRefused::NotAfterLast => "not after the chunk's last time",
            PushRefused::InvalidBucket => "invalid bucket",
        })
    }
}

impl std::error::Error for PushRefused {}
