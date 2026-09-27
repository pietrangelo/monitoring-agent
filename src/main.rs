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

mod alerts;
mod applications;
mod auth;
mod collectors;
mod environment;
mod models;
mod push;
mod routes;
mod snapshot;
mod state;

use applications::config::ApplicationsConfig;
use applications::scrape_loop::scrape_loop;
use applications::scraper::{Scraper, Timeouts};
use axum::{Router, middleware};
use collectors::SnapshotReceiver;
use collectors::sampler::SysinfoSource;
use environment::cgroup::MonitoredCgroup;
use snapshot::SnapshotSeq;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;

/// Where the API and dashboard are served.
const LISTEN: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 9090);

/// Why the agent couldn't start, or stopped serving. Logged once by `main`, never with a
/// configuration value.
enum StartupError {
    Config(applications::config::ApplicationsConfigError),
    Runtime(std::io::Error),
    Scraper(applications::scraper::ScraperError),
    Collector(tokio::task::JoinError),
    Bind(SocketAddr, std::io::Error),
    Serve(std::io::Error),
}

impl StartupError {
    /// The process exit code: `EX_CONFIG` (78, from `sysexits.h`) for a refused
    /// configuration and for nothing else, 1 for any other failure.
    fn exit_code(&self) -> u8 {
        match self {
            Self::Config(_) => 78,
            Self::Runtime(_)
            | Self::Scraper(_)
            | Self::Collector(_)
            | Self::Bind(..)
            | Self::Serve(_) => 1,
        }
    }
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(err) => write!(f, "{err}; refusing to start"),
            Self::Runtime(err) => write!(f, "Failed to start the async runtime: {err}"),
            Self::Scraper(err) => write!(f, "Failed to start scraping applications: {err}"),
            Self::Collector(err) => write!(f, "Failed to read the system: {err}"),
            Self::Bind(addr, err) => write!(f, "Failed to bind {addr}: {err}"),
            Self::Serve(err) => write!(f, "Server failed: {err}"),
        }
    }
}

fn main() -> ExitCode {
    tracing_subscriber::fmt::init();
    match start() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err}");
            ExitCode::from(err.exit_code())
        }
    }
}

/// Parses the configuration before the runtime exists, so a refusal happens before any task,
/// bind or connection can (RFC 0009 §2).
fn start() -> Result<(), StartupError> {
    let applications =
        ApplicationsConfig::parse(|key| std::env::var(key)).map_err(StartupError::Config)?;
    announce(&applications);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(StartupError::Runtime)?;
    runtime.block_on(run(applications))
}

/// Logs what the applications configuration asks for, and warns about credentials that would
/// cross the network in plain text. Names applications, never their URLs.
fn announce(applications: &ApplicationsConfig) {
    match applications {
        ApplicationsConfig::Off => {}
        ApplicationsConfig::On(apps) => tracing::info!(
            "🍃 {} Spring Boot application(s) configured, scraped every {} s",
            apps.targets().len(),
            apps.interval().as_duration().as_secs()
        ),
    }
    for name in applications.plaintext_credentials() {
        tracing::warn!(
            "Basic credentials for application {:?} would cross the network in plain text: \
             its actuator is reached without TLS, at an address that is not loopback",
            name.as_str()
        );
    }
}

/// The system collector's source: sysinfo, and the files under `/` (the OS release file and,
/// in a container, the monitored cgroup's files under `/sys/fs/cgroup`).
fn system_source(cgroup: Option<MonitoredCgroup>) -> SysinfoSource {
    SysinfoSource::new(PathBuf::from("/"), cgroup)
}

/// The agent's execution environment, classified once from the evidence under `/` and the
/// agent's own variables, read off the runtime (RFC 0014 §3).
async fn execution_environment() -> Result<environment::ExecutionEnvironment, StartupError> {
    let evidence = tokio::task::spawn_blocking(|| {
        collectors::environment::gather_evidence(
            std::path::Path::new("/"),
            |key| std::env::var(key).ok(),
            environment::evidence::CpuArchitecture::of_build(),
        )
    })
    .await
    .map_err(StartupError::Collector)?;
    let found = environment::classify(&evidence);
    tracing::info!("execution environment: {found}");
    Ok(found)
}

async fn run(applications: ApplicationsConfig) -> Result<(), StartupError> {
    let environment = execution_environment().await?;
    let cgroup = environment.monitored_cgroup().cloned();
    // The startup snapshot is read before anything can ask for one, so none ever waits.
    let (sampler, first) = collectors::first_snapshot(system_source(cgroup.clone()), environment)
        .await
        .map_err(StartupError::Collector)?;
    let system = &first.snapshot.system;
    tracing::info!(
        "resource capacity: {} CPUs, {} of memory",
        system.cpu.capacity_cpus,
        system.memory.total_display
    );
    let (publisher, snapshots) =
        tokio::sync::watch::channel(Arc::new(first.snapshot.published(SnapshotSeq::FIRST)));
    let app_state = start_applications(applications, snapshots.clone())?;

    if auth::configured_token().is_some() {
        tracing::info!("🔐 API authentication enabled (SYSTEM_AGENT_TOKEN set)");
    } else {
        tracing::info!("🔓 No auth token configured — API is open");
    }

    let collecting = tokio::spawn(collectors::background_collector(
        app_state.clone(),
        publisher,
        sampler,
        first.history,
        move || system_source(cgroup.clone()),
    ));
    // The collector runs for the agent's lifetime; if it ends, say so once, where operators look.
    tokio::spawn(async move {
        match collecting.await {
            Ok(()) => tracing::error!("The system collector ended; the snapshot will go stale"),
            Err(err) => {
                tracing::error!("The system collector failed: {err}; the snapshot will go stale")
            }
        }
    });
    spawn_push_client(snapshots, app_state.rounds.clone());

    serve(router(app_state)).await
}

/// Starts the scrape loop when applications are configured, and returns the state that serves
/// its rounds. With applications off, no loop runs and the state has no rounds.
fn start_applications(
    applications: ApplicationsConfig,
    snapshots: SnapshotReceiver,
) -> Result<Arc<state::AppState>, StartupError> {
    let ApplicationsConfig::On(applications) = applications else {
        return Ok(state::AppState::new(snapshots));
    };
    let scraper = Scraper::new(Timeouts::PRODUCTION).map_err(StartupError::Scraper)?;
    let (rounds, receiver) = tokio::sync::watch::channel(None);
    let app_state = state::AppState::with_rounds(snapshots, receiver);
    let scraping = tokio::spawn(scrape_loop(scraper, applications, app_state.run, rounds));
    // The loop runs for the agent's lifetime; if it ends, say so once, where operators look.
    tokio::spawn(async move {
        match scraping.await {
            Ok(()) => tracing::error!("The application scrape loop ended; no more rounds"),
            Err(err) => tracing::error!("The application scrape loop failed: {err}"),
        }
    });
    Ok(app_state)
}

/// Starts the push client if `PUSH_TO` names a hub. It reconnects for the agent's lifetime,
/// sending the scrape loop's rounds too when applications are on.
fn spawn_push_client(
    snapshots: SnapshotReceiver,
    rounds: Option<applications::scrape_loop::RoundReceiver>,
) {
    let Ok(hub_url) = std::env::var("PUSH_TO") else {
        return;
    };
    let token = std::env::var("PUSH_TOKEN").unwrap_or_default();
    let interval: u64 = std::env::var("PUSH_INTERVAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    tokio::spawn(async move {
        // Resolved once, off the runtime: it reads files and may shell out to `hostname`.
        let system_id = match tokio::task::spawn_blocking(push::get_persistent_id).await {
            Ok(id) => id,
            Err(err) => {
                tracing::error!("Couldn't resolve the push system id: {err}; not pushing");
                return;
            }
        };
        let mut feed = push::PushFeed {
            snapshots: push::SnapshotCursor::new(snapshots),
            rounds,
        };
        loop {
            let pushed =
                push::run_push_client(&hub_url, &token, &system_id, interval, &mut feed).await;
            match pushed {
                Ok(()) => {}
                Err(push::PushError::CollectorEnded) => {
                    tracing::error!("The system collector has ended; no longer pushing");
                    return;
                }
                Err(push::PushError::Connection(e)) => {
                    tracing::error!("Push client error: {e}, retrying in 5s...");
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                }
            }
        }
    });
}

fn router(app_state: Arc<state::AppState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .merge(routes::api::router(app_state.clone()))
        .merge(routes::applications::router(app_state.clone()))
        .merge(routes::sse::router(app_state.clone()))
        .merge(routes::ws::router(app_state))
        .nest_service("/", ServeDir::new("static"))
        .layer(middleware::from_fn(auth::require_auth))
        .layer(cors)
}

async fn serve(app: Router) -> Result<(), StartupError> {
    let listener = tokio::net::TcpListener::bind(LISTEN)
        .await
        .map_err(|err| StartupError::Bind(LISTEN, err))?;
    tracing::info!("🚀 System agent listening on http://{LISTEN}");
    tracing::info!("📊 Dashboard: http://localhost:{}/", LISTEN.port());
    axum::serve(listener, app)
        .await
        .map_err(StartupError::Serve)
}

#[cfg(test)]
mod tests {
    use super::*;
    use applications::config::ApplicationsConfigError;

    async fn panicked() -> tokio::task::JoinError {
        tokio::spawn(async { panic!("boom") })
            .await
            .expect_err("the task panicked")
    }

    #[tokio::test]
    async fn only_a_refused_configuration_exits_with_ex_config() {
        let io = || std::io::Error::other("boom");
        let addr = SocketAddr::from(([0, 0, 0, 0], 9090));
        let cases = [
            (
                "a refused configuration",
                StartupError::Config(ApplicationsConfigError::InvalidInterval),
                78,
            ),
            ("no runtime", StartupError::Runtime(io()), 1),
            (
                "no HTTP client",
                StartupError::Scraper(applications::scraper::ScraperError::for_test()),
                1,
            ),
            (
                "a panicking first reading",
                StartupError::Collector(panicked().await),
                1,
            ),
            ("a busy port", StartupError::Bind(addr, io()), 1),
            ("a failed server", StartupError::Serve(io()), 1),
        ];
        for (name, err, code) in cases {
            assert_eq!(err.exit_code(), code, "case: {name}");
        }
    }
}
