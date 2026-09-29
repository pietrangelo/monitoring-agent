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

mod application_wire;
mod applications;
mod clock;
mod collector;
mod db;
mod hourly_warning;
mod listen;
mod models;
mod presence;
mod push;
mod registry;
mod retention;
mod round_intake;
mod routes;
mod snapshot;
mod snapshot_intake;
mod state;
mod token_bucket;

use axum::Router;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;

/// Why the hub couldn't start, or stopped serving. Logged once by `main`, never with a
/// secret.
enum StartupError {
    Config(push::PushAuthError),
    Listen(listen::ListenAddressError),
    StaticDir(StaticDirError),
    Database(rusqlite::Error),
    FirstSummary(routes::sse::SummaryFailure),
    Bind(SocketAddr, std::io::Error),
    Serve(std::io::Error),
}

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(err) => write!(f, "{err}; refusing to start"),
            Self::Listen(err) => write!(f, "{err}; refusing to start"),
            Self::StaticDir(err) => write!(f, "{err}; refusing to start"),
            Self::Database(err) => write!(f, "Failed to open database system-hub.db: {err}"),
            Self::FirstSummary(failure) => write!(f, "{failure}; refusing to start"),
            Self::Bind(addr, err) => write!(f, "Failed to bind {addr}: {err}"),
            Self::Serve(err) => write!(f, "Server failed: {err}"),
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt::init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), StartupError> {
    // Configuration first, so a refused start leaves nothing behind.
    let listen = listen::ListenAddress::from_env(std::env::var(listen::ListenAddress::VARIABLE))
        .map_err(StartupError::Listen)?;
    let push_auth =
        push::PushAuth::from_env(std::env::var("HUB_PUSH_TOKEN")).map_err(StartupError::Config)?;
    let static_dir = StaticDir::from_env(std::env::var_os(StaticDir::VARIABLE))
        .check()
        .await
        .map_err(StartupError::StaticDir)?;

    let db = Arc::new(db::Database::new("system-hub.db").map_err(StartupError::Database)?);
    tracing::info!("📁 Database initialized: system-hub.db");

    // The first summary reads the database, so the state is built on the blocking pool.
    let app_state = tokio::task::spawn_blocking(move || state::AppState::new(db))
        .await
        .map_err(|err| StartupError::FirstSummary(routes::sse::SummaryFailure::Task(err)))?
        .map_err(|err| StartupError::FirstSummary(routes::sse::SummaryFailure::Serialise(err)))?;

    // Start background pollers (for HTTP-polled systems)
    collector::start_collectors(app_state.clone());
    retention::start(app_state.clone());
    routes::sse::start_publisher(app_state.clone());

    let app = app(app_state, push_auth, &static_dir);

    serve(app, listen).await
}

/// Binds `listen`, says where the hub is reached, and serves `app` until it fails.
async fn serve(app: Router, listen: listen::ListenAddress) -> Result<(), StartupError> {
    let addr = listen.get();
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|err| StartupError::Bind(addr, err))?;
    // The bound address, not the configured one: port 0 is only known once bound.
    let bound = listener
        .local_addr()
        .map_err(|err| StartupError::Bind(addr, err))?;
    let at = listen::reachable_at(bound);
    tracing::info!("🚀 System Hub listening on http://{bound}");
    tracing::info!("📊 Hub Dashboard: http://{at}/");
    tracing::info!("📡 Push endpoint: ws://{at}/api/push");

    axum::serve(listener, app)
        .await
        .map_err(StartupError::Serve)
}

/// Where the dashboard's files are served from. The container image configures a path
/// outside the data volume, so a volume created by an older image can't hide a newer
/// dashboard.
#[derive(Debug, PartialEq, Eq)]
enum StaticDir {
    /// `HUB_STATIC_DIR` unset or empty: `static` under the working directory, as before the
    /// variable existed. Not checked, so a hub run without a dashboard still serves its API.
    Default,
    /// `HUB_STATIC_DIR` names a path, which must be a directory.
    Configured(PathBuf),
}

impl StaticDir {
    const VARIABLE: &str = "HUB_STATIC_DIR";

    /// Unset or empty means the default; any other value, as it is, is configured.
    fn from_env(value: Option<OsString>) -> Self {
        match value {
            Some(path) if !path.is_empty() => Self::Configured(PathBuf::from(path)),
            Some(_) | None => Self::Default,
        }
    }

    /// A configured path must be a directory the hub can look inside (a symlink to one
    /// counts); the default is served as it is, unchecked. Looking inside takes search
    /// permission, which is what `ServeDir` needs; listing the directory is not.
    async fn check(self) -> Result<ServedStaticDir, StaticDirError> {
        let path = match self {
            Self::Default => return Ok(ServedStaticDir(PathBuf::from("static"))),
            Self::Configured(path) => path,
        };
        match tokio::fs::metadata(&path).await {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(StaticDirError::NotADirectory(path)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(StaticDirError::Missing(path));
            }
            Err(err) => return Err(StaticDirError::Unreadable(path, err.kind())),
        }
        match tokio::fs::metadata(path.join(".")).await {
            Ok(_) => Ok(ServedStaticDir(path)),
            Err(err) => Err(StaticDirError::Unreadable(path, err.kind())),
        }
    }
}

/// The directory the dashboard is served from, once checked: the only kind `app` accepts.
struct ServedStaticDir(PathBuf);

impl ServedStaticDir {
    fn path(&self) -> &Path {
        &self.0
    }
}

/// Why a configured `HUB_STATIC_DIR` can't be served. The path is operator configuration, not
/// a secret, and `Debug` formatting escapes it, so it can be logged.
enum StaticDirError {
    Missing(PathBuf),
    NotADirectory(PathBuf),
    Unreadable(PathBuf, std::io::ErrorKind),
}

impl std::fmt::Display for StaticDirError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let variable = StaticDir::VARIABLE;
        match self {
            Self::Missing(path) => write!(f, "{variable} is set to {path:?}, which does not exist"),
            Self::NotADirectory(path) => {
                write!(f, "{variable} is set to {path:?}, which is not a directory")
            }
            Self::Unreadable(path, kind) => {
                write!(
                    f,
                    "{variable} is set to {path:?}, which cannot be read ({kind})"
                )
            }
        }
    }
}

/// The hub's routes, its dashboard and CORS, as served.
fn app(
    app_state: Arc<state::AppState>,
    push_auth: push::PushAuth,
    static_dir: &ServedStaticDir,
) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .merge(routes::api::router(app_state.clone()))
        .merge(routes::applications::router(app_state.clone()))
        .merge(routes::sse::router(app_state.clone()))
        .merge(push::router(app_state, push_auth))
        .nest_service("/", ServeDir::new(static_dir.path()))
        .layer(cors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use std::os::unix::ffi::OsStringExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn the_static_dir_is_configured_by_a_non_empty_variable_and_the_default_otherwise() {
        let not_utf8 = OsString::from_vec(b"/srv/st\xffatic".to_vec());
        let configured = |path: &OsString| StaticDir::Configured(PathBuf::from(path));
        let cases = [
            ("unset", None, StaticDir::Default),
            ("empty", Some(OsString::new()), StaticDir::Default),
            (
                "an absolute path",
                Some(OsString::from("/usr/share/system-hub/static")),
                configured(&OsString::from("/usr/share/system-hub/static")),
            ),
            (
                "a relative path",
                Some(OsString::from("web")),
                configured(&OsString::from("web")),
            ),
            (
                "a blank path is kept as it is, not trimmed",
                Some(OsString::from(" ")),
                configured(&OsString::from(" ")),
            ),
            (
                "a path that isn't UTF-8",
                Some(not_utf8.clone()),
                configured(&not_utf8),
            ),
        ];
        for (name, value, expected) in cases {
            assert_eq!(StaticDir::from_env(value), expected, "case: {name}");
        }
        let default = StaticDir::Default
            .check()
            .await
            .map(|dir| dir.path().to_path_buf());
        assert_eq!(
            default.ok(),
            Some(PathBuf::from("static")),
            "the default is unchecked"
        );
    }

    /// A configured directory, checked as `run` checks it.
    async fn served(path: &Path) -> ServedStaticDir {
        match StaticDir::Configured(path.to_path_buf()).check().await {
            Ok(dir) => dir,
            Err(err) => panic!("{err}"),
        }
    }

    async fn get(app: Router, uri: &str) -> (StatusCode, String) {
        let res = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let body = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn the_dashboard_is_served_from_the_configured_static_dir() {
        let data = tempfile::tempdir().unwrap();
        let db = Arc::new(db::Database::new(data.path().join("hub.db").to_str().unwrap()).unwrap());
        let state = state::AppState::new(db).unwrap();
        // Two directories with different pages, so only the configured one can answer.
        let dirs: Vec<(tempfile::TempDir, String)> = ["image-marker", "volume-marker"]
            .into_iter()
            .map(|marker| {
                let dir = tempfile::tempdir().unwrap();
                std::fs::write(dir.path().join("index.html"), marker).unwrap();
                (dir, marker.to_string())
            })
            .collect();
        for (dir, marker) in &dirs {
            let static_dir = served(dir.path()).await;
            let app = app(state.clone(), push::PushAuth::Open, &static_dir);
            for uri in ["/", "/index.html"] {
                let (status, body) = get(app.clone(), uri).await;
                assert_eq!(status, StatusCode::OK, "{uri} from {marker}");
                assert_eq!(body, *marker, "{uri} is the configured directory's page");
            }
        }
        // No fallback to the default `static/`, which exists in the crate: a configured
        // directory without a page answers 404, not the default directory's page.
        let empty = tempfile::tempdir().unwrap();
        let app = app(
            state.clone(),
            push::PushAuth::Open,
            &served(empty.path()).await,
        );
        for uri in ["/", "/index.html"] {
            let (status, _) = get(app.clone(), uri).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "no fallback for {uri}");
        }
    }

    #[tokio::test]
    async fn the_served_app_keeps_the_api_the_applications_the_stream_the_push_endpoint_and_cors() {
        let data = tempfile::tempdir().unwrap();
        let db = Arc::new(db::Database::new(data.path().join("hub.db").to_str().unwrap()).unwrap());
        let static_dir = tempfile::tempdir().unwrap();
        let app = app(
            state::AppState::new(db).unwrap(),
            push::PushAuth::Open,
            &served(static_dir.path()).await,
        );

        let (status, body) = get(app.clone(), "/api/systems").await;
        assert_eq!(status, StatusCode::OK, "the API is served: {body}");
        assert_eq!(body, "[]", "the API answers with its own JSON");

        let (status, body) = get(app.clone(), "/api/systems/sys-1/applications").await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the applications route is served: {body}"
        );
        assert!(
            body.contains("\"system_id\":\"sys-1\""),
            "the applications route answers with its own JSON: {body}"
        );

        let (status, body) = get(app.clone(), "/api/health").await;
        assert_eq!(status, StatusCode::OK, "health is served");
        assert!(body.contains("\"ok\""), "health answers ok: {body}");

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/stream/summary")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.headers().get("content-type").map(|v| v.as_bytes()),
            Some(&b"text/event-stream"[..]),
            "the SSE stream is served"
        );

        // A plain GET is not a WebSocket upgrade; the push route answers it, not the files.
        let (status, _) = get(app.clone(), "/api/push").await;
        assert_ne!(status, StatusCode::NOT_FOUND, "the push endpoint is routed");

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .header("origin", "http://example.test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            res.headers().contains_key("access-control-allow-origin"),
            "the CORS layer is applied"
        );
    }
}
