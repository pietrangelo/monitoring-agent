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
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
    routing::get,
};
use serde::Deserialize;
use std::sync::Arc;
use subtle::ConstantTimeEq;

use crate::models::{SystemId, SystemIdError, SystemInfo, SystemStatus};
use crate::state::{AppState, LiveMetrics};

/// Deserialized from MessagePack binary payloads sent by agents.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct PushPayload {
    system_id: String,
    hostname: String,
    os_name: String,
    kernel: String,
    cpu_percent: f32,
    cpu_cores: usize,
    cpu_model: String,
    memory_percent: f32,
    memory_used_display: String,
    memory_total_display: String,
    memory_used_bytes: u64,
    memory_total_bytes: u64,
    swap_percent: f32,
    load_one: f64,
    load_five: f64,
    load_fifteen: f64,
    uptime_seconds: u64,
    uptime_display: String,
    disks: Vec<DiskItem>,
    #[serde(default)]
    top_processes: Vec<ProcessItem>,
    timestamp: u64,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct DiskItem {
    mount_point: String,
    usage_percent: f32,
    total_display: String,
    used_display: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct ProcessItem {
    pid: u32,
    name: String,
    cpu_usage: f32,
    memory_usage_display: String,
    memory_percent: f32,
}

/// The push handshake's first message. No `Debug`: `token` holds the presented secret.
#[derive(Deserialize)]
struct AuthMessage {
    #[serde(rename = "type")]
    msg_type: String,
    system_id: String,
    #[serde(default)]
    token: String,
}

/// The configured `HUB_PUSH_TOKEN`. Never empty: an unset or empty value disables push
/// authentication, which is `Option<PushToken>::None`. No `Debug`, so the secret can't
/// reach a log line.
#[derive(Clone)]
struct PushToken(String);

impl PushToken {
    fn new(value: String) -> Option<Self> {
        (!value.is_empty()).then_some(Self(value))
    }

    /// Unset, empty and non-unicode values all disable push auth, as they did before
    /// RFC 0003.
    fn from_env(value: Result<String, std::env::VarError>) -> Option<Self> {
        value.ok().and_then(Self::new)
    }

    /// Constant-time, so how long a rejection takes doesn't leak the secret's content.
    fn accepts(&self, presented: &str) -> bool {
        presented.as_bytes().ct_eq(self.0.as_bytes()).into()
    }
}

/// Why the hub refused a push handshake.
#[derive(Debug, PartialEq, Eq)]
enum HandshakeRejection {
    NotAnAuthMessage,
    InvalidToken,
    /// Carries the broken rule so the rejection's log line names it, never the id itself.
    InvalidSystemId(SystemIdError),
}

impl HandshakeRejection {
    fn message(&self) -> &'static str {
        match self {
            Self::NotAnAuthMessage => "expected auth message",
            Self::InvalidToken => "invalid token",
            // One answer for every rule: the client learns only that its id was refused.
            Self::InvalidSystemId(
                SystemIdError::Empty | SystemIdError::TooLong | SystemIdError::DotSegment,
            ) => "invalid system_id",
        }
    }
}

/// Parses the first message of a push connection into the system id it authenticates.
/// The token is checked before the id, so an unauthenticated client can't probe which ids
/// the hub accepts.
fn authenticate(
    frame: &str,
    push_token: Option<&PushToken>,
) -> Result<SystemId, HandshakeRejection> {
    let auth: AuthMessage =
        serde_json::from_str(frame).map_err(|_| HandshakeRejection::NotAnAuthMessage)?;
    if auth.msg_type != "auth" {
        return Err(HandshakeRejection::NotAnAuthMessage);
    }
    if push_token.is_some_and(|token| !token.accepts(&auth.token)) {
        return Err(HandshakeRejection::InvalidToken);
    }
    SystemId::try_from(auth.system_id).map_err(HandshakeRejection::InvalidSystemId)
}

/// What the push handler needs per connection: the shared app state and the configured
/// push token, read once when the router is built.
#[derive(Clone)]
struct PushContext {
    app: Arc<AppState>,
    push_token: Option<PushToken>,
}

pub fn router(state: Arc<AppState>) -> Router {
    let push_token = PushToken::from_env(std::env::var("HUB_PUSH_TOKEN"));
    if push_token.is_none() {
        tracing::warn!("HUB_PUSH_TOKEN is not set: any client may push to /api/push");
    }
    router_with_token(state, push_token)
}

fn router_with_token(app: Arc<AppState>, push_token: Option<PushToken>) -> Router {
    Router::new()
        .route("/api/push", get(push_handler))
        .with_state(PushContext { app, push_token })
}

async fn push_handler(ws: WebSocketUpgrade, State(ctx): State<PushContext>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_push(socket, ctx))
}

async fn handle_push(mut socket: WebSocket, ctx: PushContext) {
    tracing::info!("Push client connected");
    let Some(system_id) = handshake(&mut socket, &ctx).await else {
        return;
    };
    tracing::info!("Push client authenticated: {}", system_id.as_str());

    receive_frames(&mut socket, &ctx.app, &system_id).await;

    let _ = on_blocking_pool(&ctx.app, &system_id, mark_offline).await;
    tracing::info!("Push client disconnected: {}", system_id.as_str());
}

/// Answers the push handshake. Returns the authenticated system id, or `None` once the
/// client has been refused or opened with something other than a text message.
async fn handshake(socket: &mut WebSocket, ctx: &PushContext) -> Option<SystemId> {
    let Some(Ok(Message::Text(frame))) = socket.recv().await else {
        return None;
    };
    match authenticate(&frame, ctx.push_token.as_ref()) {
        Ok(system_id) => {
            on_blocking_pool(&ctx.app, &system_id, register_if_new)
                .await
                .ok()?;
            let _ = socket
                .send(Message::Text(r#"{"type":"auth_ok"}"#.into()))
                .await;
            Some(system_id)
        }
        Err(rejection) => {
            tracing::warn!("Push handshake rejected: {rejection:?}");
            let answer = serde_json::json!({"type": "auth_error", "message": rejection.message()});
            let _ = socket.send(Message::Text(answer.to_string())).await;
            None
        }
    }
}

/// Ingests push frames in arrival order until the connection closes or ingestion fails.
async fn receive_frames(socket: &mut WebSocket, app: &Arc<AppState>, system_id: &SystemId) {
    while let Some(msg) = socket.recv().await {
        match msg {
            Ok(Message::Binary(data)) => {
                // Frames that don't decode are dropped, as documented in ARCHITECTURE.md.
                let Ok(payload) = rmp_serde::from_slice::<PushPayload>(&data) else {
                    continue;
                };
                let ingest = move |app: &AppState, id: &SystemId| ingest_frame(app, id, &payload);
                if on_blocking_pool(app, system_id, ingest).await.is_err() {
                    break;
                }
            }
            Ok(Message::Ping(data)) => {
                let _ = socket.send(Message::Pong(data)).await;
            }
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(Message::Text(_) | Message::Pong(_)) => {}
        }
    }
}

/// Runs one unit of synchronous SQLite work off the async runtime and waits for it, so a
/// connection never has more than one unit in flight and units land in order.
async fn on_blocking_pool(
    app: &Arc<AppState>,
    system_id: &SystemId,
    work: impl FnOnce(&AppState, &SystemId) + Send + 'static,
) -> Result<(), tokio::task::JoinError> {
    let (app, id) = (Arc::clone(app), system_id.clone());
    let outcome = tokio::task::spawn_blocking(move || work(&app, &id)).await;
    if let Err(err) = &outcome {
        tracing::error!("Push work for {} failed: {err}", system_id.as_str());
    }
    outcome
}

/// Registers a system the hub hasn't seen before, under its default name.
fn register_if_new(app: &AppState, system_id: &SystemId) {
    if app
        .db
        .get_system(system_id.as_str())
        .ok()
        .flatten()
        .is_some()
    {
        return;
    }
    let sys = SystemInfo {
        id: system_id.as_str().to_string(),
        name: system_id.default_name(),
        url: "push://".to_string(),
        token: String::new(),
        status: SystemStatus::Online,
        last_seen: String::new(),
        last_error: None,
        os: None,
        hostname: None,
        kernel: None,
        cpu_model: None,
        cpu_cores: None,
        total_memory_display: None,
        total_memory_bytes: None,
        poll_interval_secs: 10,
        enabled: true,
    };
    let _ = app.db.insert_system(&sys);
}

fn ingest_frame(app: &AppState, system_id: &SystemId, payload: &PushPayload) {
    store_metrics(app, system_id.as_str(), payload);
    cache_live_metrics(app, system_id.as_str(), payload);
    update_registry(app, system_id, payload);
    app.refresh_cache();
}

fn store_metrics(app: &AppState, system_id: &str, payload: &PushPayload) {
    for (metric, value) in metric_points(payload) {
        let _ = app
            .db
            .insert_metric(system_id, &metric, value, payload.timestamp);
    }
}

/// The metric points one snapshot adds to the system's history, keyed by metric name.
fn metric_points(payload: &PushPayload) -> impl Iterator<Item = (String, f32)> + '_ {
    let scalars = [
        ("cpu", payload.cpu_percent),
        ("memory", payload.memory_percent),
        ("swap", payload.swap_percent),
        ("load1", payload.load_one as f32),
        ("load5", payload.load_five as f32),
    ];
    let disks = payload
        .disks
        .iter()
        .map(|disk| (format!("disk:{}", disk.mount_point), disk.usage_percent));
    scalars
        .into_iter()
        .map(|(metric, value)| (metric.to_string(), value))
        .chain(disks)
}

fn cache_live_metrics(app: &AppState, system_id: &str, payload: &PushPayload) {
    let live = LiveMetrics {
        cpu_percent: payload.cpu_percent,
        memory_percent: payload.memory_percent,
        load_one: payload.load_one,
        disks: payload
            .disks
            .iter()
            .map(|disk| (disk.mount_point.clone(), disk.usage_percent))
            .collect(),
        updated_at: payload.timestamp,
    };
    app.live_metrics
        .write()
        .unwrap()
        .insert(system_id.to_string(), live);
}

/// Fills in system info while its hostname or OS is missing, marks the system online, and
/// replaces a default name with the snapshot's hostname.
fn update_registry(app: &AppState, system_id: &SystemId, payload: &PushPayload) {
    let id = system_id.as_str();
    let Some(sys) = app.db.get_system(id).ok().flatten() else {
        return;
    };
    if sys.hostname.is_none() || sys.os.is_none() {
        let _ = app.db.update_system_info(
            id,
            Some(&payload.os_name),
            Some(&payload.hostname),
            Some(&payload.kernel),
            Some(&payload.cpu_model),
            Some(payload.cpu_cores),
            Some(&payload.memory_total_display),
            Some(payload.memory_total_bytes),
        );
    }
    let _ = app
        .db
        .update_system_status(id, &SystemStatus::Online, &payload.uptime_display, None);
    if system_id.is_default_name(&sys.name) {
        let _ = app
            .db
            .update_system_config(id, Some(&payload.hostname), None, None, None, None);
    }
}

fn mark_offline(app: &AppState, system_id: &SystemId) {
    let _ = app.db.update_system_status(
        system_id.as_str(),
        &SystemStatus::Offline,
        "",
        Some("push disconnected"),
    );
    app.refresh_cache();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde::Serialize;
    use tower::ServiceExt;

    fn temp_state() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Arc::new(Database::new(path.to_str().unwrap()).unwrap());
        (AppState::new(db), dir)
    }

    #[test]
    fn auth_message_deserializes_from_json() {
        let msg: AuthMessage =
            serde_json::from_str(r#"{"type":"auth","system_id":"sys-1","token":"tok"}"#).unwrap();
        assert_eq!(msg.msg_type, "auth");
        assert_eq!(msg.system_id, "sys-1");
        assert_eq!(msg.token, "tok");
    }

    #[test]
    fn auth_message_token_defaults_to_empty() {
        let msg: AuthMessage =
            serde_json::from_str(r#"{"type":"auth","system_id":"sys-1"}"#).unwrap();
        assert_eq!(msg.token, "");
    }

    #[derive(Serialize)]
    struct MirroredDisk {
        mount_point: String,
        usage_percent: f32,
        total_display: String,
        used_display: String,
    }

    #[derive(Serialize)]
    struct MirroredProcess {
        pid: u32,
        name: String,
        cpu_usage: f32,
        memory_usage_display: String,
        memory_percent: f32,
    }

    /// Mirrors `PushPayload`'s field order exactly, so a MessagePack encoding of this
    /// struct can be decoded by the real (deserialize-only) `PushPayload`.
    #[derive(Serialize)]
    struct MirroredPushPayload {
        system_id: String,
        hostname: String,
        os_name: String,
        kernel: String,
        cpu_percent: f32,
        cpu_cores: usize,
        cpu_model: String,
        memory_percent: f32,
        memory_used_display: String,
        memory_total_display: String,
        memory_used_bytes: u64,
        memory_total_bytes: u64,
        swap_percent: f32,
        load_one: f64,
        load_five: f64,
        load_fifteen: f64,
        uptime_seconds: u64,
        uptime_display: String,
        disks: Vec<MirroredDisk>,
        top_processes: Vec<MirroredProcess>,
        timestamp: u64,
    }

    fn sample_frame(hostname: &str) -> MirroredPushPayload {
        MirroredPushPayload {
            system_id: "sys-1".into(),
            hostname: hostname.into(),
            os_name: "Ubuntu".into(),
            kernel: "6.6.0".into(),
            cpu_percent: 11.0,
            cpu_cores: 4,
            cpu_model: "Generic".into(),
            memory_percent: 22.0,
            memory_used_display: "1 GB".into(),
            memory_total_display: "4 GB".into(),
            memory_used_bytes: 1_000,
            memory_total_bytes: 4_000,
            swap_percent: 33.0,
            load_one: 0.1,
            load_five: 0.2,
            load_fifteen: 0.3,
            uptime_seconds: 100,
            uptime_display: "1m".into(),
            disks: vec![
                MirroredDisk {
                    mount_point: "/".into(),
                    usage_percent: 50.0,
                    total_display: "10G".into(),
                    used_display: "5G".into(),
                },
                MirroredDisk {
                    mount_point: "/home".into(),
                    usage_percent: 70.0,
                    total_display: "100G".into(),
                    used_display: "70G".into(),
                },
            ],
            top_processes: vec![],
            timestamp: 1_700_000_000,
        }
    }

    #[test]
    fn push_payload_decodes_from_messagepack_wire_format() {
        let mirrored = sample_frame("host1");
        let packed = rmp_serde::to_vec(&mirrored).unwrap();
        let payload: PushPayload = rmp_serde::from_slice(&packed).unwrap();
        assert_eq!(payload.system_id, "sys-1");
        assert_eq!(payload.cpu_cores, 4);
        assert_eq!(payload.disks[0].mount_point, "/");
    }

    #[tokio::test]
    async fn push_upgrade_request_without_headers_is_rejected() {
        let (state, _dir) = temp_state();
        let res = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/push")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn push_upgrade_request_with_headers_but_no_real_connection_is_not_upgradable() {
        // `oneshot()` doesn't drive a real hyper connection, so there's no `OnUpgrade`
        // extension available even with a fully valid handshake -- axum reports 426.
        // The full auth handshake is covered by `push_connects_authenticates_and_registers_system`
        // below, which runs a real server.
        let (state, _dir) = temp_state();
        let res = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/push")
                    .header("Connection", "Upgrade")
                    .header("Upgrade", "websocket")
                    .header("Sec-WebSocket-Version", "13")
                    .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UPGRADE_REQUIRED);
    }

    /// Starts the push router on an ephemeral port with the given `HUB_PUSH_TOKEN`
    /// value injected, so no test has to mutate the process environment.
    async fn serve_push(state: Arc<AppState>, expected_token: &str) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router_with_token(state, PushToken::new(expected_token.to_string()));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        addr
    }

    type AgentSocket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    /// Connects as an agent and sends the auth handshake. Returns the open socket and the
    /// hub's answer, or `None` if the hub closed the connection without answering.
    async fn connect_and_auth(
        addr: std::net::SocketAddr,
        system_id: &str,
        token: &str,
    ) -> (AgentSocket, Option<serde_json::Value>) {
        let auth_msg = serde_json::json!({
            "type": "auth",
            "system_id": system_id,
            "token": token,
        });
        connect_and_send(addr, auth_msg.to_string()).await
    }

    /// Connects and sends `first_message` as the handshake text frame.
    async fn connect_and_send(
        addr: std::net::SocketAddr,
        first_message: String,
    ) -> (AgentSocket, Option<serde_json::Value>) {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        let url = format!("ws://{addr}/api/push");
        let (mut ws_stream, resp) = tokio_tungstenite::connect_async(url).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);

        ws_stream
            .send(WsMessage::Text(first_message))
            .await
            .unwrap();

        let answer = tokio::time::timeout(std::time::Duration::from_secs(5), ws_stream.next())
            .await
            .expect("timed out waiting for auth response");
        let answer = match answer {
            Some(Ok(WsMessage::Text(txt))) => Some(serde_json::from_str(&txt).unwrap()),
            _ => None,
        };
        (ws_stream, answer)
    }

    #[tokio::test]
    async fn push_handshake_without_configured_token_accepts_any_token_and_registers_system() {
        let (state, _dir) = temp_state();
        let addr = serve_push(state.clone(), "").await;

        let (_ws, answer) = connect_and_auth(addr, "test-sys-123", "").await;

        assert_eq!(answer.unwrap()["type"], "auth_ok");
        assert!(state.db.get_system("test-sys-123").unwrap().is_some());
    }

    #[tokio::test]
    async fn push_handshake_with_the_configured_token_is_accepted_and_registers_system() {
        let (state, _dir) = temp_state();
        let addr = serve_push(state.clone(), "expected-token").await;

        let (_ws, answer) = connect_and_auth(addr, "test-sys-789", "expected-token").await;

        assert_eq!(answer, Some(serde_json::json!({"type": "auth_ok"})));
        let sys = state.db.get_system("test-sys-789").unwrap().unwrap();
        assert_eq!(sys.name, "test-sys");
        assert_eq!(sys.url, "push://");
        assert_eq!(sys.status, SystemStatus::Online);
        assert!(sys.enabled);
        assert_eq!(sys.poll_interval_secs, 10);
        assert_eq!(sys.token, "", "the presented push token is not stored");
        assert_eq!(sys.last_seen, "");
        assert_eq!(sys.last_error, None);
    }

    #[tokio::test]
    async fn push_handshake_that_is_not_an_auth_message_is_rejected() {
        let (state, _dir) = temp_state();
        let addr = serve_push(state.clone(), "expected-token").await;
        let hello = r#"{"type":"hello","system_id":"test-sys-000","token":"wrong"}"#;

        let (_ws, answer) = connect_and_send(addr, hello.to_string()).await;

        assert_eq!(
            answer,
            Some(serde_json::json!({"type": "auth_error", "message": "expected auth message"}))
        );
        assert!(state.db.get_system("test-sys-000").unwrap().is_none());
    }

    #[tokio::test]
    async fn push_handshake_with_wrong_token_is_rejected() {
        let (state, _dir) = temp_state();
        let addr = serve_push(state.clone(), "expected-token").await;

        let (_ws, answer) = connect_and_auth(addr, "test-sys-456", "wrong-token").await;

        assert_eq!(
            answer,
            Some(serde_json::json!({"type": "auth_error", "message": "invalid token"}))
        );
        assert!(state.db.get_system("test-sys-456").unwrap().is_none());
    }

    #[test]
    fn push_token_from_env_is_enabled_only_by_a_non_empty_unicode_value() {
        use std::env::VarError;
        let cases = [
            (
                "set value is the push token",
                Ok("s3cret".to_string()),
                Some("s3cret"),
            ),
            (
                "non-ascii unicode value is the push token",
                Ok("pässwörd".to_string()),
                Some("pässwörd"),
            ),
            // Taken verbatim, as before RFC 0003: surrounding whitespace is part of the
            // secret, not trimmed.
            (
                "value with surrounding whitespace is the push token verbatim",
                Ok(" s3cret ".to_string()),
                Some(" s3cret "),
            ),
            ("empty value disables push auth", Ok(String::new()), None),
            (
                "unset variable disables push auth",
                Err(VarError::NotPresent),
                None,
            ),
            // Pins the behaviour kept from before RFC 0003 (fail open), recorded there
            // as an open question rather than endorsed.
            (
                "non-unicode value disables push auth",
                Err(VarError::NotUnicode("s3cret".into())),
                None,
            ),
        ];
        for (name, value, expected) in cases {
            let token = PushToken::from_env(value).map(|token| token.0);
            assert_eq!(token.as_deref(), expected, "{name}");
        }
    }

    #[test]
    fn handshake_parses_into_a_system_id_or_a_typed_rejection() {
        use HandshakeRejection::*;
        let configured = PushToken::new("expected-token".to_string());
        let configured = configured.as_ref();
        let cases = [
            (
                "matching token",
                r#"{"type":"auth","system_id":"sys-1","token":"expected-token"}"#,
                configured,
                Ok("sys-1"),
            ),
            (
                "no token configured accepts any token",
                r#"{"type":"auth","system_id":"sys-1","token":"whatever"}"#,
                None,
                Ok("sys-1"),
            ),
            (
                "no token configured accepts a missing token",
                r#"{"type":"auth","system_id":"sys-1"}"#,
                None,
                Ok("sys-1"),
            ),
            (
                "system id shorter than 8 bytes",
                r#"{"type":"auth","system_id":"pi","token":"expected-token"}"#,
                configured,
                Ok("pi"),
            ),
            (
                "wrong token of equal length",
                r#"{"type":"auth","system_id":"sys-1","token":"expected-tokem"}"#,
                configured,
                Err(InvalidToken),
            ),
            (
                "presented token is a prefix of the expected one",
                r#"{"type":"auth","system_id":"sys-1","token":"expected"}"#,
                configured,
                Err(InvalidToken),
            ),
            (
                "expected token is a prefix of the presented one",
                r#"{"type":"auth","system_id":"sys-1","token":"expected-token-and-more"}"#,
                configured,
                Err(InvalidToken),
            ),
            (
                "missing token when one is configured",
                r#"{"type":"auth","system_id":"sys-1"}"#,
                configured,
                Err(InvalidToken),
            ),
            (
                "empty system id",
                r#"{"type":"auth","system_id":"","token":"expected-token"}"#,
                configured,
                Err(InvalidSystemId(SystemIdError::Empty)),
            ),
            (
                "empty system id with a wrong token is a token failure",
                r#"{"type":"auth","system_id":"","token":"wrong"}"#,
                configured,
                Err(InvalidToken),
            ),
            (
                "message type other than auth",
                r#"{"type":"hello","system_id":"sys-1","token":"expected-token"}"#,
                configured,
                Err(NotAnAuthMessage),
            ),
            (
                "message type other than auth with a wrong token is a shape failure",
                r#"{"type":"hello","system_id":"sys-1","token":"wrong"}"#,
                configured,
                Err(NotAnAuthMessage),
            ),
            (
                "missing system id",
                r#"{"type":"auth","token":"expected-token"}"#,
                configured,
                Err(NotAnAuthMessage),
            ),
            (
                "missing system id with a wrong token is a shape failure",
                r#"{"type":"auth","token":"wrong"}"#,
                configured,
                Err(NotAnAuthMessage),
            ),
            (
                "malformed json",
                "not json",
                configured,
                Err(NotAnAuthMessage),
            ),
        ];
        for (name, frame, push_token, expected) in cases {
            let parsed = authenticate(frame, push_token).map(|id| id.as_str().to_string());
            assert_eq!(parsed, expected.map(str::to_string), "{name}");
        }
    }

    #[test]
    fn handshake_refuses_a_system_id_that_is_not_one_url_path_segment() {
        use HandshakeRejection::*;
        let configured = PushToken::new("expected-token".to_string());
        let configured = configured.as_ref();
        let too_long = "a".repeat(256);
        let cases = [
            (
                "single dot",
                ".",
                "expected-token",
                Err(InvalidSystemId(SystemIdError::DotSegment)),
            ),
            (
                "double dot",
                "..",
                "expected-token",
                Err(InvalidSystemId(SystemIdError::DotSegment)),
            ),
            (
                "over-long id",
                too_long.as_str(),
                "expected-token",
                Err(InvalidSystemId(SystemIdError::TooLong)),
            ),
            // The token is checked first, so a bad id can't be probed without it.
            (
                "single dot with a wrong token",
                ".",
                "wrong",
                Err(InvalidToken),
            ),
            (
                "over-long id with a wrong token",
                too_long.as_str(),
                "wrong",
                Err(InvalidToken),
            ),
            ("dotted hostname", "a.b", "expected-token", Ok("a.b")),
        ];
        for (name, system_id, token, expected) in cases {
            let frame = serde_json::json!({"type": "auth", "system_id": system_id, "token": token});
            let parsed =
                authenticate(&frame.to_string(), configured).map(|id| id.as_str().to_string());
            assert_eq!(parsed, expected.map(str::to_string), "{name}");
        }
    }

    #[test]
    fn handshake_rejections_keep_their_wire_messages() {
        let cases = [
            (
                HandshakeRejection::NotAnAuthMessage,
                "expected auth message",
            ),
            (HandshakeRejection::InvalidToken, "invalid token"),
            (
                HandshakeRejection::InvalidSystemId(SystemIdError::Empty),
                "invalid system_id",
            ),
            (
                HandshakeRejection::InvalidSystemId(SystemIdError::TooLong),
                "invalid system_id",
            ),
            (
                HandshakeRejection::InvalidSystemId(SystemIdError::DotSegment),
                "invalid system_id",
            ),
        ];
        for (rejection, expected) in cases {
            assert_eq!(rejection.message(), expected, "{rejection:?}");
        }
    }

    #[tokio::test]
    async fn push_handshake_with_an_invalid_system_id_is_rejected_and_not_registered() {
        let too_long = "a".repeat(256);
        let cases = [
            ("empty id", ""),
            ("single dot", "."),
            ("double dot", ".."),
            ("over-long id", too_long.as_str()),
        ];
        for (name, system_id) in cases {
            let (state, _dir) = temp_state();
            let addr = serve_push(state.clone(), "").await;

            let (_ws, answer) = connect_and_auth(addr, system_id, "").await;

            assert_eq!(
                answer,
                Some(serde_json::json!({"type": "auth_error", "message": "invalid system_id"})),
                "{name}"
            );
            assert!(state.db.get_system(system_id).unwrap().is_none(), "{name}");
        }
    }

    /// Polls `check` until it holds or 5 seconds pass; the hub processes frames on its
    /// own task, so the test can't know exactly when an effect lands.
    async fn eventually(mut check: impl FnMut() -> bool) -> bool {
        for _ in 0..500 {
            if check() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        false
    }

    /// Pings the hub and waits for its pong. The hub handles one message at a time, so the
    /// pong arrives only after every earlier frame has been fully ingested.
    async fn wait_until_hub_caught_up(ws: &mut AgentSocket) {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        ws.send(WsMessage::Ping(b"sync".to_vec())).await.unwrap();
        let pong = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(Ok(msg)) = ws.next().await {
                if matches!(msg, WsMessage::Pong(_)) {
                    return true;
                }
            }
            false
        })
        .await;
        assert_eq!(
            pong,
            Ok(true),
            "hub answered the ping after ingesting earlier frames"
        );
    }

    /// Sends one push frame per snapshot, then closes the connection and waits until the
    /// hub has marked the system offline. The offline marking runs after every frame has
    /// been ingested, so once it lands every frame's effects are visible.
    async fn push_frames_then_disconnect(
        state: &AppState,
        mut ws: AgentSocket,
        system_id: &str,
        frames: &[MirroredPushPayload],
    ) {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        for frame in frames {
            let bytes = rmp_serde::to_vec(frame).unwrap();
            ws.send(WsMessage::Binary(bytes)).await.unwrap();
        }
        ws.close(None).await.unwrap();
        let offline = eventually(|| {
            state.db.get_system(system_id).unwrap().unwrap().status == SystemStatus::Offline
        })
        .await;
        assert!(
            offline,
            "system is marked offline when the push connection closes"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            state.db.get_system(system_id).unwrap().unwrap().status,
            SystemStatus::Offline,
            "no frame ingestion lands after the offline marking"
        );
    }

    #[tokio::test]
    async fn push_frame_stores_metrics_caches_live_metrics_and_fills_system_info() {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        let system_id = "0123456789abcdef";
        let (state, _dir) = temp_state();
        let addr = serve_push(state.clone(), "").await;
        let (mut ws, answer) = connect_and_auth(addr, system_id, "").await;
        assert_eq!(answer.unwrap()["type"], "auth_ok");
        // The handshake registers the system Online; start from Offline so only the frame
        // can bring it back.
        state
            .db
            .update_system_status(system_id, &SystemStatus::Offline, "", Some("preset"))
            .unwrap();

        let frame = rmp_serde::to_vec(&sample_frame("host1")).unwrap();
        ws.send(WsMessage::Binary(frame)).await.unwrap();
        wait_until_hub_caught_up(&mut ws).await;

        let sys = state.db.get_system(system_id).unwrap().unwrap();
        assert_eq!(sys.name, "host1");
        assert_eq!(sys.status, SystemStatus::Online);
        // Pins current behaviour: the frame's uptime display is stored as `last_seen`.
        assert_eq!(sys.last_seen, "1m");
        assert_eq!(sys.last_error, None);
        assert_eq!(sys.os.as_deref(), Some("Ubuntu"));
        assert_eq!(sys.hostname.as_deref(), Some("host1"));
        assert_eq!(sys.kernel.as_deref(), Some("6.6.0"));
        assert_eq!(sys.cpu_model.as_deref(), Some("Generic"));
        assert_eq!(sys.cpu_cores, Some(4));
        assert_eq!(sys.total_memory_display.as_deref(), Some("4 GB"));
        assert_eq!(sys.total_memory_bytes, Some(4_000));

        let stored = [
            ("cpu", 11.0),
            ("memory", 22.0),
            ("swap", 33.0),
            ("load1", 0.1),
            ("load5", 0.2),
            ("disk:/", 50.0),
            ("disk:/home", 70.0),
        ];
        for (metric, value) in stored {
            let points = state.db.get_metrics(system_id, metric, 10, None).unwrap();
            assert_eq!(points.len(), 1, "{metric}: one point per frame");
            assert_eq!(points[0].value, value, "{metric}: value");
            assert_eq!(
                points[0].timestamp, 1_700_000_000,
                "{metric}: frame timestamp"
            );
        }

        let live = state
            .live_metrics
            .read()
            .unwrap()
            .get(system_id)
            .cloned()
            .unwrap();
        assert_eq!(live.cpu_percent, 11.0);
        assert_eq!(live.memory_percent, 22.0);
        assert_eq!(live.load_one, 0.1);
        assert_eq!(
            live.disks,
            vec![("/".to_string(), 50.0), ("/home".to_string(), 70.0)]
        );
        assert_eq!(live.updated_at, 1_700_000_000);
        let cached_name = |state: &AppState| {
            let cache = state.systems_cache.read().unwrap();
            let cached = cache.iter().find(|s| s.id == system_id).cloned().unwrap();
            (cached.name, cached.status)
        };
        assert_eq!(
            cached_name(&state),
            ("host1".to_string(), SystemStatus::Online),
            "systems cache is refreshed after the frame"
        );

        push_frames_then_disconnect(&state, ws, system_id, &[]).await;
        let sys = state.db.get_system(system_id).unwrap().unwrap();
        assert_eq!(sys.last_error.as_deref(), Some("push disconnected"));
        // Pins current behaviour: going offline overwrites `last_seen` with "".
        assert_eq!(sys.last_seen, "");
        assert_eq!(
            cached_name(&state),
            ("host1".to_string(), SystemStatus::Offline),
            "systems cache is refreshed after the offline marking"
        );
    }

    #[tokio::test]
    async fn push_frame_renames_only_a_system_still_carrying_its_default_name() {
        let long_id = "0123456789abcdef";
        let cases = [
            (
                "default-named system takes the snapshot's hostname",
                long_id,
                None,
                "host1",
            ),
            (
                "system renamed by an operator keeps its name",
                long_id,
                Some("my-box"),
                "my-box",
            ),
            (
                "operator name that is a shorter prefix of the id is kept",
                long_id,
                Some("0123"),
                "0123",
            ),
            (
                "operator name equal to the full id is kept",
                long_id,
                Some(long_id),
                long_id,
            ),
            (
                "operator-renamed system with an id shorter than 8 bytes keeps its name",
                "pi",
                Some("my-box"),
                "my-box",
            ),
            (
                "operator-renamed system with a multi-byte char across byte 8 keeps its name",
                "aéééé",
                Some("my-box"),
                "my-box",
            ),
        ];
        for (name, system_id, preset_name, expected_name) in cases {
            let (state, _dir) = temp_state();
            let addr = serve_push(state.clone(), "").await;
            let (ws, _) = connect_and_auth(addr, system_id, "").await;
            if let Some(preset) = preset_name {
                state
                    .db
                    .update_system_config(system_id, Some(preset), None, None, None, None)
                    .unwrap();
            }

            push_frames_then_disconnect(&state, ws, system_id, &[sample_frame("host1")]).await;

            let sys = state.db.get_system(system_id).unwrap().unwrap();
            assert_eq!(sys.name, expected_name, "{name}");
        }
    }

    /// Pins current behaviour: a frame writes system info only while `hostname` or `os`
    /// is still missing, and then overwrites every field with the snapshot's values; once
    /// both are known, later snapshots don't update it.
    #[tokio::test]
    async fn push_frame_fills_system_info_only_while_hostname_or_os_is_missing() {
        let system_id = "0123456789abcdef";
        let cases = [
            (
                "unknown system info is filled",
                None,
                None,
                ("host1", "Ubuntu"),
            ),
            (
                "known hostname with missing os is refilled",
                Some("preset-host"),
                None,
                ("host1", "Ubuntu"),
            ),
            (
                "known os with missing hostname is refilled",
                None,
                Some("Preset OS"),
                ("host1", "Ubuntu"),
            ),
            (
                "fully known system info is kept",
                Some("preset-host"),
                Some("Preset OS"),
                ("preset-host", "Preset OS"),
            ),
        ];
        for (name, preset_hostname, preset_os, (hostname, os)) in cases {
            let (state, _dir) = temp_state();
            let addr = serve_push(state.clone(), "").await;
            let (ws, _) = connect_and_auth(addr, system_id, "").await;
            state
                .db
                .update_system_info(
                    system_id,
                    preset_os,
                    preset_hostname,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap();

            push_frames_then_disconnect(&state, ws, system_id, &[sample_frame("host1")]).await;

            let sys = state.db.get_system(system_id).unwrap().unwrap();
            assert_eq!(sys.hostname.as_deref(), Some(hostname), "{name}: hostname");
            assert_eq!(sys.os.as_deref(), Some(os), "{name}: os");
        }
    }

    #[tokio::test]
    async fn push_frames_after_the_first_keep_its_system_info_and_name() {
        let system_id = "0123456789abcdef";
        let (state, _dir) = temp_state();
        let addr = serve_push(state.clone(), "").await;
        let (ws, _) = connect_and_auth(addr, system_id, "").await;
        let mut later = sample_frame("host2");
        later.os_name = "Debian".into();

        push_frames_then_disconnect(&state, ws, system_id, &[sample_frame("host1"), later]).await;

        let sys = state.db.get_system(system_id).unwrap().unwrap();
        assert_eq!(sys.hostname.as_deref(), Some("host1"));
        assert_eq!(sys.os.as_deref(), Some("Ubuntu"));
        assert_eq!(sys.name, "host1");
        let cpu = state.db.get_metrics(system_id, "cpu", 10, None).unwrap();
        assert_eq!(cpu.len(), 2, "both frames were ingested");
    }

    #[tokio::test]
    async fn push_agent_is_registered_under_its_default_name_and_renamed_to_its_hostname() {
        let cases = [
            ("id longer than 8 bytes", "0123456789abcdef", "01234567"),
            ("id shorter than 8 bytes", "pi", "pi"),
            ("multi-byte char across byte 8", "aéééé", "aééé"),
        ];
        for (name, system_id, default_name) in cases {
            let (state, _dir) = temp_state();
            let addr = serve_push(state.clone(), "").await;

            let (ws, answer) = connect_and_auth(addr, system_id, "").await;

            assert_eq!(
                answer.map(|a| a["type"].clone()),
                Some(serde_json::json!("auth_ok")),
                "{name}: hub answers the handshake instead of dropping the connection"
            );
            let registered = state.db.get_system(system_id).unwrap().unwrap();
            assert_eq!(registered.name, default_name, "{name}: default name");

            push_frames_then_disconnect(&state, ws, system_id, &[sample_frame("host1")]).await;
            let sys = state.db.get_system(system_id).unwrap().unwrap();
            assert_eq!(
                sys.name, "host1",
                "{name}: renamed to the snapshot's hostname"
            );
        }
    }
}
