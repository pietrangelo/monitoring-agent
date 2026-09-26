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

//! The actuator adapter (RFC 0009 §4): what an Actuator answered, turned into the domain's
//! readings. Actuator's JSON is parsed into DTOs here and converted in one place; unknown
//! fields are ignored.

use super::report::{MeterValue, ReportedHealth, ScrapeFailure, TimerTotals};
use serde::Deserialize;

/// One Actuator answer as read: its HTTP status, and its body (at most 64 KiB, or the read is
/// a `TransportFailure::BodyTooLarge`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActuatorResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Why no answer arrived. An HTTP status is always an answer, never one of these, so a 404 or
/// a 401 can only be written one way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportFailure {
    Connect,
    Timeout,
    /// The body was longer than the cap, so it was never parsed.
    BodyTooLarge,
}

impl From<TransportFailure> for ScrapeFailure {
    fn from(failure: TransportFailure) -> Self {
        match failure {
            TransportFailure::Connect => Self::Connect,
            TransportFailure::Timeout => Self::Timeout,
            TransportFailure::BodyTooLarge => Self::BadBody,
        }
    }
}

/// What a request to one endpoint came to: an answer, or a failure before one.
pub type Answer = Result<ActuatorResponse, TransportFailure>;

/// `/actuator/health`'s top level. The components are left alone.
#[derive(Deserialize)]
struct HealthDto {
    status: String,
}

/// `/actuator/metrics/{name}`: each statistic is kept as JSON, so one that isn't a number
/// (Jackson writes NaN as `"NaN"`) costs only itself.
#[derive(Deserialize)]
struct MetricDto {
    #[serde(default)]
    measurements: Vec<MeasurementDto>,
}

#[derive(Deserialize)]
struct MeasurementDto {
    statistic: String,
    value: serde_json::Value,
}

impl MetricDto {
    /// The named statistic, if it is there and a number.
    fn statistic(&self, name: &str) -> Option<f64> {
        self.measurements
            .iter()
            .find(|m| m.statistic == name)?
            .value
            .as_f64()
    }
}

/// `/actuator/info`'s build section.
#[derive(Deserialize)]
struct InfoDto {
    build: Option<BuildDto>,
}

#[derive(Deserialize)]
struct BuildDto {
    version: Option<String>,
}

/// A metrics answer as a meter value: a 200's parsed body, a 404's "not published", or
/// unavailable for anything else, a transport failure included.
fn metric(answer: Answer) -> MeterValue<MetricDto> {
    match answer {
        Ok(response) if response.status == 200 => serde_json::from_slice(&response.body)
            .map_or(MeterValue::Unavailable, MeterValue::Published),
        Ok(response) if response.status == 404 => MeterValue::NotPublished,
        Ok(_) | Err(_) => MeterValue::Unavailable,
    }
}

/// Takes what `pick` finds in a published body; unavailable when it finds nothing.
fn pick<T>(answer: Answer, pick: impl FnOnce(&MetricDto) -> Option<T>) -> MeterValue<T> {
    match metric(answer) {
        MeterValue::Published(dto) => {
            pick(&dto).map_or(MeterValue::Unavailable, MeterValue::Published)
        }
        MeterValue::NotPublished => MeterValue::NotPublished,
        MeterValue::Unavailable => MeterValue::Unavailable,
    }
}

/// `/actuator/health`: 200, or 503 with a body (Actuator's answer for `DOWN` and
/// `OUT_OF_SERVICE`), is a health reading; anything else is why the application is
/// unreachable.
pub fn health_of(answer: Answer) -> Result<ReportedHealth, ScrapeFailure> {
    let response = answer?;
    match response.status {
        200 | 503 => serde_json::from_slice::<HealthDto>(&response.body)
            .map(|dto| ReportedHealth::from_status(&dto.status))
            .map_err(|_| ScrapeFailure::BadBody),
        401 | 403 => Err(ScrapeFailure::Unauthorized),
        status => Err(ScrapeFailure::HttpStatus(status)),
    }
}

/// A gauge-like meter from `/actuator/metrics/{name}`: its `VALUE`.
pub fn reading_of(answer: Answer) -> MeterValue<f64> {
    pick(answer, |dto| dto.statistic("VALUE"))
}

/// A timer from `/actuator/metrics/{name}`: its `COUNT` and `TOTAL_TIME`, both required.
pub fn timer_of(answer: Answer) -> MeterValue<TimerTotals> {
    pick(answer, |dto| {
        Some(TimerTotals {
            count: dto.statistic("COUNT")?,
            total_seconds: dto.statistic("TOTAL_TIME")?,
        })
    })
}

/// A timer's `TOTAL_TIME` from `/actuator/metrics/{name}`, as a reading.
pub fn total_time_of(answer: Answer) -> MeterValue<f64> {
    pick(answer, |dto| dto.statistic("TOTAL_TIME"))
}

/// A meter's `COUNT` from `/actuator/metrics/{name}`, as a reading.
pub fn count_of(answer: Answer) -> MeterValue<f64> {
    pick(answer, |dto| dto.statistic("COUNT"))
}

/// `build.version` from `/actuator/info`, as sent; `None` when there isn't one.
pub fn version_of(answer: Answer) -> Option<String> {
    let response = answer.ok().filter(|response| response.status == 200)?;
    let info: InfoDto = serde_json::from_slice(&response.body).ok()?;
    info.build?.version
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(status: u16, body: &str) -> Answer {
        Ok(ActuatorResponse {
            status,
            body: body.as_bytes().to_vec(),
        })
    }

    /// `/actuator/metrics/jvm.memory.max`, as the Spring Boot 4.1 reference documents it.
    const GAUGE_BOOT_4: &str = r#"{
        "availableTags" : [ { "tag" : "area", "values" : [ "heap", "nonheap" ] } ],
        "baseUnit" : "bytes",
        "description" : "The maximum amount of memory in bytes that can be used for memory management",
        "measurements" : [ { "statistic" : "VALUE", "value" : 2.936016893E9 } ],
        "name" : "jvm.memory.max"
    }"#;

    /// A timer whose statistics come in another order than usual.
    const TIMER_OUT_OF_ORDER: &str = r#"{"measurements":[{"statistic":"TOTAL_TIME","value":8.25},{"statistic":"MAX","value":0.3},{"statistic":"COUNT","value":412.0}]}"#;

    /// A timer whose `TOTAL_TIME` isn't a number.
    const TIMER_WITH_NAN_TOTAL: &str = r#"{"measurements":[{"statistic":"COUNT","value":7.0},{"statistic":"TOTAL_TIME","value":"NaN"}]}"#;

    /// A timer whose `MAX` Jackson wrote as the string `"NaN"`.
    const TIMER_WITH_NAN_MAX: &str = r#"{"measurements":[{"statistic":"COUNT","value":7.0},{"statistic":"TOTAL_TIME","value":0.5},{"statistic":"MAX","value":"NaN"}]}"#;

    /// `/actuator/metrics/http.server.requests`, a timer, in Spring Boot 2.7's shape.
    const TIMER_BOOT_2: &str = r#"{
        "name":"http.server.requests","description":null,"baseUnit":"seconds",
        "measurements":[
            {"statistic":"COUNT","value":412.0},
            {"statistic":"TOTAL_TIME","value":8.25},
            {"statistic":"MAX","value":0.3}
        ],
        "availableTags":[{"tag":"outcome","values":["SUCCESS","SERVER_ERROR"]}]
    }"#;

    #[test]
    fn health_is_read_from_the_status_of_a_200_or_a_503_with_a_body() {
        let cases = [
            (
                "UP",
                ok(
                    200,
                    r#"{"status":"UP","components":{"db":{"status":"UP"}}}"#,
                ),
                Ok(ReportedHealth::Up),
            ),
            (
                "DOWN is a 503 with a body",
                ok(503, r#"{"status":"DOWN"}"#),
                Ok(ReportedHealth::Down),
            ),
            (
                "OUT_OF_SERVICE is a 503 with a body",
                ok(503, r#"{"status":"OUT_OF_SERVICE"}"#),
                Ok(ReportedHealth::OutOfService),
            ),
            (
                "a custom status",
                ok(200, r#"{"status":"DEGRADED"}"#),
                Ok(ReportedHealth::Unknown),
            ),
            (
                "Boot 4 probes add groups",
                ok(200, r#"{"status":"UP","groups":["liveness","readiness"]}"#),
                Ok(ReportedHealth::Up),
            ),
            (
                "a 503 without a parseable body",
                ok(503, "Service Unavailable"),
                Err(ScrapeFailure::BadBody),
            ),
            (
                "a 200 that isn't JSON",
                ok(200, "<html>login</html>"),
                Err(ScrapeFailure::BadBody),
            ),
            (
                "a 200 without a status",
                ok(200, r#"{"components":{}}"#),
                Err(ScrapeFailure::BadBody),
            ),
            (
                "UNKNOWN",
                ok(200, r#"{"status":"UNKNOWN"}"#),
                Ok(ReportedHealth::Unknown),
            ),
            (
                "a status that isn't a string",
                ok(200, r#"{"status":1}"#),
                Err(ScrapeFailure::BadBody),
            ),
            (
                "a 503 whose status isn't a string",
                ok(503, r#"{"status":1}"#),
                Err(ScrapeFailure::BadBody),
            ),
            (
                "a 503 with a custom status is unknown, not down",
                ok(503, r#"{"status":"DEGRADED"}"#),
                Ok(ReportedHealth::Unknown),
            ),
            (
                "a 503 whose body says UP is taken at its word",
                ok(503, r#"{"status":"UP"}"#),
                Ok(ReportedHealth::Up),
            ),
            (
                "a 503 whose JSON has no status",
                ok(503, r#"{"error":"x"}"#),
                Err(ScrapeFailure::BadBody),
            ),
            (
                "204 is not a health answer",
                ok(204, ""),
                Err(ScrapeFailure::HttpStatus(204)),
            ),
            (
                "202 with a body is not one either",
                ok(202, r#"{"status":"UP"}"#),
                Err(ScrapeFailure::HttpStatus(202)),
            ),
            (
                "429 is not a health answer",
                ok(429, r#"{"status":"UP"}"#),
                Err(ScrapeFailure::HttpStatus(429)),
            ),
            (
                "201 is not a health answer",
                ok(201, r#"{"status":"UP"}"#),
                Err(ScrapeFailure::HttpStatus(201)),
            ),
            ("401", ok(401, ""), Err(ScrapeFailure::Unauthorized)),
            ("403", ok(403, "{}"), Err(ScrapeFailure::Unauthorized)),
            (
                "404: no health endpoint exposed",
                ok(404, "{}"),
                Err(ScrapeFailure::HttpStatus(404)),
            ),
            (
                "500",
                ok(500, r#"{"status":"UP"}"#),
                Err(ScrapeFailure::HttpStatus(500)),
            ),
            (
                "a redirect is never followed",
                ok(302, ""),
                Err(ScrapeFailure::HttpStatus(302)),
            ),
            (
                "connection refused",
                Err(TransportFailure::Connect),
                Err(ScrapeFailure::Connect),
            ),
            (
                "timed out",
                Err(TransportFailure::Timeout),
                Err(ScrapeFailure::Timeout),
            ),
            (
                "body over the cap",
                Err(TransportFailure::BodyTooLarge),
                Err(ScrapeFailure::BadBody),
            ),
        ];
        for (name, answer, expected) in cases {
            assert_eq!(health_of(answer), expected, "case: {name}");
        }
    }

    #[test]
    fn a_meter_reading_is_its_value_or_says_why_there_is_none() {
        let cases = [
            (
                "Boot 4 gauge",
                ok(200, GAUGE_BOOT_4),
                MeterValue::Published(2.936016893E9),
            ),
            (
                "the VALUE among other statistics",
                ok(
                    200,
                    r#"{"measurements":[{"statistic":"COUNT","value":1.0},{"statistic":"VALUE","value":42.0}]}"#,
                ),
                MeterValue::Published(42.0),
            ),
            (
                "a negative value is passed on",
                ok(
                    200,
                    r#"{"measurements":[{"statistic":"VALUE","value":-1.0}]}"#,
                ),
                MeterValue::Published(-1.0),
            ),
            (
                "an integer value is a number too",
                ok(
                    200,
                    r#"{"measurements":[{"statistic":"VALUE","value":42}]}"#,
                ),
                MeterValue::Published(42.0),
            ),
            ("404: not published", ok(404, ""), MeterValue::NotPublished),
            (
                "404 with an error page is still not published",
                ok(404, r#"{"timestamp":"x","status":404,"error":"Not Found"}"#),
                MeterValue::NotPublished,
            ),
            (
                "no VALUE statistic",
                ok(
                    200,
                    r#"{"measurements":[{"statistic":"COUNT","value":3.0}]}"#,
                ),
                MeterValue::Unavailable,
            ),
            (
                "no measurements",
                ok(200, r#"{"name":"x"}"#),
                MeterValue::Unavailable,
            ),
            ("not JSON", ok(200, "nope"), MeterValue::Unavailable),
            (
                "a value that isn't a number",
                ok(
                    200,
                    r#"{"measurements":[{"statistic":"VALUE","value":"NaN"}]}"#,
                ),
                MeterValue::Unavailable,
            ),
            ("500", ok(500, GAUGE_BOOT_4), MeterValue::Unavailable),
            (
                "204 with a gauge body",
                ok(204, GAUGE_BOOT_4),
                MeterValue::Unavailable,
            ),
            ("401", ok(401, ""), MeterValue::Unavailable),
            (
                "timed out",
                Err(TransportFailure::Timeout),
                MeterValue::Unavailable,
            ),
        ];
        for (name, answer, expected) in cases {
            assert_eq!(reading_of(answer), expected, "case: {name}");
        }
    }

    #[test]
    fn a_timer_is_its_count_and_total_time_or_says_why_not() {
        let totals = |count, total_seconds| {
            MeterValue::Published(TimerTotals {
                count,
                total_seconds,
            })
        };
        let cases = [
            ("Boot 2 timer", ok(200, TIMER_BOOT_2), totals(412.0, 8.25)),
            (
                "statistics are found by name, in any order",
                ok(200, TIMER_OUT_OF_ORDER),
                totals(412.0, 8.25),
            ),
            (
                "a TOTAL_TIME that isn't a number is not zero",
                ok(200, TIMER_WITH_NAN_TOTAL),
                MeterValue::Unavailable,
            ),
            (
                "Jackson writes NaN as a string: one bad statistic costs only itself",
                ok(200, TIMER_WITH_NAN_MAX),
                totals(7.0, 0.5),
            ),
            (
                "404 error page",
                ok(404, r#"{"status":404}"#),
                MeterValue::NotPublished,
            ),
            (
                "404: no request served yet",
                ok(404, ""),
                MeterValue::NotPublished,
            ),
            (
                "COUNT without TOTAL_TIME",
                ok(
                    200,
                    r#"{"measurements":[{"statistic":"COUNT","value":4.0}]}"#,
                ),
                MeterValue::Unavailable,
            ),
            (
                "TOTAL_TIME without COUNT",
                ok(
                    200,
                    r#"{"measurements":[{"statistic":"TOTAL_TIME","value":4.0}]}"#,
                ),
                MeterValue::Unavailable,
            ),
            (
                "a COUNT that isn't a number is not zero",
                ok(
                    200,
                    r#"{"measurements":[{"statistic":"COUNT","value":"NaN"},{"statistic":"TOTAL_TIME","value":0.5}]}"#,
                ),
                MeterValue::Unavailable,
            ),
            (
                "500 with a valid body",
                ok(500, TIMER_BOOT_2),
                MeterValue::Unavailable,
            ),
            (
                "401 with a valid body",
                ok(401, TIMER_BOOT_2),
                MeterValue::Unavailable,
            ),
            (
                "204 with a timer body",
                ok(204, TIMER_BOOT_2),
                MeterValue::Unavailable,
            ),
            (
                "connection refused",
                Err(TransportFailure::Connect),
                MeterValue::Unavailable,
            ),
        ];
        for (name, answer, expected) in cases {
            assert_eq!(timer_of(answer), expected, "case: {name}");
        }
    }

    #[test]
    fn a_count_or_total_time_reading_takes_that_statistic_of_a_timer() {
        // A failure is never "not published": the domain reads a 404 on the 5xx meter as
        // zero errors, so a timed-out query must not look like one.
        let cases = [
            (
                "the 5xx count",
                count_of(ok(200, TIMER_BOOT_2)),
                MeterValue::Published(412.0),
            ),
            (
                "the count found by name",
                count_of(ok(200, TIMER_OUT_OF_ORDER)),
                MeterValue::Published(412.0),
            ),
            (
                "the total time found by name",
                total_time_of(ok(200, TIMER_OUT_OF_ORDER)),
                MeterValue::Published(8.25),
            ),
            (
                "a TOTAL_TIME that isn't a number",
                total_time_of(ok(200, TIMER_WITH_NAN_TOTAL)),
                MeterValue::Unavailable,
            ),
            (
                "a count from a 204 is not a meter",
                count_of(ok(204, TIMER_BOOT_2)),
                MeterValue::Unavailable,
            ),
            (
                "a total time from a 204 is not a meter",
                total_time_of(ok(204, TIMER_BOOT_2)),
                MeterValue::Unavailable,
            ),
            (
                "a sibling that isn't a number costs only itself",
                count_of(ok(200, TIMER_WITH_NAN_MAX)),
                MeterValue::Published(7.0),
            ),
            (
                "429 on the 5xx meter is not zero errors",
                count_of(ok(429, TIMER_BOOT_2)),
                MeterValue::Unavailable,
            ),
            (
                "error page 404 on the 5xx meter",
                count_of(ok(404, r#"{"status":404,"error":"Not Found"}"#)),
                MeterValue::NotPublished,
            ),
            (
                "503 on the 5xx meter is not zero errors",
                count_of(ok(503, TIMER_BOOT_2)),
                MeterValue::Unavailable,
            ),
            (
                "the 5xx meter isn't published yet",
                count_of(ok(404, "")),
                MeterValue::NotPublished,
            ),
            (
                "the 5xx query timed out",
                count_of(Err(TransportFailure::Timeout)),
                MeterValue::Unavailable,
            ),
            (
                "the 5xx query was refused",
                count_of(Err(TransportFailure::Connect)),
                MeterValue::Unavailable,
            ),
            (
                "500 with a valid body",
                count_of(ok(500, TIMER_BOOT_2)),
                MeterValue::Unavailable,
            ),
            (
                "401 with a valid body",
                count_of(ok(401, TIMER_BOOT_2)),
                MeterValue::Unavailable,
            ),
            (
                "a COUNT that isn't a number",
                count_of(ok(
                    200,
                    r#"{"measurements":[{"statistic":"COUNT","value":"NaN"}]}"#,
                )),
                MeterValue::Unavailable,
            ),
            (
                "GC pause total",
                total_time_of(ok(200, TIMER_BOOT_2)),
                MeterValue::Published(8.25),
            ),
            (
                "GC pause total beside a NaN MAX",
                total_time_of(ok(200, TIMER_WITH_NAN_MAX)),
                MeterValue::Published(0.5),
            ),
            (
                "no GC pause meter",
                total_time_of(ok(404, "")),
                MeterValue::NotPublished,
            ),
            (
                "GC pause without TOTAL_TIME",
                total_time_of(ok(200, r#"{"measurements":[]}"#)),
                MeterValue::Unavailable,
            ),
            (
                "GC pause query timed out",
                total_time_of(Err(TransportFailure::Timeout)),
                MeterValue::Unavailable,
            ),
            (
                "GC pause 500 with a valid body",
                total_time_of(ok(500, TIMER_BOOT_2)),
                MeterValue::Unavailable,
            ),
        ];
        for (name, actual, expected) in cases {
            assert_eq!(actual, expected, "case: {name}");
        }
    }

    #[test]
    fn the_version_is_build_version_from_a_200_info() {
        const WITH_VERSION: &str = r#"{"build":{"version":"2.4.1","artifact":"orders"}}"#;
        let cases = [
            ("build info present", ok(200, WITH_VERSION), Some("2.4.1")),
            (
                "kept as sent, padding included",
                ok(200, r#"{"build":{"version":" 2.4.1 "}}"#),
                Some(" 2.4.1 "),
            ),
            (
                "no build info",
                ok(200, r#"{"git":{"branch":"main"}}"#),
                None,
            ),
            ("empty info", ok(200, "{}"), None),
            (
                "build without a version",
                ok(200, r#"{"build":{"artifact":"orders"}}"#),
                None,
            ),
            (
                "a version that isn't a string",
                ok(200, r#"{"build":{"version":3}}"#),
                None,
            ),
            ("404: info not exposed", ok(404, ""), None),
            ("203 is not a 200", ok(203, WITH_VERSION), None),
            (
                "500 with a version in the body",
                ok(500, WITH_VERSION),
                None,
            ),
            (
                "401 with a version in the body",
                ok(401, WITH_VERSION),
                None,
            ),
            ("not JSON", ok(200, "nope"), None),
            ("timed out", Err(TransportFailure::Timeout), None),
        ];
        for (name, answer, expected) in cases {
            assert_eq!(version_of(answer).as_deref(), expected, "case: {name}");
        }
    }

    #[test]
    fn no_transport_failure_is_ever_read_as_not_published_or_as_health() {
        let failures = [
            TransportFailure::Connect,
            TransportFailure::Timeout,
            TransportFailure::BodyTooLarge,
        ];
        for failure in failures {
            let unavailable = [
                ("reading_of", reading_of(Err(failure))),
                ("count_of", count_of(Err(failure))),
                ("total_time_of", total_time_of(Err(failure))),
            ];
            for (reader, value) in unavailable {
                assert_eq!(value, MeterValue::Unavailable, "{reader} of {failure:?}");
            }
            assert_eq!(
                timer_of(Err(failure)),
                MeterValue::Unavailable,
                "timer_of {failure:?}"
            );
            assert_eq!(version_of(Err(failure)), None, "version_of {failure:?}");
            assert_eq!(
                health_of(Err(failure)),
                Err(ScrapeFailure::from(failure)),
                "health_of passes {failure:?} on"
            );
        }
    }
}
