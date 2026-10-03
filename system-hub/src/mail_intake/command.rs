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

//! The hub's command line (RFC 0017 §3): no argument serves; `mail-key <system id>` prints
//! that system's mail key, derived from `HUB_MAIL_KEY`, and touches no database.

use std::env::VarError;
use std::ffi::OsString;
use std::fmt;

use super::seal::{KeyError, MailMasterKey};
use crate::models::{SystemId, SystemIdError};

/// What the hub was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Serve,
    MailKey(SystemId),
}

/// Why the command line, or the key the subcommand needs, was refused. Never carries a key.
#[derive(Debug, PartialEq, Eq)]
pub enum CommandError {
    Usage,
    InvalidSystemId(SystemIdError),
    KeyMissing,
    Key(KeyError),
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("usage: system-hub [mail-key <system id>]"),
            Self::InvalidSystemId(rule) => write!(f, "invalid system id: {rule:?}"),
            Self::KeyMissing => f.write_str("HUB_MAIL_KEY must be set to derive a mail key"),
            Self::Key(KeyError::NotBase64) => f.write_str("HUB_MAIL_KEY isn't base64"),
            Self::Key(KeyError::WrongLength) => f.write_str("HUB_MAIL_KEY isn't 32 bytes"),
        }
    }
}

impl CommandError {
    /// `EX_USAGE` (64) for the command line, `EX_CONFIG` (78) for the key.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Usage | Self::InvalidSystemId(_) => 64,
            Self::KeyMissing | Self::Key(_) => 78,
        }
    }
}

/// The command in the arguments after the program name.
pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Command, CommandError> {
    let args: Vec<OsString> = args.into_iter().collect();
    match args.as_slice() {
        [] => Ok(Command::Serve),
        [command, id] if command == "mail-key" => {
            let id = id.to_str().ok_or(CommandError::Usage)?;
            SystemId::try_from(id.to_owned())
                .map(Command::MailKey)
                .map_err(CommandError::InvalidSystemId)
        }
        _ => Err(CommandError::Usage),
    }
}

/// The base64 mail key of `system_id`, from what `var(HUB_MAIL_KEY)` returned.
pub fn mail_key(
    system_id: &SystemId,
    key: Result<String, VarError>,
) -> Result<String, CommandError> {
    let key = key
        .ok()
        .filter(|key| !key.is_empty())
        .ok_or(CommandError::KeyMissing)?;
    let master = MailMasterKey::from_base64(&key).map_err(CommandError::Key)?;
    Ok(master.derive(system_id).to_base64())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn id(id: &str) -> SystemId {
        SystemId::try_from(id.to_owned()).unwrap()
    }

    /// RFC 0017 §3: no argument serves, `mail-key <id>` asks for a key; anything else is a
    /// usage error.
    #[test]
    fn the_command_line_serves_or_asks_for_a_mail_key() {
        let cases = [
            ("no argument", args(&[]), Ok(Command::Serve)),
            (
                "mail-key web-01",
                args(&["mail-key", "web-01"]),
                Ok(Command::MailKey(id("web-01"))),
            ),
            (
                "mail-key without an id",
                args(&["mail-key"]),
                Err(CommandError::Usage),
            ),
            (
                "mail-key with two ids",
                args(&["mail-key", "a", "b"]),
                Err(CommandError::Usage),
            ),
            (
                "mail-key ..",
                args(&["mail-key", ".."]),
                Err(CommandError::InvalidSystemId(SystemIdError::DotSegment)),
            ),
            (
                "another command",
                args(&["serve"]),
                Err(CommandError::Usage),
            ),
        ];
        for (name, args, expected) in cases {
            assert_eq!(parse_args(args), expected, "case {name}");
        }
    }

    /// RFC 0017 §3: the key is the HKDF vector for the id; a missing or bad master key is
    /// refused, never echoed.
    #[test]
    fn the_mail_key_is_derived_from_the_master_key() {
        use base64::Engine;
        let master = base64::engine::general_purpose::STANDARD.encode([1u8; 32]);
        let cases = [
            (
                "the vector",
                Ok(master.clone()),
                Ok("K3+RuHlQ1b7woYSIjUBPpdWhGwNhkfOkRjXt3LT2ufM=".to_string()),
            ),
            (
                "unset",
                Err(VarError::NotPresent),
                Err(CommandError::KeyMissing),
            ),
            ("empty", Ok(String::new()), Err(CommandError::KeyMissing)),
            (
                "short",
                Ok("AAAA".into()),
                Err(CommandError::Key(KeyError::WrongLength)),
            ),
        ];
        for (name, key, expected) in cases {
            assert_eq!(mail_key(&id("web-01"), key), expected, "case {name}");
        }
    }
}
