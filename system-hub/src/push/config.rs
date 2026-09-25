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

use axum::extract::ws::WebSocketUpgrade;
use std::env::VarError;
use std::time::Duration;
use subtle::ConstantTimeEq;

/// How long a client has, after the upgrade, to send its auth message.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a handshake answer may take to send. The answer is the first write, about 60
/// bytes into an empty send buffer, so this is defence in depth that no test can trigger.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an authenticated connection may go without any message. Every shipped agent
/// pings every 30 s, so this is three missed pings.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// How long an authenticated connection that sent an oversize message is held, unread,
/// before it is dropped. Shipped agents reconnect with no backoff after a drop, so this
/// spaces their reconnects.
pub const OVERSIZE_LINGER: Duration = Duration::from_secs(30);

/// The configured `HUB_PUSH_TOKEN`. Never empty. No `Debug`, so the secret can't reach a log
/// line.
#[derive(Clone)]
pub struct PushToken(String);

impl PushToken {
    pub(super) fn new(value: String) -> Option<Self> {
        (!value.is_empty()).then_some(Self(value))
    }

    /// Constant-time, so how long a rejection takes doesn't leak the secret's content.
    pub(super) fn accepts(&self, presented: &str) -> bool {
        presented.as_bytes().ct_eq(self.0.as_bytes()).into()
    }
}

/// Who may push: anyone, or only a client presenting the configured token.
#[derive(Clone)]
pub enum PushAuth {
    /// `HUB_PUSH_TOKEN` is unset or empty.
    Open,
    Required(PushToken),
}

/// Why `HUB_PUSH_TOKEN` can't be used. Never carries the value.
#[derive(Debug, PartialEq, Eq)]
pub enum PushAuthError {
    /// Set, but not valid UTF-8, so no agent could ever present it.
    NotUnicode,
}

impl std::fmt::Display for PushAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotUnicode => write!(f, "HUB_PUSH_TOKEN is set but is not valid UTF-8"),
        }
    }
}

impl PushAuth {
    /// Parses `std::env::var("HUB_PUSH_TOKEN")`. Unset or empty means open, as compose files
    /// write `VAR=` to leave a variable unset.
    pub fn from_env(value: Result<String, VarError>) -> Result<Self, PushAuthError> {
        match value {
            Ok(token) => Ok(PushToken::new(token).map_or(Self::Open, Self::Required)),
            Err(VarError::NotPresent) => Ok(Self::Open),
            Err(VarError::NotUnicode(_)) => Err(PushAuthError::NotUnicode),
        }
    }

    /// Whether a client presenting `presented` may push.
    pub(super) fn admits(&self, presented: &str) -> bool {
        match self {
            Self::Open => true,
            Self::Required(token) => token.accepts(presented),
        }
    }

    #[cfg(test)]
    pub(super) fn token(&self) -> Option<&PushToken> {
        match self {
            Self::Open => None,
            Self::Required(token) => Some(token),
        }
    }
}

/// The WebSocket limits of one push connection. tungstenite panics at upgrade unless
/// `max_write_buffer > write_buffer`, so construction checks it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PushSocketLimits {
    max_message: usize,
    write_buffer: usize,
    max_write_buffer: usize,
}

/// Why a set of socket limits can't be used.
#[derive(Debug, PartialEq, Eq)]
pub enum PushSocketLimitsError {
    /// A zero message size would refuse every message.
    ZeroMessageSize,
    /// tungstenite panics at upgrade unless the write buffer is below its cap.
    WriteBufferNotBelowMax,
}

impl PushSocketLimits {
    pub const fn new(
        max_message: usize,
        write_buffer: usize,
        max_write_buffer: usize,
    ) -> Result<Self, PushSocketLimitsError> {
        if max_message == 0 {
            return Err(PushSocketLimitsError::ZeroMessageSize);
        }
        if write_buffer >= max_write_buffer {
            return Err(PushSocketLimitsError::WriteBufferNotBelowMax);
        }
        Ok(Self {
            max_message,
            write_buffer,
            max_write_buffer,
        })
    }

    pub const PRODUCTION: Self = match Self::new(512 * 1024, 8 * 1024, 64 * 1024) {
        Ok(limits) => limits,
        Err(_) => panic!("invalid PushSocketLimits::PRODUCTION"),
    };

    /// Sets all four limits on the upgrade. axum offers only scalar setters and keeps the
    /// result private, so this is the one place they are called.
    pub fn apply(&self, upgrade: WebSocketUpgrade) -> WebSocketUpgrade {
        upgrade
            .max_message_size(self.max_message)
            .max_frame_size(self.max_message)
            .write_buffer_size(self.write_buffer)
            .max_write_buffer_size(self.max_write_buffer)
    }
}

// Evaluates PRODUCTION at `cargo check` time, so an invalid value fails there, not only at
// codegen.
const _: PushSocketLimits = PushSocketLimits::PRODUCTION;

/// Everything a push router needs, so tests can inject short deadlines and small limits
/// instead of touching the environment or waiting 10 s.
#[derive(Clone)]
pub struct PushConfig {
    pub auth: PushAuth,
    pub limits: PushSocketLimits,
    pub handshake_timeout: Duration,
    pub idle_timeout: Duration,
    pub oversize_linger: Duration,
}

impl PushConfig {
    pub fn production(auth: PushAuth) -> Self {
        Self {
            auth,
            limits: PushSocketLimits::PRODUCTION,
            handshake_timeout: HANDSHAKE_TIMEOUT,
            idle_timeout: IDLE_TIMEOUT,
            oversize_linger: OVERSIZE_LINGER,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A value `std::env::var` would report as `NotUnicode`.
    fn non_unicode() -> std::ffi::OsString {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            std::ffi::OsString::from_vec(b"s3cret-\xff".to_vec())
        }
        #[cfg(not(unix))]
        {
            std::ffi::OsString::from("s3cret")
        }
    }

    /// The configured token, for comparing outcomes; `PushToken` has no `Debug`.
    fn token_of(auth: &PushAuth) -> Option<&str> {
        auth.token().map(|token| token.0.as_str())
    }

    #[test]
    fn push_auth_is_required_by_a_token_open_when_unset_and_refused_when_not_unicode() {
        let cases = [
            (
                "set value is the push token",
                Ok("s3cret".to_string()),
                Ok(Some("s3cret")),
            ),
            (
                "non-ascii unicode value is the push token",
                Ok("pässwörd".to_string()),
                Ok(Some("pässwörd")),
            ),
            // Taken verbatim, as before RFC 0003: surrounding whitespace is part of the
            // secret, not trimmed.
            (
                "value with surrounding whitespace is the push token verbatim",
                Ok(" s3cret ".to_string()),
                Ok(Some(" s3cret ")),
            ),
            ("empty value leaves push open", Ok(String::new()), Ok(None)),
            (
                "unset variable leaves push open",
                Err(VarError::NotPresent),
                Ok(None),
            ),
            (
                "non-unicode value is refused, never treated as open",
                Err(VarError::NotUnicode(non_unicode())),
                Err(PushAuthError::NotUnicode),
            ),
        ];
        for (name, value, expected) in cases {
            let auth = PushAuth::from_env(value);
            assert_eq!(
                auth.as_ref().map(token_of),
                expected.as_ref().copied(),
                "{name}"
            );
        }
    }

    #[test]
    fn push_auth_error_names_the_variable() {
        let message = PushAuthError::NotUnicode.to_string();
        assert!(message.contains("HUB_PUSH_TOKEN"), "{message}");
    }

    /// The deadlines are a published contract (README, push protocol), and the idle deadline
    /// must leave room for the pings every shipped agent sends every 30 s.
    #[test]
    fn production_push_config_uses_the_published_deadlines() {
        const AGENT_PING_INTERVAL: Duration = Duration::from_secs(30);
        let config = PushConfig::production(PushAuth::Open);
        let cases = [
            (
                "handshake deadline",
                config.handshake_timeout,
                Duration::from_secs(10),
            ),
            (
                "idle deadline",
                config.idle_timeout,
                Duration::from_secs(90),
            ),
            (
                "oversize linger",
                config.oversize_linger,
                Duration::from_secs(30),
            ),
        ];
        for (name, actual, expected) in cases {
            assert_eq!(actual, expected, "{name}");
        }
        assert!(
            config.idle_timeout >= 3 * AGENT_PING_INTERVAL,
            "three missed pings"
        );
        assert_eq!(config.limits, PushSocketLimits::PRODUCTION);
    }

    #[test]
    fn production_push_socket_limits_are_512_kib_messages_and_an_8_kib_buffer_capped_at_64() {
        const KIB: usize = 1024;
        let expected = PushSocketLimits::new(512 * KIB, 8 * KIB, 64 * KIB);
        assert_eq!(Ok(PushSocketLimits::PRODUCTION), expected);
    }

    #[test]
    fn push_socket_limits_refuse_what_would_refuse_everything_or_panic_at_upgrade() {
        use PushSocketLimitsError::*;
        const KIB: usize = 1024;
        let cases = [
            ("production values", (512 * KIB, 8 * KIB, 64 * KIB), Ok(())),
            ("smallest accepted set", (1, 0, 1), Ok(())),
            (
                "write buffer one below its cap",
                (512 * KIB, 8 * KIB - 1, 8 * KIB),
                Ok(()),
            ),
            (
                "small messages, larger buffers",
                (4 * KIB, 16 * KIB, 32 * KIB),
                Ok(()),
            ),
            (
                "zero message size",
                (0, 8 * KIB, 64 * KIB),
                Err(ZeroMessageSize),
            ),
            (
                "write buffer equal to its cap",
                (512 * KIB, 64 * KIB, 64 * KIB),
                Err(WriteBufferNotBelowMax),
            ),
            (
                "write buffer equal to a cap other than production's",
                (512 * KIB, 8 * KIB, 8 * KIB),
                Err(WriteBufferNotBelowMax),
            ),
            (
                "write buffer above its cap",
                (512 * KIB, 128 * KIB, 64 * KIB),
                Err(WriteBufferNotBelowMax),
            ),
        ];
        for (name, (max_message, write_buffer, max_write_buffer), expected) in cases {
            // The whole value, so a constructor that swaps or replaces fields goes red too.
            let expected = expected.map(|()| PushSocketLimits {
                max_message,
                write_buffer,
                max_write_buffer,
            });
            let limits = PushSocketLimits::new(max_message, write_buffer, max_write_buffer);
            assert_eq!(limits, expected, "{name}");
        }
    }
}
