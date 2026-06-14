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


use axum::{
    extract::Request,
    http::{StatusCode, header},
    middleware::Next,
    response::Response,
};

const ENV_TOKEN: &str = "SYSTEM_AGENT_TOKEN";

/// Returns the configured token, or None if auth is disabled.
pub fn configured_token() -> Option<String> {
    std::env::var(ENV_TOKEN).ok().filter(|t| !t.is_empty())
}

/// Axum middleware that rejects requests missing the correct API token.
/// Skips the health endpoint and static files.
pub async fn require_auth(request: Request, next: Next) -> Result<Response, StatusCode> {
    let Some(expected) = configured_token() else {
        // Auth not configured — allow all
        return Ok(next.run(request).await);
    };

    let path = request.uri().path();

    // Allow health check, static files, dashboard HTML, SSE, and WS without auth
    if path == "/api/health"
        || path.starts_with("/static/")
        || path == "/"
        || path == "/dashboard"
        || path == "/index.html"
    {
        return Ok(next.run(request).await);
    }

    let token = extract_token(&request);
    if token.as_deref() == Some(&expected) {
        return Ok(next.run(request).await);
    }

    Err(StatusCode::UNAUTHORIZED)
}

fn extract_token(request: &Request) -> Option<String> {
    // 1. Check Authorization: Bearer <token>
    if let Some(auth) = request.headers().get(header::AUTHORIZATION) {
        if let Ok(val) = auth.to_str() {
            if let Some(token) = val.strip_prefix("Bearer ") {
                return Some(token.trim().to_string());
            }
        }
    }
    // 2. Check X-API-Key: <token>
    if let Some(key) = request.headers().get("X-API-Key") {
        if let Ok(val) = key.to_str() {
            return Some(val.trim().to_string());
        }
    }
    // 3. Check ?token=<token> query param
    if let Some(query) = request.uri().query() {
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("token=") {
                return Some(urlencoding(v).to_string());
            }
        }
    }
    None
}

fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(hex as char);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            out.push(' ');
            i += 1;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}
