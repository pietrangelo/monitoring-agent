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

//! Series and the series table (RFC 0010 §2): a series is `(system key, generation, metric
//! name)`, interned once as a `SeriesId`; its record holds its value kind and, per tier, the
//! last span it has a chunk in.

use crate::name::{Generation, MetricName, SystemKey};
use crate::tier::{SpanStart, Tier};
use crate::value::ValueKind;

/// A series' interned id: given in the same transaction as the series' first points, never
/// reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SeriesId(pub(crate) u32);

impl SeriesId {
    pub fn get(self) -> u32 {
        self.0
    }

    /// The id a table or a block file holds, as stored: for tests that build block-file
    /// fixtures. The store gives ids itself; nothing outside it should forge one.
    #[doc(hidden)]
    pub fn from_raw(id: u32) -> SeriesId {
        SeriesId(id)
    }
}

/// What a series is: whose, which registration, and which metric.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SeriesKey {
    pub system: SystemKey,
    pub generation: Generation,
    pub metric: MetricName,
}

/// A series' persisted record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeriesRecord {
    pub key: SeriesKey,
    pub kind: ValueKind,
    /// Per tier (raw, minute, hour), the last span holding a chunk of the series.
    pub last_spans: [Option<SpanStart>; 3],
}

/// Bytes that are no series key or record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordError {
    UnknownVersion(u8),
    Malformed,
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordError::UnknownVersion(v) => write!(f, "unknown record version {v}"),
            RecordError::Malformed => f.write_str("malformed record"),
        }
    }
}

impl std::error::Error for RecordError {}

const RECORD_V1: u8 = 1;

impl SeriesKey {
    /// The key's bytes: the length-prefixed system key, the generation (big-endian), then the
    /// metric name. Every key of one system and generation starts with [`SeriesKey::prefix`],
    /// and no system's prefix starts another's.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = SeriesKey::prefix(&self.system, self.generation);
        out.extend_from_slice(self.metric.as_str().as_bytes());
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<SeriesKey, RecordError> {
        let (&len, rest) = bytes.split_first().ok_or(RecordError::Malformed)?;
        let (system, rest) = rest
            .split_at_checked(usize::from(len))
            .ok_or(RecordError::Malformed)?;
        let (generation, metric) = rest
            .split_first_chunk::<8>()
            .ok_or(RecordError::Malformed)?;
        let metric = std::str::from_utf8(metric).map_err(|_| RecordError::Malformed)?;
        Ok(SeriesKey {
            system: SystemKey::try_from(system).map_err(|_| RecordError::Malformed)?,
            generation: Generation::new(u64::from_be_bytes(*generation)),
            metric: MetricName::try_from(metric).map_err(|_| RecordError::Malformed)?,
        })
    }

    /// The bytes every key of one system's generation starts with.
    pub fn prefix(system: &SystemKey, generation: Generation) -> Vec<u8> {
        let bytes = system.as_bytes();
        // A system key is at most 255 bytes, so its length fits the one-byte prefix.
        let mut out = Vec::with_capacity(1 + bytes.len() + 8);
        out.push(bytes.len() as u8);
        out.extend_from_slice(bytes);
        out.extend_from_slice(&generation.get().to_be_bytes());
        out
    }
}

impl SeriesRecord {
    /// A version byte, the kind's code, per tier a presence byte and its span start, then
    /// the key's length (two bytes, big-endian) and bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = vec![RECORD_V1, self.kind.code()];
        for span in self.last_spans {
            match span {
                None => out.push(0),
                Some(start) => {
                    out.push(1);
                    out.extend_from_slice(&start.get().to_be_bytes());
                }
            }
        }
        let key = self.key.to_bytes();
        // A key is at most 1 + 255 + 8 + 261 bytes.
        out.extend_from_slice(&(key.len() as u16).to_be_bytes());
        out.extend_from_slice(&key);
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<SeriesRecord, RecordError> {
        let (&version, rest) = bytes.split_first().ok_or(RecordError::Malformed)?;
        if version != RECORD_V1 {
            return Err(RecordError::UnknownVersion(version));
        }
        let (&kind, mut rest) = rest.split_first().ok_or(RecordError::Malformed)?;
        let kind = ValueKind::from_code(kind).ok_or(RecordError::Malformed)?;
        let mut last_spans = [None; 3];
        for (slot, tier) in last_spans.iter_mut().zip(Tier::ALL) {
            (*slot, rest) = read_span(rest, tier)?;
        }
        let (len, key) = rest
            .split_first_chunk::<2>()
            .ok_or(RecordError::Malformed)?;
        if key.len() != usize::from(u16::from_be_bytes(*len)) {
            return Err(RecordError::Malformed);
        }
        Ok(SeriesRecord {
            key: SeriesKey::from_bytes(key)?,
            kind,
            last_spans,
        })
    }

    /// The last span of a tier.
    pub fn last_span(&self, tier: Tier) -> Option<SpanStart> {
        self.last_spans[usize::from(tier.code())]
    }
}

/// A presence byte, then a span start on the tier's grid.
fn read_span(bytes: &[u8], tier: Tier) -> Result<(Option<SpanStart>, &[u8]), RecordError> {
    match bytes.split_first() {
        Some((0, rest)) => Ok((None, rest)),
        Some((1, rest)) => {
            let (start, rest) = rest
                .split_first_chunk::<8>()
                .ok_or(RecordError::Malformed)?;
            let span =
                SpanStart::new(tier, u64::from_be_bytes(*start)).ok_or(RecordError::Malformed)?;
            Ok((Some(span), rest))
        }
        _ => Err(RecordError::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(system: &str, generation: u64, metric: &str) -> SeriesKey {
        SeriesKey {
            system: SystemKey::try_from(system.as_bytes()).expect("valid"),
            generation: Generation::new(generation),
            metric: MetricName::try_from(metric).expect("valid"),
        }
    }

    #[test]
    fn a_key_round_trips_through_its_bytes() {
        let cases = [
            key("web01", 1, "cpu"),
            key(&"s".repeat(255), u64::MAX, "disk:/données"),
            key("a", 0, "x"),
        ];
        for k in cases {
            assert_eq!(SeriesKey::from_bytes(&k.to_bytes()), Ok(k.clone()), "{k:?}");
        }
    }

    #[test]
    fn a_key_starts_with_its_systems_prefix_and_no_other_systems() {
        let cases = [
            (
                "same system and generation",
                key("a", 1, "cpu"),
                "a",
                1,
                true,
            ),
            ("another generation", key("a", 1, "cpu"), "a", 2, false),
            (
                "a system whose id extends it",
                key("a/b", 1, "cpu"),
                "a",
                1,
                false,
            ),
            ("a system it extends", key("a", 1, "cpu"), "a/b", 1, false),
            (
                "metric shaped like an id",
                key("ab", 1, "cpu"),
                "a",
                1,
                false,
            ),
        ];
        for (name, k, system, generation, starts) in cases {
            let prefix = SeriesKey::prefix(
                &SystemKey::try_from(system.as_bytes()).expect("valid"),
                Generation::new(generation),
            );
            assert_eq!(k.to_bytes().starts_with(&prefix), starts, "{name}");
        }
    }

    #[test]
    fn keys_of_one_system_and_generation_are_contiguous_in_byte_order() {
        let mut all = [
            key("a", 2, "z"),
            key("a", 1, "cpu"),
            key("b", 1, "a"),
            key("a", 1, "memory"),
            key("a\u{1}", 1, "a"),
        ];
        all.sort_by_key(SeriesKey::to_bytes);
        let prefix = SeriesKey::prefix(
            &SystemKey::try_from(&b"a"[..]).expect("valid"),
            Generation::new(1),
        );
        let positions: Vec<usize> = all
            .iter()
            .enumerate()
            .filter(|(_, k)| k.to_bytes().starts_with(&prefix))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[1], positions[0] + 1);
    }

    #[test]
    fn malformed_key_bytes_are_refused() {
        let good = key("web", 3, "cpu").to_bytes();
        let cases: [(&str, Vec<u8>); 6] = [
            ("empty", vec![]),
            (
                "a zero-length system",
                [&[0u8][..], &[0; 8], b"cpu"].concat(),
            ),
            ("cut in the system", good[..2].to_vec()),
            ("cut in the generation", good[..6].to_vec()),
            ("no metric", good[..12].to_vec()),
            (
                "a control character in the metric",
                [&good[..12], b"c\npu"].concat(),
            ),
        ];
        for (name, bytes) in cases {
            assert_eq!(
                SeriesKey::from_bytes(&bytes),
                Err(RecordError::Malformed),
                "{name}"
            );
        }
    }

    #[test]
    fn a_record_round_trips_with_every_combination_of_last_spans() {
        let raw = SpanStart::new(Tier::Raw, 7_200);
        let minute = SpanStart::new(Tier::Minute, 3_600);
        let hour = SpanStart::new(Tier::Hour, 86_400);
        for last_spans in [
            [None, None, None],
            [raw, None, hour],
            [raw, minute, hour],
            [None, minute, None],
        ] {
            for kind in ValueKind::ALL {
                let record = SeriesRecord {
                    key: key("web01", 9, "load1"),
                    kind,
                    last_spans,
                };
                assert_eq!(
                    SeriesRecord::from_bytes(&record.to_bytes()),
                    Ok(record.clone()),
                    "{last_spans:?} {kind:?}"
                );
                assert_eq!(record.last_span(Tier::Raw), last_spans[0]);
                assert_eq!(record.last_span(Tier::Minute), last_spans[1]);
                assert_eq!(record.last_span(Tier::Hour), last_spans[2]);
            }
        }
    }

    #[test]
    fn a_record_of_an_unknown_version_or_shape_is_refused() {
        let record = SeriesRecord {
            key: key("w", 1, "cpu"),
            kind: ValueKind::Percent,
            last_spans: [None; 3],
        };
        let good = record.to_bytes();
        let mut unknown = good.clone();
        unknown[0] = 2;
        assert_eq!(
            SeriesRecord::from_bytes(&unknown),
            Err(RecordError::UnknownVersion(2))
        );
        let mut bad_kind = good.clone();
        bad_kind[1] = 0;
        assert_eq!(
            SeriesRecord::from_bytes(&bad_kind),
            Err(RecordError::Malformed)
        );
        let mut bad_presence = good.clone();
        bad_presence[2] = 7;
        assert_eq!(
            SeriesRecord::from_bytes(&bad_presence),
            Err(RecordError::Malformed)
        );
        let unaligned = SeriesRecord {
            last_spans: [None, None, None],
            ..record
        };
        let mut bytes = unaligned.to_bytes();
        bytes[2] = 1;
        bytes.splice(3..3, 61u64.to_be_bytes());
        assert_eq!(
            SeriesRecord::from_bytes(&bytes),
            Err(RecordError::Malformed),
            "a span start off its grid"
        );
        for cut in 0..good.len() {
            assert!(
                SeriesRecord::from_bytes(&good[..cut]).is_err(),
                "cut at {cut}"
            );
        }
    }
}
