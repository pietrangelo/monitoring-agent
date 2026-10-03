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

//! Opening a sealed mail report (RFC 0017 §3, §4): the armour, the header, the per-system key
//! derived from the hub's mail master key, and the AEAD. Pure: bytes and keys in, the system
//! id and the report's bytes out.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use sha2::Sha256;

use crate::models::{SystemId, SystemIdError};

const MAGIC: &[u8; 4] = b"SAMR";
const VERSION: u8 = 1;
const ARMOUR_BEGIN: &str = "-----BEGIN SYSTEM-AGENT REPORT-----";
const ARMOUR_END: &str = "-----END SYSTEM-AGENT REPORT-----";
const NONCE_BYTES: usize = 24;
/// The HKDF salt of the v1 mail key derivation.
const KEY_SALT: &[u8] = b"system-agent mail key v1";
/// The largest sealed report the hub decodes.
pub const MAX_SEALED_BYTES: usize = 512 * 1024;

/// The hub's mail master key (`HUB_MAIL_KEY`): 32 bytes. No `Debug`: it is a secret.
pub struct MailMasterKey([u8; 32]);

/// One system's mail key, derived from the master key. No `Debug`: it is a secret.
pub struct MailKey([u8; 32]);

/// Why a key's base64 form was refused. Never carries the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyError {
    NotBase64,
    WrongLength,
}

/// Why a mail report wasn't opened. No variant carries bytes, an unauthenticated id or a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenRefusal {
    /// No armour block, or base64 that doesn't decode.
    BadArmor,
    /// The armour decodes to more than `MAX_SEALED_BYTES`.
    TooLarge,
    /// The wrong magic, or a header that overruns the bytes.
    BadHeader,
    UnknownVersion(u8),
    InvalidSystemId(SystemIdError),
    /// The tag doesn't verify under the id's key.
    NotAuthentic,
}

impl MailMasterKey {
    /// The master key from its base64 form, surrounding ASCII whitespace ignored.
    pub fn from_base64(value: &str) -> Result<Self, KeyError> {
        let bytes = STANDARD
            .decode(value.trim_ascii())
            .map_err(|_| KeyError::NotBase64)?;
        <[u8; 32]>::try_from(bytes)
            .map(Self)
            .map_err(|_| KeyError::WrongLength)
    }

    /// `HKDF-SHA256(ikm = master, salt = "system-agent mail key v1", info = system id)`.
    pub fn derive(&self, system_id: &SystemId) -> MailKey {
        let mut key = [0; 32];
        // 32 bytes is far below HKDF-SHA256's 8160-byte limit, so expansion can't fail.
        let _ = Hkdf::<Sha256>::new(Some(KEY_SALT), &self.0)
            .expand(system_id.as_str().as_bytes(), &mut key);
        MailKey(key)
    }
}

impl MailKey {
    /// The base64 form an agent's `MAIL_KEY` takes.
    pub fn to_base64(&self) -> String {
        STANDARD.encode(self.0)
    }
}

/// The sealed bytes in the first armour block of `text`; anything around it is ignored.
pub fn dearmour(text: &str) -> Result<Vec<u8>, OpenRefusal> {
    let (_, after_begin) = text.split_once(ARMOUR_BEGIN).ok_or(OpenRefusal::BadArmor)?;
    let (body, _) = after_begin
        .split_once(ARMOUR_END)
        .ok_or(OpenRefusal::BadArmor)?;
    let encoded: String = body.split_ascii_whitespace().collect();
    let sealed = STANDARD
        .decode(encoded)
        .map_err(|_| OpenRefusal::BadArmor)?;
    if sealed.len() > MAX_SEALED_BYTES {
        return Err(OpenRefusal::TooLarge);
    }
    Ok(sealed)
}

/// The parts of a sealed report.
struct Sealed<'a> {
    /// Everything before the nonce: the associated data.
    header: &'a [u8],
    id: SystemId,
    nonce: &'a [u8],
    ciphertext: &'a [u8],
}

/// Splits a sealed report, parsing its id, before any key is derived.
fn parse(sealed: &[u8]) -> Result<Sealed<'_>, OpenRefusal> {
    let rest = sealed.strip_prefix(MAGIC).ok_or(OpenRefusal::BadHeader)?;
    let (&version, rest) = rest.split_first().ok_or(OpenRefusal::BadHeader)?;
    if version != VERSION {
        return Err(OpenRefusal::UnknownVersion(version));
    }
    let (&id_len, rest) = rest.split_first().ok_or(OpenRefusal::BadHeader)?;
    let id_len = usize::from(id_len);
    let id = rest.get(..id_len).ok_or(OpenRefusal::BadHeader)?;
    let rest = &rest[id_len..];
    if rest.len() < NONCE_BYTES {
        return Err(OpenRefusal::BadHeader);
    }
    let (nonce, ciphertext) = rest.split_at(NONCE_BYTES);
    let id = std::str::from_utf8(id).map_err(|_| OpenRefusal::BadHeader)?;
    let id = SystemId::try_from(id.to_owned()).map_err(OpenRefusal::InvalidSystemId)?;
    Ok(Sealed {
        header: &sealed[..MAGIC.len() + 2 + id_len],
        id,
        nonce,
        ciphertext,
    })
}

/// Opens a sealed report: its system id, parsed before any key is derived, and the report's
/// bytes, authenticated with the header.
pub fn open(sealed: &[u8], master: &MailMasterKey) -> Result<(SystemId, Vec<u8>), OpenRefusal> {
    let Sealed {
        header,
        id,
        nonce,
        ciphertext,
    } = parse(sealed)?;
    let cipher = XChaCha20Poly1305::new(&master.derive(&id).0.into());
    let payload = Payload {
        msg: ciphertext,
        aad: header,
    };
    let report = cipher
        .decrypt(XNonce::from_slice(nonce), payload)
        .map_err(|_| OpenRefusal::NotAuthentic)?;
    Ok((id, report))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const MASTER: [u8; 32] = [1; 32];

    fn master() -> MailMasterKey {
        MailMasterKey(MASTER)
    }

    fn id(id: &str) -> SystemId {
        SystemId::try_from(id.to_owned()).unwrap()
    }

    /// Seals as an agent does, with the key `master` derives for `system_id`: the hub's tests
    /// can't use the agent's crate.
    pub(crate) fn seal_for_test(system_id: &[u8], report: &[u8], key: &[u8; 32]) -> Vec<u8> {
        let mut sealed = MAGIC.to_vec();
        sealed.push(VERSION);
        sealed.push(u8::try_from(system_id.len()).unwrap());
        sealed.extend_from_slice(system_id);
        let cipher = XChaCha20Poly1305::new_from_slice(key).unwrap();
        let nonce = [3; NONCE_BYTES];
        let payload = Payload {
            msg: report,
            aad: &sealed,
        };
        let ciphertext = cipher.encrypt(XNonce::from_slice(&nonce), payload).unwrap();
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&ciphertext);
        sealed
    }

    fn key_for(system_id: &str) -> [u8; 32] {
        master().derive(&id(system_id)).0
    }

    /// RFC 0017 §3: the derivation, against a vector computed independently (Python's `hmac`),
    /// and one key per id.
    #[test]
    fn each_system_id_gets_its_own_derived_key() {
        assert_eq!(
            master().derive(&id("web-01")).to_base64(),
            "K3+RuHlQ1b7woYSIjUBPpdWhGwNhkfOkRjXt3LT2ufM="
        );
        assert_ne!(key_for("web-01"), key_for("web-02"));
        assert_ne!(key_for("web-01"), MASTER);
    }

    /// RFC 0017 §6: `HUB_MAIL_KEY` is 32 bytes of base64; nothing else is a master key.
    #[test]
    fn a_master_key_is_32_bytes_of_base64() {
        let b64 = |bytes: &[u8]| STANDARD.encode(bytes);
        let cases = [
            ("32 bytes", b64(&[1; 32]), Ok(())),
            ("with a newline", format!("{}\n", b64(&[1; 32])), Ok(())),
            ("16 bytes", b64(&[1; 16]), Err(KeyError::WrongLength)),
            ("empty", String::new(), Err(KeyError::WrongLength)),
            ("not base64", "%%%".to_string(), Err(KeyError::NotBase64)),
        ];
        for (name, value, expected) in cases {
            let got = MailMasterKey::from_base64(&value).map(|key| assert_eq!(key.0, [1; 32]));
            assert_eq!(got, expected, "case {name}");
        }
    }

    /// RFC 0017 §3: a report opens under the key derived for its id, and every other sealed
    /// input is refused with its own reason.
    #[test]
    fn a_report_opens_only_whole_and_under_its_own_ids_key() {
        let good = seal_for_test(b"web-01", b"report", &key_for("web-01"));
        let header_len = 4 + 2 + 6;
        let altered = |at: usize| {
            let mut bytes = good.clone();
            bytes[at] ^= 1;
            bytes
        };
        let mut version_2 = good.clone();
        version_2[4] = 2;
        let mut overrun = good[..6].to_vec();
        overrun[5] = 200;
        let cases: [(&str, Vec<u8>, Result<&str, OpenRefusal>); 10] = [
            ("intact", good.clone(), Ok("web-01")),
            ("wrong magic", altered(0), Err(OpenRefusal::BadHeader)),
            ("version 2", version_2, Err(OpenRefusal::UnknownVersion(2))),
            (
                "an id_len that overruns",
                overrun,
                Err(OpenRefusal::BadHeader),
            ),
            ("empty", Vec::new(), Err(OpenRefusal::BadHeader)),
            (
                "a dot-segment id",
                seal_for_test(b"..", b"report", &[0; 32]),
                Err(OpenRefusal::InvalidSystemId(SystemIdError::DotSegment)),
            ),
            (
                "an id byte altered",
                altered(header_len - 1),
                Err(OpenRefusal::NotAuthentic),
            ),
            (
                "a nonce byte altered",
                altered(header_len),
                Err(OpenRefusal::NotAuthentic),
            ),
            (
                "a ciphertext byte altered",
                altered(good.len() - 1),
                Err(OpenRefusal::NotAuthentic),
            ),
            (
                "another system's key",
                seal_for_test(b"web-01", b"report", &key_for("web-02")),
                Err(OpenRefusal::NotAuthentic),
            ),
        ];
        for (name, sealed, expected) in cases {
            let got = open(&sealed, &master());
            let got = got.as_ref().map(|(id, report)| {
                assert_eq!(report, b"report", "case {name}");
                id.as_str()
            });
            assert_eq!(got, expected.as_ref().map(|id| *id), "case {name}");
        }
    }

    /// RFC 0017 §4: the first armour block is found whatever a relay put around it or did to
    /// its line ends; no block, bad base64 and an oversize block are refused.
    #[test]
    fn the_first_armour_block_is_read_whatever_surrounds_it() {
        let block = |bytes: &[u8]| {
            let encoded = STANDARD.encode(bytes);
            let lines: Vec<&str> = encoded
                .as_bytes()
                .chunks(76)
                .map(|line| std::str::from_utf8(line).unwrap())
                .collect();
            format!("{ARMOUR_BEGIN}\n{}\n{ARMOUR_END}", lines.join("\n"))
        };
        let sealed = vec![42u8; 200];
        let cases = [
            ("bare", block(&sealed), Ok(sealed.clone())),
            (
                "a footer and a greeting",
                format!(
                    "Hello\n\n{}\n\n-- \nThis message was scanned.\n",
                    block(&sealed)
                ),
                Ok(sealed.clone()),
            ),
            (
                "CRLF line ends",
                block(&sealed).replace('\n', "\r\n"),
                Ok(sealed.clone()),
            ),
            (
                "two blocks: the first wins",
                format!("{}\n{}", block(&sealed), block(b"second")),
                Ok(sealed.clone()),
            ),
            (
                "no block",
                "just text".to_string(),
                Err(OpenRefusal::BadArmor),
            ),
            (
                "no end line",
                format!("{ARMOUR_BEGIN}\nKioq\n"),
                Err(OpenRefusal::BadArmor),
            ),
            (
                "not base64",
                format!("{ARMOUR_BEGIN}\n%%%%\n{ARMOUR_END}"),
                Err(OpenRefusal::BadArmor),
            ),
            (
                "over the limit",
                block(&vec![0u8; MAX_SEALED_BYTES + 1]),
                Err(OpenRefusal::TooLarge),
            ),
        ];
        for (name, text, expected) in cases {
            assert_eq!(dearmour(&text), expected, "case {name}");
        }
    }
}
