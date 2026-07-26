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
mod auth;
mod collectors;
mod models;
mod push;
mod routes;
mod state;

use axum::{Router, middleware};
use std::net::SocketAddr;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let app_state = state::AppState::new();

    // Load auth token from env (optional)
    if auth::configured_token().is_some() {
        tracing::info!("🔐 API authentication enabled (SYSTEM_AGENT_TOKEN set)");
    } else {
        tracing::info!("🔓 No auth token configured — API is open");
    }

    // Start background metric collection + alert evaluation
    let bg_state = app_state.clone();
    tokio::spawn(async move {
        collectors::background_collector(bg_state).await;
    });

    // Optional push client: if PUSH_TO env is set, connect to a hub
    if let Ok(hub_url) = std::env::var("PUSH_TO") {
        let token = std::env::var("PUSH_TOKEN").unwrap_or_default();
        let interval: u64 = std::env::var("PUSH_INTERVAL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2);
        let url = hub_url.clone();
        let t = token.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = push::run_push_client(&url, &t, interval).await {
                    tracing::error!("Push client error: {e}, retrying in 5s...");
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                }
            }
        });
    }

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .merge(routes::api::router(app_state.clone()))
        .merge(routes::sse::router(app_state.clone()))
        .merge(routes::ws::router(app_state.clone()))
        .nest_service("/", ServeDir::new("static"))
        .layer(middleware::from_fn(auth::require_auth))
        .layer(cors);

    let addr = SocketAddr::from(([0, 0, 0, 0], 9090));
    tracing::info!("🚀 System agent listening on http://{}", addr);
    tracing::info!("📊 Dashboard: http://localhost:9090/");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
