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
mod models;
mod push;
mod routes;
mod state;

use applications::config::ApplicationsConfig;
use axum::{Router, middleware};
use std::fmt;
use std::net::SocketAddr;
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
    Bind(SocketAddr, std::io::Error),
    Serve(std::io::Error),
}

impl StartupError {
    /// The process exit code: `EX_CONFIG` (78, from `sysexits.h`) for a refused
    /// configuration and for nothing else, 1 for any other failure.
    fn exit_code(&self) -> u8 {
        match self {
            Self::Config(_) => 78,
            Self::Runtime(_) | Self::Bind(..) | Self::Serve(_) => 1,
        }
    }
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(err) => write!(f, "{err}; refusing to start"),
            Self::Runtime(err) => write!(f, "Failed to start the async runtime: {err}"),
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
    runtime.block_on(run())
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

async fn run() -> Result<(), StartupError> {
    let app_state = state::AppState::new();

    if auth::configured_token().is_some() {
        tracing::info!("🔐 API authentication enabled (SYSTEM_AGENT_TOKEN set)");
    } else {
        tracing::info!("🔓 No auth token configured — API is open");
    }

    let bg_state = app_state.clone();
    tokio::spawn(async move {
        collectors::background_collector(bg_state).await;
    });
    spawn_push_client();

    serve(router(app_state)).await
}

/// Starts the push client if `PUSH_TO` names a hub. It reconnects for the agent's lifetime.
fn spawn_push_client() {
    let Ok(hub_url) = std::env::var("PUSH_TO") else {
        return;
    };
    let token = std::env::var("PUSH_TOKEN").unwrap_or_default();
    let interval: u64 = std::env::var("PUSH_INTERVAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    tokio::spawn(async move {
        loop {
            if let Err(e) = push::run_push_client(&hub_url, &token, interval).await {
                tracing::error!("Push client error: {e}, retrying in 5s...");
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
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

    #[test]
    fn only_a_refused_configuration_exits_with_ex_config() {
        let io = || std::io::Error::other("boom");
        let addr = SocketAddr::from(([0, 0, 0, 0], 9090));
        let cases = [
            (
                "a refused configuration",
                StartupError::Config(ApplicationsConfigError::InvalidInterval),
                78,
            ),
            ("no runtime", StartupError::Runtime(io()), 1),
            ("a busy port", StartupError::Bind(addr, io()), 1),
            ("a failed server", StartupError::Serve(io()), 1),
        ];
        for (name, err, code) in cases {
            assert_eq!(err.exit_code(), code, "case: {name}");
        }
    }
}
