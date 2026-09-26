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

//! The HTTP half of the actuator adapter (RFC 0009 §4): one scrape of one application over
//! HTTP. Requests reach only the configured host: no redirects, no proxies, bodies capped.

use std::time::Duration;

use super::config::ApplicationTarget;
use super::report::RawScrape;

/// How long a scrape waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    pub connect: Duration,
    pub request: Duration,
}

impl Timeouts {
    /// RFC 0009 §4: connect 2 s, whole request 5 s.
    pub const PRODUCTION: Self = Self {
        connect: Duration::from_secs(2),
        request: Duration::from_secs(5),
    };
}

/// The longest Actuator body read; a longer one is never parsed.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// Scrapes applications over one shared HTTP client.
pub struct Scraper {}

/// Why the scraper's HTTP client couldn't be built.
#[derive(Debug)]
pub struct ScraperError(reqwest::Error);

impl Scraper {
    pub fn new(_timeouts: Timeouts) -> Result<Self, ScraperError> {
        Ok(Self {})
    }

    /// Reads one application: its health first; if it answered, its info and the curated
    /// meters, concurrently.
    pub async fn scrape(&self, _target: &ApplicationTarget) -> RawScrape {
        RawScrape::Unreachable(super::report::ScrapeFailure::BadBody)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::applications::config::ApplicationsConfig;
    use crate::applications::report::{
        MeterValue, Meters, ReachedScrape, ReportedHealth, ScrapeFailure, TimerTotals,
    };
    use axum::body::Body;
    use axum::extract::{Request, State};
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Redirect, Response};
    use axum::{Router, routing::get};
    use futures_util::StreamExt;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// How the mock answers one endpoint.
    #[derive(Clone)]
    enum Reply {
        /// A status and a body with a `Content-Length`.
        Fixed(StatusCode, String),
        /// A 200 whose body is streamed without a `Content-Length`: `prefix`, then `padding`
        /// bytes of spaces.
        Chunked { prefix: String, padding: usize },
        /// A 200 whose body never ends.
        Endless,
        /// A fixed answer after a delay.
        Slow(Duration, String),
        /// A 200 whose body arrives one byte at a time, `Duration` apart.
        Drip(Duration, String),
        /// No answer at all: the connection is dropped.
        HangUp,
    }

    /// Everything the mock saw: each request's URI and headers, and how many were in flight
    /// at once.
    #[derive(Default)]
    struct Seen {
        requests: parking_lot::Mutex<Vec<(String, Vec<(String, String)>)>>,
        in_flight: AtomicUsize,
        peak_in_flight: AtomicUsize,
    }

    impl Seen {
        fn count(&self) -> usize {
            self.requests.lock().len()
        }

        fn header(&self, name: &str) -> Vec<Option<String>> {
            self.requests
                .lock()
                .iter()
                .map(|(_, headers)| {
                    headers
                        .iter()
                        .find(|(k, _)| k == name)
                        .map(|(_, v)| v.clone())
                })
                .collect()
        }
    }

    type Answers = HashMap<String, Reply>;

    /// A mock Actuator. Answers are keyed by "<path>" or "<path>?<tag>"; anything else is 404.
    async fn serve(answers: Answers) -> (std::net::SocketAddr, Arc<Seen>) {
        let seen = Arc::new(Seen::default());
        async fn handle(
            State((answers, seen)): State<(Arc<Answers>, Arc<Seen>)>,
            request: Request,
        ) -> Response {
            let uri = request.uri().clone();
            let headers = request
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();
            seen.requests.lock().push((uri.to_string(), headers));
            let now = seen.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            seen.peak_in_flight.fetch_max(now, Ordering::SeqCst);
            let tag = uri
                .query()
                .and_then(|q| q.strip_prefix("tag="))
                .map(|t| t.replace("%3A", ":"));
            let key = match tag {
                Some(tag) => format!("{}?{tag}", uri.path()),
                None => uri.path().to_string(),
            };
            let reply = answers.get(&key).cloned();
            let response = match reply {
                None => StatusCode::NOT_FOUND.into_response(),
                Some(Reply::Fixed(status, body)) => (status, body).into_response(),
                Some(Reply::Slow(delay, body)) => {
                    tokio::time::sleep(delay).await;
                    body.into_response()
                }
                Some(Reply::Chunked { prefix, padding }) => {
                    let chunks = std::iter::once(prefix.into_bytes())
                        .chain(std::iter::repeat_n(vec![b' '; 1024], padding / 1024))
                        .map(Ok::<_, std::io::Error>);
                    Body::from_stream(futures_util::stream::iter(chunks)).into_response()
                }
                Some(Reply::Drip(gap, body)) => {
                    let bytes =
                        futures_util::stream::iter(body.into_bytes()).then(move |b| async move {
                            tokio::time::sleep(gap).await;
                            Ok::<_, std::io::Error>(vec![b])
                        });
                    Body::from_stream(bytes).into_response()
                }
                // Panicking in the connection's task drops it before any response is written.
                Some(Reply::HangUp) => panic!("the mock hangs up, as asked"),
                Some(Reply::Endless) => {
                    let chunks = futures_util::stream::repeat_with(|| {
                        Ok::<_, std::io::Error>(vec![b' '; 8 * 1024])
                    });
                    Body::from_stream(chunks).into_response()
                }
            };
            seen.in_flight.fetch_sub(1, Ordering::SeqCst);
            response
        }
        let app = Router::new()
            .fallback(get(handle))
            .with_state((Arc::new(answers), seen.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (addr, seen)
    }

    fn metric(statistic: &str, value: f64) -> String {
        format!(r#"{{"measurements":[{{"statistic":"{statistic}","value":{value}}}]}}"#)
    }

    fn timer(count: f64, total: f64) -> String {
        format!(
            r#"{{"measurements":[{{"statistic":"COUNT","value":{count}}},{{"statistic":"TOTAL_TIME","value":{total}}},{{"statistic":"MAX","value":0.2}}]}}"#
        )
    }

    /// Every endpoint a healthy application answers, under `/manage` (a custom base path).
    fn healthy() -> Answers {
        let ok = |body: String| Reply::Fixed(StatusCode::OK, body);
        HashMap::from([
            ("/manage/health".into(), ok(r#"{"status":"UP"}"#.into())),
            (
                "/manage/info".into(),
                ok(r#"{"build":{"version":"2.4.1"}}"#.into()),
            ),
            (
                "/manage/metrics/jvm.memory.used?area:heap".into(),
                ok(metric("VALUE", 300.0)),
            ),
            (
                "/manage/metrics/jvm.memory.max?area:heap".into(),
                ok(metric("VALUE", 1024.0)),
            ),
            (
                "/manage/metrics/process.cpu.usage".into(),
                ok(metric("VALUE", 0.25)),
            ),
            (
                "/manage/metrics/jvm.threads.live".into(),
                ok(metric("VALUE", 42.0)),
            ),
            ("/manage/metrics/jvm.gc.pause".into(), ok(timer(9.0, 0.75))),
            (
                "/manage/metrics/http.server.requests".into(),
                ok(timer(412.0, 8.25)),
            ),
            (
                "/manage/metrics/http.server.requests?outcome:SERVER_ERROR".into(),
                ok(timer(6.0, 0.5)),
            ),
            (
                "/manage/metrics/hikaricp.connections.active".into(),
                ok(metric("VALUE", 3.0)),
            ),
            (
                "/manage/metrics/process.uptime".into(),
                ok(metric("VALUE", 600.0)),
            ),
        ])
    }

    fn healthy_with(key: &str, reply: Reply) -> Answers {
        let mut answers = healthy();
        answers.insert(key.to_string(), reply);
        answers
    }

    /// The single application of `SPRING_BOOT_APPS=orders=<url>`, with credentials if given.
    fn target(url: &str, credentials: Option<(&str, &str)>) -> ApplicationTarget {
        let mut vars = vec![("SPRING_BOOT_APPS".to_string(), format!("orders={url}"))];
        if let Some((user, pass)) = credentials {
            vars.push(("SPRING_BOOT_APP_ORDERS_USERNAME".into(), user.into()));
            vars.push(("SPRING_BOOT_APP_ORDERS_PASSWORD".into(), pass.into()));
        }
        let lookup = move |key: &str| {
            vars.iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .ok_or(std::env::VarError::NotPresent)
        };
        match ApplicationsConfig::parse(lookup) {
            Ok(ApplicationsConfig::On(apps)) => apps.into_targets().remove(0),
            _ => panic!("the test configuration parses"),
        }
    }

    const FAST: Timeouts = Timeouts {
        connect: Duration::from_millis(300),
        request: Duration::from_millis(500),
    };

    async fn scrape(addr: std::net::SocketAddr, credentials: Option<(&str, &str)>) -> RawScrape {
        scrape_with(FAST, addr, credentials).await
    }

    async fn scrape_with(
        timeouts: Timeouts,
        addr: std::net::SocketAddr,
        credentials: Option<(&str, &str)>,
    ) -> RawScrape {
        Scraper::new(timeouts)
            .expect("the client builds")
            .scrape(&target(&format!("http://{addr}/manage"), credentials))
            .await
    }

    fn reached(raw: RawScrape) -> ReachedScrape {
        match raw {
            RawScrape::Reached(scrape) => scrape,
            RawScrape::Unreachable(failure) => panic!("reached, not {failure:?}"),
        }
    }

    #[tokio::test]
    async fn a_healthy_application_is_read_in_full_under_its_base_path() {
        let (addr, seen) = serve(healthy()).await;
        let expected = RawScrape::Reached(ReachedScrape {
            health: ReportedHealth::Up,
            version: Some("2.4.1".into()),
            meters: Meters {
                heap_used: MeterValue::Published(300.0),
                heap_max: MeterValue::Published(1024.0),
                cpu_usage: MeterValue::Published(0.25),
                live_threads: MeterValue::Published(42.0),
                gc_pause_seconds: MeterValue::Published(0.75),
                http_requests: MeterValue::Published(TimerTotals {
                    count: 412.0,
                    total_seconds: 8.25,
                }),
                http_server_errors: MeterValue::Published(6.0),
                db_connections_active: MeterValue::Published(3.0),
                uptime_seconds: MeterValue::Published(600.0),
            },
            // Health, info and nine meters, all answered.
            own_requests: 11,
        });
        assert_eq!(scrape(addr, None).await, expected);
        assert_eq!(seen.count(), 11);
        assert!(
            seen.header("accept")
                .iter()
                .all(|a| a.as_deref() == Some("application/json")),
            "every request asks for plain JSON"
        );
        assert!(
            seen.header("authorization").iter().all(Option::is_none),
            "no credentials were configured"
        );
    }

    #[tokio::test]
    async fn basic_credentials_travel_only_as_the_authorization_header() {
        // (username, password, the header they make: base64("<username>:<password>"))
        let cases = [
            ("monitor", "p: w", "Basic bW9uaXRvcjpwOiB3"),
            ("u", "pw-orders", "Basic dTpwdy1vcmRlcnM="),
        ];
        for (user, pass, header) in cases {
            let (addr, seen) = serve(healthy()).await;
            let raw = scrape(addr, Some((user, pass))).await;
            assert!(matches!(raw, RawScrape::Reached(_)), "{user}: {raw:?}");
            let headers = seen.header("authorization");
            assert_eq!(headers.len(), 11, "{user}");
            assert!(
                headers.iter().all(|h| h.as_deref() == Some(header)),
                "{user}: {headers:?}"
            );
            // The credentials are in no URI and no other header, in any encoding: the only
            // query is the `tag` filter, and no other header carries the password or the
            // Basic value.
            for (uri, headers) in seen.requests.lock().iter() {
                let query = uri.split_once('?').map(|(_, q)| q);
                assert!(
                    query.is_none_or(|q| q.starts_with("tag=") && !q.contains('&')),
                    "{user}: an unexpected query in {uri}"
                );
                for (name, value) in headers {
                    if name != "authorization" {
                        assert!(
                            !value.contains(pass) && !value.contains(header),
                            "{user}: credentials in the {name} header"
                        );
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn every_answered_request_counts_as_the_agents_own_whatever_its_status() {
        // Actuator records every request it answered, a 404, a 500 or an over-cap body
        // included; only a request that got no answer at all isn't counted.
        let cases = [
            ("all answered", healthy(), 11),
            (
                "unpublished meters are answered",
                {
                    let mut answers = healthy();
                    answers.remove("/manage/metrics/hikaricp.connections.active");
                    answers
                },
                11,
            ),
            (
                "a 500 is answered",
                healthy_with(
                    "/manage/metrics/process.uptime",
                    Reply::Fixed(StatusCode::INTERNAL_SERVER_ERROR, String::new()),
                ),
                11,
            ),
            (
                "an over-cap body was served",
                healthy_with(
                    "/manage/metrics/process.uptime",
                    Reply::Fixed(StatusCode::OK, " ".repeat(MAX_BODY_BYTES + 1)),
                ),
                11,
            ),
            (
                "a meter whose connection dropped got no answer",
                healthy_with("/manage/metrics/process.uptime", Reply::HangUp),
                10,
            ),
            (
                "a meter that timed out got no answer",
                healthy_with(
                    "/manage/metrics/process.uptime",
                    Reply::Slow(Duration::from_secs(3), metric("VALUE", 1.0)),
                ),
                10,
            ),
        ];
        for (name, answers, own) in cases {
            let (addr, _) = serve(answers).await;
            assert_eq!(
                reached(scrape(addr, None).await).own_requests,
                own,
                "case: {name}"
            );
        }
    }

    #[tokio::test]
    async fn unpublished_and_failed_meters_are_told_apart() {
        let mut answers = healthy_with(
            "/manage/metrics/process.uptime",
            Reply::Fixed(StatusCode::INTERNAL_SERVER_ERROR, metric("VALUE", 1.0)),
        );
        answers.remove("/manage/metrics/hikaricp.connections.active");
        answers.remove("/manage/metrics/http.server.requests?outcome:SERVER_ERROR");
        let (addr, _) = serve(answers).await;
        let scrape = reached(scrape(addr, None).await);
        assert_eq!(
            scrape.meters.db_connections_active,
            MeterValue::NotPublished
        );
        assert_eq!(scrape.meters.http_server_errors, MeterValue::NotPublished);
        assert_eq!(scrape.meters.uptime_seconds, MeterValue::Unavailable);
    }

    #[tokio::test]
    async fn health_failures_make_the_application_unreachable_and_stop_the_scrape() {
        let health = |reply| healthy_with("/manage/health", reply);
        let fixed = |status, body: &str| Reply::Fixed(status, body.to_string());
        let cases = [
            (
                "401",
                health(fixed(StatusCode::UNAUTHORIZED, "")),
                ScrapeFailure::Unauthorized,
            ),
            (
                "403",
                health(fixed(StatusCode::FORBIDDEN, "")),
                ScrapeFailure::Unauthorized,
            ),
            (
                "500",
                health(fixed(StatusCode::INTERNAL_SERVER_ERROR, "")),
                ScrapeFailure::HttpStatus(500),
            ),
            (
                "a 200 that isn't JSON",
                health(fixed(StatusCode::OK, "<html>")),
                ScrapeFailure::BadBody,
            ),
            (
                "a body that never ends",
                health(Reply::Endless),
                ScrapeFailure::BadBody,
            ),
            (
                "a chunked body past the cap",
                health(Reply::Chunked {
                    prefix: r#"{"status":"UP"}"#.into(),
                    padding: MAX_BODY_BYTES,
                }),
                ScrapeFailure::BadBody,
            ),
        ];
        for (name, answers, failure) in cases {
            let (addr, seen) = serve(answers).await;
            let raw = scrape(addr, None).await;
            assert_eq!(raw, RawScrape::Unreachable(failure), "case: {name}");
            assert_eq!(seen.count(), 1, "case {name}: only health was asked");
        }
    }

    #[tokio::test]
    async fn a_down_application_answering_503_is_still_read_in_full() {
        let answers = healthy_with(
            "/manage/health",
            Reply::Fixed(
                StatusCode::SERVICE_UNAVAILABLE,
                r#"{"status":"DOWN"}"#.into(),
            ),
        );
        let (addr, _) = serve(answers).await;
        let scrape = reached(scrape(addr, None).await);
        assert_eq!(scrape.health, ReportedHealth::Down);
        assert_eq!(scrape.meters.uptime_seconds, MeterValue::Published(600.0));
    }

    #[tokio::test]
    async fn a_meter_body_is_read_up_to_the_cap_and_never_past_it() {
        let valid = metric("VALUE", 7.0);
        let padded = |len: usize| format!("{valid}{}", " ".repeat(len - valid.len()));
        let cases = [
            (
                "exactly at the cap",
                Reply::Fixed(StatusCode::OK, padded(MAX_BODY_BYTES)),
                MeterValue::Published(7.0),
            ),
            (
                "one byte over",
                Reply::Fixed(StatusCode::OK, padded(MAX_BODY_BYTES + 1)),
                MeterValue::Unavailable,
            ),
            (
                "chunked, with no length, past the cap",
                Reply::Chunked {
                    prefix: valid.clone(),
                    padding: MAX_BODY_BYTES,
                },
                MeterValue::Unavailable,
            ),
            ("never ending", Reply::Endless, MeterValue::Unavailable),
        ];
        for (name, reply, expected) in cases {
            let (addr, _) = serve(healthy_with("/manage/metrics/jvm.threads.live", reply)).await;
            let scrape = reached(scrape(addr, None).await);
            assert_eq!(scrape.meters.live_threads, expected, "case: {name}");
            assert_eq!(
                scrape.meters.heap_used,
                MeterValue::Published(300.0),
                "case {name}: the other meters are read"
            );
        }
    }

    #[tokio::test]
    async fn an_applications_meters_are_requested_concurrently() {
        let mut answers = healthy();
        for (key, reply) in answers.iter_mut() {
            if key.contains("/metrics/") || key.ends_with("/info") {
                if let Reply::Fixed(_, body) = reply {
                    *reply = Reply::Slow(Duration::from_millis(150), body.clone());
                }
            }
        }
        let (addr, seen) = serve(answers).await;
        let scrape = reached(scrape(addr, None).await);
        assert_eq!(scrape.own_requests, 11);
        assert_eq!(
            seen.peak_in_flight.load(Ordering::SeqCst),
            10,
            "info and all nine meters were in flight at once"
        );
    }

    #[tokio::test]
    async fn a_redirect_is_never_followed() {
        // The redirect target counts every request it gets; it must get none.
        let (elsewhere, elsewhere_seen) = serve(healthy()).await;
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let location = format!("http://{elsewhere}/manage/health");
        let app = Router::new().fallback(get(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            let location = location.clone();
            async move { Redirect::temporary(&location) }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let raw = scrape(addr, Some(("u", "p"))).await;
        assert_eq!(raw, RawScrape::Unreachable(ScrapeFailure::HttpStatus(307)));
        assert_eq!(elsewhere_seen.count(), 0);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_refused_connection_is_unreachable() {
        // Bind, then drop, so the port is closed.
        let addr = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        assert_eq!(
            scrape(addr, None).await,
            RawScrape::Unreachable(ScrapeFailure::Connect)
        );
    }

    #[tokio::test]
    async fn a_health_answer_slower_than_the_request_timeout_is_a_timeout() {
        let answers = healthy_with(
            "/manage/health",
            Reply::Slow(Duration::from_secs(5), r#"{"status":"UP"}"#.into()),
        );
        let (addr, _) = serve(answers).await;
        let started = std::time::Instant::now();
        assert_eq!(
            scrape(addr, None).await,
            RawScrape::Unreachable(ScrapeFailure::Timeout)
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the request timeout bounds the wait: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn the_request_timeout_bounds_the_whole_request_not_each_read() {
        // One byte every 100 ms: no single read is slow, but the whole body takes 1.5 s.
        let answers = healthy_with(
            "/manage/health",
            Reply::Drip(Duration::from_millis(100), r#"{"status":"UP"}"#.into()),
        );
        let (addr, _) = serve(answers).await;
        assert_eq!(
            scrape(addr, None).await,
            RawScrape::Unreachable(ScrapeFailure::Timeout)
        );
    }

    #[tokio::test]
    async fn the_request_timeout_is_the_longer_one_and_the_connect_timeout_the_shorter() {
        // A 400 ms answer fits a 1 s request timeout, and would not fit a swapped 100 ms one.
        let answers = healthy_with(
            "/manage/health",
            Reply::Slow(Duration::from_millis(400), r#"{"status":"UP"}"#.into()),
        );
        let (addr, _) = serve(answers).await;
        let timeouts = Timeouts {
            connect: Duration::from_millis(100),
            request: Duration::from_secs(1),
        };
        let raw = scrape_with(timeouts, addr, None).await;
        assert!(matches!(raw, RawScrape::Reached(_)), "{raw:?}");
    }

    /// Set only in the child process `environment_proxies_are_never_used` starts.
    const CHILD_VARIABLE: &str = "SCRAPER_TEST_EXPECT_NO_PROXY";
    const CHILD_TEST: &str =
        "applications::scraper::tests::child_scrapes_directly_despite_proxy_variables";

    #[tokio::test]
    #[ignore = "run by environment_proxies_are_never_used, in a child process"]
    async fn child_scrapes_directly_despite_proxy_variables() {
        assert!(
            std::env::var_os(CHILD_VARIABLE).is_some(),
            "run only as the child of environment_proxies_are_never_used"
        );
        let (addr, seen) = serve(healthy()).await;
        // A generous timeout: the child competes with the parent's other tests for CPU.
        let generous = Timeouts {
            connect: Duration::from_secs(2),
            request: Duration::from_secs(5),
        };
        let raw = scrape_with(generous, addr, Some(("u", "p"))).await;
        assert!(matches!(raw, RawScrape::Reached(_)), "{raw:?}");
        assert_eq!(seen.count(), 11);
    }

    #[test]
    fn environment_proxies_are_never_used() {
        // Setting HTTP_PROXY in this process would need `unsafe`; a child process gets it
        // safely. The proxy counts every connection it gets; it must get none.
        let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        proxy.set_nonblocking(true).unwrap();
        let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
        let exe = std::env::current_exe().expect("the test binary");
        let output = std::process::Command::new(exe)
            .args([CHILD_TEST, "--exact", "--ignored", "--test-threads=1"])
            .env(CHILD_VARIABLE, "1")
            .env("HTTP_PROXY", &proxy_url)
            .env("http_proxy", &proxy_url)
            .env("ALL_PROXY", &proxy_url)
            .env("all_proxy", &proxy_url)
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
            .output()
            .expect("the child runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "the child failed: {stdout}");
        // A renamed child would match no test and exit 0; make sure it ran.
        assert!(stdout.contains(" 1 passed"), "the child test ran: {stdout}");
        let mut connections = 0;
        while proxy.accept().is_ok() {
            connections += 1;
        }
        assert_eq!(connections, 0, "nothing went through the proxy");
    }
}
