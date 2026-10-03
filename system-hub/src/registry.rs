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

//! Fleet Registry rules the ingestion adapters apply to what an agent reports (RFC 0014 §8,
//! RFC 0007 §2).

use crate::models::{SystemInfo, SystemStatus};

/// The display rule's longest text, in bytes.
pub const MAX_DISPLAY_BYTES: usize = 64;

/// The display rule, for text an agent may send anew in every snapshot: 1..=`MAX_DISPLAY_BYTES`
/// bytes, no control character.
fn follows_display_rule(text: &str) -> bool {
    (1..=MAX_DISPLAY_BYTES).contains(&text.len()) && !text.chars().any(char::is_control)
}

/// A display the display rule refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidDisplay;

/// An agent's uptime as it displays it ("3d 4h 5m"), under the display rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UptimeDisplay(String);

impl TryFrom<String> for UptimeDisplay {
    type Error = InvalidDisplay;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match follows_display_rule(&value) {
            true => Ok(Self(value)),
            false => Err(InvalidDisplay),
        }
    }
}

impl UptimeDisplay {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What a stored snapshot writes as a system's last seen. The offline markings write the
/// column too (a failed poll its time, a push disconnect a blank), until RFC 0010's contact time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LastSeen {
    /// Push: the frame's `uptime_display`, when it follows the display rule.
    Uptime(UptimeDisplay),
    /// Poll: the poll's ISO time, built by the hub.
    PolledAt(String),
    /// Mail: the newest report's creation time, as ISO time (RFC 0017 §7).
    ReportedAt(String),
    /// Push, with a display the rule refuses: the column keeps its value.
    Unchanged,
}

/// One write of a system's status: the status, when it was last seen, and its error.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusUpdate {
    status: SystemStatus,
    last_seen: LastSeen,
    error: Option<String>,
}

impl StatusUpdate {
    /// A stored snapshot: online, seen as given, no error.
    pub fn after_snapshot(last_seen: LastSeen) -> Self {
        Self {
            status: SystemStatus::Online,
            last_seen,
            error: None,
        }
    }

    pub fn status(&self) -> &SystemStatus {
        &self.status
    }

    pub fn last_seen(&self) -> &LastSeen {
        &self.last_seen
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// How much memory a system's monitored environment may use, as its agent last reported it:
/// the bytes and their display string, always together, so the two never disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryCapacity {
    display: String,
    bytes: u64,
}

impl MemoryCapacity {
    /// The capacity an agent reported, when it reported both halves within bounds: a display
    /// under the display rule (an empty one would show as a blank), and bytes the `systems`
    /// row can hold (at most `i64::MAX`).
    pub fn reported(display: Option<&str>, bytes: Option<u64>) -> Option<Self> {
        let display = display.filter(|d| follows_display_rule(d))?;
        let bytes = bytes.filter(|b| i64::try_from(*b).is_ok())?;
        Some(Self {
            display: display.to_owned(),
            bytes,
        })
    }

    /// The capacity `system`'s row holds, when it holds both halves within `reported`'s bounds:
    /// a half-stored pair (left by a hub before RFC 0014), or a display an older hub stored
    /// past the display rule, counts as nothing stored.
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

/// Whether a system's info is still to be filled from what its agent reports: while its
/// hostname or its OS is missing. Filled once, unlike the memory capacity, which follows the
/// agent.
pub fn needs_system_info(system: &SystemInfo) -> bool {
    system.hostname.is_none() || system.os.is_none()
}

/// The url push registration writes, which makes a system a push system (RFC 0016 §1).
pub const PUSH_URL: &str = "push://";

/// Where a system's snapshots come from (RFC 0016 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemSource {
    /// Registered by a push handshake: never polled.
    Push,
    /// Any other url: polled.
    Poll,
}

impl SystemSource {
    /// `Push` for exactly `PUSH_URL`, `Poll` for any other url.
    pub fn of(url: &str) -> Self {
        if url == PUSH_URL {
            Self::Push
        } else {
            Self::Poll
        }
    }
}

/// The systems the poller polls: the enabled polled systems, in the registry's order.
pub fn polled_systems(systems: &[SystemInfo]) -> impl Iterator<Item = &SystemInfo> {
    systems.iter().filter(|system| {
        system.enabled
            && match SystemSource::of(&system.url) {
                SystemSource::Poll => true,
                SystemSource::Push => false,
            }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capacity(display: &str, bytes: u64) -> MemoryCapacity {
        MemoryCapacity {
            display: display.to_owned(),
            bytes,
        }
    }

    fn system(id: &str, enabled: bool) -> SystemInfo {
        at(id, "http://example.com", enabled)
    }

    fn push_system(id: &str, enabled: bool) -> SystemInfo {
        at(id, PUSH_URL, enabled)
    }

    fn at(id: &str, url: &str, enabled: bool) -> SystemInfo {
        SystemInfo {
            id: id.to_owned(),
            name: id.to_owned(),
            url: url.to_owned(),
            token: String::new(),
            status: SystemStatus::Unknown,
            last_seen: String::new(),
            last_error: None,
            os: None,
            hostname: None,
            kernel: None,
            cpu_model: None,
            cpu_cores: None,
            total_memory_display: None,
            total_memory_bytes: None,
            poll_interval_secs: 10,
            enabled,
        }
    }

    #[test]
    fn only_enabled_polled_systems_are_polled_in_the_registrys_order() {
        let cases: [(&str, Vec<SystemInfo>, Vec<&str>); 7] = [
            ("none", vec![], vec![]),
            (
                "all enabled",
                vec![system("a", true), system("b", true)],
                vec!["a", "b"],
            ),
            (
                "all disabled",
                vec![system("a", false), system("b", false)],
                vec![],
            ),
            (
                "mixed",
                vec![
                    system("a", false),
                    system("b", true),
                    system("c", false),
                    system("d", true),
                ],
                vec!["b", "d"],
            ),
            // RFC 0016 §3: a push system is never polled, enabled or not.
            (
                "push systems",
                vec![push_system("a", true), push_system("b", false)],
                vec![],
            ),
            (
                "push and polled",
                vec![
                    push_system("a", true),
                    system("b", true),
                    push_system("c", true),
                    system("d", false),
                    system("e", true),
                ],
                vec!["b", "e"],
            ),
            (
                "a url POST stores from push://",
                vec![at("a", "push:", true)],
                vec!["a"],
            ),
        ];
        for (case, systems, expected) in cases {
            let polled: Vec<&str> = polled_systems(&systems).map(|s| s.id.as_str()).collect();
            assert_eq!(polled, expected, "case: {case}");
        }
    }

    /// RFC 0016 §1: only the exact sentinel push registration writes makes a push system.
    #[test]
    fn only_the_push_sentinel_makes_a_push_system() {
        let cases = [
            ("the sentinel", "push://", SystemSource::Push),
            ("what POST stores from it", "push:", SystemSource::Poll),
            ("upper case", "PUSH://", SystemSource::Poll),
            ("with a host", "push://x", SystemSource::Poll),
            ("http", "http://10.0.0.1:9090", SystemSource::Poll),
            ("https", "https://agent.example", SystemSource::Poll),
            ("empty", "", SystemSource::Poll),
        ];
        for (name, url, expected) in cases {
            assert_eq!(SystemSource::of(url), expected, "case {name}");
        }
    }

    #[test]
    fn system_info_is_filled_while_the_hostname_or_the_os_is_missing() {
        let cases = [
            ("neither", None, None, true),
            ("no os", Some("web-01"), None, true),
            ("no hostname", None, Some("Ubuntu"), true),
            ("both", Some("web-01"), Some("Ubuntu"), false),
        ];
        for (case, hostname, os, expected) in cases {
            let mut known = system("a", true);
            known.hostname = hostname.map(str::to_owned);
            known.os = os.map(str::to_owned);
            assert_eq!(needs_system_info(&known), expected, "case: {case}");
        }
    }

    fn uptime_accepted(display: &str) -> bool {
        UptimeDisplay::try_from(display.to_owned()).is_ok()
    }

    #[test]
    fn an_uptime_display_follows_the_display_rule() {
        let at_bound_with_a_wide_char = format!("{}é", "a".repeat(62));
        let across_the_bound = format!("{}é", "a".repeat(63)); // 64 chars, 65 bytes
        let cases: [(&str, String, bool); 10] = [
            ("the agent's own format", "3d 4h 5m".into(), true),
            ("64 bytes", "a".repeat(64), true),
            (
                "64 bytes ending in a 2-byte char",
                at_bound_with_a_wide_char,
                true,
            ),
            ("65 bytes", "a".repeat(65), false),
            (
                "65 bytes, a 2-byte char across the bound",
                across_the_bound,
                false,
            ),
            ("empty", String::new(), false),
            ("a newline", "3d\n4h".into(), false),
            ("a tab", "3d\t4h".into(), false),
            ("DEL", "3d\u{7f}4h".into(), false),
            ("NEL, a C1 control", "3d\u{85}4h".into(), false),
        ];
        for (name, display, expected) in cases {
            let parsed = UptimeDisplay::try_from(display.clone());
            assert_eq!(parsed.is_ok(), expected, "{name}");
            if let Ok(uptime) = parsed {
                assert_eq!(uptime.as_str(), display, "{name}: kept verbatim");
            }
        }
    }

    #[test]
    fn the_display_rule_sweeps_lengths_and_control_characters() {
        // Every length from 1 to 64 bytes is accepted and every one from 65 to 128 refused, so
        // no bound but 64 bytes passes.
        for len in 1..=128 {
            assert_eq!(uptime_accepted(&"a".repeat(len)), len <= 64, "{len} bytes");
        }
        // Every C0 control, DEL and every C1 control is refused, in any position; the
        // printable characters around them are accepted.
        let controls = (0u32..0x20).chain(0x7f..=0x9f).filter_map(char::from_u32);
        for control in controls {
            for display in [format!("{control}3d"), format!("3d{control}4h")] {
                assert!(!uptime_accepted(&display), "U+{:04X}", control as u32);
            }
        }
        for printable in [' ', '~', '\u{a0}', 'é', '日'] {
            assert!(
                uptime_accepted(&format!("3d{printable}4h")),
                "{printable:?}"
            );
        }
    }

    #[test]
    fn a_stored_snapshot_marks_the_system_online_as_seen_with_no_error() {
        let cases = [
            (
                "an uptime",
                LastSeen::Uptime(UptimeDisplay("3d 4h 5m".into())),
            ),
            (
                "a poll time",
                LastSeen::PolledAt("2026-09-29T10:00:00Z".into()),
            ),
            ("unchanged", LastSeen::Unchanged),
        ];
        for (name, last_seen) in cases {
            let update = StatusUpdate::after_snapshot(last_seen.clone());
            assert_eq!(update.status(), &SystemStatus::Online, "{name}");
            assert_eq!(update.last_seen(), &last_seen, "{name}");
            assert_eq!(update.error(), None, "{name}");
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

    #[test]
    fn a_capacity_report_follows_the_display_rule_and_the_rows_range() {
        let max = i64::MAX as u64;
        let gb = 33_285_996_544;
        let across_the_bound = format!("{}é", "a".repeat(63)); // 64 chars, 65 bytes
        // (name, display, bytes, reported)
        let cases: [(&str, String, u64, bool); 11] = [
            ("the agent's largest display", "16384.0 PB".into(), gb, true),
            ("a 64-byte display", "a".repeat(64), gb, true),
            ("a 65-byte display", "a".repeat(65), gb, false),
            (
                "65 bytes, a 2-byte char across the bound",
                across_the_bound,
                gb,
                false,
            ),
            ("a newline", "31.0\nGB".into(), gb, false),
            ("NEL, a C1 control", "31.0\u{85}GB".into(), gb, false),
            ("DEL", "31.0\u{7f}GB".into(), gb, false),
            ("bytes of i64::MAX", "8.0 EB".into(), max, true),
            ("bytes of i64::MAX + 1", "8.0 EB".into(), max + 1, false),
            (
                "the top bit plus change",
                "8.0 EB".into(),
                (1 << 63) | 0x1234,
                false,
            ),
            ("bytes of u64::MAX", "16384.0 PB".into(), u64::MAX, false),
        ];
        for (name, display, bytes, expected) in cases {
            let reported = MemoryCapacity::reported(Some(&display), Some(bytes));
            let expected = expected.then(|| capacity(&display, bytes));
            assert_eq!(reported, expected, "{name}");
        }
        // Every value max + 2^k lies above the row's range, and every 2^k within it.
        for k in 0..63 {
            let over = MemoryCapacity::reported(Some("x"), Some(max + (1 << k)));
            assert_eq!(over, None, "max + 2^{k}");
            let within = MemoryCapacity::reported(Some("x"), Some(1 << k));
            assert_eq!(within, Some(capacity("x", 1 << k)), "2^{k}");
        }
    }

    #[test]
    fn the_refresh_replaces_a_stored_capacity_the_rule_refuses_and_ignores_out_of_range_bytes() {
        let valid = || Some(capacity("31.0 GB", 33_285_996_544));
        let long_display = "a".repeat(65);
        let stored_long = MemoryCapacity::stored(&row(Some(&long_display), Some(33_285_996_544)));
        assert_eq!(
            stored_long, None,
            "a stored 65-byte display counts as nothing stored"
        );
        assert_eq!(
            memory_capacity_refresh(stored_long.as_ref(), valid()),
            valid(),
            "a valid report replaces it"
        );
        let out_of_range = MemoryCapacity::reported(Some("8.0 EB"), Some(i64::MAX as u64 + 1));
        assert_eq!(
            memory_capacity_refresh(valid().as_ref(), out_of_range),
            None,
            "bytes above i64::MAX write nothing over a stored capacity"
        );
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
