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

//! The block file format (RFC 0010 §5): one closed span of one tier, written once.
//!
//! ```text
//! header : magic "HUBBLK" · format u8 · tier u8 · span start u64 · chunk count u32 · index offset u64
//! data   : the span's chunks, ordered by (SeriesId, seq)
//! index  : per chunk (SeriesId u32, seq u16, offset u64, length u32, CRC-32C u32), sorted,
//!          in index blocks of 1,024 entries
//! summary: per index block (first SeriesId u32, offset u64, CRC-32C u32 of the index block)
//! trailer: summary offset u64 · CRC-32C of header and summary · magic
//! ```
//!
//! Every field is big-endian. A chunk's CRC covers (tier, span start, `SeriesId`, seq) and its
//! bytes, so an index entry pointing at another series' chunk fails its check. Pure functions
//! over bytes: the reader (`block::file`) reads the ranges they name.

use crate::bytes::{Reader, Short};
use crate::series::SeriesId;
use crate::tier::{SpanStart, Tier};

/// The one format this version writes and reads.
pub const BLOCK_FORMAT_V1: u8 = 1;
const MAGIC: &[u8; 6] = b"HUBBLK";
pub const HEADER_LEN: usize = 6 + 1 + 1 + 8 + 4 + 8;
pub const TRAILER_LEN: usize = 8 + 4 + 6;
const INDEX_ENTRY_LEN: usize = 4 + 2 + 8 + 4 + 4;
const SUMMARY_ENTRY_LEN: usize = 4 + 8 + 4;
/// Index entries per index block.
pub const INDEX_BLOCK_ENTRIES: usize = 1_024;

/// Why bytes are not (part of) the block file they should be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockError {
    /// No `HUBBLK` where the header or the trailer should start.
    BadMagic,
    UnknownFormat(u8),
    /// A file of another tier or span than its name says.
    WrongSpan,
    /// A CRC-32C that doesn't match.
    Checksum,
    /// Fields that contradict each other or the file's length.
    Malformed,
}

impl std::fmt::Display for BlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockError::BadMagic => f.write_str("not a block file"),
            BlockError::UnknownFormat(v) => write!(f, "unknown block format {v}"),
            BlockError::WrongSpan => f.write_str("a block file of another span"),
            BlockError::Checksum => f.write_str("a block file checksum mismatch"),
            BlockError::Malformed => f.write_str("a malformed block file"),
        }
    }
}

impl std::error::Error for BlockError {}

/// Chunks handed to the encoder out of (`SeriesId`, seq) order, or one too long to index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unordered;

/// One chunk to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkRef<'a> {
    pub id: SeriesId,
    pub seq: u16,
    pub bytes: &'a [u8],
}

/// One index entry: where a chunk is and what its bytes must hash to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub id: SeriesId,
    pub seq: u16,
    pub offset: u64,
    pub length: u32,
    crc: u32,
}

/// One summary entry: an index block's first series, offset and CRC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SummaryEntry {
    first: SeriesId,
    offset: u64,
    crc: u32,
}

/// A block file's header and summary, checked: what a store keeps of each file in memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockSummary {
    tier: Tier,
    span: SpanStart,
    chunk_count: u32,
    index_offset: u64,
    summary_offset: u64,
    entries: Vec<SummaryEntry>,
}

/// The byte range of one index block in its file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexBlockRange {
    pub offset: u64,
    pub len: usize,
}

/// A chunk's CRC: over the tier, span, series and seq, then its bytes.
fn chunk_crc(tier: Tier, span: SpanStart, id: SeriesId, seq: u16, bytes: &[u8]) -> u32 {
    let mut key = Vec::with_capacity(1 + 8 + 4 + 2);
    key.push(tier.code());
    key.extend_from_slice(&span.get().to_be_bytes());
    key.extend_from_slice(&id.0.to_be_bytes());
    key.extend_from_slice(&seq.to_be_bytes());
    crc32c::crc32c_append(crc32c::crc32c(&key), bytes)
}

/// The whole file for a span's chunks, which must come in strictly increasing (`SeriesId`, seq)
/// order.
pub fn encode_block(
    tier: Tier,
    span: SpanStart,
    chunks: &[ChunkRef<'_>],
) -> Result<Vec<u8>, Unordered> {
    let count = u32::try_from(chunks.len()).map_err(|_| Unordered)?;
    let ordered = chunks
        .windows(2)
        .all(|w| (w[0].id, w[0].seq) < (w[1].id, w[1].seq));
    if !ordered || chunks.iter().any(|c| u32::try_from(c.bytes.len()).is_err()) {
        return Err(Unordered);
    }
    let data_len: usize = chunks.iter().map(|c| c.bytes.len()).sum();
    let index_offset = (HEADER_LEN + data_len) as u64;
    let mut out = header_bytes(tier, span, count, index_offset);
    let mut index = Vec::with_capacity(chunks.len() * INDEX_ENTRY_LEN);
    for c in chunks {
        put_index_entry(&mut index, tier, span, c, out.len() as u64);
        out.extend_from_slice(c.bytes);
    }
    let summary = summary_bytes(&index, chunks, index_offset);
    out.extend_from_slice(&index);
    let summary_offset = out.len() as u64;
    out.extend_from_slice(&summary);
    let crc = crc32c::crc32c_append(crc32c::crc32c(&out[..HEADER_LEN]), &summary);
    out.extend_from_slice(&summary_offset.to_be_bytes());
    out.extend_from_slice(&crc.to_be_bytes());
    out.extend_from_slice(MAGIC);
    Ok(out)
}

fn header_bytes(tier: Tier, span: SpanStart, count: u32, index_offset: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN);
    out.extend_from_slice(MAGIC);
    out.push(BLOCK_FORMAT_V1);
    out.push(tier.code());
    out.extend_from_slice(&span.get().to_be_bytes());
    out.extend_from_slice(&count.to_be_bytes());
    out.extend_from_slice(&index_offset.to_be_bytes());
    out
}

fn put_index_entry(index: &mut Vec<u8>, tier: Tier, span: SpanStart, c: &ChunkRef<'_>, at: u64) {
    index.extend_from_slice(&c.id.0.to_be_bytes());
    index.extend_from_slice(&c.seq.to_be_bytes());
    index.extend_from_slice(&at.to_be_bytes());
    index.extend_from_slice(&(c.bytes.len() as u32).to_be_bytes());
    index.extend_from_slice(&chunk_crc(tier, span, c.id, c.seq, c.bytes).to_be_bytes());
}

/// Per index block of 1,024 entries: its first series, its offset and its CRC.
fn summary_bytes(index: &[u8], chunks: &[ChunkRef<'_>], index_offset: u64) -> Vec<u8> {
    let block_len = INDEX_BLOCK_ENTRIES * INDEX_ENTRY_LEN;
    index
        .chunks(block_len)
        .zip(chunks.chunks(INDEX_BLOCK_ENTRIES))
        .enumerate()
        .flat_map(|(i, (bytes, block))| {
            let mut entry = Vec::with_capacity(SUMMARY_ENTRY_LEN);
            entry.extend_from_slice(&block[0].id.0.to_be_bytes());
            entry.extend_from_slice(&(index_offset + (i * block_len) as u64).to_be_bytes());
            entry.extend_from_slice(&crc32c::crc32c(bytes).to_be_bytes());
            entry
        })
        .collect()
}

/// The summary offset a trailer names, once its magic is checked.
pub fn read_trailer(trailer: &[u8]) -> Result<u64, BlockError> {
    let mut r = Reader::new(trailer);
    let (offset, _crc, magic) = (|| Ok::<_, Short>((r.u64()?, r.u32()?, r.array::<6>()?)))()
        .map_err(|_| BlockError::Malformed)?;
    r.finish().map_err(|_| BlockError::Malformed)?;
    if &magic != MAGIC {
        return Err(BlockError::BadMagic);
    }
    Ok(offset)
}

/// A trailer's CRC of the header and summary; the trailer was read by [`read_trailer`].
fn trailer_crc(trailer: &[u8]) -> Result<u32, BlockError> {
    let bytes = trailer.get(8..12).ok_or(BlockError::Malformed)?;
    let bytes: [u8; 4] = bytes.try_into().map_err(|_| BlockError::Malformed)?;
    Ok(u32::from_be_bytes(bytes))
}

/// The header's chunk count and index offset, its magic, format, tier and span checked.
fn read_header(tier: Tier, span: SpanStart, header: &[u8]) -> Result<(u32, u64), BlockError> {
    let mut r = Reader::new(header);
    let malformed = |_: Short| BlockError::Malformed;
    if &r.array::<6>().map_err(malformed)? != MAGIC {
        return Err(BlockError::BadMagic);
    }
    let format = r.u8().map_err(malformed)?;
    if format != BLOCK_FORMAT_V1 {
        return Err(BlockError::UnknownFormat(format));
    }
    if r.u8().map_err(malformed)? != tier.code() || r.u64().map_err(malformed)? != span.get() {
        return Err(BlockError::WrongSpan);
    }
    let counts = (r.u32().map_err(malformed)?, r.u64().map_err(malformed)?);
    r.finish().map_err(malformed)?;
    Ok(counts)
}

fn read_summary_entry(bytes: &[u8]) -> Result<SummaryEntry, BlockError> {
    let mut r = Reader::new(bytes);
    let entry = (|| {
        Ok::<_, Short>(SummaryEntry {
            first: SeriesId(r.u32()?),
            offset: r.u64()?,
            crc: r.u32()?,
        })
    })()
    .map_err(|_| BlockError::Malformed)?;
    r.finish().map_err(|_| BlockError::Malformed)?;
    Ok(entry)
}

fn read_index_entry(bytes: &[u8]) -> Result<IndexEntry, BlockError> {
    let mut r = Reader::new(bytes);
    let entry = (|| {
        Ok::<_, Short>(IndexEntry {
            id: SeriesId(r.u32()?),
            seq: r.u16()?,
            offset: r.u64()?,
            length: r.u32()?,
            crc: r.u32()?,
        })
    })()
    .map_err(|_| BlockError::Malformed)?;
    r.finish().map_err(|_| BlockError::Malformed)?;
    Ok(entry)
}

impl BlockSummary {
    /// The header and the summary (the bytes from the summary offset to the end of the file,
    /// trailer included) of a file of `len` bytes named for `tier` and `span`, checked against
    /// each other, the file's length and the trailer's CRC.
    pub fn parse(
        tier: Tier,
        span: SpanStart,
        header: &[u8],
        summary_and_trailer: &[u8],
        len: u64,
    ) -> Result<BlockSummary, BlockError> {
        let (chunk_count, index_offset) = read_header(tier, span, header)?;
        let split = summary_and_trailer
            .len()
            .checked_sub(TRAILER_LEN)
            .ok_or(BlockError::Malformed)?;
        let (summary, trailer) = summary_and_trailer.split_at(split);
        let summary_offset = read_trailer(trailer)?;
        if summary_offset.checked_add(summary_and_trailer.len() as u64) != Some(len) {
            return Err(BlockError::Malformed);
        }
        if crc32c::crc32c_append(crc32c::crc32c(header), summary) != trailer_crc(trailer)? {
            return Err(BlockError::Checksum);
        }
        if summary.len() % SUMMARY_ENTRY_LEN != 0 {
            return Err(BlockError::Malformed);
        }
        let parsed = BlockSummary {
            tier,
            span,
            chunk_count,
            index_offset,
            summary_offset,
            entries: summary
                .chunks(SUMMARY_ENTRY_LEN)
                .map(read_summary_entry)
                .collect::<Result<_, _>>()?,
        };
        if parsed.laid_out() {
            Ok(parsed)
        } else {
            Err(BlockError::Malformed)
        }
    }

    /// The header's and the summary's fields agree: the index right before the summary, one
    /// summary entry per index block, each at its block's place, first series non-decreasing.
    fn laid_out(&self) -> bool {
        let count = self.chunk_count as usize;
        let block_len = (INDEX_BLOCK_ENTRIES * INDEX_ENTRY_LEN) as u64;
        let index_end = (count as u64)
            .checked_mul(INDEX_ENTRY_LEN as u64)
            .and_then(|len| self.index_offset.checked_add(len));
        let at_place = self.entries.iter().enumerate().all(|(i, e)| {
            (i as u64)
                .checked_mul(block_len)
                .and_then(|o| o.checked_add(self.index_offset))
                == Some(e.offset)
        });
        self.index_offset >= HEADER_LEN as u64
            && index_end == Some(self.summary_offset)
            && self.entries.len() == count.div_ceil(INDEX_BLOCK_ENTRIES)
            && at_place
            && self.entries.windows(2).all(|w| w[0].first <= w[1].first)
    }

    pub fn chunk_count(&self) -> u32 {
        self.chunk_count
    }

    /// The index blocks that may hold entries of `id`: those whose first series is at most
    /// `id` and whose successor's is at least `id`.
    pub fn index_blocks_for(&self, id: SeriesId) -> Vec<usize> {
        (0..self.entries.len())
            .filter(|&i| {
                self.entries[i].first <= id
                    && self.entries.get(i + 1).is_none_or(|next| next.first >= id)
            })
            .collect()
    }

    /// Where index block `i` lies.
    pub fn index_block(&self, i: usize) -> Option<IndexBlockRange> {
        let entry = self.entries.get(i)?;
        let before = i.checked_mul(INDEX_BLOCK_ENTRIES)?;
        let count = (self.chunk_count as usize).checked_sub(before)?;
        Some(IndexBlockRange {
            offset: entry.offset,
            len: count.min(INDEX_BLOCK_ENTRIES) * INDEX_ENTRY_LEN,
        })
    }

    /// Index block `i`'s entries, checked against its CRC, its order and the data region.
    pub fn decode_index_block(
        &self,
        i: usize,
        bytes: &[u8],
    ) -> Result<Vec<IndexEntry>, BlockError> {
        let (range, summary) = self
            .index_block(i)
            .zip(self.entries.get(i))
            .ok_or(BlockError::Malformed)?;
        if bytes.len() != range.len || crc32c::crc32c(bytes) != summary.crc {
            return Err(BlockError::Checksum);
        }
        let entries: Vec<IndexEntry> = bytes
            .chunks(INDEX_ENTRY_LEN)
            .map(read_index_entry)
            .collect::<Result<_, _>>()?;
        let sorted = entries
            .windows(2)
            .all(|w| (w[0].id, w[0].seq) < (w[1].id, w[1].seq));
        let first = entries.first().map(|e| e.id) == Some(summary.first);
        let inside = entries.iter().all(|e| {
            e.offset >= HEADER_LEN as u64
                && e.offset
                    .checked_add(u64::from(e.length))
                    .is_some_and(|end| end <= self.index_offset)
        });
        if sorted && first && inside {
            Ok(entries)
        } else {
            Err(BlockError::Malformed)
        }
    }

    /// A chunk's bytes, if they hash to its entry's CRC under this file's tier and span.
    pub fn check_chunk<'b>(
        &self,
        entry: &IndexEntry,
        bytes: &'b [u8],
    ) -> Result<&'b [u8], BlockError> {
        let crc = chunk_crc(self.tier, self.span, entry.id, entry.seq, bytes);
        if bytes.len() == entry.length as usize && crc == entry.crc {
            Ok(bytes)
        } else {
            Err(BlockError::Checksum)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPAN: u64 = 1_800_057_600;

    fn span() -> SpanStart {
        SpanStart::new(Tier::Raw, SPAN).expect("on the grid")
    }

    /// Chunks of `series` series with `per` seqs each, the bytes naming their id and seq.
    fn chunks(series: u32, per: u16) -> Vec<(SeriesId, u16, Vec<u8>)> {
        (0..series)
            .flat_map(|id| {
                (0..per).map(move |seq| {
                    let len = 1 + (id as usize * 7 + usize::from(seq)) % 40;
                    let mut bytes = vec![0xA5; len];
                    bytes[0] = (id % 251) as u8;
                    (SeriesId(id * 3 + 1), seq, bytes)
                })
            })
            .collect()
    }

    fn refs(chunks: &[(SeriesId, u16, Vec<u8>)]) -> Vec<ChunkRef<'_>> {
        chunks
            .iter()
            .map(|(id, seq, bytes)| ChunkRef {
                id: *id,
                seq: *seq,
                bytes,
            })
            .collect()
    }

    /// Parses a whole file as the reader does: header, trailer, summary.
    fn summary(file: &[u8]) -> Result<BlockSummary, BlockError> {
        let len = file.len() as u64;
        let trailer = file
            .get(file.len().saturating_sub(TRAILER_LEN)..)
            .unwrap_or(&[]);
        let at = read_trailer(trailer)?;
        let at = usize::try_from(at).map_err(|_| BlockError::Malformed)?;
        let tail = file.get(at..).ok_or(BlockError::Malformed)?;
        let header = file.get(..HEADER_LEN).unwrap_or(file);
        BlockSummary::parse(Tier::Raw, span(), header, tail, len)
    }

    /// Every chunk of `id`, read through the summary, the index blocks and the CRCs.
    fn read_series(
        file: &[u8],
        s: &BlockSummary,
        id: SeriesId,
    ) -> Result<Vec<(u16, Vec<u8>)>, BlockError> {
        let mut out = Vec::new();
        for i in s.index_blocks_for(id) {
            let range = s.index_block(i).ok_or(BlockError::Malformed)?;
            let start = range.offset as usize;
            let entries = s.decode_index_block(i, &file[start..start + range.len])?;
            for e in entries.iter().filter(|e| e.id == id) {
                let at = e.offset as usize;
                let bytes = s.check_chunk(e, &file[at..at + e.length as usize])?;
                out.push((e.seq, bytes.to_vec()));
            }
        }
        Ok(out)
    }

    #[test]
    fn a_file_round_trips_every_chunk_through_its_summary_and_index() {
        // 1, 1,024 and 1,025 entries: one index block, one full, one full plus one entry.
        for (series, per) in [(1, 1), (512, 2), (205, 5)] {
            let all = chunks(series, per);
            let file = encode_block(Tier::Raw, span(), &refs(&all)).expect("ordered");
            let s = summary(&file).expect("parses");
            assert_eq!(s.chunk_count() as usize, all.len());
            for id in 0..series {
                let id = SeriesId(id * 3 + 1);
                let expected: Vec<(u16, Vec<u8>)> = all
                    .iter()
                    .filter(|c| c.0 == id)
                    .map(|c| (c.1, c.2.clone()))
                    .collect();
                assert_eq!(
                    read_series(&file, &s, id),
                    Ok(expected),
                    "{series}×{per}: {id:?}"
                );
            }
        }
    }

    #[test]
    fn the_sparse_index_finds_the_first_and_last_series_and_misses_an_absent_one() {
        let all = chunks(700, 3); // 2,100 entries: three index blocks
        let file = encode_block(Tier::Raw, span(), &refs(&all)).expect("ordered");
        let s = summary(&file).expect("parses");
        let first = SeriesId(1);
        let last = SeriesId(699 * 3 + 1);
        assert_eq!(read_series(&file, &s, first).map(|c| c.len()), Ok(3));
        assert_eq!(read_series(&file, &s, last).map(|c| c.len()), Ok(3));
        for absent in [
            SeriesId(0),
            SeriesId(2),
            SeriesId(700 * 3 + 1),
            SeriesId(u32::MAX),
        ] {
            assert_eq!(read_series(&file, &s, absent), Ok(vec![]), "{absent:?}");
        }
        // Block 1 starts at entry 1,024 (series id 1,024), block 2 at entry 2,048 (id 2,047).
        assert_eq!(s.index_blocks_for(last), vec![2]);
        assert_eq!(s.index_blocks_for(SeriesId(1_024)), vec![0, 1]);
        assert_eq!(s.index_blocks_for(SeriesId(2_047)), vec![1, 2]);
        assert_eq!(s.index_blocks_for(first), vec![0]);
        assert_eq!(s.index_block(3), None);
    }

    #[test]
    fn a_series_whose_chunks_straddle_two_index_blocks_is_read_from_both() {
        // Entries 1,020..=1,029 are one series: it straddles the first index block's end.
        let mut all: Vec<(SeriesId, u16, Vec<u8>)> = (0..1_020)
            .map(|i| (SeriesId(i), 0, vec![1, 2, 3]))
            .collect();
        all.extend((0..10).map(|seq| (SeriesId(5_000), seq, vec![seq as u8; 5])));
        all.extend((0..10).map(|i| (SeriesId(6_000 + i), 0, vec![9])));
        let file = encode_block(Tier::Raw, span(), &refs(&all)).expect("ordered");
        let s = summary(&file).expect("parses");
        let read = read_series(&file, &s, SeriesId(5_000)).expect("reads");
        assert_eq!(
            read.iter().map(|c| c.0).collect::<Vec<_>>(),
            (0..10).collect::<Vec<u16>>()
        );
        assert_eq!(read[9].1, vec![9u8; 5]);
    }

    #[test]
    fn an_empty_span_is_a_valid_file_with_no_index_block() {
        let file = encode_block(Tier::Raw, span(), &[]).expect("empty is ordered");
        let s = summary(&file).expect("parses");
        assert_eq!(s.chunk_count(), 0);
        assert_eq!(s.index_blocks_for(SeriesId(1)), Vec::<usize>::new());
    }

    #[test]
    fn chunks_out_of_order_or_repeated_are_refused() {
        let a = [1u8];
        let cases = [
            ("series backwards", vec![(2, 0), (1, 0)]),
            ("seq backwards", vec![(1, 1), (1, 0)]),
            ("repeated", vec![(1, 0), (1, 0)]),
        ];
        for (name, order) in cases {
            let refs: Vec<ChunkRef<'_>> = order
                .iter()
                .map(|&(id, seq)| ChunkRef {
                    id: SeriesId(id),
                    seq,
                    bytes: &a,
                })
                .collect();
            assert_eq!(
                encode_block(Tier::Raw, span(), &refs),
                Err(Unordered),
                "{name}"
            );
        }
    }

    #[test]
    fn the_header_names_the_tier_and_span_and_a_file_of_another_is_refused() {
        let file = encode_block(Tier::Minute, span(), &refs(&chunks(2, 1))).expect("ordered");
        assert_eq!(&file[..6], b"HUBBLK");
        assert_eq!(file[6], BLOCK_FORMAT_V1);
        assert_eq!(file[7], Tier::Minute.code());
        assert_eq!(file[8..16], SPAN.to_be_bytes());
        assert_eq!(file[16..20], 2u32.to_be_bytes(), "chunk count");
        let data: usize = chunks(2, 1).iter().map(|c| c.2.len()).sum();
        assert_eq!(
            file[20..28],
            ((HEADER_LEN + data) as u64).to_be_bytes(),
            "index offset: right after the data"
        );
        let at = read_trailer(&file[file.len() - TRAILER_LEN..]).expect("trailer") as usize;
        let len = file.len() as u64;
        let parse =
            |tier, span| BlockSummary::parse(tier, span, &file[..HEADER_LEN], &file[at..], len);
        assert!(parse(Tier::Minute, span()).is_ok());
        assert_eq!(parse(Tier::Raw, span()), Err(BlockError::WrongSpan));
        let other = SpanStart::new(Tier::Minute, SPAN + 3_600).expect("grid");
        assert_eq!(parse(Tier::Minute, other), Err(BlockError::WrongSpan));
    }

    #[test]
    fn an_unknown_format_byte_refuses_the_file() {
        let mut file = encode_block(Tier::Raw, span(), &refs(&chunks(3, 1))).expect("ordered");
        file[6] = 2;
        assert_eq!(summary(&file), Err(BlockError::UnknownFormat(2)));
    }

    #[test]
    fn a_flipped_bit_in_the_header_summary_or_trailer_refuses_the_file() {
        let good = encode_block(Tier::Raw, span(), &refs(&chunks(300, 4))).expect("ordered");
        let at = read_trailer(&good[good.len() - TRAILER_LEN..]).expect("trailer") as usize;
        // Every byte of the header and of the summary and trailer, but the format byte (its own
        // refusal, above), the tier and span bytes (WrongSpan, above).
        let positions = (16..HEADER_LEN).chain(at..good.len()).chain(0..6);
        for pos in positions {
            let mut file = good.clone();
            file[pos] ^= 0x10;
            assert!(summary(&file).is_err(), "byte {pos} flipped");
        }
    }

    #[test]
    fn a_truncated_or_extended_file_is_refused() {
        let good = encode_block(Tier::Raw, span(), &refs(&chunks(10, 2))).expect("ordered");
        for cut in [0, 1, HEADER_LEN, good.len() / 2, good.len() - 1] {
            assert!(summary(&good[..cut]).is_err(), "cut at {cut}");
        }
        let mut longer = good.clone();
        longer.extend_from_slice(&[0; 16]);
        assert!(summary(&longer).is_err(), "bytes after the trailer");
    }

    #[test]
    fn a_flipped_byte_in_an_index_block_fails_its_check() {
        let all = chunks(600, 2);
        let file = encode_block(Tier::Raw, span(), &refs(&all)).expect("ordered");
        let s = summary(&file).expect("parses");
        let range = s.index_block(1).expect("a second index block");
        for k in [0, 5, 13, range.len - 1] {
            let mut bytes = file[range.offset as usize..range.offset as usize + range.len].to_vec();
            bytes[k] ^= 1;
            assert_eq!(
                s.decode_index_block(1, &bytes),
                Err(BlockError::Checksum),
                "byte {k}"
            );
        }
        let bytes = &file[range.offset as usize..range.offset as usize + range.len];
        assert_eq!(
            s.decode_index_block(1, &bytes[1..]),
            Err(BlockError::Checksum)
        );
        assert_eq!(
            s.decode_index_block(0, bytes),
            Err(BlockError::Checksum),
            "another block's bytes"
        );
    }

    #[test]
    fn a_flipped_byte_in_a_chunk_fails_its_check() {
        let all = chunks(4, 2);
        let file = encode_block(Tier::Raw, span(), &refs(&all)).expect("ordered");
        let s = summary(&file).expect("parses");
        let range = s.index_block(0).expect("one index block");
        let start = range.offset as usize;
        let entries = s
            .decode_index_block(0, &file[start..start + range.len])
            .expect("index");
        let e = entries[3];
        let at = e.offset as usize;
        let mut bytes = file[at..at + e.length as usize].to_vec();
        assert_eq!(s.check_chunk(&e, &bytes), Ok(&bytes[..]));
        bytes[0] ^= 0x80;
        assert_eq!(s.check_chunk(&e, &bytes), Err(BlockError::Checksum));
    }

    #[test]
    fn an_index_entry_naming_another_series_fails_the_chunks_check() {
        // The entry's CRC covers the series: an entry whose id flipped to another existing
        // series' never serves that series this chunk.
        let all = chunks(4, 1);
        let file = encode_block(Tier::Raw, span(), &refs(&all)).expect("ordered");
        let s = summary(&file).expect("parses");
        let range = s.index_block(0).expect("one index block");
        let start = range.offset as usize;
        let entries = s
            .decode_index_block(0, &file[start..start + range.len])
            .expect("index");
        let (a, b) = (entries[0], entries[1]);
        let at = a.offset as usize;
        let bytes = &file[at..at + a.length as usize];
        for forged in [
            IndexEntry { id: b.id, ..a },
            IndexEntry {
                seq: a.seq + 1,
                ..a
            },
        ] {
            assert_eq!(
                s.check_chunk(&forged, bytes),
                Err(BlockError::Checksum),
                "{forged:?}"
            );
        }
        // The same chunk under another tier's or span's summary fails too.
        let minute = encode_block(Tier::Minute, span(), &refs(&all)).expect("ordered");
        let at_m = read_trailer(&minute[minute.len() - TRAILER_LEN..]).expect("t") as usize;
        let other = BlockSummary::parse(
            Tier::Minute,
            span(),
            &minute[..HEADER_LEN],
            &minute[at_m..],
            minute.len() as u64,
        )
        .expect("parses");
        assert_eq!(other.check_chunk(&a, bytes), Err(BlockError::Checksum));
        let later = SpanStart::new(Tier::Raw, SPAN + 3_600).expect("grid");
        let raw = encode_block(Tier::Raw, later, &refs(&all)).expect("ordered");
        let at_r = read_trailer(&raw[raw.len() - TRAILER_LEN..]).expect("t") as usize;
        let other_span = BlockSummary::parse(
            Tier::Raw,
            later,
            &raw[..HEADER_LEN],
            &raw[at_r..],
            raw.len() as u64,
        )
        .expect("parses");
        assert_eq!(
            other_span.check_chunk(&a, bytes),
            Err(BlockError::Checksum),
            "another span"
        );
    }

    /// Recomputes the trailer's CRC over the header and the summary, as a forger who can write
    /// the file would.
    fn reseal_trailer(file: &mut [u8]) {
        let at = read_trailer(&file[file.len() - TRAILER_LEN..]).expect("t") as usize;
        let mut covered = file[..HEADER_LEN].to_vec();
        covered.extend_from_slice(&file[at..file.len() - TRAILER_LEN]);
        let t = file.len() - TRAILER_LEN + 8;
        file[t..t + 4].copy_from_slice(&crc32c::crc32c(&covered).to_be_bytes());
    }

    /// Recomputes index block 0's CRC in the summary (its entry: first id, offset, CRC), then
    /// the trailer's.
    fn reseal_index_block_0(file: &mut [u8], range: IndexBlockRange) {
        let start = range.offset as usize;
        let crc = crc32c::crc32c(&file[start..start + range.len]);
        let at = read_trailer(&file[file.len() - TRAILER_LEN..]).expect("t") as usize;
        file[at + 12..at + 16].copy_from_slice(&crc.to_be_bytes());
        reseal_trailer(file);
    }

    #[test]
    fn a_forged_index_block_with_a_valid_crc_is_checked_for_order_and_the_data_region() {
        // Someone who can write the file can forge CRCs (A08): offsets must still stay inside
        // the data region, so a read never runs into the index, and entries must be sorted.
        let all = chunks(3, 1);
        let good = encode_block(Tier::Raw, span(), &refs(&all)).expect("ordered");
        let range = summary(&good)
            .expect("parses")
            .index_block(0)
            .expect("index");
        let start = range.offset as usize;
        let entry = move |k: usize| start + k * INDEX_ENTRY_LEN;
        let offset_of = move |file: &[u8], k: usize| {
            u64::from_be_bytes(file[entry(k) + 6..entry(k) + 14].try_into().expect("8"))
        };
        type Forge = Box<dyn Fn(&mut Vec<u8>)>;
        let cases: Vec<(&str, Forge)> = vec![
            (
                "an offset at the index itself",
                Box::new(move |f: &mut Vec<u8>| {
                    f[entry(1) + 6..entry(1) + 14].copy_from_slice(&(start as u64).to_be_bytes())
                }),
            ),
            (
                "an offset inside the data whose length runs into the index",
                Box::new(move |f: &mut Vec<u8>| {
                    let at = offset_of(f, 2);
                    let len = (start as u64 - at + 1) as u32;
                    f[entry(2) + 14..entry(2) + 18].copy_from_slice(&len.to_be_bytes())
                }),
            ),
            (
                "an offset inside the header",
                Box::new(move |f: &mut Vec<u8>| {
                    f[entry(0) + 6..entry(0) + 14].copy_from_slice(&3u64.to_be_bytes())
                }),
            ),
            (
                "two entries out of order",
                Box::new(move |f: &mut Vec<u8>| {
                    let first = f[entry(1)..entry(2)].to_vec();
                    let second = f[entry(2)..entry(3)].to_vec();
                    f[entry(1)..entry(2)].copy_from_slice(&second);
                    f[entry(2)..entry(3)].copy_from_slice(&first);
                }),
            ),
        ];
        for (name, forge) in cases {
            let mut file = good.clone();
            forge(&mut file);
            reseal_index_block_0(&mut file, range);
            let s = summary(&file).expect("the forged summary parses");
            assert_eq!(
                s.decode_index_block(0, &file[start..start + range.len]),
                Err(BlockError::Malformed),
                "{name}"
            );
        }
    }

    #[test]
    fn a_forged_index_block_must_be_strictly_ordered_within_a_series() {
        // One series' chunks with their seqs swapped, and an entry repeated over its neighbour
        // (the chunk served twice): both resealed, both refused.
        let all = chunks(2, 3);
        let good = encode_block(Tier::Raw, span(), &refs(&all)).expect("ordered");
        let range = summary(&good)
            .expect("parses")
            .index_block(0)
            .expect("index");
        let start = range.offset as usize;
        let entry = move |k: usize| start + k * INDEX_ENTRY_LEN;
        type Forge = Box<dyn Fn(&mut Vec<u8>)>;
        let cases: Vec<(&str, Forge)> = vec![
            (
                "seqs 0 and 1 of one series swapped",
                Box::new(move |f: &mut Vec<u8>| {
                    let first = f[entry(0)..entry(1)].to_vec();
                    let second = f[entry(1)..entry(2)].to_vec();
                    f[entry(0)..entry(1)].copy_from_slice(&second);
                    f[entry(1)..entry(2)].copy_from_slice(&first);
                }),
            ),
            (
                "an entry repeated over its neighbour",
                Box::new(move |f: &mut Vec<u8>| {
                    let first = f[entry(1)..entry(2)].to_vec();
                    f[entry(2)..entry(3)].copy_from_slice(&first);
                }),
            ),
        ];
        for (name, forge) in cases {
            let mut file = good.clone();
            forge(&mut file);
            reseal_index_block_0(&mut file, range);
            let s = summary(&file).expect("the forged summary parses");
            assert_eq!(
                s.decode_index_block(0, &file[start..start + range.len]),
                Err(BlockError::Malformed),
                "{name}"
            );
        }
    }

    #[test]
    fn an_index_block_whose_first_entry_isnt_its_summarys_first_series_is_malformed() {
        // A forged summary first id (still non-decreasing, CRCs resealed) would make the sparse
        // index skip the block, so reads would miss data silently: the block refuses instead.
        let all = chunks(400, 3); // two index blocks; block 1 starts at series id 1,024
        let good = encode_block(Tier::Raw, span(), &refs(&all)).expect("ordered");
        let at = read_trailer(&good[good.len() - TRAILER_LEN..]).expect("t") as usize;
        let e = at + SUMMARY_ENTRY_LEN;
        // Lower and higher than the true 1,024 (a higher one sends series 1,024..1,099 to
        // block 0 only); both keep the summary non-decreasing.
        for forged in [1_000u32, 1_100] {
            let mut file = good.clone();
            file[e..e + 4].copy_from_slice(&forged.to_be_bytes());
            reseal_trailer(&mut file);
            let s = summary(&file).expect("the forged summary parses");
            let range = s.index_block(1).expect("block 1");
            let start = range.offset as usize;
            assert_eq!(
                s.decode_index_block(1, &file[start..start + range.len]),
                Err(BlockError::Malformed),
                "first id forged to {forged}"
            );
        }
    }

    #[test]
    fn a_file_length_that_disagrees_with_the_summary_offset_is_malformed() {
        let file = encode_block(Tier::Raw, span(), &refs(&chunks(5, 2))).expect("ordered");
        let at = read_trailer(&file[file.len() - TRAILER_LEN..]).expect("t") as usize;
        let len = file.len() as u64;
        for wrong in [len - 1, len + 1, 0, u64::MAX] {
            assert_eq!(
                BlockSummary::parse(Tier::Raw, span(), &file[..HEADER_LEN], &file[at..], wrong),
                Err(BlockError::Malformed),
                "len {wrong}"
            );
        }
        assert!(
            BlockSummary::parse(Tier::Raw, span(), &file[..HEADER_LEN], &file[at..], len).is_ok()
        );
    }

    #[test]
    fn a_forged_header_or_summary_with_a_valid_crc_that_contradicts_itself_is_malformed() {
        let all = chunks(400, 3); // 1,200 entries: two index blocks
        let good = encode_block(Tier::Raw, span(), &refs(&all)).expect("ordered");
        let at = read_trailer(&good[good.len() - TRAILER_LEN..]).expect("t") as usize;
        type Forge = Box<dyn Fn(&mut Vec<u8>)>;
        let cases: Vec<(&str, Forge)> = vec![
            (
                "a chunk count of 1 with two summary entries",
                Box::new(|f: &mut Vec<u8>| f[16..20].copy_from_slice(&1u32.to_be_bytes())),
            ),
            (
                "a chunk count of 1,201: still two index blocks, but the index runs past the summary",
                Box::new(|f: &mut Vec<u8>| f[16..20].copy_from_slice(&1_201u32.to_be_bytes())),
            ),
            (
                "a chunk count of 2,048: two full index blocks",
                Box::new(|f: &mut Vec<u8>| f[16..20].copy_from_slice(&2_048u32.to_be_bytes())),
            ),
            (
                "a chunk count of 3,000 with two summary entries",
                Box::new(|f: &mut Vec<u8>| f[16..20].copy_from_slice(&3_000u32.to_be_bytes())),
            ),
            (
                "an index offset past the summary",
                Box::new(move |f: &mut Vec<u8>| {
                    f[20..28].copy_from_slice(&(at as u64 + 1).to_be_bytes())
                }),
            ),
            (
                "an index offset inside the header",
                Box::new(|f: &mut Vec<u8>| f[20..28].copy_from_slice(&4u64.to_be_bytes())),
            ),
            (
                "a summary entry's offset off its block's place",
                Box::new(move |f: &mut Vec<u8>| {
                    let e = at + SUMMARY_ENTRY_LEN + 4;
                    let off = u64::from_be_bytes(f[e..e + 8].try_into().expect("8"));
                    f[e..e + 8].copy_from_slice(&(off + 22).to_be_bytes())
                }),
            ),
            (
                "summary entries' first series going backwards",
                Box::new(move |f: &mut Vec<u8>| {
                    let e = at + SUMMARY_ENTRY_LEN;
                    f[e..e + 4].copy_from_slice(&0u32.to_be_bytes())
                }),
            ),
        ];
        for (name, forge) in cases {
            let mut file = good.clone();
            forge(&mut file);
            reseal_trailer(&mut file);
            assert_eq!(summary(&file), Err(BlockError::Malformed), "{name}");
        }
    }
}
