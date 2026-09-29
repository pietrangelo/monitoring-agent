//! Fleet History: the snapshot rule (RFC 0007 §1). Both paths, push and poll, turn what an
//! agent reported into a `ReportedSnapshot` at their edge; the rule turns that into the
//! `Snapshot` the hub keeps, and counts what it left out. Pure: no I/O, no clock reads.

use std::time::Instant;

use crate::hourly_warning::{HourlyWarning, hourly_warning};

/// A snapshot as an adapter read it, before the snapshot rule. `None`: not reported.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReportedSnapshot {
    pub cpu: Option<f32>,
    pub memory: Option<f32>,
    pub swap: Option<f32>,
    pub load1: Option<f64>,
    pub load5: Option<f64>,
    /// In reported order.
    pub disks: Vec<ReportedDisk>,
}

/// One disk as an adapter read it, before the snapshot rule.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReportedDisk {
    pub mount_point: Option<String>,
    pub usage_percent: Option<f32>,
}

/// A snapshot as the hub keeps it. History and live metrics are both built from it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// Finite, at most one per scalar.
    scalars: Vec<(Scalar, f32)>,
    /// Finite, at most `MAX_DISKS`, in reported order.
    disks: Vec<(MountPoint, f32)>,
}

/// A snapshot's scalar metrics: metric names `cpu`, `memory`, `swap`, `load1`, `load5`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scalar {
    Cpu,
    Memory,
    Swap,
    Load1,
    Load5,
}

impl Scalar {
    /// The scalar's metric name.
    pub fn metric_name(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::Swap => "swap",
            Self::Load1 => "load1",
            Self::Load5 => "load5",
        }
    }
}

/// A disk's mount point: 1..=`MAX_MOUNT_POINT_BYTES` bytes, no control character.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountPoint(String);

/// A mount point the snapshot rule refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidMountPoint;

impl TryFrom<String> for MountPoint {
    type Error = InvalidMountPoint;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let within_length = (1..=MAX_MOUNT_POINT_BYTES).contains(&value.len());
        match within_length && !value.chars().any(char::is_control) {
            true => Ok(Self(value)),
            false => Err(InvalidMountPoint),
        }
    }
}

/// When a snapshot was taken, in Unix seconds: at most `i64::MAX` (SQLite's integer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotTime(i64);

/// A snapshot time above `i64::MAX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotTimeOutOfRange;

impl SnapshotTime {
    /// The time in Unix seconds.
    pub fn seconds(self) -> i64 {
        self.0
    }

    /// The time `retention_secs` before this one, never before 0: a series' points older
    /// than it are past their retention.
    pub fn cutoff(self, retention_secs: u64) -> Self {
        Self(self.0.saturating_sub_unsigned(retention_secs).max(0))
    }
}

impl TryFrom<u64> for SnapshotTime {
    type Error = SnapshotTimeOutOfRange;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        i64::try_from(value)
            .map(Self)
            .map_err(|_| SnapshotTimeOutOfRange)
    }
}

/// What the snapshot rule left out of one snapshot, by reason.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LeftOut {
    pub not_reported: usize,
    pub not_finite: usize,
    pub invalid_mount_point: usize,
    pub over_disk_limit: usize,
}

impl LeftOut {
    /// Whether the rule left nothing out.
    pub fn is_nothing(&self) -> bool {
        *self == Self::default()
    }

    fn count(&mut self, reason: Refusal) {
        let counter = match reason {
            Refusal::NotReported => &mut self.not_reported,
            Refusal::NotFinite => &mut self.not_finite,
            Refusal::InvalidMountPoint => &mut self.invalid_mount_point,
        };
        *counter += 1;
    }
}

/// Why the snapshot rule left out one value, before the disk limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    NotReported,
    NotFinite,
    InvalidMountPoint,
}

/// The most disks a snapshot keeps.
pub const MAX_DISKS: usize = 1024;
/// The longest mount point a snapshot keeps, in bytes.
pub const MAX_MOUNT_POINT_BYTES: usize = 256;

/// The snapshot rule: keeps every reported, finite value, and the first `MAX_DISKS` disks
/// that pass, and counts the rest by reason.
pub fn snapshot_rule(reported: ReportedSnapshot) -> (Snapshot, LeftOut) {
    let mut left_out = LeftOut::default();
    let scalars = keep_scalars(&reported, &mut left_out);
    let disks = keep_disks(reported.disks, &mut left_out);
    (Snapshot { scalars, disks }, left_out)
}

/// The reported scalars that pass, in scalar order; loads are narrowed to `f32` first.
fn keep_scalars(reported: &ReportedSnapshot, left_out: &mut LeftOut) -> Vec<(Scalar, f32)> {
    let narrow = |load: Option<f64>| load.map(|value| value as f32);
    [
        (Scalar::Cpu, reported.cpu),
        (Scalar::Memory, reported.memory),
        (Scalar::Swap, reported.swap),
        (Scalar::Load1, narrow(reported.load1)),
        (Scalar::Load5, narrow(reported.load5)),
    ]
    .into_iter()
    .filter_map(|(scalar, value)| match finite(value) {
        Ok(kept) => Some((scalar, kept)),
        Err(reason) => {
            left_out.count(reason);
            None
        }
    })
    .collect()
}

/// The first `MAX_DISKS` disks that pass, in reported order.
fn keep_disks(disks: Vec<ReportedDisk>, left_out: &mut LeftOut) -> Vec<(MountPoint, f32)> {
    disks.into_iter().fold(Vec::new(), |mut kept, disk| {
        match (disk_rule(disk), kept.len() < MAX_DISKS) {
            (Err(reason), _) => left_out.count(reason),
            (Ok(passing), true) => kept.push(passing),
            (Ok(_), false) => left_out.over_disk_limit += 1,
        }
        kept
    })
}

/// One disk: its mount point is checked before its usage.
fn disk_rule(disk: ReportedDisk) -> Result<(MountPoint, f32), Refusal> {
    let mount_point = disk
        .mount_point
        .ok_or(InvalidMountPoint)
        .and_then(MountPoint::try_from)
        .map_err(|InvalidMountPoint| Refusal::InvalidMountPoint)?;
    Ok((mount_point, finite(disk.usage_percent)?))
}

fn finite(value: Option<f32>) -> Result<f32, Refusal> {
    match value {
        None => Err(Refusal::NotReported),
        Some(value) if value.is_finite() => Ok(value),
        Some(_) => Err(Refusal::NotFinite),
    }
}

impl Snapshot {
    /// The snapshot's metric points: cpu, memory, swap, load1, load5, then disk:<mount point>.
    pub fn metric_points(&self) -> impl Iterator<Item = (String, f32)> + '_ {
        let scalars = self
            .scalars
            .iter()
            .map(|(scalar, value)| (scalar.metric_name().to_string(), *value));
        let disks = self
            .disks
            .iter()
            .map(|(MountPoint(mount), value)| (format!("disk:{mount}"), *value));
        scalars.chain(disks)
    }

    /// The kept value of `scalar`; `None` when the rule left it out.
    pub fn scalar(&self, scalar: Scalar) -> Option<f32> {
        self.scalars
            .iter()
            .find(|(kept, _)| *kept == scalar)
            .map(|(_, value)| *value)
    }

    /// The kept disks, as mount point and usage, in reported order.
    pub fn disks(&self) -> impl Iterator<Item = (&str, f32)> + '_ {
        self.disks
            .iter()
            .map(|(MountPoint(mount), usage)| (mount.as_str(), *usage))
    }
}

/// How one snapshot's left-out values are logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftOutLog {
    Nothing,
    Warn,
    Debug,
}

/// Decides from when this system last warned, and returns the warning time the system's
/// next live metrics keep: `previous` unchanged, or `now` after a `Warn`.
pub fn left_out_log(
    previous: Option<Instant>,
    left_out: &LeftOut,
    now: Instant,
) -> (LeftOutLog, Option<Instant>) {
    if left_out.is_nothing() {
        return (LeftOutLog::Nothing, previous);
    }
    match hourly_warning(previous, now) {
        HourlyWarning::Warn => (LeftOutLog::Warn, Some(now)),
        HourlyWarning::Quiet => (LeftOutLog::Debug, previous),
    }
}

/// A system's latest snapshot, as the snapshot rule kept it, held for the dashboard (RFC 0007
/// §4), with when its left-out values were last logged at `warn`. Built only by `following`,
/// so no entry starts its warning time from a default.
#[derive(Debug)]
pub struct LiveMetrics {
    snapshot: Snapshot,
    time: SnapshotTime,
    left_out_warned_at: Option<Instant>,
}

impl LiveMetrics {
    /// The entry that follows `previous` with a newly stored snapshot, and how the snapshot's
    /// left-out values are logged. The warning time comes from `left_out_log` alone, so a host
    /// leaving something out of every snapshot warns once an hour, not every snapshot.
    pub fn following(
        previous: Option<&LiveMetrics>,
        snapshot: Snapshot,
        time: SnapshotTime,
        left_out: &LeftOut,
        now: Instant,
    ) -> (Self, LeftOutLog) {
        let warned_at = previous.and_then(|entry| entry.left_out_warned_at);
        let (log, left_out_warned_at) = left_out_log(warned_at, left_out, now);
        let entry = Self {
            snapshot,
            time,
            left_out_warned_at,
        };
        (entry, log)
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub fn time(&self) -> SnapshotTime {
        self.time
    }

    /// When this system's left-out values were last logged at `warn`.
    #[cfg(test)]
    pub fn left_out_warned_at(&self) -> Option<Instant> {
        self.left_out_warned_at
    }
}

/// The retention of a metric with no valid `metric_retention` row: a day, in seconds.
const DEFAULT_RETENTION_SECS: u64 = 86_400;

/// A metric's retention in seconds, from its stored `metric_retention` value: no row or a
/// negative value is a day, otherwise the stored value.
pub fn snapshot_retention(stored: Option<i64>) -> u64 {
    stored
        .and_then(|secs| u64::try_from(secs).ok())
        .unwrap_or(DEFAULT_RETENTION_SECS)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The kept snapshot, built from its parts.
    fn kept(scalars: &[(Scalar, f32)], disks: &[(&str, f32)]) -> Snapshot {
        Snapshot {
            scalars: scalars.to_vec(),
            disks: disks
                .iter()
                .map(|(mount, usage)| (MountPoint((*mount).to_string()), *usage))
                .collect(),
        }
    }

    fn disk(mount_point: &str, usage_percent: f32) -> ReportedDisk {
        ReportedDisk {
            mount_point: Some(mount_point.to_string()),
            usage_percent: Some(usage_percent),
        }
    }

    /// Every scalar reported and finite.
    fn all_scalars(disks: Vec<ReportedDisk>) -> ReportedSnapshot {
        ReportedSnapshot {
            cpu: Some(10.0),
            memory: Some(20.0),
            swap: Some(30.0),
            load1: Some(1.5),
            load5: Some(2.5),
            disks,
        }
    }

    const ALL_SCALARS: [(Scalar, f32); 5] = [
        (Scalar::Cpu, 10.0),
        (Scalar::Memory, 20.0),
        (Scalar::Swap, 30.0),
        (Scalar::Load1, 1.5),
        (Scalar::Load5, 2.5),
    ];

    /// `n` valid disks, `/d0`, `/d1`, … with usage 0, 1, …
    fn valid_disks(n: usize) -> Vec<ReportedDisk> {
        (0..n).map(|i| disk(&format!("/d{i}"), i as f32)).collect()
    }

    fn kept_disks(range: std::ops::Range<usize>) -> Vec<(MountPoint, f32)> {
        range
            .map(|i| (MountPoint(format!("/d{i}")), i as f32))
            .collect()
    }

    fn left_out(
        not_reported: usize,
        not_finite: usize,
        invalid_mount_point: usize,
        over_disk_limit: usize,
    ) -> LeftOut {
        LeftOut {
            not_reported,
            not_finite,
            invalid_mount_point,
            over_disk_limit,
        }
    }

    #[test]
    fn the_snapshot_rule_keeps_valid_disks_up_to_the_limit() {
        let cases = [
            ("0 valid disks", 0, 0..0, 0),
            ("1 valid disk", 1, 0..1, 0),
            ("1024 valid disks", 1024, 0..1024, 0),
            ("1025 valid disks", 1025, 0..1024, 1),
        ];
        for (case, reported, kept_range, over) in cases {
            let (snapshot, counts) = snapshot_rule(all_scalars(valid_disks(reported)));
            assert_eq!(snapshot.scalars, ALL_SCALARS.to_vec(), "case: {case}");
            assert_eq!(snapshot.disks, kept_disks(kept_range), "case: {case}");
            assert_eq!(counts, left_out(0, 0, 0, over), "case: {case}");
        }
    }

    #[test]
    fn the_disk_limit_holds_across_a_sweep_of_disk_counts() {
        for reported in 0..=MAX_DISKS + 6 {
            let case = format!("{reported} valid disks");
            let (snapshot, counts) = snapshot_rule(all_scalars(valid_disks(reported)));
            let kept = reported.min(1024);
            assert_eq!(snapshot.disks, kept_disks(0..kept), "case: {case}");
            assert_eq!(
                counts,
                left_out(0, 0, 0, reported.saturating_sub(1024)),
                "case: {case}"
            );
        }
    }

    #[test]
    fn the_disk_limit_counts_only_disks_that_pass() {
        let absent = ReportedDisk {
            mount_point: None,
            usage_percent: Some(1.0),
        };
        let not_finite = ReportedDisk {
            mount_point: Some("/nan".to_string()),
            usage_percent: Some(f32::NAN),
        };
        // (name, reported, kept indices of `/dN`, left out)
        let cases = [
            (
                "1025 reported, 2 invalid (first and last): 1023 kept, none over",
                {
                    let mut disks = valid_disks(1025);
                    disks[0] = absent.clone();
                    disks[1024] = not_finite.clone();
                    disks
                },
                (1..1024).collect::<Vec<_>>(),
                left_out(0, 1, 1, 0),
            ),
            (
                "1025 reported, 2 invalid (both at the start): 1023 kept, none over",
                {
                    let mut disks = valid_disks(1025);
                    disks[0] = absent.clone();
                    disks[1] = absent.clone();
                    disks
                },
                (2..1025).collect(),
                left_out(0, 0, 2, 0),
            ),
            (
                "1026 reported, 1 invalid first: the next 1024 kept, one over",
                {
                    let mut disks = valid_disks(1026);
                    disks[0] = absent.clone();
                    disks
                },
                (1..1025).collect(),
                left_out(0, 0, 1, 1),
            ),
            (
                "1027 reported, 1 invalid in the middle: first 1024 valid kept, two over",
                {
                    let mut disks = valid_disks(1027);
                    disks[500] = not_finite.clone();
                    disks
                },
                (0..500).chain(501..1025).collect(),
                left_out(0, 1, 0, 2),
            ),
            (
                "1024 valid, then absent mount, NaN usage, one valid: invalid ones keep their reason",
                {
                    let mut disks = valid_disks(1024);
                    disks.push(absent.clone());
                    disks.push(not_finite.clone());
                    disks.push(disk("/d1024", 1024.0));
                    disks
                },
                (0..1024).collect(),
                left_out(0, 1, 1, 1),
            ),
        ];
        for (case, disks, kept_indices, expected) in cases {
            let (snapshot, counts) = snapshot_rule(all_scalars(disks));
            let expected_disks: Vec<_> = kept_indices
                .into_iter()
                .map(|i| (MountPoint(format!("/d{i}")), i as f32))
                .collect();
            assert_eq!(snapshot.disks.len(), expected_disks.len(), "case: {case}");
            assert_eq!(snapshot.disks, expected_disks, "case: {case}");
            assert_eq!(counts, expected, "case: {case}");
        }
    }

    // A disk wrong in both its mount point and its usage counts once, under the mount
    // point: the rule checks mount points before usage, in the RFC's order.
    #[test]
    fn a_disk_wrong_in_both_mount_point_and_usage_counts_under_the_mount_point() {
        let cases = [
            ("absent mount, usage not reported", None, None),
            ("absent mount, NaN usage", None, Some(f32::NAN)),
            (
                "empty mount, +inf usage",
                Some(String::new()),
                Some(f32::INFINITY),
            ),
            (
                "tab in mount, not reported",
                Some("/a\tb".to_string()),
                None,
            ),
        ];
        for (case, mount_point, usage_percent) in cases {
            let reported = all_scalars(vec![
                disk("/", 1.0),
                ReportedDisk {
                    mount_point,
                    usage_percent,
                },
            ]);
            let (snapshot, counts) = snapshot_rule(reported);
            assert_eq!(snapshot, kept(&ALL_SCALARS, &[("/", 1.0)]), "case: {case}");
            assert_eq!(counts, left_out(0, 0, 1, 0), "case: {case}");
        }
    }

    /// Runs the rule on `/before`, the mount point under test, and `/after`, and returns
    /// whether the middle one was kept, checking its neighbours are kept either way.
    fn mount_point_kept(case: &str, mount_point: Option<String>) -> bool {
        let reported = all_scalars(vec![
            disk("/before", 1.0),
            ReportedDisk {
                mount_point: mount_point.clone(),
                usage_percent: Some(2.0),
            },
            disk("/after", 3.0),
        ]);
        let (snapshot, counts) = snapshot_rule(reported);
        let is_kept = counts == LeftOut::default();
        let expected = match (&mount_point, is_kept) {
            (Some(mount), true) => kept(
                &ALL_SCALARS,
                &[("/before", 1.0), (mount.as_str(), 2.0), ("/after", 3.0)],
            ),
            _ => kept(&ALL_SCALARS, &[("/before", 1.0), ("/after", 3.0)]),
        };
        assert_eq!(snapshot, expected, "case: {case}");
        if !is_kept {
            assert_eq!(counts, left_out(0, 0, 1, 0), "case: {case}");
        }
        is_kept
    }

    #[test]
    fn a_mount_point_is_kept_only_within_256_bytes_and_without_control_characters() {
        let a = |n: usize| "a".repeat(n);
        let cases = [
            ("/", Some("/".to_string()), true),
            ("256 ascii bytes", Some(a(256)), true),
            ("257 ascii bytes", Some(a(257)), false),
            (
                "256 bytes ending in a 2-byte char",
                Some(a(254) + "é"),
                true,
            ),
            (
                "257 bytes, a 2-byte char across the bound",
                Some(a(255) + "é"),
                false,
            ),
            ("non-ascii path", Some("/mnt/café".to_string()), true),
            ("a space", Some("/mnt/my disk".to_string()), true),
            ("empty", Some(String::new()), false),
            ("absent", None, false),
            ("a tab", Some("/mnt/a\tb".to_string()), false),
            ("a newline", Some("/mnt/a\nb".to_string()), false),
            ("DEL", Some("/mnt/a\u{7f}b".to_string()), false),
            (
                "NEL, a C1 control",
                Some("/mnt/a\u{85}b".to_string()),
                false,
            ),
            ("only a newline", Some("\n".to_string()), false),
            (
                "U+2028 is not a control character",
                Some("/mnt/a\u{2028}b".to_string()),
                true,
            ),
        ];
        for (case, mount_point, expected) in cases {
            assert_eq!(
                mount_point_kept(case, mount_point),
                expected,
                "case: {case}"
            );
        }
    }

    #[test]
    fn a_mount_point_length_sweep_keeps_1_to_256_bytes() {
        for len in 0..=300 {
            let case = format!("{len} ascii bytes");
            let expected = (1..=MAX_MOUNT_POINT_BYTES).contains(&len);
            assert_eq!(
                mount_point_kept(&case, Some("/".repeat(len))),
                expected,
                "case: {case}"
            );
        }
        for len in 250..=260 {
            let mount = "b".repeat(len - 3) + "€"; // a 3-byte char ending at `len` bytes
            let case = format!("{len} bytes ending in a 3-byte char");
            assert_eq!(
                mount_point_kept(&case, Some(mount)),
                len <= 256,
                "case: {case}"
            );
        }
    }

    #[test]
    fn a_mount_point_character_sweep_refuses_c0_del_and_c1() {
        let is_c0_del_or_c1 = |c: u32| c <= 0x1f || (0x7f..=0x9f).contains(&c);
        let chars = (0..=0x2ffu32).chain([0x2028, 0x2029, 0xfeff, 0xfffd, 0x1f600]);
        for code in chars {
            let Some(c) = char::from_u32(code) else {
                continue;
            };
            let case = format!("/mnt/x{c}y (U+{code:04X})");
            assert_eq!(
                mount_point_kept(&case, Some(format!("/mnt/x{c}y"))),
                !is_c0_del_or_c1(code),
                "case: {case}"
            );
        }
    }

    /// Writes `value` into one scalar of `reported`, the loads as `f64`.
    fn set_scalar(reported: &mut ReportedSnapshot, scalar: Scalar, value: Option<f64>) {
        let narrow = value.map(|v| v as f32);
        match scalar {
            Scalar::Cpu => reported.cpu = narrow,
            Scalar::Memory => reported.memory = narrow,
            Scalar::Swap => reported.swap = narrow,
            Scalar::Load1 => reported.load1 = value,
            Scalar::Load5 => reported.load5 = value,
        }
    }

    const DISKS: [(&str, f32); 2] = [("/", 40.0), ("/home", 50.0)];

    fn two_disks() -> Vec<ReportedDisk> {
        DISKS.iter().map(|(m, u)| disk(m, *u)).collect()
    }

    #[test]
    fn a_scalar_not_reported_or_not_finite_is_left_out_alone() {
        let bad = [
            ("not reported", None, left_out(1, 0, 0, 0)),
            ("NaN", Some(f64::NAN), left_out(0, 1, 0, 0)),
            ("+inf", Some(f64::INFINITY), left_out(0, 1, 0, 0)),
            ("-inf", Some(f64::NEG_INFINITY), left_out(0, 1, 0, 0)),
        ];
        for (scalar, _) in ALL_SCALARS {
            for (label, value, expected_counts) in bad {
                let case = format!("{scalar:?} {label}");
                let mut reported = all_scalars(two_disks());
                set_scalar(&mut reported, scalar, value);
                let (snapshot, counts) = snapshot_rule(reported);
                let rest: Vec<_> = ALL_SCALARS
                    .into_iter()
                    .filter(|(s, _)| *s != scalar)
                    .collect();
                assert_eq!(snapshot, kept(&rest, &DISKS), "case: {case}");
                assert_eq!(counts, expected_counts, "case: {case}");
            }
        }
    }

    #[test]
    fn a_load_is_narrowed_to_f32_before_the_finite_check() {
        // (name, load, kept as)
        let cases = [
            ("1e300: finite as f64, infinite as f32", 1e300, None),
            ("-1e300", -1e300, None),
            ("1e39, just past f32", 1e39, None),
            ("f32::MAX", f64::from(f32::MAX), Some(f32::MAX)),
            ("f32::MIN", f64::from(f32::MIN), Some(f32::MIN)),
            ("zero", 0.0, Some(0.0)),
            ("0.75", 0.75, Some(0.75)),
        ];
        for scalar in [Scalar::Load1, Scalar::Load5] {
            for (label, load, expected) in cases {
                let case = format!("{scalar:?} {label}");
                let mut reported = all_scalars(two_disks());
                set_scalar(&mut reported, scalar, Some(load));
                let (snapshot, counts) = snapshot_rule(reported);
                let scalars: Vec<_> = ALL_SCALARS
                    .into_iter()
                    .filter_map(|(s, v)| match (s == scalar, expected) {
                        (false, _) => Some((s, v)),
                        (true, kept_as) => kept_as.map(|k| (s, k)),
                    })
                    .collect();
                assert_eq!(snapshot, kept(&scalars, &DISKS), "case: {case}");
                let not_finite = usize::from(expected.is_none());
                assert_eq!(counts, left_out(0, not_finite, 0, 0), "case: {case}");
            }
        }
    }

    #[test]
    fn finite_extremes_are_kept_as_reported() {
        let values = [
            f32::MAX,
            f32::MIN,
            -0.0,
            f32::MIN_POSITIVE,
            f32::from_bits(1), // the smallest subnormal
            -5.0,
            250.0,
        ];
        for (scalar, _) in ALL_SCALARS {
            for value in values {
                let case = format!("{scalar:?} {value:e}");
                let mut reported = all_scalars(vec![disk("/", value)]);
                set_scalar(&mut reported, scalar, Some(f64::from(value)));
                let (snapshot, counts) = snapshot_rule(reported);
                let scalars: Vec<_> = ALL_SCALARS
                    .into_iter()
                    .map(|(s, v)| if s == scalar { (s, value) } else { (s, v) })
                    .collect();
                assert_eq!(snapshot, kept(&scalars, &[("/", value)]), "case: {case}");
                // f32 `==` treats -0.0 and 0.0 as equal: compare bits so the sign is kept too.
                let kept_bits: Vec<u32> = snapshot
                    .scalars
                    .iter()
                    .map(|(_, v)| v.to_bits())
                    .chain(snapshot.disks.iter().map(|(_, v)| v.to_bits()))
                    .collect();
                let expected_bits: Vec<u32> = scalars
                    .iter()
                    .map(|(_, v)| v.to_bits())
                    .chain([value.to_bits()])
                    .collect();
                assert_eq!(kept_bits, expected_bits, "case: {case} (bits)");
                assert_eq!(counts, LeftOut::default(), "case: {case}");
            }
        }
    }

    #[test]
    fn a_disk_usage_not_reported_or_not_finite_leaves_out_that_disk_alone() {
        let cases = [
            ("usage not reported", None, left_out(1, 0, 0, 0)),
            ("usage NaN", Some(f32::NAN), left_out(0, 1, 0, 0)),
            ("usage +inf", Some(f32::INFINITY), left_out(0, 1, 0, 0)),
            ("usage -inf", Some(f32::NEG_INFINITY), left_out(0, 1, 0, 0)),
        ];
        for (case, usage, expected_counts) in cases {
            let reported = all_scalars(vec![
                disk("/", 40.0),
                ReportedDisk {
                    mount_point: Some("/bad".to_string()),
                    usage_percent: usage,
                },
                disk("/home", 50.0),
            ]);
            let (snapshot, counts) = snapshot_rule(reported);
            assert_eq!(snapshot, kept(&ALL_SCALARS, &DISKS), "case: {case}");
            assert_eq!(counts, expected_counts, "case: {case}");
        }
    }

    #[test]
    fn metric_points_name_the_scalars_then_the_disks_in_reported_order() {
        let (snapshot, _) = snapshot_rule(all_scalars(vec![
            disk("/z", 1.0),
            disk("/", 2.0),
            disk("/m", 3.0),
        ]));
        let expected = [
            ("cpu", 10.0),
            ("memory", 20.0),
            ("swap", 30.0),
            ("load1", 1.5),
            ("load5", 2.5),
            ("disk:/z", 1.0),
            ("disk:/", 2.0),
            ("disk:/m", 3.0),
        ]
        .map(|(name, value)| (name.to_string(), value));
        let points: Vec<_> = snapshot.metric_points().collect();
        assert_eq!(points, expected.to_vec(), "case: every scalar and 3 disks");
    }

    #[test]
    fn a_snapshot_reads_back_each_kept_scalar_and_none_for_one_left_out() {
        let snapshot = kept(
            &[(Scalar::Memory, 20.0), (Scalar::Load5, 2.5)],
            &[("/", 50.0), ("/home", 70.0)],
        );
        let cases = [
            ("cpu, left out", Scalar::Cpu, None),
            ("memory, kept", Scalar::Memory, Some(20.0)),
            ("swap, left out", Scalar::Swap, None),
            ("load1, left out", Scalar::Load1, None),
            ("load5, kept", Scalar::Load5, Some(2.5)),
        ];
        for (case, scalar, expected) in cases {
            assert_eq!(snapshot.scalar(scalar), expected, "case: {case}");
        }
        let disks: Vec<(&str, f32)> = snapshot.disks().collect();
        assert_eq!(
            disks,
            [("/", 50.0), ("/home", 70.0)],
            "disks in reported order"
        );
    }

    #[test]
    fn metric_points_keep_the_scalar_order_for_every_subset_of_scalars() {
        let names = ["cpu", "memory", "swap", "load1", "load5"];
        for mask in 0u8..32 {
            let case = format!("scalars reported mask {mask:05b}");
            let mut reported = all_scalars(vec![disk("/data", 7.0)]);
            let mut expected = Vec::new();
            for (bit, ((scalar, value), name)) in ALL_SCALARS.into_iter().zip(names).enumerate() {
                if mask & (1 << bit) == 0 {
                    set_scalar(&mut reported, scalar, None);
                } else {
                    expected.push((name.to_string(), value));
                }
            }
            expected.push(("disk:/data".to_string(), 7.0));
            let (snapshot, counts) = snapshot_rule(reported);
            let points: Vec<_> = snapshot.metric_points().collect();
            assert_eq!(points, expected, "case: {case}");
            let missing = 5 - mask.count_ones() as usize;
            assert_eq!(counts, left_out(missing, 0, 0, 0), "case: {case}");
        }
    }

    #[test]
    fn a_snapshot_time_is_at_most_i64_max() {
        let max = i64::MAX as u64;
        let cases = [
            ("0", 0, Ok(0)),
            ("1", 1, Ok(1)),
            ("a 2026 time", 1_790_000_000, Ok(1_790_000_000)),
            ("i64::MAX - 1", max - 1, Ok(i64::MAX - 1)),
            ("i64::MAX", max, Ok(i64::MAX)),
            ("i64::MAX + 1", max + 1, Err(SnapshotTimeOutOfRange)),
            ("i64::MAX + 2", max + 2, Err(SnapshotTimeOutOfRange)),
            ("u64::MAX", u64::MAX, Err(SnapshotTimeOutOfRange)),
        ];
        for (case, input, expected) in cases {
            let time = SnapshotTime::try_from(input).map(SnapshotTime::seconds);
            assert_eq!(time, expected, "case: {case}");
        }
    }

    #[test]
    fn a_snapshot_time_sweep_keeps_every_value_up_to_i64_max() {
        // Powers of two and their neighbours, across the whole u64 range.
        let inputs = (0..64).flat_map(|p| {
            let v = 1u64 << p;
            [v - 1, v, v + 1]
        });
        for input in inputs.chain([u64::MAX - 1, u64::MAX]) {
            let case = format!("{input}");
            let expected = i64::try_from(input).map_err(|_| SnapshotTimeOutOfRange);
            let time = SnapshotTime::try_from(input).map(SnapshotTime::seconds);
            assert_eq!(time, expected, "case: {case}");
        }
    }

    #[test]
    fn a_retention_cutoff_is_the_retention_before_the_time_and_never_before_zero() {
        let max = i64::MAX as u64;
        let cases = [
            (
                "a day before a 2026 time",
                1_790_000_000,
                86_400,
                1_789_913_600,
            ),
            ("an hour before", 100_000, 3_600, 96_400),
            ("no retention", 100_000, 0, 100_000),
            ("exactly the time since 0", 86_400, 86_400, 0),
            ("more than the time since 0", 5, 86_400, 0),
            ("at 0", 0, 86_400, 0),
            ("i64::MAX, no retention", max, 0, i64::MAX),
            ("i64::MAX, a day", max, 86_400, i64::MAX - 86_400),
            ("i64::MAX, all of it", max, max, 0),
            ("u64::MAX of retention", 1_790_000_000, u64::MAX, 0),
        ];
        for (case, time, retention, expected) in cases {
            let time = SnapshotTime::try_from(time).unwrap();
            assert_eq!(time.cutoff(retention).seconds(), expected, "case: {case}");
        }
    }

    #[test]
    fn left_out_values_warn_at_most_once_an_hour() {
        let base = Instant::now();
        let at = |secs: u64| base + Duration::from_secs(secs);
        let nothing = LeftOut::default();
        let something = left_out(0, 1, 0, 0);
        let cases = [
            (
                "nothing left out, no previous warning",
                None,
                nothing,
                at(10),
                (LeftOutLog::Nothing, None),
            ),
            (
                "nothing left out, a previous warning is kept",
                Some(at(0)),
                nothing,
                at(10),
                (LeftOutLog::Nothing, Some(at(0))),
            ),
            (
                "nothing left out, an hour after a warning, it is kept",
                Some(at(0)),
                nothing,
                at(7_200),
                (LeftOutLog::Nothing, Some(at(0))),
            ),
            (
                "something left out, no previous warning",
                None,
                something,
                at(10),
                (LeftOutLog::Warn, Some(at(10))),
            ),
            (
                "something left out, 59 min 59 s after a warning",
                Some(at(0)),
                something,
                at(3_599),
                (LeftOutLog::Debug, Some(at(0))),
            ),
            (
                "something left out, exactly 1 h after a warning",
                Some(at(0)),
                something,
                at(3_600),
                (LeftOutLog::Warn, Some(at(3_600))),
            ),
        ];
        for (case, previous, counts, now, expected) in cases {
            assert_eq!(
                left_out_log(previous, &counts, now),
                expected,
                "case: {case}"
            );
        }
    }

    #[test]
    fn left_out_log_sweeps_every_reason_and_the_hour_from_both_sides() {
        let base = Instant::now();
        let at = |secs: u64| base + Duration::from_secs(secs);
        let reasons = [
            ("not reported", left_out(1, 0, 0, 0)),
            ("not finite", left_out(0, 2, 0, 0)),
            ("invalid mount point", left_out(0, 0, 3, 0)),
            ("over the disk limit", left_out(0, 0, 0, 4)),
            ("every reason", left_out(1, 1, 1, 1)),
        ];
        let seconds = [0, 1, 60, 1_800, 3_598, 3_599, 3_600, 3_601, 7_200, 86_400];
        for (reason, counts) in reasons {
            for secs in seconds {
                let case = format!("{reason}, {secs} s after a warning at 100 s");
                let now = at(100 + secs);
                let expected = if secs < 3_600 {
                    (LeftOutLog::Debug, Some(at(100)))
                } else {
                    (LeftOutLog::Warn, Some(now))
                };
                assert_eq!(
                    left_out_log(Some(at(100)), &counts, now),
                    expected,
                    "case: {case}"
                );
                let case = format!("{reason}, never warned, now at {secs} s");
                assert_eq!(
                    left_out_log(None, &counts, at(secs)),
                    (LeftOutLog::Warn, Some(at(secs))),
                    "case: {case}"
                );
                let case = format!("nothing left out, {secs} s after a warning at 100 s");
                assert_eq!(
                    left_out_log(Some(at(100)), &LeftOut::default(), now),
                    (LeftOutLog::Nothing, Some(at(100))),
                    "case: {case}"
                );
            }
        }
        let case = "something left out, a clock behind the last warning";
        assert_eq!(
            left_out_log(Some(at(100)), &left_out(1, 0, 0, 0), at(0)),
            (LeftOutLog::Debug, Some(at(100))),
            "case: {case}"
        );
    }

    /// RFC 0007 §1, §4: an entry takes its warning time from the entry it follows, through
    /// `left_out_log`, never from a default.
    #[test]
    fn a_live_metrics_entry_carries_the_warning_time_over_from_the_one_it_follows() {
        let start = Instant::now();
        let now = start + Duration::from_secs(10);
        let entry = |warned_at: Option<Instant>| LiveMetrics {
            snapshot: Snapshot::default(),
            time: SnapshotTime(1),
            left_out_warned_at: warned_at,
        };
        let one_left_out = LeftOut {
            not_finite: 1,
            ..LeftOut::default()
        };
        let an_hour_before_now = now - Duration::from_secs(3_600);
        let cases = [
            (
                "first entry, nothing left out",
                None,
                LeftOut::default(),
                (LeftOutLog::Nothing, None),
            ),
            (
                "first entry, left out",
                None,
                one_left_out,
                (LeftOutLog::Warn, Some(now)),
            ),
            (
                "never warned, left out",
                Some(entry(None)),
                one_left_out,
                (LeftOutLog::Warn, Some(now)),
            ),
            (
                "warned 10 s ago, left out",
                Some(entry(Some(start))),
                one_left_out,
                (LeftOutLog::Debug, Some(start)),
            ),
            (
                "warned 10 s ago, nothing left out",
                Some(entry(Some(start))),
                LeftOut::default(),
                (LeftOutLog::Nothing, Some(start)),
            ),
            (
                "warned an hour ago, left out",
                Some(entry(Some(an_hour_before_now))),
                one_left_out,
                (LeftOutLog::Warn, Some(now)),
            ),
        ];
        for (case, previous, left_out, expected) in cases {
            let kept = kept(&[(Scalar::Cpu, 1.0)], &[("/", 2.0)]);
            let (next, log) = LiveMetrics::following(
                previous.as_ref(),
                kept.clone(),
                SnapshotTime(7),
                &left_out,
                now,
            );
            assert_eq!((log, next.left_out_warned_at()), expected, "case: {case}");
            assert_eq!(
                (next.snapshot(), next.time()),
                (&kept, SnapshotTime(7)),
                "case: {case}"
            );
        }
    }

    #[test]
    fn snapshot_retention_is_a_day_for_none_and_negatives() {
        let cases = [
            ("None", None, 86_400),
            ("0", Some(0), 0),
            ("3,600", Some(3_600), 3_600),
            ("i64::MAX", Some(i64::MAX), i64::MAX as u64),
            ("-1", Some(-1), 86_400),
            ("i64::MIN", Some(i64::MIN), 86_400),
        ];
        for (case, stored, expected) in cases {
            assert_eq!(snapshot_retention(stored), expected, "case: {case}");
        }
    }

    #[test]
    fn snapshot_retention_sweep_keeps_every_non_negative_value() {
        let values = (-5..=5)
            .chain(86_395..=86_405)
            .chain((0..63).flat_map(|p| [1i64 << p, -(1i64 << p)]))
            .chain([i64::MAX, i64::MAX - 1, i64::MIN, i64::MIN + 1]);
        for stored in values {
            let case = format!("{stored}");
            let expected = u64::try_from(stored).unwrap_or(86_400);
            assert_eq!(snapshot_retention(Some(stored)), expected, "case: {case}");
        }
    }
}
