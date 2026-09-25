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

//! The push receiver's configuration: values read once at startup, parsed into types that
//! can't hold an invalid setting.

use subtle::ConstantTimeEq;

/// The configured `HUB_PUSH_TOKEN`. Never empty: an unset or empty value disables push
/// authentication, which is `Option<PushToken>::None`. No `Debug`, so the secret can't
/// reach a log line.
#[derive(Clone)]
pub(super) struct PushToken(String);

impl PushToken {
    pub(super) fn new(value: String) -> Option<Self> {
        (!value.is_empty()).then_some(Self(value))
    }

    /// Unset, empty and non-unicode values all disable push auth, as they did before
    /// RFC 0003.
    pub(super) fn from_env(value: Result<String, std::env::VarError>) -> Option<Self> {
        value.ok().and_then(Self::new)
    }

    /// Constant-time, so how long a rejection takes doesn't leak the secret's content.
    pub(super) fn accepts(&self, presented: &str) -> bool {
        presented.as_bytes().ct_eq(self.0.as_bytes()).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_token_from_env_is_enabled_only_by_a_non_empty_unicode_value() {
        use std::env::VarError;
        let cases = [
            (
                "set value is the push token",
                Ok("s3cret".to_string()),
                Some("s3cret"),
            ),
            (
                "non-ascii unicode value is the push token",
                Ok("pässwörd".to_string()),
                Some("pässwörd"),
            ),
            // Taken verbatim, as before RFC 0003: surrounding whitespace is part of the
            // secret, not trimmed.
            (
                "value with surrounding whitespace is the push token verbatim",
                Ok(" s3cret ".to_string()),
                Some(" s3cret "),
            ),
            ("empty value disables push auth", Ok(String::new()), None),
            (
                "unset variable disables push auth",
                Err(VarError::NotPresent),
                None,
            ),
            // Pins the behaviour kept from before RFC 0003 (fail open), recorded there
            // as an open question rather than endorsed.
            (
                "non-unicode value disables push auth",
                Err(VarError::NotUnicode("s3cret".into())),
                None,
            ),
        ];
        for (name, value, expected) in cases {
            let token = PushToken::from_env(value).map(|token| token.0);
            assert_eq!(token.as_deref(), expected, "{name}");
        }
    }
}
