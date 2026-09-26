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

//! What one scrape of one application reports: its health, version and curated gauges, with
//! rates computed from the previous scrape (RFC 0009 §3). Pure: the adapter hands in what it
//! read and when, and gets a report back.

use std::collections::BTreeMap;
use std::time::Instant;

/// One of the curated values the agent derives from an application's Micrometer meters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
    /// The name on the wire and in hub metric names (`app:<name>:<wire name>`).
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
}

/// Why a scrape of an application's health failed. It stays in the agent's log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrapeFailure {
    Connect,
    Timeout,
    Unauthorized,
    HttpStatus(u16),
    BadBody,
}

/// What `/actuator/health` said about the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportedHealth {
    Up,
    Down,
    OutOfService,
    Unknown,
}

impl ReportedHealth {
    /// Maps Actuator's top-level `status`; a custom status is `Unknown`.
    pub fn from_status(status: &str) -> Self {
        match status {
            "UP" => Self::Up,
            "DOWN" => Self::Down,
            "OUT_OF_SERVICE" => Self::OutOfService,
            _ => Self::Unknown,
        }
    }
}

/// An application's health as the agent reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationHealth {
    Reported(ReportedHealth),
    Unreachable(ScrapeFailure),
}

/// One Micrometer meter as a scrape read it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MeterValue<T> {
    /// The meter's value.
    Published(T),
    /// Actuator answered 404: the application doesn't publish this meter (yet).
    NotPublished,
    /// The request failed some other way; whatever depends on it is missing this round.
    Unavailable,
}

impl MeterValue<f64> {
    /// The value, if the meter was published with a finite one.
    fn finite(self) -> Option<f64> {
        match self {
            Self::Published(value) if value.is_finite() => Some(value),
            Self::Published(_) | Self::NotPublished | Self::Unavailable => None,
        }
    }
}

/// A Micrometer timer's cumulative totals.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimerTotals {
    pub count: f64,
    pub total_seconds: f64,
}

/// Every meter the curated gauges are derived from, as read in one scrape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Meters {
    pub heap_used: MeterValue<f64>,
    pub heap_max: MeterValue<f64>,
    /// `process.cpu.usage`, a fraction of one.
    pub cpu_usage: MeterValue<f64>,
    pub live_threads: MeterValue<f64>,
    /// `jvm.gc.pause`'s `TOTAL_TIME`, in seconds, cumulative.
    pub gc_pause_seconds: MeterValue<f64>,
    /// `http.server.requests`, all outcomes.
    pub http_requests: MeterValue<TimerTotals>,
    /// `http.server.requests?tag=outcome:SERVER_ERROR`'s `COUNT`.
    pub http_server_errors: MeterValue<f64>,
    pub db_connections_active: MeterValue<f64>,
    pub uptime_seconds: MeterValue<f64>,
}

/// A scrape that reached the application.
#[derive(Debug, Clone, PartialEq)]
pub struct ReachedScrape {
    pub health: ReportedHealth,
    /// `build.version` from `/actuator/info`, as sent.
    pub version: Option<String>,
    pub meters: Meters,
    /// Requests the agent completed against the application in this scrape. Actuator counts
    /// them in `http.server.requests`, so the next scrape subtracts them.
    pub own_requests: u32,
}

/// What one scrape of one application gathered.
#[derive(Debug, Clone, PartialEq)]
pub enum RawScrape {
    /// The health request failed; nothing else was read.
    Unreachable(ScrapeFailure),
    Reached(ReachedScrape),
}

/// An application's version: its `build.version`, not blank, cut to its first 64 `char`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationVersion(String);

impl ApplicationVersion {
    const MAX_CHARS: usize = 64;

    /// `None` for a blank version, which says nothing.
    fn parse(raw: &str) -> Option<Self> {
        let cut: String = raw.chars().take(Self::MAX_CHARS).collect();
        (!cut.trim().is_empty()).then_some(Self(cut))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An application's gauges from one scrape: finite values only, and a gauge that couldn't be
/// derived is absent, never zero.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Gauges(BTreeMap<ApplicationGauge, f64>);

impl Gauges {
    fn from_values(values: impl IntoIterator<Item = (ApplicationGauge, Option<f64>)>) -> Self {
        Self(
            values
                .into_iter()
                .filter_map(|(gauge, value)| Some((gauge, value.filter(|v| v.is_finite())?)))
                .collect(),
        )
    }

    pub fn iter(&self) -> impl Iterator<Item = (ApplicationGauge, f64)> + '_ {
        self.0.iter().map(|(gauge, value)| (*gauge, *value))
    }
}

/// The report of one scrape of one application.
#[derive(Debug, Clone, PartialEq)]
pub enum ApplicationReport {
    /// The health request failed: no version, no gauges.
    Unreachable(ScrapeFailure),
    Reached {
        health: ReportedHealth,
        version: Option<ApplicationVersion>,
        gauges: Gauges,
    },
}

impl ApplicationReport {
    pub fn health(&self) -> ApplicationHealth {
        match self {
            Self::Unreachable(failure) => ApplicationHealth::Unreachable(*failure),
            Self::Reached { health, .. } => ApplicationHealth::Reported(*health),
        }
    }
}

/// A cumulative counter's value, and when it was read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CounterSample {
    pub value: f64,
    pub at: Instant,
}

/// Per second between two samples. `None` on the first sample, on a counter reset
/// (`curr < prev`), when no time has passed or time ran backwards, and when the result isn't
/// finite.
pub fn rate_per_second(prev: Option<&CounterSample>, curr: &CounterSample) -> Option<f64> {
    let prev = prev?;
    if curr.value < prev.value {
        return None;
    }
    // No time passed divides by zero, which the finite filter drops with every other
    // non-finite rate.
    let elapsed = curr.at.checked_duration_since(prev.at)?.as_secs_f64();
    Some((curr.value - prev.value) / elapsed).filter(|rate| rate.is_finite())
}

/// The cumulative counters rates are derived from, each finite or absent.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Counters {
    requests: Option<f64>,
    request_seconds: Option<f64>,
    server_errors: Option<f64>,
    gc_pause_seconds: Option<f64>,
    uptime_seconds: Option<f64>,
}

impl Counters {
    fn of(meters: &Meters) -> Self {
        let finite = |value: f64| Some(value).filter(|v| v.is_finite());
        let (requests, request_seconds) = match meters.http_requests {
            MeterValue::Published(timer) => (finite(timer.count), finite(timer.total_seconds)),
            MeterValue::NotPublished | MeterValue::Unavailable => (None, None),
        };
        // A 404 while requests are published means no 5xx has happened yet: zero, so the
        // first burst shows as a rate.
        let server_errors = match meters.http_server_errors {
            MeterValue::NotPublished if requests.is_some() => Some(0.0),
            value => value.finite(),
        };
        Self {
            requests,
            request_seconds,
            server_errors,
            gc_pause_seconds: meters.gc_pause_seconds.finite(),
            uptime_seconds: meters.uptime_seconds.finite(),
        }
    }

    /// Whether the application restarted between `self` and `next`: any counter, uptime
    /// included, went down.
    fn reset_before(&self, next: &Self) -> bool {
        // Destructured in full, so a new counter can't be left out of the restart check.
        let Self {
            requests,
            request_seconds,
            server_errors,
            gc_pause_seconds,
            uptime_seconds,
        } = *self;
        [
            (requests, next.requests),
            (request_seconds, next.request_seconds),
            (server_errors, next.server_errors),
            (gc_pause_seconds, next.gc_pause_seconds),
            (uptime_seconds, next.uptime_seconds),
        ]
        .into_iter()
        .any(|pair| matches!(pair, (Some(prev), Some(curr)) if curr < prev))
    }
}

/// What the previous reachable scrape left for the next one's rates.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Baseline {
    counters: Counters,
    at: Instant,
    /// Requests the agent made in that scrape, counted by the next one.
    own_requests: u32,
}

/// The rates between a baseline and a scrape; all absent on a first scrape or a restart.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Rates {
    requests_per_second: Option<f64>,
    server_errors_per_second: Option<f64>,
    mean_latency_ms: Option<f64>,
    gc_pause_percent: Option<f64>,
}

impl Baseline {
    /// The rates from this baseline to `counters`, read at `now`.
    fn rates_to(&self, counters: &Counters, now: Instant) -> Rates {
        let sample = |value: Option<f64>, at| value.map(|value| CounterSample { value, at });
        let rate = |prev: Option<f64>, curr: Option<f64>| {
            rate_per_second(sample(prev, self.at).as_ref(), &sample(curr, now)?)
        };
        // The agent's own requests from the previous scrape are in this delta; take them out,
        // never below nothing.
        let own = f64::from(self.own_requests);
        let net_requests = counters
            .requests
            .zip(self.counters.requests)
            .map(|(curr, prev)| (curr - own).max(prev));
        Rates {
            requests_per_second: rate(self.counters.requests, net_requests),
            server_errors_per_second: rate(self.counters.server_errors, counters.server_errors),
            mean_latency_ms: self.mean_latency_ms(counters),
            gc_pause_percent: rate(self.counters.gc_pause_seconds, counters.gc_pause_seconds)
                .map(|rate| rate * 100.0),
        }
    }

    /// Request time over request count in the interval, including the agent's own requests.
    fn mean_latency_ms(&self, counters: &Counters) -> Option<f64> {
        let delta = |prev: Option<f64>, curr: Option<f64>| Some(curr? - prev?);
        let count = delta(self.counters.requests, counters.requests)?;
        let seconds = delta(self.counters.request_seconds, counters.request_seconds)?;
        (count > 0.0).then(|| seconds / count * 1000.0)
    }
}

impl ApplicationGauge {
    /// Every gauge, in declaration order.
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

    /// This gauge's value from one scrape's meters and rates, if it can be derived. An
    /// exhaustive match, so a new gauge must say where it comes from.
    fn derive(self, meters: &Meters, rates: &Rates) -> Option<f64> {
        let non_negative = |value: MeterValue<f64>| value.finite().filter(|v| *v >= 0.0);
        match self {
            Self::HeapUsedBytes => meters.heap_used.finite(),
            Self::HeapMaxBytes => non_negative(meters.heap_max),
            Self::CpuPercent => non_negative(meters.cpu_usage).map(|usage| usage * 100.0),
            Self::LiveThreads => meters.live_threads.finite(),
            Self::GcPausePercent => rates.gc_pause_percent,
            Self::HttpRequestsPerSecond => rates.requests_per_second,
            Self::HttpServerErrorsPerSecond => rates.server_errors_per_second,
            Self::HttpMeanLatencyMs => rates.mean_latency_ms,
            Self::DbConnectionsActive => meters.db_connections_active.finite(),
            Self::UptimeSeconds => meters.uptime_seconds.finite(),
        }
    }
}

/// One application's previous scrape, which the next scrape's rates are computed from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScrapeHistory {
    /// `None` before the first reachable scrape, and after an unreachable one.
    baseline: Option<Baseline>,
}

impl ScrapeHistory {
    /// Folds one scrape into a report, and the history the next scrape starts from: an
    /// unreachable scrape clears the baseline; a reached one becomes it.
    pub fn advance(self, raw: RawScrape, now: Instant) -> (ScrapeHistory, ApplicationReport) {
        match raw {
            RawScrape::Unreachable(failure) => {
                (Self::default(), ApplicationReport::Unreachable(failure))
            }
            RawScrape::Reached(scrape) => self.fold(scrape, now),
        }
    }

    /// Counters, then rates against the baseline unless the application restarted, then the
    /// report.
    fn fold(self, scrape: ReachedScrape, now: Instant) -> (ScrapeHistory, ApplicationReport) {
        let counters = Counters::of(&scrape.meters);
        let rates = self
            .baseline
            .filter(|baseline| !baseline.counters.reset_before(&counters))
            .map(|baseline| baseline.rates_to(&counters, now))
            .unwrap_or_default();
        let gauges = Gauges::from_values(
            ApplicationGauge::ALL.map(|gauge| (gauge, gauge.derive(&scrape.meters, &rates))),
        );
        let report = ApplicationReport::Reached {
            health: scrape.health,
            version: scrape
                .version
                .as_deref()
                .and_then(ApplicationVersion::parse),
            gauges,
        };
        let baseline = Baseline {
            counters,
            at: now,
            own_requests: scrape.own_requests,
        };
        (
            Self {
                baseline: Some(baseline),
            },
            report,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A report's gauges as a map; empty for an unreachable report.
    fn gauges_of(report: &ApplicationReport) -> BTreeMap<ApplicationGauge, f64> {
        match report {
            ApplicationReport::Unreachable(_) => BTreeMap::new(),
            ApplicationReport::Reached { gauges, .. } => gauges.iter().collect(),
        }
    }

    /// A report's version as text.
    fn version_of(report: &ApplicationReport) -> Option<String> {
        match report {
            ApplicationReport::Unreachable(_) => None,
            ApplicationReport::Reached { version, .. } => {
                version.as_ref().map(|v| v.as_str().to_string())
            }
        }
    }
    use ApplicationGauge as G;
    use std::time::Duration;

    #[test]
    fn every_gauge_has_its_rfc_wire_name() {
        let cases = [
            (G::HeapUsedBytes, "heap_used_bytes"),
            (G::HeapMaxBytes, "heap_max_bytes"),
            (G::CpuPercent, "cpu_percent"),
            (G::LiveThreads, "live_threads"),
            (G::GcPausePercent, "gc_pause_percent"),
            (G::HttpRequestsPerSecond, "http_requests_per_second"),
            (
                G::HttpServerErrorsPerSecond,
                "http_server_errors_per_second",
            ),
            (G::HttpMeanLatencyMs, "http_mean_latency_ms"),
            (G::DbConnectionsActive, "db_connections_active"),
            (G::UptimeSeconds, "uptime_seconds"),
        ];
        for (gauge, name) in cases {
            assert_eq!(gauge.wire_name(), name, "{gauge:?}");
        }
    }

    #[test]
    fn actuator_status_maps_to_reported_health() {
        let cases = [
            ("up", "UP", ReportedHealth::Up),
            ("down", "DOWN", ReportedHealth::Down),
            (
                "out of service",
                "OUT_OF_SERVICE",
                ReportedHealth::OutOfService,
            ),
            ("unknown", "UNKNOWN", ReportedHealth::Unknown),
            ("a custom status", "DEGRADED", ReportedHealth::Unknown),
            (
                "lower case is not Actuator's",
                "up",
                ReportedHealth::Unknown,
            ),
            ("empty", "", ReportedHealth::Unknown),
        ];
        for (name, status, expected) in cases {
            assert_eq!(
                ReportedHealth::from_status(status),
                expected,
                "case: {name}"
            );
        }
    }

    #[test]
    fn a_rate_needs_an_earlier_lower_or_equal_sample_and_elapsed_time() {
        let t0 = Instant::now();
        let at = |secs: u64, value: f64| CounterSample {
            value,
            at: t0 + Duration::from_secs(secs),
        };
        let cases = [
            ("first sample", None, at(15, 10.0), None),
            (
                "steady increase",
                Some(at(0, 10.0)),
                at(15, 40.0),
                Some(2.0),
            ),
            ("no change", Some(at(0, 10.0)), at(15, 10.0), Some(0.0)),
            (
                "divides by the elapsed time, not a fixed interval",
                Some(at(0, 10.0)),
                at(10, 40.0),
                Some(3.0),
            ),
            ("counter reset", Some(at(0, 10.0)), at(15, 4.0), None),
            ("no time passed", Some(at(15, 10.0)), at(15, 12.0), None),
            (
                "a NaN value gives no rate",
                Some(at(0, 10.0)),
                at(15, f64::NAN),
                None,
            ),
            (
                "a rate that overflows is none, never infinite",
                Some(at(0, -f64::MAX)),
                at(1, f64::MAX),
                None,
            ),
            (
                "the earlier sample is later: no rate",
                Some(at(30, 10.0)),
                at(15, 40.0),
                None,
            ),
        ];
        for (name, prev, curr, expected) in cases {
            assert_eq!(
                rate_per_second(prev.as_ref(), &curr),
                expected,
                "case: {name}"
            );
        }
    }

    /// Meters an application that publishes everything reports, with cumulative totals.
    fn meters(
        requests: f64,
        request_seconds: f64,
        errors: MeterValue<f64>,
        gc: f64,
        uptime: f64,
    ) -> Meters {
        Meters {
            heap_used: MeterValue::Published(300.0 * 1024.0 * 1024.0),
            heap_max: MeterValue::Published(1024.0 * 1024.0 * 1024.0),
            cpu_usage: MeterValue::Published(0.25),
            live_threads: MeterValue::Published(42.0),
            gc_pause_seconds: MeterValue::Published(gc),
            http_requests: MeterValue::Published(TimerTotals {
                count: requests,
                total_seconds: request_seconds,
            }),
            http_server_errors: errors,
            db_connections_active: MeterValue::Published(3.0),
            uptime_seconds: MeterValue::Published(uptime),
        }
    }

    fn reached(meters: Meters, own_requests: u32) -> RawScrape {
        RawScrape::Reached(ReachedScrape {
            health: ReportedHealth::Up,
            version: Some("2.4.1".into()),
            meters,
            own_requests,
        })
    }

    /// Folds `raws` through one history, `gap_secs` apart; returns every report's gauges.
    fn scrapes(gap_secs: u64, raws: Vec<RawScrape>) -> Vec<BTreeMap<ApplicationGauge, f64>> {
        let t0 = Instant::now();
        let mut history = ScrapeHistory::default();
        let mut gauges = Vec::new();
        for (i, raw) in (0u64..).zip(raws) {
            let (next, report) = history.advance(raw, t0 + Duration::from_secs(i * gap_secs));
            history = next;
            gauges.push(gauges_of(&report));
        }
        gauges
    }

    /// Two scrapes 15 s apart; returns the second report's gauges.
    fn second(first: RawScrape, second: RawScrape) -> BTreeMap<ApplicationGauge, f64> {
        scrapes(15, vec![first, second]).remove(1)
    }

    const RATES: [ApplicationGauge; 4] = [
        G::HttpRequestsPerSecond,
        G::HttpServerErrorsPerSecond,
        G::HttpMeanLatencyMs,
        G::GcPausePercent,
    ];

    fn assert_rates(gauges: &BTreeMap<ApplicationGauge, f64>, present: bool, case: &str) {
        for rate in RATES {
            assert_eq!(
                gauges.contains_key(&rate),
                present,
                "{case}: {rate:?} present should be {present}: {gauges:?}"
            );
        }
    }

    #[test]
    fn the_first_scrape_reports_gauges_but_no_rates() {
        let (_, report) = ScrapeHistory::default().advance(
            reached(
                meters(100.0, 5.0, MeterValue::Published(2.0), 1.0, 600.0),
                12,
            ),
            Instant::now(),
        );
        assert_eq!(
            report.health(),
            ApplicationHealth::Reported(ReportedHealth::Up)
        );
        assert_eq!(version_of(&report).as_deref(), Some("2.4.1"));
        let expected = BTreeMap::from([
            (G::HeapUsedBytes, 300.0 * 1024.0 * 1024.0),
            (G::HeapMaxBytes, 1024.0 * 1024.0 * 1024.0),
            (G::CpuPercent, 25.0),
            (G::LiveThreads, 42.0),
            (G::DbConnectionsActive, 3.0),
            (G::UptimeSeconds, 600.0),
        ]);
        assert_eq!(gauges_of(&report), expected);
    }

    #[test]
    fn a_second_scrape_derives_every_rate() {
        // 20 s apart (not the default interval): 312 more requests, 12 of them the agent's
        // own; 3 s more request time; 30 more server errors; 0.3 s more GC pause.
        let gauges = scrapes(
            20,
            vec![
                reached(
                    meters(100.0, 5.0, MeterValue::Published(2.0), 1.0, 600.0),
                    12,
                ),
                reached(
                    meters(412.0, 8.0, MeterValue::Published(32.0), 1.3, 620.0),
                    12,
                ),
            ],
        )
        .remove(1);
        let near = |gauge: G, expected: f64| {
            let actual = gauges.get(&gauge).copied();
            assert!(
                actual.is_some_and(|a| (a - expected).abs() < 1e-9),
                "{gauge:?}: {actual:?}, expected {expected}"
            );
        };
        near(G::HttpRequestsPerSecond, (312.0 - 12.0) / 20.0);
        near(G::HttpServerErrorsPerSecond, 30.0 / 20.0);
        near(G::HttpMeanLatencyMs, 3.0 / 312.0 * 1000.0);
        near(G::GcPausePercent, 0.3 / 20.0 * 100.0);
        near(G::UptimeSeconds, 620.0);
    }

    #[test]
    fn gauge_rules_for_missing_negative_and_non_finite_meters() {
        let t0 = Instant::now();
        let with = |change: fn(&mut Meters)| {
            let mut m = meters(100.0, 5.0, MeterValue::Published(2.0), 1.0, 600.0);
            change(&mut m);
            gauges_of(&ScrapeHistory::default().advance(reached(m, 0), t0).1)
        };
        let untouched = with(|_| {});
        /// (case, how the meters change, the gauge it concerns, its expected value)
        type GaugeRule = (&'static str, fn(&mut Meters), G, Option<f64>);
        let cases: [GaugeRule; 8] = [
            (
                "zero CPU (an idle JVM) is kept",
                |m| m.cpu_usage = MeterValue::Published(0.0),
                G::CpuPercent,
                Some(0.0),
            ),
            (
                "negative max heap is missing",
                |m| m.heap_max = MeterValue::Published(-1.0),
                G::HeapMaxBytes,
                None,
            ),
            (
                "negative CPU is missing",
                |m| m.cpu_usage = MeterValue::Published(-0.01),
                G::CpuPercent,
                None,
            ),
            (
                "NaN CPU is missing",
                |m| m.cpu_usage = MeterValue::Published(f64::NAN),
                G::CpuPercent,
                None,
            ),
            (
                "infinite threads are missing",
                |m| m.live_threads = MeterValue::Published(f64::INFINITY),
                G::LiveThreads,
                None,
            ),
            (
                "an unpublished pool is missing, not zero",
                |m| m.db_connections_active = MeterValue::NotPublished,
                G::DbConnectionsActive,
                None,
            ),
            (
                "an unavailable heap is missing",
                |m| m.heap_used = MeterValue::Unavailable,
                G::HeapUsedBytes,
                None,
            ),
            (
                "zero heap max is kept",
                |m| m.heap_max = MeterValue::Published(0.0),
                G::HeapMaxBytes,
                Some(0.0),
            ),
        ];
        for (name, change, gauge, expected) in cases {
            // Only the gauge under test changes; every other gauge is kept.
            let mut expected_gauges = untouched.clone();
            match expected {
                Some(value) => expected_gauges.insert(gauge, value),
                None => expected_gauges.remove(&gauge),
            };
            assert_eq!(with(change), expected_gauges, "case: {name}");
        }
    }

    #[test]
    fn the_server_error_rate_starts_at_zero_only_when_requests_are_published() {
        let cases = [
            (
                "404 with requests published: no 5xx yet, so a burst shows",
                MeterValue::NotPublished,
                MeterValue::Published(6.0),
                true,
                Some(6.0 / 15.0),
            ),
            (
                "404 twice with requests published: zero errors",
                MeterValue::NotPublished,
                MeterValue::NotPublished,
                true,
                Some(0.0),
            ),
            (
                "an unavailable error meter is missing, never zero",
                MeterValue::Unavailable,
                MeterValue::Unavailable,
                true,
                None,
            ),
            (
                "404 without any requests published: missing",
                MeterValue::NotPublished,
                MeterValue::NotPublished,
                false,
                None,
            ),
            (
                "an unavailable baseline gives no rate, never one from zero",
                MeterValue::Unavailable,
                MeterValue::Published(6.0),
                true,
                None,
            ),
        ];
        for (name, first, second_errors, with_requests, expected) in cases {
            let make = |errors: MeterValue<f64>, count: f64| {
                let mut m = meters(count, count * 0.01, errors, 1.0, 600.0);
                if !with_requests {
                    m.http_requests = MeterValue::NotPublished;
                }
                reached(m, 0)
            };
            let gauges = second(make(first, 100.0), make(second_errors, 130.0));
            let actual = gauges.get(&G::HttpServerErrorsPerSecond).copied();
            assert_eq!(actual, expected, "case: {name}");
        }
    }

    #[test]
    fn a_restart_resets_every_counter_even_when_counts_grew_past_the_old_ones() {
        // Uptime went down: the application restarted and has already served more requests
        // than before. No rate is derived this round.
        let gauges = second(
            reached(
                meters(100.0, 5.0, MeterValue::Published(2.0), 1.0, 600.0),
                0,
            ),
            reached(meters(500.0, 9.0, MeterValue::Published(9.0), 2.0, 20.0), 0),
        );
        for rate in [
            G::HttpRequestsPerSecond,
            G::HttpServerErrorsPerSecond,
            G::HttpMeanLatencyMs,
            G::GcPausePercent,
        ] {
            assert_eq!(gauges.get(&rate), None, "{rate:?} after an uptime drop");
        }
        assert_eq!(gauges.get(&G::UptimeSeconds), Some(&20.0));
    }

    #[test]
    fn any_counter_going_down_resets_every_counter() {
        // Uptime is unpublished, so only the counters can reveal a restart. Each row lowers
        // one counter in the second scrape; the control row lowers none.
        // (case, requests, request seconds, errors, gc seconds, rates expected)
        let cases = [
            (
                "nothing went down: the control",
                400.0,
                8.0,
                10.0,
                1.3,
                true,
            ),
            ("the request count went down", 50.0, 8.0, 10.0, 1.3, false),
            (
                "the request seconds went down",
                400.0,
                4.0,
                10.0,
                1.3,
                false,
            ),
            ("the error count went down", 400.0, 8.0, 1.0, 1.3, false),
            ("the GC total went down", 400.0, 8.0, 10.0, 0.5, false),
        ];
        for (name, requests, request_seconds, errors, gc, rates) in cases {
            let mut first = meters(100.0, 5.0, MeterValue::Published(9.0), 1.0, 600.0);
            let mut next = meters(
                requests,
                request_seconds,
                MeterValue::Published(errors),
                gc,
                615.0,
            );
            first.uptime_seconds = MeterValue::NotPublished;
            next.uptime_seconds = MeterValue::NotPublished;
            assert_rates(&second(reached(first, 0), reached(next, 0)), rates, name);
        }
    }

    #[test]
    fn own_requests_are_subtracted_and_the_rate_is_floored_at_zero() {
        let cases = [
            (
                "some of the traffic is the agent's",
                112.0,
                12,
                Some(100.0 / 15.0),
            ),
            ("all of the traffic is the agent's", 112.0, 112, Some(0.0)),
            ("the agent counted more than arrived", 112.0, 200, Some(0.0)),
        ];
        for (name, delta, own, expected) in cases {
            let gauges = second(
                reached(
                    meters(100.0, 5.0, MeterValue::Published(0.0), 1.0, 600.0),
                    own,
                ),
                reached(
                    meters(100.0 + delta, 6.0, MeterValue::Published(0.0), 1.0, 615.0),
                    0,
                ),
            );
            let actual = gauges.get(&G::HttpRequestsPerSecond).copied();
            assert!(
                matches!((actual, expected), (Some(a), Some(e)) if (a - e).abs() < 1e-9),
                "case {name}: {actual:?}, expected {expected:?}"
            );
        }
    }

    #[test]
    fn mean_latency_is_missing_when_no_request_arrived() {
        let gauges = second(
            reached(
                meters(100.0, 5.0, MeterValue::Published(0.0), 1.0, 600.0),
                0,
            ),
            reached(
                meters(100.0, 5.0, MeterValue::Published(0.0), 1.0, 615.0),
                0,
            ),
        );
        assert_eq!(gauges.get(&G::HttpMeanLatencyMs), None);
        assert_eq!(gauges.get(&G::HttpRequestsPerSecond), Some(&0.0));
    }

    #[test]
    fn an_unreachable_scrape_reports_no_gauges_and_clears_the_baseline() {
        let t0 = Instant::now();
        let secs = |s| t0 + Duration::from_secs(s);
        let failures = [
            ScrapeFailure::Connect,
            ScrapeFailure::Timeout,
            ScrapeFailure::Unauthorized,
            ScrapeFailure::HttpStatus(500),
            ScrapeFailure::BadBody,
        ];
        for failure in failures {
            let (history, _) = ScrapeHistory::default().advance(
                reached(
                    meters(100.0, 5.0, MeterValue::Published(0.0), 1.0, 600.0),
                    0,
                ),
                secs(0),
            );
            let (history, report) = history.advance(RawScrape::Unreachable(failure), secs(15));
            assert_eq!(report.health(), ApplicationHealth::Unreachable(failure));
            assert_eq!(version_of(&report), None, "{failure:?}");
            assert!(
                gauges_of(&report).is_empty(),
                "{failure:?}: no gauges: {:?}",
                gauges_of(&report)
            );
            // Back after the outage: no rate averaged across it.
            let (_, report) = history.advance(
                reached(
                    meters(400.0, 8.0, MeterValue::Published(0.0), 1.3, 630.0),
                    0,
                ),
                secs(30),
            );
            assert_eq!(
                gauges_of(&report).get(&G::HttpRequestsPerSecond),
                None,
                "{failure:?}"
            );
        }
    }

    #[test]
    fn the_version_is_cut_to_64_chars_on_a_char_boundary() {
        let long_ascii = "v".repeat(70);
        // 63 ASCII chars then a 4-byte char: byte 64 falls inside the last char.
        let straddling = format!("{}😀tail", "v".repeat(63));
        let cases = [
            ("short", "2.4.1".to_string(), Some("2.4.1".to_string())),
            ("exactly 64", "v".repeat(64), Some("v".repeat(64))),
            ("longer", long_ascii, Some("v".repeat(64))),
            (
                "multibyte at the edge",
                straddling,
                Some(format!("{}😀", "v".repeat(63))),
            ),
            (
                "kept as sent, padding included",
                " 2.4.1 ".to_string(),
                Some(" 2.4.1 ".to_string()),
            ),
            ("empty says nothing", String::new(), None),
            ("blank says nothing", "   ".to_string(), None),
            (
                "blank within the first 64 chars says nothing",
                format!("{}2.4.1", " ".repeat(64)),
                None,
            ),
        ];
        for (name, version, expected) in cases {
            let raw = RawScrape::Reached(ReachedScrape {
                health: ReportedHealth::Up,
                version: Some(version),
                meters: meters(1.0, 0.1, MeterValue::Published(0.0), 0.0, 1.0),
                own_requests: 0,
            });
            let (_, report) = ScrapeHistory::default().advance(raw, Instant::now());
            assert_eq!(version_of(&report), expected, "case: {name}");
        }
    }

    #[test]
    fn two_fully_published_scrapes_derive_every_gauge() {
        let all = scrapes(
            15,
            vec![
                reached(
                    meters(100.0, 5.0, MeterValue::Published(2.0), 1.0, 600.0),
                    0,
                ),
                reached(
                    meters(160.0, 5.6, MeterValue::Published(4.0), 1.2, 615.0),
                    0,
                ),
            ],
        );
        // Listed independently of `ApplicationGauge::ALL`, so a gauge missing from it shows.
        let every_gauge = [
            G::HeapUsedBytes,
            G::HeapMaxBytes,
            G::CpuPercent,
            G::LiveThreads,
            G::GcPausePercent,
            G::HttpRequestsPerSecond,
            G::HttpServerErrorsPerSecond,
            G::HttpMeanLatencyMs,
            G::DbConnectionsActive,
            G::UptimeSeconds,
        ];
        let derived: Vec<G> = all[1].keys().copied().collect();
        assert_eq!(derived, every_gauge, "every gauge is derived");
        assert_eq!(
            ApplicationGauge::ALL,
            every_gauge,
            "ALL lists every gauge once"
        );
    }

    #[test]
    fn reported_health_is_kept_whatever_it_is() {
        for health in [
            ReportedHealth::Up,
            ReportedHealth::Down,
            ReportedHealth::OutOfService,
            ReportedHealth::Unknown,
        ] {
            let raw = RawScrape::Reached(ReachedScrape {
                health,
                version: None,
                meters: meters(1.0, 0.1, MeterValue::Published(0.0), 0.0, 1.0),
                own_requests: 0,
            });
            let (_, report) = ScrapeHistory::default().advance(raw, Instant::now());
            assert_eq!(
                report.health(),
                ApplicationHealth::Reported(health),
                "{health:?}"
            );
        }
    }

    #[test]
    fn a_published_error_count_falling_to_unpublished_is_a_reset() {
        // A 404 reads as zero errors when requests are published, so 6 then 404 is a count
        // going down, while 0 then 404 is no change.
        let cases = [
            ("six errors, then 404: a reset", 6.0, false),
            ("zero errors, then 404: no change", 0.0, true),
        ];
        for (name, errors, rates) in cases {
            let gauges = second(
                reached(
                    meters(100.0, 5.0, MeterValue::Published(errors), 1.0, 600.0),
                    0,
                ),
                reached(meters(130.0, 5.3, MeterValue::NotPublished, 1.1, 615.0), 0),
            );
            assert_rates(&gauges, rates, name);
        }
    }

    #[test]
    fn equal_uptime_is_not_a_restart() {
        let gauges = second(
            reached(
                meters(100.0, 5.0, MeterValue::Published(0.0), 1.0, 600.0),
                0,
            ),
            reached(
                meters(130.0, 5.3, MeterValue::Published(0.0), 1.1, 600.0),
                0,
            ),
        );
        assert_rates(&gauges, true, "uptime unchanged");
    }

    #[test]
    fn the_scrape_after_a_reset_is_the_next_baseline() {
        let all = scrapes(
            15,
            vec![
                reached(
                    meters(100.0, 5.0, MeterValue::Published(2.0), 1.0, 600.0),
                    0,
                ),
                // Restarted: uptime went down.
                reached(meters(500.0, 9.0, MeterValue::Published(9.0), 2.0, 20.0), 0),
                reached(
                    meters(530.0, 9.3, MeterValue::Published(12.0), 2.3, 35.0),
                    0,
                ),
            ],
        );
        assert_rates(&all[1], false, "the reset scrape");
        assert_rates(&all[2], true, "the scrape after the reset");
        let requests = all[2].get(&G::HttpRequestsPerSecond).copied();
        assert!(
            requests.is_some_and(|r| (r - 30.0 / 15.0).abs() < 1e-9),
            "measured from the reset scrape: {requests:?}"
        );
    }

    #[test]
    fn the_scrape_after_recovering_from_an_outage_is_the_next_baseline() {
        let all = scrapes(
            15,
            vec![
                reached(
                    meters(100.0, 5.0, MeterValue::Published(0.0), 1.0, 600.0),
                    0,
                ),
                RawScrape::Unreachable(ScrapeFailure::Connect),
                reached(
                    meters(400.0, 8.0, MeterValue::Published(0.0), 1.3, 630.0),
                    0,
                ),
                reached(
                    meters(460.0, 8.6, MeterValue::Published(3.0), 1.6, 645.0),
                    0,
                ),
            ],
        );
        assert_rates(&all[2], false, "the recovery scrape");
        assert_rates(&all[3], true, "the scrape after recovery");
    }

    #[test]
    fn a_meter_missing_for_one_scrape_starts_a_new_baseline_for_its_rate() {
        let mut gap = meters(130.0, 5.3, MeterValue::Published(0.0), 0.0, 615.0);
        gap.gc_pause_seconds = MeterValue::Unavailable;
        let all = scrapes(
            15,
            vec![
                reached(
                    meters(100.0, 5.0, MeterValue::Published(0.0), 1.0, 600.0),
                    0,
                ),
                reached(gap, 0),
                reached(
                    meters(160.0, 5.6, MeterValue::Published(0.0), 1.9, 630.0),
                    0,
                ),
            ],
        );
        assert_eq!(all[1].get(&G::GcPausePercent), None, "gc unavailable");
        assert_eq!(
            all[2].get(&G::GcPausePercent),
            None,
            "no gc rate across the gap: the gap started a new baseline"
        );
        assert!(
            all[2].contains_key(&G::HttpRequestsPerSecond),
            "the other rates go on: {:?}",
            all[2]
        );
    }

    #[test]
    fn a_non_finite_counter_gives_no_rate_and_leaves_the_others() {
        use G::{GcPausePercent as Gc, HttpMeanLatencyMs as Latency};
        use G::{HttpRequestsPerSecond as Requests, HttpServerErrorsPerSecond as Errors};
        type Change = fn(&mut Meters, &mut Meters);
        // (case, what goes non-finite and where, rates that must be missing)
        let cases: [(&str, Change, &[G]); 9] = [
            (
                "GC NaN before",
                |a, _| a.gc_pause_seconds = MeterValue::Published(f64::NAN),
                &[Gc],
            ),
            (
                "GC infinite before",
                |a, _| a.gc_pause_seconds = MeterValue::Published(f64::INFINITY),
                &[Gc],
            ),
            (
                "GC negative infinite after",
                |_, b| b.gc_pause_seconds = MeterValue::Published(f64::NEG_INFINITY),
                &[Gc],
            ),
            (
                "request count infinite before",
                |a, _| {
                    a.http_requests = MeterValue::Published(TimerTotals {
                        count: f64::INFINITY,
                        total_seconds: 5.0,
                    })
                },
                &[Requests, Latency],
            ),
            (
                "request seconds negative infinite after",
                |_, b| {
                    b.http_requests = MeterValue::Published(TimerTotals {
                        count: 130.0,
                        total_seconds: f64::NEG_INFINITY,
                    })
                },
                &[Latency],
            ),
            (
                "error count infinite before",
                |a, _| a.http_server_errors = MeterValue::Published(f64::INFINITY),
                &[Errors],
            ),
            (
                "error count NaN after",
                |_, b| b.http_server_errors = MeterValue::Published(f64::NAN),
                &[Errors],
            ),
            // A non-finite uptime can't reveal a restart, and hides nothing else.
            (
                "uptime infinite before",
                |a, _| a.uptime_seconds = MeterValue::Published(f64::INFINITY),
                &[],
            ),
            // Finite counters whose rate overflows: the rate is dropped, never infinite.
            (
                "GC total jumping to f64::MAX",
                |a, b| {
                    a.gc_pause_seconds = MeterValue::Published(0.0);
                    b.gc_pause_seconds = MeterValue::Published(f64::MAX);
                },
                &[Gc],
            ),
        ];
        for (name, change, missing) in cases {
            let mut first = meters(100.0, 5.0, MeterValue::Published(0.0), 1.0, 600.0);
            let mut next = meters(130.0, 5.3, MeterValue::Published(3.0), 1.3, 615.0);
            change(&mut first, &mut next);
            let gauges = second(reached(first, 0), reached(next, 0));
            for rate in RATES {
                let present = !missing.contains(&rate);
                assert_eq!(
                    gauges.contains_key(&rate),
                    present,
                    "case {name}: {rate:?} present should be {present}: {gauges:?}"
                );
            }
            assert!(
                gauges.values().all(|v| v.is_finite()),
                "case {name}: every gauge is finite: {gauges:?}"
            );
        }
    }
}
