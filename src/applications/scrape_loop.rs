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

//! The scrape loop (RFC 0009 §5): scrape every application, fold each scrape into its
//! history, publish the round, then sleep a full interval.

use std::future::Future;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::watch;

use super::config::{ApplicationName, ApplicationTarget, Applications};
use super::report::RawScrape;
use super::round::{HealthChange, NamedReport, RoundSequence, ScrapeRound, WatchedApplication};
use super::scraper::Scraper;
use crate::alerts::AgentRun;

/// The latest scrape round; `None` until the first one is published.
pub type RoundSender = watch::Sender<Option<Arc<ScrapeRound>>>;
pub type RoundReceiver = watch::Receiver<Option<Arc<ScrapeRound>>>;

/// Something that scrapes one application: the real `Scraper`, or a test's stub.
pub trait ScrapeApplication: Send + Sync + 'static {
    fn scrape(&self, target: &ApplicationTarget) -> impl Future<Output = RawScrape> + Send;
}

impl ScrapeApplication for Scraper {
    fn scrape(&self, target: &ApplicationTarget) -> impl Future<Output = RawScrape> + Send {
        Scraper::scrape(self, target)
    }
}

/// Runs for the agent's lifetime: a round, its publication, then a full interval's sleep, so
/// two publications are always at least one interval apart however long a round took.
pub async fn scrape_loop(
    scraper: impl ScrapeApplication,
    applications: Applications,
    run: AgentRun,
    rounds: RoundSender,
) {
    let interval = applications.interval();
    let mut watched: Vec<WatchedApplication> = applications
        .into_targets()
        .into_iter()
        .map(WatchedApplication::new)
        .collect();
    let mut sequence = RoundSequence::new(run);
    loop {
        let reports = scrape_round(&scraper, &mut watched).await;
        let round = ScrapeRound {
            id: sequence.mint(),
            interval,
            scraped_at: unix_now(),
            applications: reports,
        };
        rounds.send_replace(Some(Arc::new(round)));
        tokio::time::sleep(interval.as_duration()).await;
    }
}

/// Every application scraped at once, each folded into its own history at the time its own
/// scrape finished, so a slow application doesn't stretch its siblings' rates.
async fn scrape_round(
    scraper: &impl ScrapeApplication,
    watched: &mut [WatchedApplication],
) -> Vec<NamedReport> {
    let scrapes = watched.iter().map(|app| async {
        let raw = scraper.scrape(app.target()).await;
        // Tokio's clock, so paused-time tests see the time a scrape took.
        (raw, tokio::time::Instant::now().into_std())
    });
    // `join_all` keeps the order of `watched`, whatever order the scrapes finish in.
    let scraped = futures_util::future::join_all(scrapes).await;
    watched
        .iter_mut()
        .zip(scraped)
        .map(|(app, (raw, now))| {
            let (report, change) = app.fold(raw, now);
            log_health_change(&report.name, change);
            report
        })
        .collect()
}

/// Seconds since the Unix epoch on the agent's clock; a clock set before 1970 reads as the
/// epoch, which only a local reader of `scraped_at` ever sees.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Names the application and the failure, never its URL or credentials.
fn log_health_change(name: &ApplicationName, change: Option<HealthChange>) {
    match change {
        Some(HealthChange::BecameUnreachable(failure)) => tracing::warn!(
            "Application {:?} is unreachable: {failure:?}",
            name.as_str()
        ),
        Some(HealthChange::Recovered) => {
            tracing::info!("Application {:?} is reachable again", name.as_str())
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::applications::config::ApplicationsConfig;
    use crate::applications::report::{
        ApplicationGauge, ApplicationReport, MeterValue, Meters, ReachedScrape, ReportedHealth,
        ScrapeFailure, TimerTotals,
    };
    use std::collections::HashMap;
    use std::time::Duration;
    use tokio::time::Instant;

    /// Answers each application by name; `answer` gets the name and how many times that
    /// application was scraped before, and says how long the scrape takes and what it gives.
    struct Stub {
        calls: parking_lot::Mutex<HashMap<String, u32>>,
        answer: fn(&str, u32) -> (Duration, RawScrape),
    }

    impl Stub {
        fn new(answer: fn(&str, u32) -> (Duration, RawScrape)) -> Self {
            Self {
                calls: parking_lot::Mutex::default(),
                answer,
            }
        }
    }

    impl ScrapeApplication for Stub {
        fn scrape(&self, target: &ApplicationTarget) -> impl Future<Output = RawScrape> + Send {
            let name = target.name().as_str().to_string();
            let call = {
                let mut calls = self.calls.lock();
                let call = calls.entry(name.clone()).or_default();
                *call += 1;
                *call - 1
            };
            let (delay, raw) = (self.answer)(&name, call);
            async move {
                tokio::time::sleep(delay).await;
                raw
            }
        }
    }

    fn applications(apps: &str) -> Applications {
        let vars = [
            ("SPRING_BOOT_APPS", apps.to_string()),
            ("SPRING_BOOT_SCRAPE_INTERVAL", "10".to_string()),
        ];
        let lookup = move |key: &str| {
            vars.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
                .ok_or(std::env::VarError::NotPresent)
        };
        match ApplicationsConfig::parse(lookup) {
            Ok(ApplicationsConfig::On(apps)) => apps,
            _ => panic!("the test configuration parses"),
        }
    }

    /// A reached scrape whose only meter is `http.server.requests` at `count`.
    fn reached(count: f64) -> RawScrape {
        RawScrape::Reached(ReachedScrape {
            health: ReportedHealth::Up,
            version: None,
            meters: Meters {
                heap_used: MeterValue::NotPublished,
                heap_max: MeterValue::NotPublished,
                cpu_usage: MeterValue::NotPublished,
                live_threads: MeterValue::NotPublished,
                gc_pause_seconds: MeterValue::NotPublished,
                http_requests: MeterValue::Published(TimerTotals {
                    count,
                    total_seconds: 0.0,
                }),
                http_server_errors: MeterValue::NotPublished,
                db_connections_active: MeterValue::NotPublished,
                uptime_seconds: MeterValue::NotPublished,
            },
            own_requests: 0,
        })
    }

    fn run() -> AgentRun {
        AgentRun::new(uuid::Uuid::from_u128(42))
    }

    /// Starts the loop and returns the receiver it publishes to.
    fn start(stub: Stub, apps: &str) -> RoundReceiver {
        let (sender, receiver) = watch::channel(None);
        tokio::spawn(scrape_loop(stub, applications(apps), run(), sender));
        receiver
    }

    /// The next published round, and when it was published; fails after a minute of
    /// (paused) time.
    async fn next_round(receiver: &mut RoundReceiver) -> (Arc<ScrapeRound>, Instant) {
        let changed = tokio::time::timeout(Duration::from_secs(60), receiver.changed()).await;
        assert!(
            matches!(changed, Ok(Ok(()))),
            "a round within a minute, from a running loop: {changed:?}"
        );
        let round = receiver.borrow_and_update().clone().expect("a round");
        (round, Instant::now())
    }

    #[tokio::test(start_paused = true)]
    async fn each_round_holds_every_applications_report_in_order_under_the_next_round_id() {
        let stub = Stub::new(|name, _| match name {
            "orders" => (Duration::ZERO, reached(1.0)),
            _ => (
                Duration::ZERO,
                RawScrape::Unreachable(ScrapeFailure::Connect),
            ),
        });
        let mut receiver = start(stub, "orders=http://127.0.0.1:1,billing=http://127.0.0.1:2");
        for seq in [1, 2] {
            let (round, _) = next_round(&mut receiver).await;
            assert_eq!(round.id.run, run(), "round {seq}");
            assert_eq!(round.id.seq, seq);
            assert_eq!(round.interval.as_duration(), Duration::from_secs(10));
            let names: Vec<&str> = round.applications.iter().map(|a| a.name.as_str()).collect();
            assert_eq!(names, ["orders", "billing"], "round {seq}");
            assert!(
                matches!(
                    round.applications[0].report,
                    ApplicationReport::Reached {
                        health: ReportedHealth::Up,
                        ..
                    }
                ),
                "round {seq}"
            );
            assert_eq!(
                round.applications[1].report,
                ApplicationReport::Unreachable(ScrapeFailure::Connect),
                "round {seq}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_round_then_a_fast_one_are_still_published_a_full_interval_apart() {
        // The first round takes 9 s of the 10 s interval, the second none.
        let stub = Stub::new(|_, call| {
            let delay = if call == 0 { 9 } else { 0 };
            (Duration::from_secs(delay), reached(1.0))
        });
        let started = Instant::now();
        let mut receiver = start(stub, "orders=http://127.0.0.1:1");
        let (_, first) = next_round(&mut receiver).await;
        let (_, second) = next_round(&mut receiver).await;
        assert_eq!(
            first - started,
            Duration::from_secs(9),
            "the first round starts at once"
        );
        assert!(
            second - first >= Duration::from_secs(10),
            "published {:?} apart",
            second - first
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_rounds_applications_are_scraped_concurrently() {
        // Two applications taking 6 s each: together 6 s, one after the other 12 s.
        let stub = Stub::new(|_, _| (Duration::from_secs(6), reached(1.0)));
        let started = Instant::now();
        let mut receiver = start(stub, "orders=http://127.0.0.1:1,billing=http://127.0.0.1:2");
        let (_, published) = next_round(&mut receiver).await;
        assert_eq!(published - started, Duration::from_secs(6));
    }

    #[tokio::test]
    async fn a_round_is_stamped_with_the_time_it_was_scraped() {
        let unix_now = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
        };
        let before = unix_now().expect("the clock is past the epoch");
        let stub = Stub::new(|_, _| (Duration::ZERO, reached(1.0)));
        let mut receiver = start(stub, "orders=http://127.0.0.1:1");
        let (round, _) = next_round(&mut receiver).await;
        let after = unix_now().expect("the clock is past the epoch");
        assert!(
            (before..=after).contains(&round.scraped_at),
            "{} not within {before}..={after}",
            round.scraped_at
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_applications_rates_come_from_its_own_previous_scrape() {
        // `orders` serves 100 requests per round, `billing` 30; rounds are 10 s apart.
        let stub = Stub::new(|name, call| {
            let per_round = if name == "orders" { 100.0 } else { 30.0 };
            (Duration::ZERO, reached(per_round * f64::from(call + 1)))
        });
        let mut receiver = start(stub, "orders=http://127.0.0.1:1,billing=http://127.0.0.1:2");
        next_round(&mut receiver).await;
        let (round, _) = next_round(&mut receiver).await;
        let rates: Vec<Option<f64>> = round
            .applications
            .iter()
            .map(|app| match &app.report {
                ApplicationReport::Reached { gauges, .. } => gauges
                    .iter()
                    .find(|(gauge, _)| *gauge == ApplicationGauge::HttpRequestsPerSecond)
                    .map(|(_, rate)| rate),
                ApplicationReport::Unreachable(_) => None,
            })
            .collect();
        assert_eq!(rates, [Some(10.0), Some(3.0)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_sibling_does_not_move_an_applications_rate() {
        // `orders` answers at once with 100 requests per round, so its scrapes are 10 s apart
        // whatever `billing` does. `billing` serves 30 per round and takes 5 s in the second
        // round only, so its scrapes are 15 s apart. Listed either way round, each rate comes
        // from that application's own scrapes.
        let cases = [
            (
                "fast one first",
                "orders=http://127.0.0.1:1,billing=http://127.0.0.1:2",
            ),
            (
                "slow one first",
                "billing=http://127.0.0.1:2,orders=http://127.0.0.1:1",
            ),
        ];
        for (name, apps) in cases {
            let stub = Stub::new(|name, call| match name {
                "orders" => (Duration::ZERO, reached(100.0 * f64::from(call + 1))),
                _ => (
                    Duration::from_secs(5 * u64::from(call)),
                    reached(30.0 * f64::from(call + 1)),
                ),
            });
            let mut receiver = start(stub, apps);
            next_round(&mut receiver).await;
            let (round, _) = next_round(&mut receiver).await;
            let rates: HashMap<&str, Option<f64>> = round
                .applications
                .iter()
                .map(|app| {
                    let rate = match &app.report {
                        ApplicationReport::Reached { gauges, .. } => gauges
                            .iter()
                            .find(|(gauge, _)| *gauge == ApplicationGauge::HttpRequestsPerSecond)
                            .map(|(_, rate)| rate),
                        ApplicationReport::Unreachable(_) => None,
                    };
                    (app.name.as_str(), rate)
                })
                .collect();
            assert_eq!(rates["orders"], Some(10.0), "case {name}: 100 over 10 s");
            assert_eq!(rates["billing"], Some(2.0), "case {name}: 30 over 15 s");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_rate_is_over_the_measured_time_between_scrapes_not_the_interval() {
        // The second scrape takes 5 s, so the two scrapes are 15 s apart, not 10.
        let stub = Stub::new(|_, call| {
            let delay = if call == 1 { 5 } else { 0 };
            (
                Duration::from_secs(delay),
                reached(150.0 * f64::from(call + 1)),
            )
        });
        let mut receiver = start(stub, "orders=http://127.0.0.1:1");
        next_round(&mut receiver).await;
        let (round, _) = next_round(&mut receiver).await;
        let rate = match &round.applications[0].report {
            ApplicationReport::Reached { gauges, .. } => gauges
                .iter()
                .find(|(gauge, _)| *gauge == ApplicationGauge::HttpRequestsPerSecond)
                .map(|(_, rate)| rate),
            ApplicationReport::Unreachable(_) => None,
        };
        assert_eq!(rate, Some(10.0), "150 requests over 15 s");
    }
}
