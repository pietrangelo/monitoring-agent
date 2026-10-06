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

//! Block files on disk (RFC 0010 §5, §6): `blocks/<tier>/`, private directories; a file written
//! to its temporary, synced, then linked to its name, which fails rather than replace an
//! existing file; reads by offset (`pread`) of the ranges the format names. Blocking I/O: only
//! the store's own threads call it, never an async runtime.

use std::collections::BTreeSet;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::format::{BlockError, BlockSummary, HEADER_LEN, TRAILER_LEN, read_trailer};
use super::name::{BlockName, FileKind};
use crate::series::SeriesId;
use crate::tier::Tier;

/// The largest summary and trailer a file may claim: a span's summary is ≈ 16 B per 1,024
/// chunks, so anything near this is a damaged trailer, not a summary.
const SUMMARY_MAX: u64 = 16 << 20;

/// Why a block file couldn't be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileError {
    /// No file of that name.
    Missing,
    /// The file is not the block file it should be.
    Damaged(BlockError),
    /// An I/O error reading it.
    Io,
}

impl From<BlockError> for FileError {
    fn from(e: BlockError) -> FileError {
        FileError::Damaged(e)
    }
}

/// Why a block file couldn't be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteError {
    /// A file of its name already exists: it is never replaced.
    Exists,
    /// Creating, writing, syncing or linking it failed.
    Io(ErrorKind),
}

/// What a scan of the tier directories found.
#[derive(Debug, Default)]
pub(crate) struct Scan {
    pub blocks: BTreeSet<BlockName>,
    pub temporaries: Vec<PathBuf>,
}

/// `HUB_DATA_DIR/blocks`.
#[derive(Debug)]
pub(crate) struct BlockDir {
    root: PathBuf,
}

impl BlockDir {
    /// `blocks/` and one directory per tier, created mode 0700 when absent; existing ones are
    /// left as they are.
    pub(crate) fn create(data_dir: &Path) -> io::Result<BlockDir> {
        let root = data_dir.join("blocks");
        private_dir(&root)?;
        for tier in Tier::ALL {
            private_dir(&root.join(tier.name()))?;
        }
        Ok(BlockDir { root })
    }

    fn tier_dir(&self, tier: Tier) -> PathBuf {
        self.root.join(tier.name())
    }

    pub(crate) fn path(&self, name: &BlockName) -> PathBuf {
        self.tier_dir(name.tier).join(name.file_name())
    }

    /// Every block file and temporary in the tier directories; anything else is left alone.
    pub(crate) fn scan(&self) -> io::Result<Scan> {
        let mut scan = Scan::default();
        for tier in Tier::ALL {
            for entry in fs::read_dir(self.tier_dir(tier))? {
                let entry = entry?;
                let Some(file) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                match BlockName::classify(tier, &file) {
                    FileKind::Block(name) => {
                        scan.blocks.insert(name);
                    }
                    FileKind::Temporary => scan.temporaries.push(entry.path()),
                    FileKind::Foreign => {}
                }
            }
        }
        Ok(scan)
    }

    /// Deletes every temporary: no row ever names one (§8 step 2).
    pub(crate) fn remove_temporaries(&self) -> io::Result<()> {
        for path in self.scan()?.temporaries {
            remove(&path)?;
        }
        Ok(())
    }

    /// Writes a file under its name, durably, never over an existing one (§6 *Span handoff*
    /// step 3). On failure nothing of this call's stays under either name.
    pub(crate) fn write(&self, name: &BlockName, bytes: &[u8]) -> Result<(), WriteError> {
        let dir = self.tier_dir(name.tier);
        let (tmp, path) = (dir.join(name.temporary_name()), self.path(name));
        write_temporary(&tmp, bytes).map_err(|e| WriteError::Io(e.kind()))?;
        let linked = fs::hard_link(&tmp, &path).map_err(|e| match e.kind() {
            ErrorKind::AlreadyExists => WriteError::Exists,
            kind => WriteError::Io(kind),
        });
        let _ = fs::remove_file(&tmp);
        linked?;
        if let Err(e) = File::open(&dir).and_then(|d| d.sync_all()) {
            let _ = fs::remove_file(&path);
            return Err(WriteError::Io(e.kind()));
        }
        Ok(())
    }

    /// Unlinks a file; one already gone is no error.
    pub(crate) fn unlink(&self, name: &BlockName) -> io::Result<()> {
        remove(&self.path(name))
    }

    /// A file's header and summary, checked against its name and length.
    pub(crate) fn read_summary(&self, name: &BlockName) -> Result<BlockSummary, FileError> {
        let file = open(&self.path(name))?;
        let len = file.metadata().map_err(|_| FileError::Io)?.len();
        if len < (HEADER_LEN + TRAILER_LEN) as u64 {
            return Err(BlockError::Malformed.into());
        }
        let header = read_at(&file, 0, HEADER_LEN as u64)?;
        let trailer = read_at(&file, len - TRAILER_LEN as u64, TRAILER_LEN as u64)?;
        let offset = read_trailer(&trailer)?;
        let tail = len.checked_sub(offset).ok_or(BlockError::Malformed)?;
        if tail > SUMMARY_MAX {
            return Err(BlockError::Malformed.into());
        }
        let summary = read_at(&file, offset, tail)?;
        Ok(BlockSummary::parse(
            name.tier, name.span, &header, &summary, len,
        )?)
    }

    /// A series' chunks in a file, by seq, each checked against its CRC.
    pub(crate) fn read_series(
        &self,
        name: &BlockName,
        summary: &BlockSummary,
        id: SeriesId,
    ) -> Result<Vec<(u16, Vec<u8>)>, FileError> {
        let file = open(&self.path(name))?;
        let mut chunks = Vec::new();
        for i in summary.index_blocks_for(id) {
            let range = summary.index_block(i).ok_or(BlockError::Malformed)?;
            let index = read_at(&file, range.offset, range.len as u64)?;
            for entry in summary.decode_index_block(i, &index)? {
                if entry.id != id {
                    continue;
                }
                let bytes = read_at(&file, entry.offset, u64::from(entry.length))?;
                summary.check_chunk(&entry, &bytes)?;
                chunks.push((entry.seq, bytes));
            }
        }
        Ok(chunks)
    }
}

/// A directory of mode 0700 when this call creates it (whatever the umask); an existing one is
/// left as it is.
fn private_dir(path: &Path) -> io::Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700)),
        Err(e) if e.kind() == ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(e) => Err(e),
    }
}

/// Creates the temporary (never over an existing one, which isn't ours) and writes it durably;
/// a temporary this call created is removed if writing it fails.
fn write_temporary(tmp: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    if written.is_err() {
        let _ = fs::remove_file(tmp);
    }
    written
}

fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn open(path: &Path) -> Result<File, FileError> {
    File::open(path).map_err(|e| match e.kind() {
        ErrorKind::NotFound => FileError::Missing,
        _ => FileError::Io,
    })
}

/// `len` bytes at `offset`; a file shorter than that is damaged, not an I/O error.
fn read_at(file: &File, offset: u64, len: u64) -> Result<Vec<u8>, FileError> {
    let len = usize::try_from(len).map_err(|_| BlockError::Malformed)?;
    let mut buf = vec![0; len];
    file.read_exact_at(&mut buf, offset)
        .map_err(|e| match e.kind() {
            ErrorKind::UnexpectedEof => FileError::Damaged(BlockError::Malformed),
            _ => FileError::Io,
        })?;
    Ok(buf)
}
