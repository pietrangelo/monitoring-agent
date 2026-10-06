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

//! The names a series is made of (RFC 0010 §2): the system key, the generation and the
//! metric name. The store knows no other hub term.

use std::fmt;

/// The longest metric name: `disk:` plus the 256-byte mount points RFC 0007 allows.
pub const MAX_METRIC_NAME_BYTES: usize = 261;

/// The longest system key: a system id is at most 255 bytes.
pub const MAX_SYSTEM_KEY_BYTES: usize = 255;

/// A metric name: 1 to 261 bytes of UTF-8 with no control character.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MetricName(String);

/// A system id as the store keys it: 1 to 255 bytes.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SystemKey(Vec<u8>);

/// A registration of a system (RFC 0011): a system deleted and registered again gets a new
/// generation, so its old series never mix with the new ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Generation(u64);

/// A name the store refuses (RFC 0010 §2: refused as an invalid name at the hub edge and in
/// the store).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidName {
    Empty,
    TooLong,
    ControlCharacter,
}

impl fmt::Display for InvalidName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            InvalidName::Empty => "empty name",
            InvalidName::TooLong => "name too long",
            InvalidName::ControlCharacter => "name holds a control character",
        })
    }
}

impl std::error::Error for InvalidName {}

impl TryFrom<&str> for MetricName {
    type Error = InvalidName;

    fn try_from(name: &str) -> Result<Self, Self::Error> {
        check_length(name.len(), MAX_METRIC_NAME_BYTES)?;
        if name.chars().any(char::is_control) {
            return Err(InvalidName::ControlCharacter);
        }
        Ok(MetricName(name.to_owned()))
    }
}

impl MetricName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for MetricName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl TryFrom<&[u8]> for SystemKey {
    type Error = InvalidName;

    fn try_from(key: &[u8]) -> Result<Self, Self::Error> {
        check_length(key.len(), MAX_SYSTEM_KEY_BYTES)?;
        Ok(SystemKey(key.to_vec()))
    }
}

impl SystemKey {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SystemKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SystemKey({:?})", String::from_utf8_lossy(&self.0))
    }
}

fn check_length(len: usize, max: usize) -> Result<(), InvalidName> {
    match len {
        0 => Err(InvalidName::Empty),
        len if len > max => Err(InvalidName::TooLong),
        _ => Ok(()),
    }
}

impl Generation {
    pub const fn new(value: u64) -> Generation {
        Generation(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_names_are_one_to_261_bytes_without_control_characters() {
        let mount = format!("/{}", "m".repeat(255));
        let too_long = format!("disk:/{}", "m".repeat(256));
        let cases: [(&str, String, Result<(), InvalidName>); 12] = [
            ("a scalar", "cpu".into(), Ok(())),
            ("one byte", "x".into(), Ok(())),
            ("261 bytes", format!("disk:{mount}"), Ok(())),
            ("262 bytes", too_long, Err(InvalidName::TooLong)),
            ("empty", String::new(), Err(InvalidName::Empty)),
            ("non-ASCII", "disk:/mnt/données".into(), Ok(())),
            (
                "a newline",
                "disk:/a\nb".into(),
                Err(InvalidName::ControlCharacter),
            ),
            ("a NUL", "cpu\0".into(), Err(InvalidName::ControlCharacter)),
            (
                "DEL",
                "cpu\u{7f}".into(),
                Err(InvalidName::ControlCharacter),
            ),
            (
                "a C1 control",
                "cpu\u{85}".into(),
                Err(InvalidName::ControlCharacter),
            ),
            (
                "a tab",
                "app:a\tb:up".into(),
                Err(InvalidName::ControlCharacter),
            ),
            ("a space", "disk:/my disk".into(), Ok(())),
        ];
        for (name, input, expected) in cases {
            let got = MetricName::try_from(input.as_str());
            assert_eq!(got.as_ref().map(|_| ()).map_err(|e| *e), expected, "{name}");
            if let Ok(metric) = got {
                assert_eq!(metric.as_str(), input, "{name}: the text is kept");
            }
        }
    }

    #[test]
    fn a_261_byte_name_of_multibyte_characters_is_kept_and_one_more_byte_is_not() {
        // 87 three-byte characters are 261 bytes.
        let kept = "€".repeat(87);
        assert_eq!(kept.len(), 261);
        assert!(MetricName::try_from(kept.as_str()).is_ok());
        let past = format!("{kept}x");
        assert_eq!(
            MetricName::try_from(past.as_str()),
            Err(InvalidName::TooLong)
        );
    }

    #[test]
    fn system_keys_are_one_to_255_bytes() {
        let cases = [
            ("one byte", vec![b'a'], Ok(())),
            ("255 bytes", vec![b'a'; 255], Ok(())),
            ("256 bytes", vec![b'a'; 256], Err(InvalidName::TooLong)),
            ("empty", vec![], Err(InvalidName::Empty)),
        ];
        for (name, input, expected) in cases {
            let got = SystemKey::try_from(input.as_slice());
            assert_eq!(got.as_ref().map(|_| ()).map_err(|e| *e), expected, "{name}");
            if let Ok(key) = got {
                assert_eq!(
                    key.as_bytes(),
                    input.as_slice(),
                    "{name}: the bytes are kept"
                );
            }
        }
    }

    #[test]
    fn a_generation_keeps_its_value_and_orders_by_it() {
        assert_eq!(Generation::new(7).get(), 7);
        assert!(Generation::new(1) < Generation::new(2));
    }
}
