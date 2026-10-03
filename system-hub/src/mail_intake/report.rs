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

//! The mail report as the hub reads it (RFC 0017 §2): the agent's named MessagePack map,
//! parsed once at the edge into domain values with every bound of the RFC. Unknown keys are
//! ignored; an unknown reason is `Other`; a metric or severity is kept as the string it is.

use serde::Deserialize;
use uuid::Uuid;

use crate::application_wire::ApplicationFrameDto;
use crate::registry::{MemoryCapacity, UptimeDisplay};
use crate::snapshot::{ReportedDisk, ReportedSnapshot, SnapshotTime};

/// The report kind this hub reads.
const KIND: &str = "mail-report.v1";
/// The most samples one report carries.
pub const MAX_MAILED_SNAPSHOTS: usize = 60;
/// The most alerts one report carries.
pub const MAX_MAILED_ALERTS: usize = 64;
/// How long before `created_at − interval` a sample may be: mail batches aren't exact.
const SAMPLE_SLACK_BEFORE: u64 = 120;
/// How long after `created_at` a sample may be: an NTP step back between a sample and the close.
const SAMPLE_SLACK_AFTER: u64 = 5;
/// The longest alert message, and the longest of an alert's other strings.
const MAX_MESSAGE_BYTES: usize = 512;
const MAX_FIELD_BYTES: usize = 64;

/// The agent's time between scheduled reports: 60 s to a day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailInterval(u64);

impl MailInterval {
    pub fn secs(self) -> u64 {
        self.0
    }
}

/// Identifies one report: the agent run and its sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportId {
    pub run: Uuid,
    pub seq: u64,
}

/// Why the agent mailed the report; a reason this hub doesn't know is `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportReason {
    Scheduled,
    Incident,
    Other,
}

/// What the newest sample says about its system, for the registry fill.
#[derive(Debug, Clone, PartialEq)]
pub struct MailedInfo {
    pub hostname: String,
    pub os_name: String,
    pub kernel: String,
    pub cpu_model: String,
    pub cpu_cores: usize,
}

/// One sample, ready for the snapshot rule.
#[derive(Debug, Clone, PartialEq)]
pub struct MailedSnapshot {
    pub time: SnapshotTime,
    pub reported: ReportedSnapshot,
    /// `None` when the display breaks the display rule: the stored last seen is kept.
    pub uptime: Option<UptimeDisplay>,
    /// `None` when the pair breaks `MemoryCapacity::reported`'s bounds.
    pub memory: Option<MemoryCapacity>,
    pub info: Option<MailedInfo>,
}

/// One alert incident, as the alert-record rule stores it.
#[derive(Debug, Clone, PartialEq)]
pub struct MailedAlert {
    pub id: String,
    pub metric: String,
    pub severity: String,
    pub current_value: f32,
    pub message: String,
    pub fired_at: String,
}

/// An opened, parsed report.
#[derive(Debug)]
pub struct MailReport {
    pub id: ReportId,
    pub created_at: SnapshotTime,
    pub interval: MailInterval,
    pub reason: ReportReason,
    /// Oldest first; 1 to `MAX_MAILED_SNAPSHOTS`.
    pub snapshots: Vec<MailedSnapshot>,
    pub alerts: Vec<MailedAlert>,
    pub round: Option<ApplicationFrameDto>,
}

/// Why an authentic report was refused (`BadReport` in RFC 0017 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportRefusal {
    /// Not a MessagePack report map.
    NotAReport,
    UnknownKind,
    /// The run isn't a UUID.
    BadRun,
    IntervalOutOfRange,
    /// `created_at` or a sample's time above `i64::MAX`.
    TimeOutOfRange,
    NoSnapshots,
    TooManySnapshots,
    /// A sample outside `created_at − interval − 120 s ..= created_at + 5 s`, or out of order.
    SampleOutOfBounds,
    TooManyAlerts,
    /// An alert string over its bound.
    FieldTooLong,
}

/// Parses an opened report's bytes.
pub fn decode(bytes: &[u8]) -> Result<MailReport, ReportRefusal> {
    let dto: ReportDto = rmp_serde::from_slice(bytes).map_err(|_| ReportRefusal::NotAReport)?;
    MailReport::try_from(dto)
}

impl TryFrom<ReportDto> for MailReport {
    type Error = ReportRefusal;

    fn try_from(dto: ReportDto) -> Result<Self, Self::Error> {
        if dto.kind != KIND {
            return Err(ReportRefusal::UnknownKind);
        }
        let run = Uuid::parse_str(&dto.run).map_err(|_| ReportRefusal::BadRun)?;
        let interval = MailInterval::try_from(dto.interval_secs)?;
        let created_at = time(dto.created_at)?;
        let snapshots = samples(dto.snapshots, dto.created_at, interval)?;
        if dto.alerts.len() > MAX_MAILED_ALERTS {
            return Err(ReportRefusal::TooManyAlerts);
        }
        let alerts = dto.alerts.into_iter().map(MailedAlert::try_from);
        Ok(Self {
            id: ReportId { run, seq: dto.seq },
            created_at,
            interval,
            reason: reason(&dto.reason),
            snapshots,
            alerts: alerts.collect::<Result<_, _>>()?,
            round: dto.round,
        })
    }
}

impl TryFrom<u64> for MailInterval {
    type Error = ReportRefusal;

    fn try_from(secs: u64) -> Result<Self, Self::Error> {
        match secs {
            60..=86_400 => Ok(Self(secs)),
            _ => Err(ReportRefusal::IntervalOutOfRange),
        }
    }
}

fn time(seconds: u64) -> Result<SnapshotTime, ReportRefusal> {
    SnapshotTime::try_from(seconds).map_err(|_| ReportRefusal::TimeOutOfRange)
}

fn reason(wire: &str) -> ReportReason {
    match wire {
        "scheduled" => ReportReason::Scheduled,
        "incident" => ReportReason::Incident,
        _ => ReportReason::Other,
    }
}

/// The samples, oldest first, each within `created_at − interval − 120 s ..= created_at + 5 s`.
fn samples(
    dtos: Vec<SnapshotDto>,
    created_at: u64,
    interval: MailInterval,
) -> Result<Vec<MailedSnapshot>, ReportRefusal> {
    match dtos.len() {
        0 => return Err(ReportRefusal::NoSnapshots),
        n if n > MAX_MAILED_SNAPSHOTS => return Err(ReportRefusal::TooManySnapshots),
        _ => {}
    }
    let earliest = created_at.saturating_sub(interval.secs() + SAMPLE_SLACK_BEFORE);
    let latest = created_at.saturating_add(SAMPLE_SLACK_AFTER);
    let in_bounds = dtos
        .iter()
        .all(|dto| (earliest..=latest).contains(&dto.collected_at));
    let ordered = dtos
        .windows(2)
        .all(|pair| pair[0].collected_at <= pair[1].collected_at);
    if !(in_bounds && ordered) {
        return Err(ReportRefusal::SampleOutOfBounds);
    }
    dtos.into_iter().map(MailedSnapshot::try_from).collect()
}

impl TryFrom<SnapshotDto> for MailedSnapshot {
    type Error = ReportRefusal;

    fn try_from(dto: SnapshotDto) -> Result<Self, Self::Error> {
        let reported = ReportedSnapshot {
            cpu: Some(dto.cpu_percent),
            memory: Some(dto.memory_percent),
            swap: Some(dto.swap_percent),
            load1: Some(dto.load_one),
            load5: Some(dto.load_five),
            disks: dto
                .disks
                .into_iter()
                .map(|disk| ReportedDisk {
                    mount_point: Some(disk.mount_point),
                    usage_percent: Some(disk.usage_percent),
                })
                .collect(),
        };
        Ok(Self {
            time: time(dto.collected_at)?,
            reported,
            uptime: UptimeDisplay::try_from(dto.uptime_display).ok(),
            memory: MemoryCapacity::reported(
                Some(&dto.memory_total_display),
                Some(dto.memory_total_bytes),
            ),
            info: dto.info.map(MailedInfo::from),
        })
    }
}

impl From<InfoDto> for MailedInfo {
    fn from(dto: InfoDto) -> Self {
        Self {
            hostname: dto.hostname,
            os_name: dto.os_name,
            kernel: dto.kernel,
            cpu_model: dto.cpu_model,
            cpu_cores: dto.cpu_cores,
        }
    }
}

impl TryFrom<AlertDto> for MailedAlert {
    type Error = ReportRefusal;

    fn try_from(dto: AlertDto) -> Result<Self, Self::Error> {
        let fields = [&dto.id, &dto.metric, &dto.severity, &dto.fired_at];
        let too_long = dto.message.len() > MAX_MESSAGE_BYTES
            || fields.iter().any(|field| field.len() > MAX_FIELD_BYTES);
        if too_long {
            return Err(ReportRefusal::FieldTooLong);
        }
        Ok(Self {
            id: dto.id,
            metric: dto.metric,
            severity: dto.severity,
            current_value: dto.current_value,
            message: dto.message,
            fired_at: dto.fired_at,
        })
    }
}

#[derive(Deserialize)]
struct ReportDto {
    kind: String,
    run: String,
    seq: u64,
    created_at: u64,
    interval_secs: u64,
    reason: String,
    snapshots: Vec<SnapshotDto>,
    alerts: Vec<AlertDto>,
    round: Option<ApplicationFrameDto>,
}

#[derive(Deserialize)]
struct InfoDto {
    hostname: String,
    os_name: String,
    kernel: String,
    cpu_model: String,
    cpu_cores: usize,
}

#[derive(Deserialize)]
struct SnapshotDto {
    collected_at: u64,
    info: Option<InfoDto>,
    cpu_percent: f32,
    memory_percent: f32,
    memory_total_bytes: u64,
    memory_total_display: String,
    swap_percent: f32,
    load_one: f64,
    load_five: f64,
    uptime_display: String,
    disks: Vec<DiskDto>,
}

#[derive(Deserialize)]
struct DiskDto {
    mount_point: String,
    usage_percent: f32,
}

#[derive(Deserialize)]
struct AlertDto {
    id: String,
    metric: String,
    severity: String,
    current_value: f32,
    message: String,
    fired_at: String,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde::Serialize;

    const GOLDEN: &[u8] = include_bytes!("../../../testdata/mail-report-v1.msgpack");
    const RUN: &str = "6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b";

    /// RFC 0017 §2: the agent's v1 golden parses into the report it describes.
    #[test]
    fn the_v1_golden_parses_into_its_report() {
        let report = decode(GOLDEN);
        assert!(report.is_ok(), "the golden parses: {report:?}");
        let report = report.unwrap();

        assert_eq!(
            report.id,
            ReportId {
                run: Uuid::parse_str(RUN).unwrap(),
                seq: 7
            }
        );
        assert_eq!(report.created_at.seconds(), 1_700_000_300);
        assert_eq!(report.interval.secs(), 300);
        assert_eq!(report.reason, ReportReason::Scheduled);
        let times: Vec<i64> = report.snapshots.iter().map(|s| s.time.seconds()).collect();
        assert_eq!(times, [1_700_000_240, 1_700_000_300]);
        let newest = &report.snapshots[1];
        assert_eq!(
            report.snapshots[0].info, None,
            "only the newest has the info"
        );
        assert_eq!(
            newest.info.as_ref().map(|info| info.hostname.as_str()),
            Some("web-01")
        );
        assert_eq!(newest.reported.cpu, Some(12.5));
        assert_eq!(newest.reported.load5, Some(0.25));
        assert_eq!(
            newest.reported.disks,
            [ReportedDisk {
                mount_point: Some("/".into()),
                usage_percent: Some(50.0)
            }]
        );
        assert_eq!(
            newest.uptime.as_ref().map(UptimeDisplay::as_str),
            Some("1h 0m")
        );
        assert_eq!(
            newest.memory,
            MemoryCapacity::reported(Some("8 KB"), Some(8192))
        );
        assert_eq!(
            report.alerts,
            [MailedAlert {
                id: format!("{RUN}-1"),
                metric: "cpu".into(),
                severity: "warning".into(),
                current_value: 95.0,
                message: "CPU high".into(),
                fired_at: "2023-11-14T22:18:20Z".into(),
            }]
        );
        assert!(report.round.is_none());
    }

    /// A report as a test writes it, with `rmp_serde::to_vec_named`: the golden pins the
    /// agent's encoding, so the variants only need the same shape.
    #[derive(Serialize, Clone)]
    pub(crate) struct TestReport {
        pub(crate) kind: String,
        pub(crate) run: String,
        pub(crate) seq: u64,
        pub(crate) created_at: u64,
        pub(crate) interval_secs: u64,
        pub(crate) reason: String,
        pub(crate) snapshots: Vec<TestSnapshot>,
        pub(crate) alerts: Vec<TestAlert>,
        pub(crate) round: Option<()>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub(crate) later_field: Option<u64>,
    }

    #[derive(Serialize, Clone)]
    pub(crate) struct TestSnapshot {
        pub(crate) collected_at: u64,
        pub(crate) info: Option<()>,
        pub(crate) cpu_percent: f32,
        pub(crate) memory_percent: f32,
        pub(crate) memory_used_bytes: u64,
        pub(crate) memory_total_bytes: u64,
        pub(crate) memory_total_display: String,
        pub(crate) swap_percent: f32,
        pub(crate) load_one: f64,
        pub(crate) load_five: f64,
        pub(crate) load_fifteen: f64,
        pub(crate) uptime_seconds: u64,
        pub(crate) uptime_display: String,
        pub(crate) disks: Vec<()>,
    }

    #[derive(Serialize, Clone)]
    struct TestAlert {
        id: String,
        metric: String,
        severity: String,
        current_value: f32,
        message: String,
        fired_at: String,
    }

    pub(crate) const CREATED: u64 = 1_700_000_300;

    pub(crate) fn sample(collected_at: u64) -> TestSnapshot {
        TestSnapshot {
            collected_at,
            info: None,
            cpu_percent: 1.0,
            memory_percent: 2.0,
            memory_used_bytes: 1,
            memory_total_bytes: 2,
            memory_total_display: "2 B".into(),
            swap_percent: 0.0,
            load_one: 0.1,
            load_five: 0.2,
            load_fifteen: 0.3,
            uptime_seconds: 1,
            uptime_display: "1m".into(),
            disks: Vec::new(),
        }
    }

    fn alert() -> TestAlert {
        TestAlert {
            id: "run-1".into(),
            metric: "cpu".into(),
            severity: "warning".into(),
            current_value: 95.0,
            message: "CPU high".into(),
            fired_at: "2023-11-14T22:18:20Z".into(),
        }
    }

    fn base() -> TestReport {
        TestReport {
            kind: KIND.into(),
            run: RUN.into(),
            seq: 1,
            created_at: CREATED,
            interval_secs: 300,
            reason: "scheduled".into(),
            snapshots: vec![sample(CREATED)],
            alerts: vec![alert()],
            round: None,
            later_field: None,
        }
    }

    pub(crate) fn with(change: impl FnOnce(&mut TestReport)) -> Vec<u8> {
        let mut report = base();
        change(&mut report);
        rmp_serde::to_vec_named(&report).unwrap()
    }

    /// RFC 0017 §2: every bound, each refused with its reason, and what a later agent may send
    /// that a v1 hub still reads.
    #[test]
    fn a_report_is_parsed_within_its_bounds() {
        use ReportRefusal::*;
        let earliest = CREATED - 300 - 120;
        let cases: Vec<(&str, Vec<u8>, Result<(), ReportRefusal>)> = vec![
            ("the base report", with(|_| {}), Ok(())),
            ("not MessagePack", b"\xc1".to_vec(), Err(NotAReport)),
            (
                "another kind",
                with(|r| r.kind = "mail-report.v2".into()),
                Err(UnknownKind),
            ),
            (
                "a run that isn't a UUID",
                with(|r| r.run = "run".into()),
                Err(BadRun),
            ),
            (
                "an interval of 59 s",
                with(|r| r.interval_secs = 59),
                Err(IntervalOutOfRange),
            ),
            (
                "an interval over a day",
                with(|r| r.interval_secs = 86_401),
                Err(IntervalOutOfRange),
            ),
            (
                "an interval of a day",
                with(|r| r.interval_secs = 86_400),
                Ok(()),
            ),
            (
                "created_at over i64",
                with(|r| r.created_at = u64::MAX),
                Err(TimeOutOfRange),
            ),
            ("no sample", with(|r| r.snapshots.clear()), Err(NoSnapshots)),
            (
                "61 samples",
                with(|r| r.snapshots = vec![sample(CREATED); 61]),
                Err(TooManySnapshots),
            ),
            (
                "60 samples",
                with(|r| r.snapshots = vec![sample(CREATED); 60]),
                Ok(()),
            ),
            (
                "a sample 5 s ahead",
                with(|r| r.snapshots = vec![sample(CREATED + 5)]),
                Ok(()),
            ),
            (
                "a sample 6 s ahead",
                with(|r| r.snapshots = vec![sample(CREATED + 6)]),
                Err(SampleOutOfBounds),
            ),
            (
                "the earliest sample",
                with(|r| r.snapshots = vec![sample(earliest)]),
                Ok(()),
            ),
            (
                "a sample before the earliest",
                with(|r| r.snapshots = vec![sample(earliest - 1)]),
                Err(SampleOutOfBounds),
            ),
            (
                "samples out of order",
                with(|r| r.snapshots = vec![sample(CREATED), sample(CREATED - 60)]),
                Err(SampleOutOfBounds),
            ),
            (
                "65 alerts",
                with(|r| r.alerts = vec![alert(); 65]),
                Err(TooManyAlerts),
            ),
            ("64 alerts", with(|r| r.alerts = vec![alert(); 64]), Ok(())),
            (
                "a 513-byte message",
                with(|r| r.alerts[0].message = "m".repeat(513)),
                Err(FieldTooLong),
            ),
            (
                "a 65-byte severity",
                with(|r| r.alerts[0].severity = "s".repeat(65)),
                Err(FieldTooLong),
            ),
            ("a later key", with(|r| r.later_field = Some(1)), Ok(())),
        ];
        for (name, bytes, expected) in cases {
            assert_eq!(decode(&bytes).map(|_| ()), expected, "case {name}");
        }
    }

    /// RFC 0017 §2: closed sets stay open at the wire: an unknown reason is `Other`, and an
    /// unknown metric or severity is kept as sent.
    #[test]
    fn a_later_agents_new_variants_are_read_not_refused() {
        let bytes = with(|r| {
            r.reason = "shutdown".into();
            r.alerts[0].metric = "gpu".into();
            r.alerts[0].severity = "emergency".into();
        });

        let report = decode(&bytes);
        assert!(report.is_ok(), "the report parses: {report:?}");
        let report = report.unwrap();

        assert_eq!(report.reason, ReportReason::Other);
        assert_eq!(report.alerts[0].metric, "gpu");
        assert_eq!(report.alerts[0].severity, "emergency");
    }
}
