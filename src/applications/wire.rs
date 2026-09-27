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

//! The wire shape of one application report, shared by the applications poll response (JSON,
//! by field name) and the application frame (MessagePack, by field position; RFC 0009 §6, §7).

use serde::Serialize;
use std::collections::BTreeMap;

use crate::applications::report::{ApplicationHealth, ApplicationReport, ReportedHealth};
use crate::applications::round::NamedReport;

/// One application report on the wire. The application frame encodes it positionally, so the
/// field *order* is that contract: `testdata/application-frame-v1.msgpack` pins it.
/// A field only the JSON response needs means splitting this DTO in two, never appending to
/// it: a fifth element would make a v1 hub drop every application frame.
#[derive(Debug, Serialize)]
pub struct ApplicationReportDto {
    name: String,
    health: &'static str,
    version: Option<String>,
    /// Keyed by gauge wire name; a gauge that couldn't be derived is left out.
    gauges: BTreeMap<&'static str, f64>,
}

impl From<&NamedReport> for ApplicationReportDto {
    fn from(named: &NamedReport) -> Self {
        Self {
            name: named.name.as_str().to_string(),
            health: health_wire_name(named.report.health()),
            version: match &named.report {
                ApplicationReport::Reached { version, .. } => {
                    version.as_ref().map(|version| version.as_str().to_string())
                }
                ApplicationReport::Unreachable(_) => None,
            },
            gauges: match &named.report {
                ApplicationReport::Reached { gauges, .. } => gauges
                    .iter()
                    .map(|(gauge, value)| (gauge.wire_name(), value))
                    .collect(),
                ApplicationReport::Unreachable(_) => BTreeMap::new(),
            },
        }
    }
}

/// `health` on the wire. The `ScrapeFailure` behind `unreachable` stays in the agent's log.
fn health_wire_name(health: ApplicationHealth) -> &'static str {
    match health {
        ApplicationHealth::Reported(ReportedHealth::Up) => "up",
        ApplicationHealth::Reported(ReportedHealth::Down) => "down",
        ApplicationHealth::Reported(ReportedHealth::OutOfService) => "out_of_service",
        ApplicationHealth::Reported(ReportedHealth::Unknown) => "unknown",
        ApplicationHealth::Unreachable(_) => "unreachable",
    }
}

#[cfg(test)]
pub use fixtures::{sample_round, sparse_round};

#[cfg(test)]
mod fixtures {
    use crate::alerts::AgentRun;
    use crate::applications::config::{Applications, ApplicationsConfig};
    use crate::applications::report::{
        MeterValue, Meters, RawScrape, ReachedScrape, ReportedHealth, ScrapeFailure, ScrapeHistory,
    };
    use crate::applications::round::{NamedReport, RoundId, ScrapeRound};

    fn applications_config() -> Applications {
        let vars = [
            (
                "SPRING_BOOT_APPS",
                "orders=http://127.0.0.1:1,billing=http://127.0.0.1:2",
            ),
            // Not the default, so the served interval can't be the default by accident.
            ("SPRING_BOOT_SCRAPE_INTERVAL", "20"),
        ];
        let lookup = move |key: &str| {
            vars.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
                .ok_or(std::env::VarError::NotPresent)
        };
        match ApplicationsConfig::parse(lookup) {
            Ok(ApplicationsConfig::On(apps)) => apps,
            _ => panic!("the test configuration parses"),
        }
    }

    /// A round of two applications: `orders` reached, with a version and two gauges; `billing` unreachable.
    /// A round unlike `sample_round` in every respect the encoding reads: one application,
    /// reached but down, no version, a single gauge, every 3600 s.
    pub fn sparse_round(run: AgentRun, seq: u64) -> ScrapeRound {
        let vars = [
            ("SPRING_BOOT_APPS", "solo=http://127.0.0.1:3"),
            ("SPRING_BOOT_SCRAPE_INTERVAL", "3600"),
        ];
        let lookup = move |key: &str| {
            vars.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
                .ok_or(std::env::VarError::NotPresent)
        };
        let Ok(ApplicationsConfig::On(apps)) = ApplicationsConfig::parse(lookup) else {
            panic!("the sparse configuration parses");
        };
        let meters = Meters {
            heap_used: MeterValue::NotPublished,
            heap_max: MeterValue::NotPublished,
            cpu_usage: MeterValue::NotPublished,
            live_threads: MeterValue::Published(12.0),
            gc_pause_seconds: MeterValue::NotPublished,
            http_requests: MeterValue::NotPublished,
            http_server_errors: MeterValue::NotPublished,
            db_connections_active: MeterValue::NotPublished,
            uptime_seconds: MeterValue::NotPublished,
        };
        let raw = RawScrape::Reached(ReachedScrape {
            health: ReportedHealth::Down,
            version: None,
            meters,
            own_requests: 0,
        });
        let target = &apps.targets()[0];
        ScrapeRound {
            id: RoundId { run, seq },
            interval: apps.interval(),
            scraped_at: 1_790_000_000,
            applications: vec![NamedReport {
                name: target.name().clone(),
                report: ScrapeHistory::default()
                    .advance(raw, std::time::Instant::now())
                    .1,
            }],
        }
    }

    pub fn sample_round(run: AgentRun, seq: u64) -> ScrapeRound {
        let apps = applications_config();
        let meters = Meters {
            heap_used: MeterValue::Published(300.0),
            heap_max: MeterValue::NotPublished,
            cpu_usage: MeterValue::NotPublished,
            live_threads: MeterValue::NotPublished,
            gc_pause_seconds: MeterValue::NotPublished,
            http_requests: MeterValue::NotPublished,
            http_server_errors: MeterValue::NotPublished,
            db_connections_active: MeterValue::NotPublished,
            uptime_seconds: MeterValue::Published(600.5),
        };
        let raws = [
            RawScrape::Reached(ReachedScrape {
                health: ReportedHealth::Up,
                version: Some("2.4.1".into()),
                meters,
                own_requests: 11,
            }),
            RawScrape::Unreachable(ScrapeFailure::Timeout),
        ];
        let now = std::time::Instant::now();
        let applications = apps
            .targets()
            .iter()
            .zip(raws)
            .map(|(target, raw)| NamedReport {
                name: target.name().clone(),
                report: ScrapeHistory::default().advance(raw, now).1,
            })
            .collect();
        ScrapeRound {
            id: RoundId { run, seq },
            interval: apps.interval(),
            scraped_at: 1_790_000_000,
            applications,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::applications::report::ScrapeFailure;

    #[test]
    fn every_application_health_has_its_wire_name() {
        use ApplicationHealth::{Reported, Unreachable};
        let cases = [
            (Reported(ReportedHealth::Up), "up"),
            (Reported(ReportedHealth::Down), "down"),
            (Reported(ReportedHealth::OutOfService), "out_of_service"),
            (Reported(ReportedHealth::Unknown), "unknown"),
            (Unreachable(ScrapeFailure::Connect), "unreachable"),
            (Unreachable(ScrapeFailure::Unauthorized), "unreachable"),
        ];
        for (health, wire) in cases {
            assert_eq!(health_wire_name(health), wire, "case: {health:?}");
        }
    }
}
