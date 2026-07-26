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

mod collector;
mod db;
mod models;
mod push;
mod routes;
mod state;

use axum::Router;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let db = Arc::new(db::Database::new("system-hub.db").expect("Failed to open database"));
    tracing::info!("📁 Database initialized: system-hub.db");

    let app_state = state::AppState::new(db);

    // Start background pollers (for HTTP-polled systems)
    collector::start_collectors(app_state.clone());

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .merge(routes::api::router(app_state.clone()))
        .merge(routes::sse::router(app_state.clone()))
        .merge(push::router(app_state.clone()))
        .nest_service("/", ServeDir::new("static"))
        .layer(cors);

    let addr = SocketAddr::from(([0, 0, 0, 0], 9091));
    tracing::info!("🚀 System Hub listening on http://{}", addr);
    tracing::info!("📊 Hub Dashboard: http://localhost:9091/");
    tracing::info!("📡 Push endpoint: ws://localhost:9091/api/push");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
