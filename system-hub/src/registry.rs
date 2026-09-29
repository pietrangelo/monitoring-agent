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

//! Fleet Registry rules the ingestion adapters apply to what an agent reports (RFC 0014 §8).

use crate::models::SystemInfo;

/// How much memory a system's monitored environment may use, as its agent last reported it:
/// the bytes and their display string, always together, so the two never disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryCapacity {
    display: String,
    bytes: u64,
}

impl MemoryCapacity {
    /// The capacity an agent reported, when it reported both halves. An empty display isn't
    /// one: it would show as a blank.
    pub fn reported(display: Option<&str>, bytes: Option<u64>) -> Option<Self> {
        let display = display.filter(|d| !d.is_empty())?;
        Some(Self {
            display: display.to_owned(),
            bytes: bytes?,
        })
    }

    /// The capacity `system`'s row holds, when it holds both halves: a half-stored pair (left
    /// by a hub before RFC 0014) counts as nothing stored.
    pub fn stored(system: &SystemInfo) -> Option<Self> {
        Self::reported(
            system.total_memory_display.as_deref(),
            system.total_memory_bytes,
        )
    }

    /// A capacity for tests that don't exercise `reported`.
    #[cfg(test)]
    pub fn fixture(display: &str, bytes: u64) -> Self {
        Self {
            display: display.to_owned(),
            bytes,
        }
    }

    pub fn display(&self) -> &str {
        &self.display
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// The memory capacity to store after an agent reported `reported`, `stored` being what the
/// system's row holds: the reported one when it differs, else nothing to write. Unlike the rest
/// of a system's info, which is filled once, the capacity follows the agent: a container's
/// limit or a VM's memory can change, and an agent upgrade can switch it from the host's to
/// the container's. A report without one never clears the stored one.
pub fn memory_capacity_refresh(
    stored: Option<&MemoryCapacity>,
    reported: Option<MemoryCapacity>,
) -> Option<MemoryCapacity> {
    reported.filter(|capacity| stored != Some(capacity))
}

/// How often the hub polls a system, in seconds: at most `i64::MAX`, the range of the
/// `systems` row's column, so a stored interval always reads back. Held as the column's
/// type, never negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollInterval(i64);

/// A poll interval the `systems` row can't hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollIntervalOutOfRange;

impl TryFrom<u64> for PollInterval {
    type Error = PollIntervalOutOfRange;

    fn try_from(secs: u64) -> Result<Self, Self::Error> {
        i64::try_from(secs)
            .map(Self)
            .map_err(|_| PollIntervalOutOfRange)
    }
}

impl From<PollInterval> for u64 {
    /// The interval in seconds; exact, since a poll interval is never negative.
    fn from(interval: PollInterval) -> Self {
        interval.0.unsigned_abs()
    }
}

impl PollInterval {
    /// The value the `systems` row's column stores.
    pub fn column_value(self) -> i64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::SystemStatus;

    fn capacity(display: &str, bytes: u64) -> MemoryCapacity {
        MemoryCapacity {
            display: display.to_owned(),
            bytes,
        }
    }

    #[test]
    fn a_poll_interval_is_at_most_the_rows_range() {
        let max = i64::MAX as u64;
        let cases: [(&str, u64, Option<u64>); 12] = [
            ("one second", 1, Some(1)),
            ("POST's clamp floor", 5, Some(5)),
            ("the default", 10, Some(10)),
            ("half a minute", 30, Some(30)),
            ("an hour", 3600, Some(3600)),
            ("one below the column's maximum", max - 1, Some(max - 1)),
            ("the column's maximum", max, Some(max)),
            ("one past the column's maximum", max + 1, None),
            ("two past the column's maximum", max + 2, None),
            ("the top bit plus change", (1u64 << 63) | 0x1234_5678, None),
            ("one below u64::MAX", u64::MAX - 1, None),
            ("u64::MAX", u64::MAX, None),
        ];
        for (name, secs, expected) in cases {
            let parsed = PollInterval::try_from(secs).map(u64::from);
            assert_eq!(parsed.ok(), expected, "{name}");
        }
        // Every value max + 2^k for k in 0..63 lies above the column's range,
        // so no finite list of refused literals can pass this sweep.
        for k in 0..63 {
            let secs = max + (1u64 << k);
            let parsed = PollInterval::try_from(secs).map(u64::from);
            assert_eq!(parsed.ok(), None, "max + 2^{k} = {secs}");
        }
        // Every value 2^k and max - 2^k for k in 0..63 lies within the column's
        // range, so no finite list of accepted literals can pass this sweep.
        for k in 0..63 {
            for secs in [1u64 << k, max - (1u64 << k)] {
                let parsed = PollInterval::try_from(secs).map(u64::from);
                assert_eq!(parsed.ok(), Some(secs), "2^{k} sweep: {secs}");
            }
        }
    }

    #[test]
    fn a_capacity_is_reported_only_with_both_halves() {
        // (name, display, bytes, expected)
        let cases = [
            (
                "both",
                Some("512.0 MB"),
                Some(536_870_912),
                Some(capacity("512.0 MB", 536_870_912)),
            ),
            (
                "zero bytes is a report",
                Some("0.0 B"),
                Some(0),
                Some(capacity("0.0 B", 0)),
            ),
            ("no display", None, Some(536_870_912), None),
            ("an empty display", Some(""), Some(536_870_912), None),
            ("no bytes", Some("512.0 MB"), None, None),
            ("neither", None, None, None),
        ];
        for (name, display, bytes, expected) in cases {
            assert_eq!(MemoryCapacity::reported(display, bytes), expected, "{name}");
        }
    }

    fn row(display: Option<&str>, bytes: Option<u64>) -> SystemInfo {
        SystemInfo {
            id: "sys".into(),
            name: "sys".into(),
            url: String::new(),
            token: String::new(),
            status: SystemStatus::Online,
            last_seen: String::new(),
            last_error: None,
            os: Some("Debian 12".into()),
            hostname: Some("web-01".into()),
            kernel: None,
            cpu_model: None,
            cpu_cores: None,
            total_memory_display: display.map(str::to_owned),
            total_memory_bytes: bytes,
            poll_interval_secs: 10,
            enabled: true,
        }
    }

    #[test]
    fn a_rows_capacity_is_stored_only_with_both_halves() {
        // (name, display, bytes, expected)
        let cases = [
            (
                "both",
                Some("31.0 GB"),
                Some(33_285_996_544),
                Some(capacity("31.0 GB", 33_285_996_544)),
            ),
            ("bytes only", None, Some(33_285_996_544), None),
            ("display only", Some("31.0 GB"), None, None),
            ("neither", None, None, None),
        ];
        for (name, display, bytes, expected) in cases {
            assert_eq!(
                MemoryCapacity::stored(&row(display, bytes)),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn the_stored_capacity_follows_what_the_agent_reports() {
        let host = || Some(capacity("31.0 GB", 33_285_996_544));
        let container = || Some(capacity("512.0 MB", 536_870_912));
        // (name, stored display, stored bytes, reported, expected write)
        let cases = [
            ("nothing stored yet", None, None, container(), container()),
            (
                "an agent upgraded into a container",
                Some("31.0 GB"),
                Some(33_285_996_544),
                container(),
                container(),
            ),
            (
                "unchanged: nothing to write",
                Some("512.0 MB"),
                Some(536_870_912),
                container(),
                None,
            ),
            (
                "only the bytes changed",
                Some("512.0 MB"),
                Some(536_870_000),
                container(),
                container(),
            ),
            (
                "only the display changed",
                Some("0.5 GB"),
                Some(536_870_912),
                container(),
                container(),
            ),
            ("half stored", Some("31.0 GB"), None, host(), host()),
            (
                "no report keeps what is stored",
                Some("31.0 GB"),
                Some(33_285_996_544),
                None,
                None,
            ),
            ("no report and nothing stored", None, None, None, None),
        ];
        for (name, display, bytes, reported, expected) in cases {
            assert_eq!(
                memory_capacity_refresh(
                    MemoryCapacity::stored(&row(display, bytes)).as_ref(),
                    reported
                ),
                expected,
                "{name}"
            );
        }
    }
}
