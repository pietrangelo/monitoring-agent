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

//! Fleet History's view of Spring Boot applications (RFC 0009 §8): the scrape rounds agents
//! report, which of them the hub admits, how fresh the shown one is, and the metric points a
//! round adds. Pure: the adapters hand in parsed values and the time.

use std::collections::{BTreeMap, VecDeque};
use std::hash::{BuildHasher, Hash, Hasher, RandomState};
use std::time::{Duration, Instant};

use uuid::Uuid;

/// How long the hub keeps an `app:*` point without a `metric_retention` row of its own.
pub const APPLICATION_RETENTION_SECS: u64 = 86_400;

/// The most applications one round may report, as the agent's configuration allows.
pub const MAX_APPLICATIONS: usize = 16;

/// How an operator names an application: 1 to 64 bytes of `[A-Za-z0-9_.-]`, not `.` or `..`.
/// The agent's rule, declared again here: the hub trusts no agent to have applied it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ApplicationName(String);

impl ApplicationName {
    const MAX_LEN: usize = 64;

    pub fn parse(value: &str) -> Option<Self> {
        let allowed = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-');
        let valid = (1..=Self::MAX_LEN).contains(&value.len())
            && value.bytes().all(allowed)
            && value != "."
            && value != "..";
        valid.then(|| Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What an application's Actuator reported about its health, or that the agent couldn't
/// reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApplicationHealth {
    Up,
    Down,
    OutOfService,
    Unknown,
    Unreachable,
}

impl ApplicationHealth {
    /// Reads the wire name; one this hub doesn't know is `Unknown`, so a later agent's
    /// addition degrades instead of refusing the round.
    pub fn from_wire(name: &str) -> Self {
        match name {
            "up" => Self::Up,
            "down" => Self::Down,
            "out_of_service" => Self::OutOfService,
            "unreachable" => Self::Unreachable,
            _ => Self::Unknown,
        }
    }

    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::OutOfService => "out_of_service",
            Self::Unknown => "unknown",
            Self::Unreachable => "unreachable",
        }
    }
}

/// An application's version, not blank, cut to its first 64 `char`s.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ApplicationVersion(String);

impl ApplicationVersion {
    const MAX_CHARS: usize = 64;

    /// `None` for a blank version, which says nothing.
    pub fn parse(raw: &str) -> Option<Self> {
        let cut: String = raw.chars().take(Self::MAX_CHARS).collect();
        (!cut.trim().is_empty()).then_some(Self(cut))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One of the curated values an agent derives from an application's meters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ApplicationGauge {
    HeapUsedBytes,
    HeapMaxBytes,
    CpuPercent,
    LiveThreads,
    GcPausePercent,
    HttpRequestsPerSecond,
    HttpServerErrorsPerSecond,
    HttpMeanLatencyMs,
    DbConnectionsActive,
    UptimeSeconds,
}

impl ApplicationGauge {
    pub const ALL: [Self; 10] = [
        Self::HeapUsedBytes,
        Self::HeapMaxBytes,
        Self::CpuPercent,
        Self::LiveThreads,
        Self::GcPausePercent,
        Self::HttpRequestsPerSecond,
        Self::HttpServerErrorsPerSecond,
        Self::HttpMeanLatencyMs,
        Self::DbConnectionsActive,
        Self::UptimeSeconds,
    ];

    /// The name on the wire and in metric names (`app:<name>:<wire name>`).
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::HeapUsedBytes => "heap_used_bytes",
            Self::HeapMaxBytes => "heap_max_bytes",
            Self::CpuPercent => "cpu_percent",
            Self::LiveThreads => "live_threads",
            Self::GcPausePercent => "gc_pause_percent",
            Self::HttpRequestsPerSecond => "http_requests_per_second",
            Self::HttpServerErrorsPerSecond => "http_server_errors_per_second",
            Self::HttpMeanLatencyMs => "http_mean_latency_ms",
            Self::DbConnectionsActive => "db_connections_active",
            Self::UptimeSeconds => "uptime_seconds",
        }
    }

    pub fn from_wire(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|gauge| gauge.wire_name() == name)
    }
}

/// An application's gauges from one scrape: known gauges with finite values only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Gauges(BTreeMap<ApplicationGauge, f64>);

impl Gauges {
    /// Keeps the gauges this hub knows, with finite values, and skips the rest.
    pub fn from_wire<'a>(raw: impl IntoIterator<Item = (&'a str, f64)>) -> Self {
        Self(
            raw.into_iter()
                .filter(|(_, value)| value.is_finite())
                .filter_map(|(name, value)| Some((ApplicationGauge::from_wire(name)?, value)))
                .collect(),
        )
    }

    pub fn iter(&self) -> impl Iterator<Item = (ApplicationGauge, f64)> + '_ {
        self.0.iter().map(|(gauge, value)| (*gauge, *value))
    }
}

/// One application's report in a scrape round.
#[derive(Debug, Clone, PartialEq)]
pub struct ApplicationReport {
    pub name: ApplicationName,
    pub health: ApplicationHealth,
    pub version: Option<ApplicationVersion>,
    pub gauges: Gauges,
}

/// Why a reported scrape round was refused as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrapeRoundError {
    /// More than `MAX_APPLICATIONS`.
    TooManyApplications,
    InvalidApplicationName,
    /// Two reports under one name.
    DuplicateApplicationName,
    /// The round id's run isn't a UUID.
    InvalidRun,
    /// Outside 10..=3600 s.
    InvalidInterval,
}

/// Identifies a scrape round: the agent run that minted it and its place in that run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RoundId {
    run: Uuid,
    seq: u64,
}

impl RoundId {
    /// Parses the run as a UUID, so what the hub keeps of an id has a fixed size.
    pub fn parse(run: &str, seq: u64) -> Result<Self, ScrapeRoundError> {
        let run = Uuid::try_parse(run).map_err(|_| ScrapeRoundError::InvalidRun)?;
        Ok(Self { run, seq })
    }

    #[cfg(test)]
    pub fn run(self) -> Uuid {
        self.run
    }

    #[cfg(test)]
    pub fn seq(self) -> u64 {
        self.seq
    }
}

/// The time an agent leaves between two scrape rounds: 10 s to 3600 s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScrapeInterval(Duration);

impl ScrapeInterval {
    const SECS: std::ops::RangeInclusive<u64> = 10..=3600;

    pub fn from_secs(secs: u64) -> Result<Self, ScrapeRoundError> {
        Self::SECS
            .contains(&secs)
            .then(|| Self(Duration::from_secs(secs)))
            .ok_or(ScrapeRoundError::InvalidInterval)
    }

    pub fn as_secs(self) -> u64 {
        self.0.as_secs()
    }
}

/// Every application report from one pass of an agent over its applications: at most
/// `MAX_APPLICATIONS`, under distinct names.
#[derive(Debug, Clone, PartialEq)]
pub struct ScrapeRound {
    id: RoundId,
    interval: ScrapeInterval,
    applications: Vec<ApplicationReport>,
}

impl ScrapeRound {
    pub fn new(
        id: RoundId,
        interval: ScrapeInterval,
        applications: Vec<ApplicationReport>,
    ) -> Result<Self, ScrapeRoundError> {
        if applications.len() > MAX_APPLICATIONS {
            return Err(ScrapeRoundError::TooManyApplications);
        }
        let mut names = std::collections::HashSet::new();
        if !applications.iter().all(|app| names.insert(&app.name)) {
            return Err(ScrapeRoundError::DuplicateApplicationName);
        }
        Ok(Self {
            id,
            interval,
            applications,
        })
    }

    pub fn id(&self) -> RoundId {
        self.id
    }

    pub fn applications(&self) -> &[ApplicationReport] {
        &self.applications
    }
}

/// The round the hub shows for a system, and when it arrived, in seconds since the Unix
/// epoch on the hub's clock.
#[derive(Debug, Clone, PartialEq)]
pub struct HeldRound {
    pub round: ScrapeRound,
    pub received_at: u64,
}

/// A digest of a round's content as converted: its interval and every report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RoundDigest(u64);

/// Computes round digests under one SipHash key for the whole hub process, so both ingestion
/// paths give one round one digest, and no sender can compute a digest in advance. Built once
/// at startup: each `RandomState` has a key of its own.
pub struct RoundDigester(RandomState);

impl RoundDigester {
    pub fn new() -> Self {
        Self(RandomState::new())
    }

    pub fn digest(&self, round: &ScrapeRound) -> RoundDigest {
        let mut hasher = self.0.build_hasher();
        round.interval.hash(&mut hasher);
        round.applications.len().hash(&mut hasher);
        for app in &round.applications {
            app.name.hash(&mut hasher);
            app.health.hash(&mut hasher);
            app.version.hash(&mut hasher);
            for (gauge, value) in app.gauges.iter() {
                gauge.hash(&mut hasher);
                value.to_bits().hash(&mut hasher);
            }
            // Ends each application's gauges, so they can't run into the next application.
            app.gauges.0.len().hash(&mut hasher);
        }
        RoundDigest(hasher.finish())
    }
}

impl Default for RoundDigester {
    fn default() -> Self {
        Self::new()
    }
}

/// How many accepted rounds a system remembers, to recognise an exact re-send.
pub const RECENT_ROUNDS: usize = 8;

/// The rounds the hub accepted most recently for one system, oldest first.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecentRounds(VecDeque<(RoundId, RoundDigest)>);

impl RecentRounds {
    pub fn contains(&self, id: RoundId, digest: RoundDigest) -> bool {
        self.0.contains(&(id, digest))
    }

    /// Remembers an accepted round, forgetting the oldest past `RECENT_ROUNDS`.
    pub fn remember(&mut self, id: RoundId, digest: RoundDigest) {
        if self.0.len() == RECENT_ROUNDS {
            self.0.pop_front();
        }
        self.0.push_back((id, digest));
    }
}

/// One token of a source's pace refills every this long.
pub const MIN_ROUND_SPACING: Duration = Duration::from_secs(8);

/// The most tokens a source's pace holds: one round, plus one early round after a reconnect.
const PACE_BURST: u8 = 2;

/// A token bucket for one source of rounds (one push connection, or the poller for one
/// system), on the monotonic clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourcePace {
    tokens: u8,
    last_refill: Instant,
}

/// A source sent a round before its pace allowed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooSoon;

impl SourcePace {
    /// A new source's pace: a full bucket.
    pub fn new(now: Instant) -> Self {
        Self {
            tokens: PACE_BURST,
            last_refill: now,
        }
    }

    /// Refills for the time elapsed, then spends one token.
    pub fn take(self, now: Instant) -> Result<Self, TooSoon> {
        let refilled = self.refill(now);
        let tokens = refilled.tokens.checked_sub(1).ok_or(TooSoon)?;
        Ok(Self { tokens, ..refilled })
    }

    /// Adds one token per whole `MIN_ROUND_SPACING` elapsed, keeping the remainder. A full
    /// bucket banks nothing: its clock restarts at `now`.
    fn refill(self, now: Instant) -> Self {
        if self.tokens >= PACE_BURST {
            return Self {
                tokens: PACE_BURST,
                last_refill: now.max(self.last_refill),
            };
        }
        let elapsed = now.saturating_duration_since(self.last_refill);
        let periods = elapsed.as_nanos() / MIN_ROUND_SPACING.as_nanos();
        let missing = u128::from(PACE_BURST - self.tokens);
        if periods >= missing {
            return Self {
                tokens: PACE_BURST,
                last_refill: now,
            };
        }
        // `periods < missing <= PACE_BURST`, so both conversions are exact.
        let periods = periods as u32;
        Self {
            tokens: self.tokens + periods as u8,
            last_refill: self.last_refill + MIN_ROUND_SPACING * periods,
        }
    }
}

/// What the hub does with a round a source reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Store and show it; `pace` is the source's pace after spending a token.
    Accept { pace: SourcePace },
    /// An exact re-send of a round accepted recently.
    Duplicate,
    /// The source's pace refused it.
    TooSoon,
}

/// Decides whether a round is stored. A duplicate is the same id *and* the same digest as a
/// recently accepted round: nothing compares a clock or a sequence the sender asserts.
pub fn admit(
    recent: &RecentRounds,
    pace: SourcePace,
    incoming: RoundId,
    digest: RoundDigest,
    now: Instant,
) -> Admission {
    if recent.contains(incoming, digest) {
        return Admission::Duplicate;
    }
    match pace.take(now) {
        Ok(pace) => Admission::Accept { pace },
        Err(TooSoon) => Admission::TooSoon,
    }
}

/// Whether the shown round is still current.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    Stale,
}

/// How long past two scrape intervals a round stays fresh: the poller's tick.
const FRESHNESS_SLACK_SECS: u64 = 30;

/// Stale once `now` is more than two scrape intervals and the poll tick past the round's
/// arrival.
pub fn freshness(held: &HeldRound, now: u64) -> Freshness {
    let fresh_for = 2 * held.round.interval.as_secs() + FRESHNESS_SLACK_SECS;
    if now > held.received_at.saturating_add(fresh_for) {
        Freshness::Stale
    } else {
        Freshness::Fresh
    }
}

/// The metric points one round adds to its system's history: `app:<name>:<gauge>` per
/// gauge, and `app:<name>:up`, 1 when the application reported `Up` and 0 otherwise.
pub fn application_points(round: &ScrapeRound) -> Vec<(String, f32)> {
    round
        .applications
        .iter()
        .flat_map(|app| {
            let name = app.name.as_str();
            let up = if app.health == ApplicationHealth::Up {
                1.0
            } else {
                0.0
            };
            app.gauges
                .iter()
                .map(move |(gauge, value)| {
                    (format!("app:{name}:{}", gauge.wire_name()), value as f32)
                })
                .chain(std::iter::once((format!("app:{name}:up"), up)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN: &str = "6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b";

    fn name(value: &str) -> ApplicationName {
        ApplicationName::parse(value).expect("a valid test name")
    }

    fn report(app: &str, health: ApplicationHealth) -> ApplicationReport {
        ApplicationReport {
            name: name(app),
            health,
            version: ApplicationVersion::parse("2.4.1"),
            gauges: Gauges::from_wire([("heap_used_bytes", 300.0), ("live_threads", 12.0)]),
        }
    }

    fn id(seq: u64) -> RoundId {
        RoundId::parse(RUN, seq).expect("a valid test id")
    }

    fn interval(secs: u64) -> ScrapeInterval {
        ScrapeInterval::from_secs(secs).expect("a valid test interval")
    }

    fn round_of(seq: u64, applications: Vec<ApplicationReport>) -> ScrapeRound {
        ScrapeRound::new(id(seq), interval(15), applications).expect("a valid test round")
    }

    fn round(seq: u64) -> ScrapeRound {
        round_of(seq, vec![report("orders", ApplicationHealth::Up)])
    }

    #[test]
    fn application_names_follow_the_agents_rule() {
        let cases = [
            ("a plain name", "orders", true),
            ("every allowed byte", "Az09_.-", true),
            ("one byte", "a", true),
            ("64 bytes", &"a".repeat(64), true),
            ("65 bytes", &"a".repeat(65), false),
            ("empty", "", false),
            ("dot", ".", false),
            ("dot dot", "..", false),
            ("a space", "or ders", false),
            ("a slash", "or/ders", false),
            ("non-ASCII", "ördérs", false),
            ("markup", "<b>", false),
            ("three dots", "...", true),
            ("a leading dot", ".a", true),
            ("a colon, which would split metric names", "a:b", false),
            ("a double quote", "a\"b", false),
            ("a single quote", "a'b", false),
            ("an ampersand", "a&b", false),
            ("a backslash", "a\\b", false),
            ("a greater-than", "a>b", false),
            ("a newline", "a\nb", false),
            ("a tab", "a\tb", false),
            ("a NUL", "a\0b", false),
        ];
        for byte in 0..=u8::MAX {
            let value = [b'a', byte, b'a'];
            let allowed = byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-');
            let parsed = std::str::from_utf8(&value)
                .ok()
                .and_then(ApplicationName::parse);
            assert_eq!(parsed.is_some(), allowed, "byte {byte:#04x}");
        }
        for (case, value, valid) in cases {
            let parsed = ApplicationName::parse(value);
            assert_eq!(parsed.is_some(), valid, "case: {case}");
            if let Some(parsed) = parsed {
                assert_eq!(parsed.as_str(), value, "case: {case}");
            }
        }
    }

    #[test]
    fn every_health_has_its_wire_name_and_an_unknown_one_reads_as_unknown() {
        use ApplicationHealth::*;
        let known = [
            ("up", Up),
            ("down", Down),
            ("out_of_service", OutOfService),
            ("unknown", Unknown),
            ("unreachable", Unreachable),
        ];
        for (wire, health) in known {
            assert_eq!(ApplicationHealth::from_wire(wire), health, "case: {wire}");
            assert_eq!(health.wire_name(), wire, "case: {wire}");
        }
        for wire in ["UP", "degraded", "", "up "] {
            assert_eq!(
                ApplicationHealth::from_wire(wire),
                Unknown,
                "case: {wire:?}"
            );
        }
    }

    #[test]
    fn a_version_is_cut_to_64_chars_on_a_char_boundary_and_a_blank_one_is_none() {
        let straddling = format!("{}é-tail", "v".repeat(63));
        let cases = [
            ("short", "2.4.1".to_string(), Some("2.4.1".to_string())),
            ("64 chars", "v".repeat(64), Some("v".repeat(64))),
            ("65 chars", "v".repeat(65), Some("v".repeat(64))),
            (
                "a multibyte 64th char",
                straddling,
                Some(format!("{}é", "v".repeat(63))),
            ),
            (
                "surrounding spaces, kept",
                " 2.4.1 ".to_string(),
                Some(" 2.4.1 ".to_string()),
            ),
            ("empty", String::new(), None),
            ("blank", "   ".to_string(), None),
            (
                "blank for its first 64 chars",
                format!("{}x", " ".repeat(64)),
                None,
            ),
        ];
        for (case, raw, expected) in cases {
            let parsed = ApplicationVersion::parse(&raw).map(|v| v.as_str().to_string());
            assert_eq!(parsed, expected, "case: {case}");
        }
    }

    #[test]
    fn every_gauge_has_its_rfc_wire_name() {
        let names = [
            "heap_used_bytes",
            "heap_max_bytes",
            "cpu_percent",
            "live_threads",
            "gc_pause_percent",
            "http_requests_per_second",
            "http_server_errors_per_second",
            "http_mean_latency_ms",
            "db_connections_active",
            "uptime_seconds",
        ];
        for (gauge, name) in ApplicationGauge::ALL.into_iter().zip(names) {
            assert_eq!(gauge.wire_name(), name, "{gauge:?}");
            assert_eq!(ApplicationGauge::from_wire(name), Some(gauge), "{name}");
        }
        assert_eq!(ApplicationGauge::from_wire("heap_used"), None);
    }

    #[test]
    fn gauges_keep_known_names_with_finite_values_only() {
        let many_unknown: Vec<(String, f64)> =
            (0..1000).map(|i| (format!("extra_{i}"), 1.0)).collect();
        let raw = [
            ("heap_used_bytes", 300.0),
            ("live_threads", f64::NAN),
            ("cpu_percent", f64::INFINITY),
            ("uptime_seconds", f64::NEG_INFINITY),
            ("a_later_gauge", 5.0),
            ("db_connections_active", -1.0),
        ]
        .into_iter()
        .chain(many_unknown.iter().map(|(k, v)| (k.as_str(), *v)));
        let gauges: Vec<_> = Gauges::from_wire(raw).iter().collect();
        assert_eq!(
            gauges,
            [
                (ApplicationGauge::HeapUsedBytes, 300.0),
                (ApplicationGauge::DbConnectionsActive, -1.0),
            ]
        );
    }

    #[test]
    fn a_repeated_gauge_keeps_its_last_value() {
        let gauges: Vec<_> = Gauges::from_wire([("live_threads", 1.0), ("live_threads", 2.0)])
            .iter()
            .collect();
        assert_eq!(gauges, [(ApplicationGauge::LiveThreads, 2.0)]);
    }

    #[test]
    fn a_round_ids_run_must_be_a_uuid() {
        let cases = [
            ("hyphenated", RUN.to_string(), Ok(7)),
            (
                "not a uuid",
                "run-1".to_string(),
                Err(ScrapeRoundError::InvalidRun),
            ),
            ("empty", String::new(), Err(ScrapeRoundError::InvalidRun)),
            (
                "512 KiB",
                "a".repeat(512 * 1024),
                Err(ScrapeRoundError::InvalidRun),
            ),
        ];
        for (case, run, expected) in cases {
            let parsed = RoundId::parse(&run, 7);
            assert_eq!(parsed.map(RoundId::seq), expected, "case: {case}");
            if let Ok(parsed) = parsed {
                assert_eq!(parsed.run().hyphenated().to_string(), RUN, "case: {case}");
            }
        }
    }

    #[test]
    fn a_scrape_interval_is_10_to_3600_seconds() {
        let cases = [
            (0, false),
            (9, false),
            (10, true),
            (15, true),
            (3600, true),
            (3601, false),
            (u64::MAX, false),
        ];
        for (secs, valid) in cases {
            let parsed = ScrapeInterval::from_secs(secs);
            assert_eq!(
                parsed.map(ScrapeInterval::as_secs),
                if valid {
                    Ok(secs)
                } else {
                    Err(ScrapeRoundError::InvalidInterval)
                },
                "case: {secs}"
            );
        }
    }

    #[test]
    fn a_round_holds_at_most_16_applications_under_distinct_names() {
        let apps = |n: usize| -> Vec<ApplicationReport> {
            (0..n)
                .map(|i| report(&format!("app{i}"), ApplicationHealth::Up))
                .collect()
        };
        let duplicated = vec![
            report("orders", ApplicationHealth::Up),
            report("billing", ApplicationHealth::Up),
            report("orders", ApplicationHealth::Down),
        ];
        let cases = [
            ("none", apps(0), Ok(0)),
            ("16", apps(16), Ok(16)),
            ("17", apps(17), Err(ScrapeRoundError::TooManyApplications)),
            (
                "a repeated name",
                duplicated,
                Err(ScrapeRoundError::DuplicateApplicationName),
            ),
            (
                "names differing in case are distinct",
                vec![
                    report("orders", ApplicationHealth::Up),
                    report("Orders", ApplicationHealth::Up),
                ],
                Ok(2),
            ),
        ];
        for (case, applications, expected) in cases {
            let built = ScrapeRound::new(id(1), interval(15), applications);
            assert_eq!(
                built.map(|round| round.applications().len()),
                expected,
                "case: {case}"
            );
        }
    }

    #[test]
    fn one_digester_gives_one_round_one_digest_on_any_thread() {
        let digester = std::sync::Arc::new(RoundDigester::new());
        let here = digester.digest(&round(1));
        let shared = std::sync::Arc::clone(&digester);
        let there = std::thread::spawn(move || shared.digest(&round(1)))
            .join()
            .unwrap();
        assert_eq!(here, there);
    }

    #[test]
    fn a_digest_changes_with_any_content_of_the_round() {
        use ApplicationHealth::*;
        let digester = RoundDigester::new();
        let app =
            |name: &str, health, version: Option<&str>, gauges: &[(&str, f64)]| ApplicationReport {
                name: ApplicationName::parse(name).unwrap(),
                health,
                version: version.and_then(ApplicationVersion::parse),
                gauges: Gauges::from_wire(gauges.iter().copied()),
            };
        let orders = || app("orders", Up, Some("2.4.1"), &[("heap_used_bytes", 300.0)]);
        let billing = |health| app("billing", health, None, &[]);
        let two = |second: ApplicationReport| round_of(1, vec![orders(), second]);
        // (case, a round, a round differing from it in that one respect)
        let cases = [
            (
                "health up/down",
                round_of(1, vec![orders()]),
                round_of(
                    1,
                    vec![app(
                        "orders",
                        Down,
                        Some("2.4.1"),
                        &[("heap_used_bytes", 300.0)],
                    )],
                ),
            ),
            (
                "health down/unreachable",
                two(billing(Down)),
                two(billing(Unreachable)),
            ),
            (
                "health unknown/out of service",
                two(billing(Unknown)),
                two(billing(OutOfService)),
            ),
            (
                "the second application's health",
                two(billing(Up)),
                two(billing(Down)),
            ),
            (
                "a version",
                round_of(1, vec![orders()]),
                round_of(
                    1,
                    vec![app(
                        "orders",
                        Up,
                        Some("2.4.2"),
                        &[("heap_used_bytes", 300.0)],
                    )],
                ),
            ),
            (
                "a version against none",
                round_of(1, vec![orders()]),
                round_of(
                    1,
                    vec![app("orders", Up, None, &[("heap_used_bytes", 300.0)])],
                ),
            ),
            (
                "a fractional gauge value",
                round_of(1, vec![orders()]),
                round_of(
                    1,
                    vec![app(
                        "orders",
                        Up,
                        Some("2.4.1"),
                        &[("heap_used_bytes", 300.4)],
                    )],
                ),
            ),
            (
                "the same value under another gauge",
                round_of(1, vec![orders()]),
                round_of(
                    1,
                    vec![app(
                        "orders",
                        Up,
                        Some("2.4.1"),
                        &[("heap_max_bytes", 300.0)],
                    )],
                ),
            ),
            (
                "an extra gauge",
                round_of(1, vec![orders()]),
                round_of(
                    1,
                    vec![app(
                        "orders",
                        Up,
                        Some("2.4.1"),
                        &[("heap_used_bytes", 300.0), ("live_threads", 1.0)],
                    )],
                ),
            ),
            (
                "only the second gauge's value",
                round_of(
                    1,
                    vec![app(
                        "orders",
                        Up,
                        None,
                        &[("heap_used_bytes", 300.0), ("live_threads", 12.0)],
                    )],
                ),
                round_of(
                    1,
                    vec![app(
                        "orders",
                        Up,
                        None,
                        &[("heap_used_bytes", 300.0), ("live_threads", 13.0)],
                    )],
                ),
            ),
            (
                "a name",
                round_of(1, vec![orders()]),
                round_of(
                    1,
                    vec![app(
                        "orders2",
                        Up,
                        Some("2.4.1"),
                        &[("heap_used_bytes", 300.0)],
                    )],
                ),
            ),
            (
                "the interval",
                round_of(1, vec![orders()]),
                ScrapeRound::new(id(1), interval(20), vec![orders()]).unwrap(),
            ),
            (
                "another application",
                round_of(1, vec![orders()]),
                two(billing(Up)),
            ),
            (
                "the order of applications",
                two(billing(Up)),
                round_of(1, vec![billing(Up), orders()]),
            ),
            (
                "only the first of two applications' health",
                two(billing(Up)),
                round_of(
                    1,
                    vec![
                        app("orders", Down, Some("2.4.1"), &[("heap_used_bytes", 300.0)]),
                        billing(Up),
                    ],
                ),
            ),
            (
                "only the first of two applications' version",
                two(billing(Up)),
                round_of(
                    1,
                    vec![
                        app("orders", Up, Some("2.4.2"), &[("heap_used_bytes", 300.0)]),
                        billing(Up),
                    ],
                ),
            ),
            (
                "only the first of two applications' gauge",
                two(billing(Up)),
                round_of(
                    1,
                    vec![
                        app("orders", Up, Some("2.4.1"), &[("heap_used_bytes", 301.0)]),
                        billing(Up),
                    ],
                ),
            ),
        ];
        for (case, a, b) in cases {
            assert_ne!(digester.digest(&a), digester.digest(&b), "case: {case}");
        }
        let healths = [Up, Down, OutOfService, Unknown, Unreachable];
        let by_health: std::collections::HashSet<RoundDigest> = healths
            .iter()
            .map(|health| digester.digest(&two(billing(*health))))
            .collect();
        assert_eq!(
            by_health.len(),
            healths.len(),
            "every health digests differently"
        );
    }

    #[test]
    fn recent_rounds_forget_the_oldest_past_eight() {
        let digester = RoundDigester::new();
        let mut recent = RecentRounds::default();
        for seq in 1..=9 {
            recent.remember(id(seq), digester.digest(&round(seq)));
        }
        assert!(
            !recent.contains(id(1), digester.digest(&round(1))),
            "the 9th pushed out the 1st"
        );
        for seq in 2..=9 {
            assert!(
                recent.contains(id(seq), digester.digest(&round(seq))),
                "seq {seq} is still remembered"
            );
        }
    }

    #[test]
    fn only_an_exact_re_send_of_a_recent_round_is_a_duplicate() {
        let digester = RoundDigester::new();
        let now = Instant::now();
        let honest = round(5);
        let honest_digest = digester.digest(&honest);
        let forged = round_of(6, vec![report("orders", ApplicationHealth::Down)]);
        let mut recent = RecentRounds::default();
        recent.remember(honest.id(), honest_digest);
        recent.remember(forged.id(), digester.digest(&forged));
        let other_run = RoundId::parse("00000000-0000-0000-0000-000000000001", 5).unwrap();

        let cases = [
            ("an exact repeat", honest.id(), honest_digest, false),
            (
                "a higher seq, same run",
                id(7),
                digester.digest(&round(7)),
                true,
            ),
            (
                "a lower seq, same run",
                id(2),
                digester.digest(&round(2)),
                true,
            ),
            (
                "u64::MAX under the run",
                id(u64::MAX),
                digester.digest(&round(u64::MAX)),
                true,
            ),
            ("a new run, same seq", other_run, honest_digest, true),
            (
                "the honest round under a pre-claimed id",
                id(6),
                digester.digest(&round(6)),
                true,
            ),
        ];
        for (case, incoming, digest, accepted) in cases {
            let admission = admit(&recent, SourcePace::new(now), incoming, digest, now);
            let expected_accept = matches!(admission, Admission::Accept { .. });
            assert_eq!(expected_accept, accepted, "case: {case}: {admission:?}");
            if !accepted {
                assert_eq!(admission, Admission::Duplicate, "case: {case}");
            }
        }
        let empty = admit(
            &RecentRounds::default(),
            SourcePace::new(now),
            honest.id(),
            honest_digest,
            now,
        );
        assert!(matches!(empty, Admission::Accept { .. }), "nothing recent");
    }

    #[test]
    fn a_duplicate_spends_no_token_and_a_refusal_is_too_soon() {
        let digester = RoundDigester::new();
        let now = Instant::now();
        let seen = round(1);
        let mut recent = RecentRounds::default();
        recent.remember(seen.id(), digester.digest(&seen));
        let spent = SourcePace::new(now).take(now).unwrap().take(now).unwrap();
        assert_eq!(
            admit(&recent, spent, seen.id(), digester.digest(&seen), now),
            Admission::Duplicate,
            "a duplicate is recognised even with an empty bucket"
        );
        assert_eq!(
            admit(&recent, spent, id(2), digester.digest(&round(2)), now),
            Admission::TooSoon
        );
        let fresh = SourcePace::new(now);
        match admit(&recent, fresh, id(2), digester.digest(&round(2)), now) {
            Admission::Accept { pace } => assert_eq!(pace, fresh.take(now).unwrap()),
            other => panic!("expected Accept, got {other:?}"),
        }
    }

    #[test]
    fn a_source_paces_two_rounds_then_one_per_8_seconds() {
        let base = Instant::now();
        let at = |ms: u64| base + Duration::from_millis(ms);
        let full = SourcePace::new(base);
        let empty = full.take(at(0)).unwrap().take(at(0)).unwrap();
        let cases = [
            ("a third round back to back", empty, at(0), false),
            ("just below 8 s", empty, at(7_999), false),
            ("exactly 8 s", empty, at(8_000), true),
            ("past 8 s", empty, at(9_000), true),
            ("a clock that didn't move", full, at(0), true),
            // A source's clock never runs backwards, but the arithmetic must not panic.
            (
                "an earlier now",
                empty,
                base - Duration::from_secs(1),
                false,
            ),
        ];
        for (case, pace, now, allowed) in cases {
            assert_eq!(pace.take(now).is_ok(), allowed, "case: {case}");
        }

        // A refill only ever banks two rounds, however long the source was quiet.
        let rested = empty
            .take(at(3_600_000))
            .unwrap()
            .take(at(3_600_000))
            .unwrap();
        assert_eq!(rested.take(at(3_600_000)), Err(TooSoon), "the burst is 2");

        // A full bucket banks nothing while it waits: two rounds at 15.9 s, then none at 16 s.
        let waited = full.take(at(15_900)).unwrap().take(at(15_900)).unwrap();
        assert_eq!(
            waited.take(at(16_000)),
            Err(TooSoon),
            "a full bucket doesn't bank time"
        );

        // Partial progress isn't lost: 12 s after empty, one round, and the next at 16 s.
        let after_one = empty.take(at(12_000)).unwrap();
        assert!(after_one.take(at(15_999)).is_err(), "not before 16 s");
        assert!(after_one.take(at(16_000)).is_ok(), "at 16 s");
    }

    #[test]
    fn a_re_sent_round_after_a_handshake_leaves_room_for_the_next_one() {
        let now = Instant::now();
        let connection = SourcePace::new(now);
        let after_resend = connection.take(now).unwrap();
        assert!(
            after_resend.take(now + Duration::from_secs(3)).is_ok(),
            "the next round 3 s later is accepted too"
        );
    }

    #[test]
    fn two_sources_pace_independently() {
        let now = Instant::now();
        let hostile = SourcePace::new(now).take(now).unwrap().take(now).unwrap();
        let honest = SourcePace::new(now);
        assert!(hostile.take(now).is_err());
        assert!(
            honest.take(now).is_ok(),
            "the honest source keeps its tokens"
        );
    }

    #[test]
    fn a_held_round_goes_stale_past_two_intervals_and_the_poll_tick() {
        let held = HeldRound {
            round: round(1), // 15 s interval: stale past 1000 + 30 + 30
            received_at: 1_000,
        };
        let cases = [
            ("on arrival", 1_000, Freshness::Fresh),
            ("exactly at the bound", 1_060, Freshness::Fresh),
            ("just past it", 1_061, Freshness::Stale),
            ("a clock behind the arrival", 900, Freshness::Fresh),
        ];
        for (case, now, expected) in cases {
            assert_eq!(freshness(&held, now), expected, "case: {case}");
        }
        let hourly = HeldRound {
            round: ScrapeRound::new(id(1), interval(3600), vec![]).unwrap(),
            received_at: 1_000,
        };
        assert_eq!(
            freshness(&hourly, 1_000 + 7_230),
            Freshness::Fresh,
            "hourly, at the bound"
        );
        assert_eq!(
            freshness(&hourly, 1_000 + 7_231),
            Freshness::Stale,
            "hourly, past it"
        );
        let far = HeldRound {
            round: round(1),
            received_at: u64::MAX,
        };
        assert_eq!(freshness(&far, u64::MAX), Freshness::Fresh, "no overflow");
    }

    #[test]
    fn a_round_adds_its_gauges_and_an_up_point_per_application() {
        use ApplicationHealth::*;
        for (health, up) in [
            (Up, 1.0),
            (Down, 0.0),
            (OutOfService, 0.0),
            (Unknown, 0.0),
            (Unreachable, 0.0),
        ] {
            let points = application_points(&round_of(1, vec![report("orders", health)]));
            assert_eq!(
                points,
                [
                    ("app:orders:heap_used_bytes".to_string(), 300.0),
                    ("app:orders:live_threads".to_string(), 12.0),
                    ("app:orders:up".to_string(), up),
                ],
                "case: {health:?}"
            );
        }
        let two = round_of(
            1,
            vec![
                report("orders", Up),
                ApplicationReport {
                    name: name("billing"),
                    health: Unreachable,
                    version: None,
                    gauges: Gauges::default(),
                },
            ],
        );
        assert_eq!(
            application_points(&two).last(),
            Some(&("app:billing:up".to_string(), 0.0))
        );
        assert_eq!(application_points(&two).len(), 4);
    }
}
