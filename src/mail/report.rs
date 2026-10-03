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

//! The mail report (RFC 0017 §2): what one message carries, as domain values, and its wire
//! shape, a MessagePack map per struct (`rmp_serde::to_vec_named`), so a later field is one
//! a v1 hub ignores. Closed sets (the reason, an alert's metric and severity) are strings on
//! the wire. Pinned by `testdata/mail-report-v1.msgpack`.

use std::time::Duration;

use serde::Serialize;

use crate::alerts::{ActiveAlert, AgentRun, AlertMetric, AlertSeverity};
use crate::applications::round::ScrapeRound;
use crate::push::application_frame::ApplicationFrame;
use crate::snapshot::PublishedSnapshot;

/// The only report kind this agent sends.
const KIND: &str = "mail-report.v1";
/// The longest alert message a report carries, in bytes.
const MAX_ALERT_MESSAGE_BYTES: usize = 512;

/// The time between scheduled reports (`MAIL_INTERVAL`): 60 s to a day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailInterval(Duration);

/// `MAIL_INTERVAL` outside 60 to 86400 seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntervalOutOfRange;

impl TryFrom<u64> for MailInterval {
    type Error = IntervalOutOfRange;

    fn try_from(secs: u64) -> Result<Self, Self::Error> {
        match secs {
            60..=86_400 => Ok(Self(Duration::from_secs(secs))),
            _ => Err(IntervalOutOfRange),
        }
    }
}

impl MailInterval {
    pub fn as_duration(self) -> Duration {
        self.0
    }
}

/// Identifies one report: the agent run and a sequence counting the run's reports from 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportId {
    pub run: AgentRun,
    pub seq: u64,
}

/// Why a report was mailed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportReason {
    Scheduled,
    /// An alert incident became active.
    Incident,
}

/// What the system said about itself; the report sends it with its newest sample only.
#[derive(Debug, Clone, PartialEq)]
pub struct MailedInfo {
    pub hostname: String,
    pub os_name: String,
    pub kernel: String,
    pub cpu_model: String,
    pub cpu_cores: usize,
}

/// One kept sample: what a push snapshot frame carries, without the top processes.
#[derive(Debug, Clone, PartialEq)]
pub struct MailedSnapshot {
    pub collected_at: u64,
    pub info: MailedInfo,
    pub cpu_percent: f32,
    pub memory_percent: f32,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub memory_total_display: String,
    pub swap_percent: f32,
    pub load: [f64; 3],
    pub uptime_seconds: u64,
    pub uptime_display: String,
    /// Each disk's mount point and usage percent.
    pub disks: Vec<(String, f32)>,
}

impl From<&PublishedSnapshot> for MailedSnapshot {
    fn from(published: &PublishedSnapshot) -> Self {
        let system = &published.system;
        Self {
            collected_at: published.collected_at,
            info: MailedInfo {
                hostname: system.hostname.clone(),
                os_name: system.os.pretty_name.clone(),
                kernel: system.kernel.clone(),
                cpu_model: system.cpu.model.clone(),
                cpu_cores: system.cpu.logical_cores,
            },
            cpu_percent: system.cpu.usage_percent,
            memory_percent: system.memory.usage_percent,
            memory_used_bytes: system.memory.used_bytes,
            memory_total_bytes: system.memory.total_bytes,
            memory_total_display: system.memory.total_display.clone(),
            swap_percent: system.swap.usage_percent,
            load: [
                system.load_average.one,
                system.load_average.five,
                system.load_average.fifteen,
            ],
            uptime_seconds: system.uptime_seconds,
            uptime_display: system.uptime_display.clone(),
            disks: system
                .disks
                .iter()
                .map(|disk| (disk.mount_point.clone(), disk.usage_percent))
                .collect(),
        }
    }
}

/// One alert incident that was active since the previous report.
#[derive(Debug, Clone, PartialEq)]
pub struct MailedAlert {
    pub id: String,
    pub metric: AlertMetric,
    pub severity: AlertSeverity,
    pub current_value: f32,
    /// At most `MAX_ALERT_MESSAGE_BYTES`, cut on a character boundary.
    pub message: String,
    pub fired_at: String,
}

impl From<&ActiveAlert> for MailedAlert {
    fn from(alert: &ActiveAlert) -> Self {
        Self {
            id: alert.id.clone(),
            metric: alert.rule.metric.clone(),
            severity: alert.rule.severity.clone(),
            current_value: alert.current_value,
            message: cut_to(&alert.message, MAX_ALERT_MESSAGE_BYTES).to_owned(),
            fired_at: alert.fired_at.clone(),
        }
    }
}

/// The longest prefix of `text` that is at most `max` bytes and ends on a character boundary.
fn cut_to(text: &str, max: usize) -> &str {
    let end = (0..=max.min(text.len()))
        .rev()
        .find(|&end| text.is_char_boundary(end))
        .unwrap_or(0);
    &text[..end]
}

/// What one message carries.
#[derive(Debug, Clone)]
pub struct MailReport {
    pub id: ReportId,
    pub created_at: u64,
    pub interval: MailInterval,
    pub reason: ReportReason,
    /// Oldest first; at least one.
    pub snapshots: Vec<MailedSnapshot>,
    pub alerts: Vec<MailedAlert>,
    pub round: Option<ScrapeRound>,
}

#[derive(Serialize)]
struct ReportDto<'a> {
    kind: &'static str,
    run: String,
    seq: u64,
    created_at: u64,
    interval_secs: u64,
    reason: &'static str,
    snapshots: Vec<SnapshotDto<'a>>,
    alerts: Vec<AlertDto<'a>>,
    round: Option<ApplicationFrame>,
}

#[derive(Serialize)]
struct InfoDto<'a> {
    hostname: &'a str,
    os_name: &'a str,
    kernel: &'a str,
    cpu_model: &'a str,
    cpu_cores: usize,
}

#[derive(Serialize)]
struct SnapshotDto<'a> {
    collected_at: u64,
    info: Option<InfoDto<'a>>,
    cpu_percent: f32,
    memory_percent: f32,
    memory_used_bytes: u64,
    memory_total_bytes: u64,
    memory_total_display: &'a str,
    swap_percent: f32,
    load_one: f64,
    load_five: f64,
    load_fifteen: f64,
    uptime_seconds: u64,
    uptime_display: &'a str,
    disks: Vec<DiskDto<'a>>,
}

#[derive(Serialize)]
struct DiskDto<'a> {
    mount_point: &'a str,
    usage_percent: f32,
}

#[derive(Serialize)]
struct AlertDto<'a> {
    id: &'a str,
    metric: &'static str,
    severity: &'static str,
    current_value: f32,
    message: &'a str,
    fired_at: &'a str,
}

/// Encodes a report as a v1 mail report: the plaintext that is sealed.
pub fn encode(report: &MailReport) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    rmp_serde::to_vec_named(&ReportDto::from(report))
}

impl<'a> From<&'a MailReport> for ReportDto<'a> {
    fn from(report: &'a MailReport) -> Self {
        let newest = report.snapshots.len().saturating_sub(1);
        Self {
            kind: KIND,
            run: report.id.run.as_uuid().hyphenated().to_string(),
            seq: report.id.seq,
            created_at: report.created_at,
            interval_secs: report.interval.as_duration().as_secs(),
            reason: match report.reason {
                ReportReason::Scheduled => "scheduled",
                ReportReason::Incident => "incident",
            },
            snapshots: (report.snapshots.iter().enumerate())
                .map(|(at, sample)| SnapshotDto::of(sample, at == newest))
                .collect(),
            alerts: report.alerts.iter().map(AlertDto::from).collect(),
            round: report.round.as_ref().map(ApplicationFrame::from),
        }
    }
}

impl<'a> SnapshotDto<'a> {
    /// A sample's wire shape; only the newest carries the system info.
    fn of(sample: &'a MailedSnapshot, newest: bool) -> Self {
        let info = &sample.info;
        Self {
            collected_at: sample.collected_at,
            info: newest.then_some(InfoDto {
                hostname: &info.hostname,
                os_name: &info.os_name,
                kernel: &info.kernel,
                cpu_model: &info.cpu_model,
                cpu_cores: info.cpu_cores,
            }),
            cpu_percent: sample.cpu_percent,
            memory_percent: sample.memory_percent,
            memory_used_bytes: sample.memory_used_bytes,
            memory_total_bytes: sample.memory_total_bytes,
            memory_total_display: &sample.memory_total_display,
            swap_percent: sample.swap_percent,
            load_one: sample.load[0],
            load_five: sample.load[1],
            load_fifteen: sample.load[2],
            uptime_seconds: sample.uptime_seconds,
            uptime_display: &sample.uptime_display,
            disks: (sample.disks.iter())
                .map(|(mount_point, usage_percent)| DiskDto {
                    mount_point,
                    usage_percent: *usage_percent,
                })
                .collect(),
        }
    }
}

impl<'a> From<&'a MailedAlert> for AlertDto<'a> {
    fn from(alert: &'a MailedAlert) -> Self {
        Self {
            id: &alert.id,
            metric: match alert.metric {
                AlertMetric::Cpu => "cpu",
                AlertMetric::Memory => "memory",
                AlertMetric::Swap => "swap",
                AlertMetric::Disk => "disk",
                AlertMetric::Load1 => "load1",
                AlertMetric::Load5 => "load5",
                AlertMetric::Load15 => "load15",
            },
            severity: match alert.severity {
                AlertSeverity::Info => "info",
                AlertSeverity::Warning => "warning",
                AlertSeverity::Critical => "critical",
            },
            current_value: alert.current_value,
            message: &alert.message,
            fired_at: &alert.fired_at,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::snapshot::fixtures;

    /// The contract both crates read: written by an encoder independent of rmp-serde.
    const GOLDEN: &[u8] = include_bytes!("../../testdata/mail-report-v1.msgpack");
    const RUN: &str = "6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b";

    fn sample(collected_at: u64) -> MailedSnapshot {
        MailedSnapshot {
            collected_at,
            info: MailedInfo {
                hostname: "web-01".into(),
                os_name: "Debian 12".into(),
                kernel: "6.1.0".into(),
                cpu_model: "Xeon".into(),
                cpu_cores: 4,
            },
            cpu_percent: 12.5,
            memory_percent: 40.0,
            memory_used_bytes: 4096,
            memory_total_bytes: 8192,
            memory_total_display: "8 KB".into(),
            swap_percent: 0.0,
            load: [0.5, 0.25, 0.125],
            uptime_seconds: 3600,
            uptime_display: "1h 0m".into(),
            disks: vec![("/".into(), 50.0)],
        }
    }

    /// The report `testdata/generate_mail_report_v1.py` writes.
    pub(crate) fn golden_report() -> MailReport {
        MailReport {
            id: ReportId {
                run: AgentRun::new(uuid::Uuid::parse_str(RUN).unwrap()),
                seq: 7,
            },
            created_at: 1_700_000_300,
            interval: MailInterval(Duration::from_secs(300)),
            reason: ReportReason::Scheduled,
            snapshots: vec![sample(1_700_000_240), sample(1_700_000_300)],
            alerts: vec![MailedAlert {
                id: format!("{RUN}-1"),
                metric: AlertMetric::Cpu,
                severity: AlertSeverity::Warning,
                current_value: 95.0,
                message: "CPU high".into(),
                fired_at: "2023-11-14T22:18:20Z".into(),
            }],
            round: None,
        }
    }

    /// RFC 0017 §2: the encoding is the golden's, byte for byte: a map per struct, the system
    /// info on the newest sample only, the closed sets as strings.
    #[test]
    fn a_report_encodes_as_the_v1_golden() {
        assert_eq!(encode(&golden_report()).unwrap(), GOLDEN);
    }

    /// RFC 0017 §2: an alert's message is cut to 512 bytes, on a character boundary.
    #[test]
    fn an_alert_message_is_cut_to_512_bytes_on_a_character_boundary() {
        let cases = [
            ("short", "CPU high".to_string(), "CPU high".to_string()),
            ("exactly 512", "a".repeat(512), "a".repeat(512)),
            ("513 ascii", "a".repeat(513), "a".repeat(512)),
            (
                "a 2-byte character across 512",
                format!("{}é", "a".repeat(511)),
                "a".repeat(511),
            ),
        ];
        for (name, message, expected) in cases {
            let mut alert = crate::alerts::fixtures::active_alert();
            alert.message = message;
            assert_eq!(MailedAlert::from(&alert).message, expected, "case {name}");
        }
    }

    /// RFC 0017 §5: `MAIL_INTERVAL` is 60 s to a day.
    #[test]
    fn a_mail_interval_is_a_minute_to_a_day() {
        let cases = [
            (59, false),
            (60, true),
            (300, true),
            (86_400, true),
            (86_401, false),
            (0, false),
        ];
        for (secs, ok) in cases {
            let got = MailInterval::try_from(secs).map(MailInterval::as_duration);
            assert_eq!(
                got.ok(),
                ok.then_some(Duration::from_secs(secs)),
                "{secs} s"
            );
        }
    }

    /// RFC 0017 §2: a sample keeps what a snapshot frame carries, at its `collected_at`.
    #[test]
    fn a_sample_keeps_what_a_snapshot_frame_carries() {
        let published = fixtures::published();
        let system = &published.system;

        let sample = MailedSnapshot::from(&published);

        assert_eq!(sample.collected_at, published.collected_at);
        assert_eq!(sample.info.hostname, system.hostname);
        assert_eq!(sample.info.os_name, system.os.pretty_name);
        assert_eq!(sample.info.cpu_cores, system.cpu.logical_cores);
        assert_eq!(sample.cpu_percent, system.cpu.usage_percent);
        assert_eq!(sample.memory_total_bytes, system.memory.total_bytes);
        assert_eq!(
            sample.load,
            [
                system.load_average.one,
                system.load_average.five,
                system.load_average.fifteen
            ]
        );
        assert_eq!(sample.uptime_display, system.uptime_display);
        let disks: Vec<(String, f32)> = system
            .disks
            .iter()
            .map(|disk| (disk.mount_point.clone(), disk.usage_percent))
            .collect();
        assert!(!disks.is_empty(), "the fixture has disks");
        assert_eq!(sample.disks, disks);
    }
}
