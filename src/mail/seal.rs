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

//! Sealing a mail report (RFC 0017 §3, §4): XChaCha20-Poly1305 under the system's mail key,
//! with the header authenticated as associated data, then base64 armour. Pure: the nonce is
//! the caller's, so tests are deterministic.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

use crate::push::identity::AgentId;

/// The sealed report's first bytes.
pub const MAGIC: &[u8; 4] = b"SAMR";
/// The sealed report's version.
pub const VERSION: u8 = 1;
/// The armour's first and last lines.
pub const ARMOUR_BEGIN: &str = "-----BEGIN SYSTEM-AGENT REPORT-----";
pub const ARMOUR_END: &str = "-----END SYSTEM-AGENT REPORT-----";
/// The armour's longest base64 line.
const ARMOUR_COLUMNS: usize = 76;

/// This system's mail key (`MAIL_KEY`): 32 bytes. No `Debug`, no `Display`: it is a secret.
pub struct MailKey([u8; 32]);

/// Why `MAIL_KEY` was refused. Never carries the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailKeyError {
    NotBase64,
    WrongLength,
}

impl MailKey {
    /// A key from its base64 form, surrounding ASCII whitespace ignored.
    pub fn from_base64(value: &str) -> Result<Self, MailKeyError> {
        let bytes = STANDARD
            .decode(value.trim_ascii())
            .map_err(|_| MailKeyError::NotBase64)?;
        let key = <[u8; 32]>::try_from(bytes).map_err(|_| MailKeyError::WrongLength)?;
        Ok(Self(key))
    }
}

/// A 24-byte XChaCha20 nonce, drawn from the OS RNG by the adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nonce(pub [u8; 24]);

/// Why a report couldn't be sealed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealError {
    /// An id over 255 bytes, which `AgentId` never holds.
    IdTooLong,
    /// The cipher refused the message (beyond its limit, about 256 GiB).
    Encrypt,
}

/// The header the hub reads before it opens a report: magic, version, the id's length and the
/// id. It is the AEAD's associated data.
fn header(id: &AgentId) -> Result<Vec<u8>, SealError> {
    let id = id.as_str().as_bytes();
    let id_len = u8::try_from(id.len()).map_err(|_| SealError::IdTooLong)?;
    let mut header = Vec::with_capacity(MAGIC.len() + 2 + id.len());
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&[VERSION, id_len]);
    header.extend_from_slice(id);
    Ok(header)
}

/// Seals `report` (the MessagePack report) for `id` under `key`.
pub fn seal(
    id: &AgentId,
    report: &[u8],
    key: &MailKey,
    nonce: &Nonce,
) -> Result<Vec<u8>, SealError> {
    let mut sealed = header(id)?;
    let cipher = XChaCha20Poly1305::new(&key.0.into());
    let payload = Payload {
        msg: report,
        aad: &sealed,
    };
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce.0), payload)
        .map_err(|_| SealError::Encrypt)?;
    sealed.extend_from_slice(&nonce.0);
    sealed.extend_from_slice(&ciphertext);
    Ok(sealed)
}

/// The armoured text a message body carries.
pub fn armour(sealed: &[u8]) -> String {
    let encoded = STANDARD.encode(sealed);
    // Base64 is ASCII, so every chunk is a whole line of text.
    let lines = encoded
        .as_bytes()
        .chunks(ARMOUR_COLUMNS)
        .map(|line| String::from_utf8_lossy(line));
    std::iter::once(ARMOUR_BEGIN.into())
        .chain(lines)
        .chain(std::iter::once(ARMOUR_END.into()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7; 32];
    const NONCE: Nonce = Nonce([9; 24]);

    fn key() -> MailKey {
        MailKey(KEY)
    }

    fn id(id: &str) -> AgentId {
        AgentId::try_from(id).unwrap()
    }

    fn open(sealed: &[u8], aad_len: usize, key: [u8; 32]) -> Option<Vec<u8>> {
        let cipher = XChaCha20Poly1305::new_from_slice(&key).unwrap();
        let (aad, rest) = sealed.split_at(aad_len);
        let (nonce, ciphertext) = rest.split_at(24);
        let payload = Payload {
            msg: ciphertext,
            aad,
        };
        cipher.decrypt(XNonce::from_slice(nonce), payload).ok()
    }

    /// RFC 0017 §3: `MAIL_KEY` is 32 bytes of base64; nothing else is a key.
    #[test]
    fn a_mail_key_is_32_bytes_of_base64() {
        let b64 = |bytes: &[u8]| STANDARD.encode(bytes);
        let padded = format!("  {}\n", b64(&[1; 32]));
        let cases = [
            ("32 bytes", b64(&[1; 32]), Ok(())),
            ("surrounded by whitespace", padded, Ok(())),
            ("31 bytes", b64(&[1; 31]), Err(MailKeyError::WrongLength)),
            ("33 bytes", b64(&[1; 33]), Err(MailKeyError::WrongLength)),
            ("empty", String::new(), Err(MailKeyError::WrongLength)),
            (
                "not base64",
                "not base64!".to_string(),
                Err(MailKeyError::NotBase64),
            ),
        ];
        for (name, value, expected) in cases {
            let got = MailKey::from_base64(&value).map(|key| assert_eq!(key.0, [1; 32]));
            assert_eq!(got, expected, "case {name}");
        }
    }

    /// RFC 0017 §3: the layout, `magic | version | id_len | id | nonce | ciphertext`, and the
    /// header is the associated data: a report opens with the key, and changing any header
    /// byte, or using another key, makes it fail.
    #[test]
    fn a_sealed_report_carries_its_header_and_opens_only_unaltered_under_its_key() {
        let report = b"a report";
        let sealed = seal(&id("web-01"), report, &key(), &NONCE).unwrap();

        let mut expected_head = b"SAMR\x01\x06web-01".to_vec();
        expected_head.extend_from_slice(&NONCE.0);
        assert!(sealed.starts_with(&expected_head), "{sealed:?}");
        assert_eq!(sealed.len(), expected_head.len() + report.len() + 16);
        let aad_len = expected_head.len() - 24;
        assert_eq!(open(&sealed, aad_len, KEY).as_deref(), Some(&report[..]));
        for at in 0..aad_len {
            let mut altered = sealed.clone();
            altered[at] ^= 1;
            assert_eq!(
                open(&altered, aad_len, KEY),
                None,
                "header byte {at} altered"
            );
        }
        assert_eq!(open(&sealed, aad_len, [8; 32]), None, "another key");
    }

    /// RFC 0017 §4: the armour is the sealed bytes in base64, at most 76 columns, between the
    /// two armour lines.
    #[test]
    fn the_armour_wraps_base64_between_its_lines() {
        let sealed: Vec<u8> = (0..=255).collect();

        let text = armour(&sealed);

        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.first(), Some(&ARMOUR_BEGIN));
        assert_eq!(lines.last(), Some(&ARMOUR_END));
        let body = &lines[1..lines.len() - 1];
        assert!(body.iter().all(|line| line.len() <= 76), "{body:?}");
        assert!(body[..body.len() - 1].iter().all(|line| line.len() == 76));
        assert_eq!(STANDARD.decode(body.concat()).unwrap(), sealed);
    }

    /// RFC 0017 §3: the v1 golden report, sealed for `web-01` under the key `system-hub
    /// mail-key web-01` derives from a master key of 32 bytes of 1 (an independent HKDF
    /// vector), with a nonce of 24 bytes of 9. The hub's tests open these same bytes.
    #[test]
    fn the_golden_report_seals_as_the_sealed_golden() {
        const SEALED: &[u8] = include_bytes!("../../testdata/mail-report-v1.sealed");
        const REPORT: &[u8] = include_bytes!("../../testdata/mail-report-v1.msgpack");
        let key = MailKey::from_base64("K3+RuHlQ1b7woYSIjUBPpdWhGwNhkfOkRjXt3LT2ufM=").unwrap();

        let sealed = seal(&id("web-01"), REPORT, &key, &NONCE).unwrap();

        assert_eq!(sealed, SEALED);
    }
}
