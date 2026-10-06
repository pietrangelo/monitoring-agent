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

//! Fixed-width, big-endian fields of the store's persisted values, read without panicking.

/// Bytes ended early, or more bytes followed the last field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Short;

/// A cursor over persisted bytes.
pub(crate) struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { rest: bytes }
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], Short> {
        let (head, rest) = self.rest.split_first_chunk::<N>().ok_or(Short)?;
        self.rest = rest;
        Ok(*head)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, Short> {
        self.array::<1>().map(|[b]| b)
    }

    pub(crate) fn u16(&mut self) -> Result<u16, Short> {
        self.array().map(u16::from_be_bytes)
    }

    pub(crate) fn u32(&mut self) -> Result<u32, Short> {
        self.array().map(u32::from_be_bytes)
    }

    pub(crate) fn u64(&mut self) -> Result<u64, Short> {
        self.array().map(u64::from_be_bytes)
    }

    pub(crate) fn i64(&mut self) -> Result<i64, Short> {
        self.array().map(i64::from_be_bytes)
    }

    pub(crate) fn i128(&mut self) -> Result<i128, Short> {
        self.array().map(i128::from_be_bytes)
    }

    pub(crate) fn bytes(&mut self, len: usize) -> Result<&'a [u8], Short> {
        let (head, rest) = self.rest.split_at_checked(len).ok_or(Short)?;
        self.rest = rest;
        Ok(head)
    }

    /// A field of bytes after its two-byte length.
    pub(crate) fn u16_prefixed(&mut self) -> Result<&'a [u8], Short> {
        let len = self.u16()?;
        self.bytes(usize::from(len))
    }

    /// Succeeds only when every byte was read.
    pub(crate) fn finish(self) -> Result<(), Short> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(Short)
        }
    }
}

/// Appends a field of bytes after its two-byte length; the caller bounds it to `u16::MAX`.
pub(crate) fn put_u16_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    debug_assert!(bytes.len() <= usize::from(u16::MAX));
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_read_back_in_order_and_the_reader_must_end_exactly() {
        let mut out = vec![7u8];
        out.extend_from_slice(&0x0102u16.to_be_bytes());
        out.extend_from_slice(&u64::MAX.to_be_bytes());
        out.extend_from_slice(&(-5i128).to_be_bytes());
        put_u16_prefixed(&mut out, b"abc");
        let mut r = Reader::new(&out);
        assert_eq!(r.u8(), Ok(7));
        assert_eq!(r.u16(), Ok(0x0102));
        assert_eq!(r.u64(), Ok(u64::MAX));
        assert_eq!(r.i128(), Ok(-5));
        assert_eq!(r.u16_prefixed(), Ok(&b"abc"[..]));
        assert_eq!(r.finish(), Ok(()));
        let mut r = Reader::new(&out);
        assert_eq!(r.u8(), Ok(7));
        assert_eq!(r.finish(), Err(Short), "bytes left over");
    }

    #[test]
    fn reading_past_the_end_is_short_not_a_panic() {
        let mut r = Reader::new(&[1, 2, 3]);
        assert_eq!(r.u32(), Err(Short));
        assert_eq!(r.bytes(4), Err(Short));
        assert_eq!(r.u16_prefixed(), Err(Short), "length 0x0102 over 1 byte");
        let mut r = Reader::new(&[]);
        assert_eq!(r.u8(), Err(Short));
        assert_eq!(r.i64(), Err(Short));
    }
}
