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
use subtle::ConstantTimeEq;

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
    if tokens_match(token.as_deref(), &expected) {
        return Ok(next.run(request).await);
    }

    Err(StatusCode::UNAUTHORIZED)
}

/// Constant-time comparison of the presented token against the expected one, to avoid
/// leaking the secret's contents through a timing side-channel (OWASP A02 / API2).
fn tokens_match(presented: Option<&str>, expected: &str) -> bool {
    match presented {
        Some(token) => token.as_bytes().ct_eq(expected.as_bytes()).into(),
        None => false,
    }
}

fn extract_token(request: &Request) -> Option<String> {
    // 1. Check Authorization: Bearer <token>
    if let Some(auth) = request.headers().get(header::AUTHORIZATION)
        && let Ok(val) = auth.to_str()
        && let Some(token) = val.strip_prefix("Bearer ")
    {
        return Some(token.trim().to_string());
    }
    // 2. Check X-API-Key: <token>
    if let Some(key) = request.headers().get("X-API-Key")
        && let Ok(val) = key.to_str()
    {
        return Some(val.trim().to_string());
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use axum::{Router, middleware};
    use std::sync::Mutex;
    use tower::ServiceExt;

    /// SYSTEM_AGENT_TOKEN is process-global; serialize every test that touches it
    /// so parallel `cargo test` threads don't race on the env var.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard<'a> {
        _lock: std::sync::MutexGuard<'a, ()>,
    }

    impl<'a> EnvGuard<'a> {
        fn set(value: &str) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            unsafe {
                std::env::set_var(ENV_TOKEN, value);
            }
            Self { _lock: lock }
        }

        fn unset() -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            unsafe {
                std::env::remove_var(ENV_TOKEN);
            }
            Self { _lock: lock }
        }
    }

    impl Drop for EnvGuard<'_> {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var(ENV_TOKEN);
            }
        }
    }

    fn test_router() -> Router {
        Router::new()
            .route("/api/protected", get(|| async { "ok" }))
            .route("/api/health", get(|| async { "ok" }))
            .route("/", get(|| async { "ok" }))
            .route("/dashboard", get(|| async { "ok" }))
            .route("/index.html", get(|| async { "ok" }))
            .route("/static/app.js", get(|| async { "ok" }))
            .layer(middleware::from_fn(require_auth))
    }

    #[test]
    fn configured_token_none_when_unset() {
        let _g = EnvGuard::unset();
        assert_eq!(configured_token(), None);
    }

    #[test]
    fn configured_token_none_when_empty() {
        let _g = EnvGuard::set("");
        assert_eq!(configured_token(), None);
    }

    #[test]
    fn configured_token_some_when_set() {
        let _g = EnvGuard::set("secret123");
        assert_eq!(configured_token(), Some("secret123".to_string()));
    }

    #[tokio::test]
    async fn require_auth_allows_everything_when_unconfigured() {
        let _g = EnvGuard::unset();
        let res = test_router()
            .oneshot(
                Request::builder()
                    .uri("/api/protected")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn require_auth_rejects_missing_token() {
        let _g = EnvGuard::set("supersecret");
        let res = test_router()
            .oneshot(
                Request::builder()
                    .uri("/api/protected")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn require_auth_rejects_wrong_bearer_token() {
        let _g = EnvGuard::set("supersecret");
        let res = test_router()
            .oneshot(
                Request::builder()
                    .uri("/api/protected")
                    .header(header::AUTHORIZATION, "Bearer wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn require_auth_accepts_correct_bearer_token() {
        let _g = EnvGuard::set("supersecret");
        let res = test_router()
            .oneshot(
                Request::builder()
                    .uri("/api/protected")
                    .header(header::AUTHORIZATION, "Bearer supersecret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn require_auth_accepts_correct_api_key_header() {
        let _g = EnvGuard::set("supersecret");
        let res = test_router()
            .oneshot(
                Request::builder()
                    .uri("/api/protected")
                    .header("X-API-Key", "supersecret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn require_auth_accepts_correct_query_token() {
        let _g = EnvGuard::set("supersecret");
        let res = test_router()
            .oneshot(
                Request::builder()
                    .uri("/api/protected?token=supersecret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn require_auth_bypasses_health_static_and_dashboard_paths() {
        let _g = EnvGuard::set("supersecret");
        for path in [
            "/api/health",
            "/",
            "/dashboard",
            "/index.html",
            "/static/app.js",
        ] {
            let res = test_router()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                res.status(),
                StatusCode::OK,
                "path {path} should bypass auth"
            );
        }
    }

    #[test]
    fn extract_token_prefers_bearer_header() {
        let req = Request::builder()
            .uri("/x?token=fromquery")
            .header(header::AUTHORIZATION, "Bearer fromheader")
            .header("X-API-Key", "fromapikey")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_token(&req), Some("fromheader".to_string()));
    }

    #[test]
    fn extract_token_falls_back_to_api_key_then_query() {
        let req = Request::builder()
            .uri("/x?token=fromquery")
            .header("X-API-Key", "fromapikey")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_token(&req), Some("fromapikey".to_string()));

        let req = Request::builder()
            .uri("/x?token=fromquery")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_token(&req), Some("fromquery".to_string()));
    }

    #[test]
    fn extract_token_none_when_absent() {
        let req = Request::builder().uri("/x").body(Body::empty()).unwrap();
        assert_eq!(extract_token(&req), None);
    }

    #[test]
    fn extract_token_trims_whitespace_on_bearer_and_api_key() {
        let req = Request::builder()
            .uri("/x")
            .header(header::AUTHORIZATION, "Bearer   spaced-token  ")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_token(&req), Some("spaced-token".to_string()));
    }

    #[test]
    fn extract_token_query_param_among_other_pairs() {
        let req = Request::builder()
            .uri("/x?foo=bar&token=mid&baz=qux")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_token(&req), Some("mid".to_string()));
    }

    #[test]
    fn urlencoding_decodes_percent_and_plus() {
        assert_eq!(urlencoding("a%20b"), "a b");
        assert_eq!(urlencoding("a+b"), "a b");
        assert_eq!(urlencoding("hello"), "hello");
        assert_eq!(urlencoding(""), "");
    }

    #[test]
    fn urlencoding_leaves_malformed_percent_sequences_literal() {
        // "%zz" is not valid hex, so each byte is copied through as-is.
        assert_eq!(urlencoding("%zz"), "%zz");
    }

    #[test]
    fn urlencoding_trailing_percent_without_enough_bytes() {
        assert_eq!(urlencoding("100%"), "100%");
    }

    #[test]
    fn tokens_match_accepts_equal_tokens() {
        assert!(tokens_match(Some("supersecret"), "supersecret"));
    }

    #[test]
    fn tokens_match_rejects_different_tokens_of_equal_length() {
        assert!(!tokens_match(Some("aaaaaaaa"), "bbbbbbbb"));
    }

    #[test]
    fn tokens_match_rejects_different_lengths() {
        assert!(!tokens_match(Some("short"), "much-longer-token"));
        assert!(!tokens_match(Some("much-longer-token"), "short"));
    }

    #[test]
    fn tokens_match_rejects_none() {
        assert!(!tokens_match(None, "supersecret"));
    }

    #[test]
    fn tokens_match_empty_expected_only_matches_empty_presented() {
        assert!(tokens_match(Some(""), ""));
        assert!(!tokens_match(Some("x"), ""));
    }
}
