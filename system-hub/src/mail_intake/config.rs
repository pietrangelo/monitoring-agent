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

//! The mail intake's configuration (RFC 0017 §6, §8): `HUB_MAIL_DIR` and `HUB_MAIL_KEY`,
//! parsed once at startup. Both or neither: there is no open mode.

use std::env::VarError;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

use super::seal::{KeyError, MailMasterKey};

pub const DIR_VARIABLE: &str = "HUB_MAIL_DIR";
pub const KEY_VARIABLE: &str = "HUB_MAIL_KEY";

/// Whether the hub reads mail, and from where, with which master key.
pub enum MailIntakeConfig {
    Off,
    On {
        maildir: PathBuf,
        key: MailMasterKey,
    },
}

/// Why the mail configuration refused startup. Never carries a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailIntakeConfigError {
    /// One of the two variables is set without the other.
    HalfConfigured,
    /// `HUB_MAIL_KEY` isn't UTF-8.
    KeyNotUnicode,
    Key(KeyError),
}

impl fmt::Display for MailIntakeConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HalfConfigured => {
                write!(f, "{DIR_VARIABLE} and {KEY_VARIABLE} must be set together")
            }
            Self::KeyNotUnicode => write!(f, "{KEY_VARIABLE} isn't valid UTF-8"),
            Self::Key(KeyError::NotBase64) => write!(f, "{KEY_VARIABLE} isn't base64"),
            Self::Key(KeyError::WrongLength) => write!(f, "{KEY_VARIABLE} isn't 32 bytes"),
        }
    }
}

impl MailIntakeConfig {
    /// From what `var_os(HUB_MAIL_DIR)` and `var(HUB_MAIL_KEY)` return; empty means unset.
    pub fn from_env(
        dir: Option<OsString>,
        key: Result<String, VarError>,
    ) -> Result<Self, MailIntakeConfigError> {
        let dir = dir.filter(|dir| !dir.is_empty());
        let key = match key {
            Ok(key) if key.is_empty() => None,
            Ok(key) => Some(key),
            Err(VarError::NotPresent) => None,
            Err(VarError::NotUnicode(_)) => return Err(MailIntakeConfigError::KeyNotUnicode),
        };
        match (dir, key) {
            (None, None) => Ok(Self::Off),
            (Some(dir), Some(key)) => Ok(Self::On {
                maildir: PathBuf::from(dir),
                key: MailMasterKey::from_base64(&key).map_err(MailIntakeConfigError::Key)?,
            }),
            (Some(_), None) | (None, Some(_)) => Err(MailIntakeConfigError::HalfConfigured),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn key() -> String {
        base64::engine::general_purpose::STANDARD.encode([1u8; 32])
    }

    /// RFC 0017 §6: both or neither; a key that isn't 32 bytes of base64, or isn't UTF-8,
    /// refuses startup.
    #[test]
    fn the_mail_intake_is_on_only_with_both_variables() {
        use MailIntakeConfigError::*;
        let dir = || Some(OsString::from("/var/mail/hub"));
        let not_unicode = Err(VarError::NotUnicode(OsString::from("x")));
        let cases: Vec<(
            &str,
            Option<OsString>,
            Result<String, VarError>,
            Result<bool, MailIntakeConfigError>,
        )> = vec![
            ("neither", None, Err(VarError::NotPresent), Ok(false)),
            (
                "both empty",
                Some(OsString::new()),
                Ok(String::new()),
                Ok(false),
            ),
            ("both", dir(), Ok(key()), Ok(true)),
            (
                "a dir without a key",
                dir(),
                Err(VarError::NotPresent),
                Err(HalfConfigured),
            ),
            (
                "a dir with an empty key",
                dir(),
                Ok(String::new()),
                Err(HalfConfigured),
            ),
            ("a key without a dir", None, Ok(key()), Err(HalfConfigured)),
            (
                "a short key",
                dir(),
                Ok("AAAA".into()),
                Err(Key(KeyError::WrongLength)),
            ),
            (
                "a key that isn't base64",
                dir(),
                Ok("%%%".into()),
                Err(Key(KeyError::NotBase64)),
            ),
            (
                "a key that isn't UTF-8",
                dir(),
                not_unicode,
                Err(KeyNotUnicode),
            ),
        ];
        for (name, dir, key, expected) in cases {
            let got = MailIntakeConfig::from_env(dir, key).map(|config| match config {
                MailIntakeConfig::Off => false,
                MailIntakeConfig::On { maildir, .. } => {
                    assert_eq!(maildir, PathBuf::from("/var/mail/hub"), "case {name}");
                    true
                }
            });
            assert_eq!(got, expected, "case {name}");
        }
    }
}
