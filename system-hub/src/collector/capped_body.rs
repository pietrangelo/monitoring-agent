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

//! Reading a poll answer's body under a cap, so an agent can't make the hub buffer more than
//! each answer needs (RFC 0009 §8, RFC 0007 §1).

/// Why a capped body read failed.
#[derive(Debug)]
pub enum CappedBodyError {
    /// The body passed the cap, by its `Content-Length` or by the bytes read.
    TooLarge,
    Transport(reqwest::Error),
}

/// Reads a body chunk by chunk, refusing it once it passes `cap` bytes, whatever
/// `Content-Length` claims.
pub async fn read_capped(
    resp: &mut reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, CappedBodyError> {
    if resp.content_length().is_some_and(|len| len > cap as u64) {
        return Err(CappedBodyError::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(CappedBodyError::Transport)? {
        if body.len() + chunk.len() > cap {
            return Err(CappedBodyError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
