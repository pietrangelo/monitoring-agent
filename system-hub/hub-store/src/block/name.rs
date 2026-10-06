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

//! Block file names (RFC 0010 §5): `blocks/<tier>/<span start>[.r<n>].blk`, derived from the
//! file's typed key and rewrite number, never stored as a string. A handoff writes rewrite 0
//! (`S.blk`); each rewrite of the span takes the next number (`S.r1.blk`, …).

use crate::tier::{SpanStart, Tier};

/// How many times a span's file was rewritten: 0 for the handoff's file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rewrite(pub u32);

/// One block file: its tier, its span and its rewrite number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockName {
    pub tier: Tier,
    pub span: SpanStart,
    pub rewrite: Rewrite,
}

/// What a file found in a tier's directory is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Block(BlockName),
    /// A block file still being written (`….blk.tmp`): no row ever names one.
    Temporary,
    /// Anything else: not the store's, left alone.
    Foreign,
}

impl BlockName {
    /// The file's name inside its tier's directory.
    pub fn file_name(&self) -> String {
        match self.rewrite {
            Rewrite(0) => format!("{}.blk", self.span.get()),
            Rewrite(n) => format!("{}.r{n}.blk", self.span.get()),
        }
    }

    /// The name of the temporary file it is written to before its rename.
    pub fn temporary_name(&self) -> String {
        format!("{}.tmp", self.file_name())
    }

    /// What a file named `name` in `tier`'s directory is. Only the canonical spelling is a
    /// block file: decimal digits with no leading zero, a span on the tier's grid, and a
    /// rewrite number from 1 (`.r0` is spelled without it).
    pub fn classify(tier: Tier, name: &str) -> FileKind {
        if let Some(stem) = name.strip_suffix(".blk.tmp") {
            return match parse_stem(tier, stem) {
                Some(_) => FileKind::Temporary,
                None => FileKind::Foreign,
            };
        }
        name.strip_suffix(".blk")
            .and_then(|stem| parse_stem(tier, stem))
            .map_or(FileKind::Foreign, FileKind::Block)
    }
}

/// `<span>` or `<span>.r<n>`, canonically spelled.
fn parse_stem(tier: Tier, stem: &str) -> Option<BlockName> {
    let (span, rewrite) = match stem.split_once(".r") {
        Some((span, n)) => (span, canonical::<u32>(n).filter(|n| *n > 0)?),
        None => (stem, 0),
    };
    let span = SpanStart::new(tier, canonical::<u64>(span)?)?;
    Some(BlockName {
        tier,
        span,
        rewrite: Rewrite(rewrite),
    })
}

/// A decimal number spelled as it is written: digits only, no leading zero but `0` itself.
fn canonical<T: std::str::FromStr>(digits: &str) -> Option<T> {
    let spelled = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'));
    spelled.then(|| digits.parse().ok()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_800_057_600;

    fn name(tier: Tier, span: u64, rewrite: u32) -> BlockName {
        BlockName {
            tier,
            span: SpanStart::new(tier, span).expect("on the grid"),
            rewrite: Rewrite(rewrite),
        }
    }

    #[test]
    fn a_name_is_its_span_and_rewrite_number_and_reads_back() {
        let cases = [
            (name(Tier::Raw, S, 0), "1800057600.blk"),
            (name(Tier::Minute, S, 1), "1800057600.r1.blk"),
            (name(Tier::Hour, S, u32::MAX), "1800057600.r4294967295.blk"),
            (name(Tier::Raw, 0, 0), "0.blk"),
        ];
        for (block, file) in cases {
            assert_eq!(block.file_name(), file);
            assert_eq!(block.temporary_name(), format!("{file}.tmp"));
            assert_eq!(
                BlockName::classify(block.tier, file),
                FileKind::Block(block),
                "{file}"
            );
            assert_eq!(
                BlockName::classify(block.tier, &block.temporary_name()),
                FileKind::Temporary,
                "{file}.tmp"
            );
        }
    }

    #[test]
    fn only_the_canonical_spelling_is_a_block_file() {
        let foreign = [
            "",
            ".blk",
            "1800057600",
            "1800057600.blk.bak",
            "01800057600.blk",
            "+1800057600.blk",
            "1800057600.r0.blk",
            "1800057600.r01.blk",
            "1800057600.r.blk",
            "1800057600.r4294967296.blk",
            "18446744073709551616.blk",
            "1800057601.blk",
            "1800057600.R1.blk",
            "1800057600.r+1.blk",
            "1800057600.r-1.blk",
            "1800057600.r 1.blk",
            "1800057600.r1.r2.blk",
            "-0.blk",
            "1800057600.r1.blk.tmp.tmp",
            "x.blk.tmp",
            "notes.txt",
        ];
        for file in foreign {
            assert_eq!(
                BlockName::classify(Tier::Raw, file),
                FileKind::Foreign,
                "{file:?}"
            );
        }
    }

    #[test]
    fn only_a_canonical_block_name_with_tmp_is_temporary() {
        // Temporary files are deleted at open: one that isn't exactly a block file's temporary
        // name is not the store's, and is left alone.
        let foreign = [
            ".blk.tmp",
            "01800057600.blk.tmp",
            "1800057601.blk.tmp",
            "1800057600.r0.blk.tmp",
            "1800057600.r+1.blk.tmp",
            "9junk.blk.tmp",
            "1800057600.tmp",
            "1800057600.blk.TMP",
        ];
        for file in foreign {
            assert_eq!(
                BlockName::classify(Tier::Raw, file),
                FileKind::Foreign,
                "{file:?}"
            );
        }
        for file in ["1800057600.blk.tmp", "1800057600.r7.blk.tmp", "0.blk.tmp"] {
            assert_eq!(
                BlockName::classify(Tier::Raw, file),
                FileKind::Temporary,
                "{file:?}"
            );
        }
    }

    #[test]
    fn a_span_off_its_tiers_grid_is_foreign() {
        // An hour span is a day: 1800057600 + 3600 is on the raw grid, not the hour one.
        let file = format!("{}.blk", S + 3_600);
        assert!(matches!(
            BlockName::classify(Tier::Raw, &file),
            FileKind::Block(_)
        ));
        assert_eq!(BlockName::classify(Tier::Hour, &file), FileKind::Foreign);
    }
}
