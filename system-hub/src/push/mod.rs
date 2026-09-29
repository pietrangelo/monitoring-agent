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
use std::time::{Duration, Instant};

use crate::application_wire;
use crate::applications::{ScrapeRound, SourcePace};
use crate::clock::unix_now;
use crate::db::RoundStored;
use crate::models::{SystemId, SystemIdError};
use crate::round_intake::{self, Arrival};

mod config;
mod ingest;
use crate::state::AppState;
pub use config::{PushAuth, PushAuthError, PushConfig};
use ingest::{PushPayload, ingest_frame, mark_offline, register_if_new};

/// The push handshake's first message. No `Debug`: `token` holds the presented secret.
#[derive(Deserialize)]
struct AuthMessage {
    #[serde(rename = "type")]
    msg_type: String,
    system_id: String,
    #[serde(default)]
    token: String,
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
fn authenticate(frame: &str, push_auth: &PushAuth) -> Result<SystemId, HandshakeRejection> {
    let message: AuthMessage =
        serde_json::from_str(frame).map_err(|_| HandshakeRejection::NotAnAuthMessage)?;
    if message.msg_type != "auth" {
        return Err(HandshakeRejection::NotAnAuthMessage);
    }
    if !push_auth.admits(&message.token) {
        return Err(HandshakeRejection::InvalidToken);
    }
    SystemId::try_from(message.system_id).map_err(HandshakeRejection::InvalidSystemId)
}

/// What the push handler needs per connection: the shared app state and the push
/// configuration, fixed when the router is built.
#[derive(Clone)]
struct PushContext {
    app: Arc<AppState>,
    config: PushConfig,
}

/// The push router with the production deadlines and limits.
pub fn router(state: Arc<AppState>, auth: PushAuth) -> Router {
    match &auth {
        PushAuth::Open => {
            tracing::warn!("HUB_PUSH_TOKEN is not set: any client may push to /api/push");
        }
        PushAuth::Required(_) => {}
    }
    router_with_config(state, PushConfig::production(auth))
}

fn router_with_config(app: Arc<AppState>, config: PushConfig) -> Router {
    Router::new()
        .route("/api/push", get(push_handler))
        .with_state(PushContext { app, config })
}

async fn push_handler(ws: WebSocketUpgrade, State(ctx): State<PushContext>) -> impl IntoResponse {
    ctx.config
        .limits
        .apply(ws)
        .on_upgrade(move |socket| handle_push(socket, ctx))
}

/// Why a handshake got no `auth_ok`.
#[derive(Debug)]
enum Refusal {
    /// Shape, token or system id; answered with the rejection's own message.
    Rejected(HandshakeRejection),
    /// No first message within the handshake deadline.
    Timeout,
    /// The authenticated id couldn't be checked or registered (RFC 0007 §4). The agent
    /// retries any `auth_error` after 5 s, so a failing registry is never retried at once.
    RegistryUnavailable {
        id: SystemId,
        failure: RegistryFailure,
    },
}

/// Why registering a push id failed.
#[derive(Debug)]
enum RegistryFailure {
    /// The database couldn't check or insert the row.
    Database(rusqlite::Error),
    /// The registration unit panicked; `on_blocking_pool` logged the `JoinError`.
    Panicked,
}

impl Refusal {
    fn message(&self) -> &'static str {
        match self {
            Self::Rejected(rejection) => rejection.message(),
            Self::Timeout => "handshake timeout",
            Self::RegistryUnavailable { .. } => "registry unavailable",
        }
    }

    /// Logs the refusal at `warn`, one line per handshake: a registry failure names the id in
    /// `Debug` form and the error, and no refusal names the token.
    fn log(&self) {
        match self {
            Self::Rejected(_) | Self::Timeout => tracing::warn!("Push handshake refused: {self:?}"),
            Self::RegistryUnavailable {
                id,
                failure: RegistryFailure::Database(err),
            } => tracing::warn!("Push registration of {:?} failed: {err}", id.as_str()),
            Self::RegistryUnavailable {
                id,
                failure: RegistryFailure::Panicked,
            } => tracing::warn!("Push registration of {:?} panicked", id.as_str()),
        }
    }
}

/// How a handshake ended, so `handle_push` has one exit per case.
enum Handshake {
    /// Registered; `answer` says whether `auth_ok` was delivered.
    Authenticated { id: SystemId, answer: Answer },
    /// Answered with an `auth_error`; nothing was registered.
    Refused(Refusal),
    /// The socket ended, errored, sent an oversize message, or sent a first message that
    /// isn't text. No answer.
    Closed,
}

enum Answer {
    Delivered,
    Failed,
}

async fn handle_push(mut socket: WebSocket, ctx: PushContext) {
    tracing::info!("Push client connected");
    let (system_id, answer) = match handshake(&mut socket, &ctx).await {
        Handshake::Authenticated { id, answer } => (id, answer),
        Handshake::Refused(refusal) => return refusal.log(),
        Handshake::Closed => return,
    };
    tracing::info!("Push client authenticated: {:?}", system_id.as_str());

    match answer {
        Answer::Delivered => receive_frames(&mut socket, &ctx, &system_id).await,
        // The client never learned it was accepted; its connection ends here.
        Answer::Failed => tracing::warn!(
            "Push handshake answer to {:?} could not be sent; ending the connection",
            system_id.as_str()
        ),
    }

    let _ = on_blocking_pool(&ctx.app, &system_id, mark_offline).await;
    tracing::info!("Push client disconnected: {:?}", system_id.as_str());
}

/// What one receive under a deadline produced.
enum Received {
    Message(Message),
    /// The peer closed, the socket errored, or the stream ended.
    Ended,
    /// tungstenite refused a message over the size limit.
    Oversize,
    TimedOut,
}

async fn recv_within(socket: &mut WebSocket, deadline: Duration) -> Received {
    match tokio::time::timeout(deadline, socket.recv()).await {
        Err(_elapsed) => Received::TimedOut,
        Ok(Some(Ok(Message::Close(_))) | None) => Received::Ended,
        Ok(Some(Ok(message))) => Received::Message(message),
        Ok(Some(Err(err))) if is_oversize(&err) => Received::Oversize,
        Ok(Some(Err(_))) => Received::Ended,
    }
}

/// Reads the auth message within the handshake deadline, registers the system, and answers.
async fn handshake(socket: &mut WebSocket, ctx: &PushContext) -> Handshake {
    let frame = match recv_within(socket, ctx.config.handshake_timeout).await {
        Received::Message(Message::Text(frame)) => frame,
        Received::TimedOut => return refuse(socket, Refusal::Timeout).await,
        Received::Oversize => {
            // Dropped at once, with no linger: there is no agent here to protect.
            tracing::warn!("Push auth message over the size limit; dropping the connection");
            return Handshake::Closed;
        }
        Received::Ended
        | Received::Message(
            Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Close(_),
        ) => return Handshake::Closed,
    };
    let system_id = match authenticate(&frame, &ctx.config.auth) {
        Ok(system_id) => system_id,
        Err(rejection) => return refuse(socket, Refusal::Rejected(rejection)).await,
    };
    if let Err(failure) = register(ctx, &system_id).await {
        let refusal = Refusal::RegistryUnavailable {
            id: system_id,
            failure,
        };
        return refuse(socket, refusal).await;
    }
    let answer = send_answer(socket, r#"{"type":"auth_ok"}"#.to_string()).await;
    Handshake::Authenticated {
        id: system_id,
        answer,
    }
}

/// Registers the authenticated id off the async runtime.
async fn register(ctx: &PushContext, system_id: &SystemId) -> Result<(), RegistryFailure> {
    on_blocking_pool(&ctx.app, system_id, register_if_new)
        .await
        .map_err(|_| RegistryFailure::Panicked)?
        .map_err(RegistryFailure::Database)
}

async fn refuse(socket: &mut WebSocket, refusal: Refusal) -> Handshake {
    let answer = serde_json::json!({"type": "auth_error", "message": refusal.message()});
    // Whether the answer arrives changes nothing: the connection ends either way.
    let _ = send_answer(socket, answer.to_string()).await;
    Handshake::Refused(refusal)
}

/// Sends a handshake answer under `SEND_TIMEOUT`, so no send is awaited without a bound.
async fn send_answer(socket: &mut WebSocket, answer: String) -> Answer {
    match tokio::time::timeout(config::SEND_TIMEOUT, socket.send(Message::Text(answer))).await {
        Ok(Ok(())) => Answer::Delivered,
        Ok(Err(_)) | Err(_) => Answer::Failed,
    }
}

/// Ingests push frames in arrival order until the connection ends, goes idle past its
/// deadline, sends an oversize message, or ingestion fails. tungstenite answers pings on its
/// own, so the hub never awaits a send here.
async fn receive_frames(socket: &mut WebSocket, ctx: &PushContext, system_id: &SystemId) {
    let mut rounds = ConnectionRounds::new(Instant::now());
    receive_until_end(socket, ctx, system_id, &mut rounds).await;
    if rounds.refusals > 0 {
        tracing::info!(
            "Push connection of {:?} refused {} application frame(s)",
            system_id.as_str(),
            rounds.refusals
        );
    }
}

async fn receive_until_end(
    socket: &mut WebSocket,
    ctx: &PushContext,
    system_id: &SystemId,
    rounds: &mut ConnectionRounds,
) {
    loop {
        match recv_within(socket, ctx.config.idle_timeout).await {
            Received::Message(Message::Binary(data)) => {
                if let Err(stop) = ingest(ctx, system_id, &data, rounds).await {
                    tracing::info!(
                        "Ending the push connection of {:?}: {stop:?}",
                        system_id.as_str()
                    );
                    return;
                }
            }
            Received::Message(
                Message::Text(_) | Message::Ping(_) | Message::Pong(_) | Message::Close(_),
            ) => {}
            Received::Ended => return,
            Received::TimedOut => {
                tracing::info!(
                    "Push client idle past its deadline: {:?}",
                    system_id.as_str()
                );
                return;
            }
            Received::Oversize => {
                tracing::warn!(
                    "Push message over the size limit from {:?}; dropping the connection \
                     after the linger",
                    system_id.as_str()
                );
                // Held unread, so an agent that reconnects with no backoff waits too.
                tokio::time::sleep(ctx.config.oversize_linger).await;
                return;
            }
        }
    }
}

/// What one push connection keeps of the rounds it sends: its own pace, and how many of its
/// application frames were refused.
struct ConnectionRounds {
    pace: SourcePace,
    refusals: u64,
}

impl ConnectionRounds {
    fn new(now: Instant) -> Self {
        Self {
            pace: SourcePace::new(now),
            refusals: 0,
        }
    }

    /// Counts a refused application frame, and says whether it is the connection's first:
    /// logged at `warn`, the rest at `debug`, so a sender can't flood the log.
    fn note_refusal(&mut self) -> RefusedFrame {
        self.refusals += 1;
        if self.refusals == 1 {
            RefusedFrame::First
        } else {
            RefusedFrame::Repeat
        }
    }
}

/// Which of a connection's refused application frames this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefusedFrame {
    First,
    Repeat,
}

/// Why a push connection stops ingesting.
#[derive(Debug)]
enum IngestStop {
    /// A unit of blocking storage work failed (`on_blocking_pool` logs why).
    WorkFailed,
    /// The system's row is gone, so nothing more from this connection can be stored.
    SystemGone,
}

/// Decodes and stores one data frame. Frames that don't decode are dropped, as documented in
/// ARCHITECTURE.md; only a failed unit of storage work is an error.
async fn ingest(
    ctx: &PushContext,
    system_id: &SystemId,
    data: &[u8],
    rounds: &mut ConnectionRounds,
) -> Result<(), IngestStop> {
    if let Ok(payload) = rmp_serde::from_slice::<PushPayload>(data) {
        let work = move |app: &AppState, id: &SystemId| ingest_frame(app, id, &payload);
        return on_blocking_pool(&ctx.app, system_id, work)
            .await
            .map_err(|_| IngestStop::WorkFailed);
    }
    let Some(frame) = application_wire::decode(data) else {
        return Ok(());
    };
    match ScrapeRound::try_from(frame) {
        Ok(round) => ingest_round(ctx, system_id, round, rounds).await,
        Err(refusal) => {
            let id = system_id.as_str();
            match rounds.note_refusal() {
                RefusedFrame::First => {
                    tracing::warn!("Refused an application frame from {id:?}: {refusal:?}")
                }
                RefusedFrame::Repeat => {
                    tracing::debug!("Refused an application frame from {id:?}: {refusal:?}")
                }
            }
            Ok(())
        }
    }
}

/// Admits and stores one scrape round off the async runtime, under this connection's pace.
async fn ingest_round(
    ctx: &PushContext,
    system_id: &SystemId,
    round: ScrapeRound,
    rounds: &mut ConnectionRounds,
) -> Result<(), IngestStop> {
    let pace = rounds.pace;
    let work = move |app: &AppState, id: &SystemId| {
        let arrival = Arrival {
            now: Instant::now(),
            received_at: unix_now(),
        };
        round_intake::store_round(app, id, round, pace, arrival)
    };
    let (stored, pace) = on_blocking_pool(&ctx.app, system_id, work)
        .await
        .map_err(|_| IngestStop::WorkFailed)?;
    rounds.pace = pace;
    match stored {
        Ok(RoundStored::SystemGone) => Err(IngestStop::SystemGone),
        Ok(RoundStored::Stored | RoundStored::Duplicate | RoundStored::TooSoon) => Ok(()),
        Err(err) => {
            tracing::warn!(
                "Storing a scrape round from {:?} failed: {err}",
                system_id.as_str()
            );
            Ok(())
        }
    }
}

/// Whether a receive error is tungstenite refusing a message over the size limit.
fn is_oversize(err: &axum::Error) -> bool {
    use std::error::Error as _;
    use tokio_tungstenite::tungstenite::{Error as WsError, error::CapacityError};
    err.source()
        .and_then(|source| source.downcast_ref::<WsError>())
        .is_some_and(|ws| matches!(ws, WsError::Capacity(CapacityError::MessageTooLong { .. })))
}

/// Runs one unit of synchronous SQLite work off the async runtime and waits for it, so a
/// connection never has more than one unit in flight and units land in order.
async fn on_blocking_pool<T: Send + 'static>(
    app: &Arc<AppState>,
    system_id: &SystemId,
    work: impl FnOnce(&AppState, &SystemId) -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    let (app, id) = (Arc::clone(app), system_id.clone());
    let outcome = tokio::task::spawn_blocking(move || work(&app, &id)).await;
    if let Err(err) = &outcome {
        tracing::error!("Push work for {:?} failed: {err}", system_id.as_str());
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::ingest::tests::{MirroredProcess, MirroredPushPayload, sample_frame};
    use super::*;
    use crate::db::Database;
    use crate::models::SystemStatus;
    use crate::registry::MemoryCapacity;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
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

    #[tokio::test]
    async fn push_upgrade_request_without_headers_is_rejected() {
        let (state, _dir) = temp_state();
        let res = router(state, PushAuth::Open)
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
        let res = router(state, PushAuth::Open)
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

    /// The push auth a test hub runs with: open for `""`, else that token.
    fn auth_for(expected_token: &str) -> PushAuth {
        PushToken::new(expected_token.to_string()).map_or(PushAuth::Open, PushAuth::Required)
    }

    /// Starts the push router on an ephemeral port with the production deadlines and limits
    /// and the given token injected, so no test has to mutate the process environment.
    async fn serve_push(state: Arc<AppState>, expected_token: &str) -> std::net::SocketAddr {
        serve_push_with(state, PushConfig::production(auth_for(expected_token))).await
    }

    /// Starts the push router with an injected configuration.
    async fn serve_push_with(state: Arc<AppState>, config: PushConfig) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        serve_push_on(listener, state, config)
    }

    fn serve_push_on(
        listener: tokio::net::TcpListener,
        state: Arc<AppState>,
        config: PushConfig,
    ) -> std::net::SocketAddr {
        let addr = listener.local_addr().unwrap();
        let app = router_with_config(state, config);
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
    fn handshake_parses_into_a_system_id_or_a_typed_rejection() {
        use HandshakeRejection::*;
        let configured = auth_for("expected-token");
        let configured = &configured;
        let open = &PushAuth::Open;
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
                open,
                Ok("sys-1"),
            ),
            (
                "no token configured accepts a missing token",
                r#"{"type":"auth","system_id":"sys-1"}"#,
                open,
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
        let configured = auth_for("expected-token");
        let configured = &configured;
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

    /// A second connection to the test hub's database file, for planting what no `Database`
    /// method writes.
    fn raw_connection(dir: &tempfile::TempDir) -> rusqlite::Connection {
        rusqlite::Connection::open(dir.path().join("test.db")).unwrap()
    }

    fn registry_unavailable() -> Option<serde_json::Value> {
        Some(serde_json::json!({"type": "auth_error", "message": "registry unavailable"}))
    }

    /// RFC 0007 §4: a registration that fails is answered, so the agent retries after 5 s.
    #[tokio::test]
    async fn a_failed_registration_answers_registry_unavailable_and_a_known_id_still_gets_auth_ok()
    {
        let (state, dir) = temp_state();
        let addr = serve_push(state.clone(), "").await;
        let (_known, answer) = connect_and_auth(addr, "known", "").await;
        assert_eq!(answer.unwrap()["type"], "auth_ok", "registered first");
        raw_connection(&dir)
            .execute_batch(
                "CREATE TRIGGER refuse BEFORE INSERT ON systems
                 BEGIN SELECT RAISE(ABORT, 'refused'); END;",
            )
            .unwrap();

        let (_new, new_answer) = connect_and_auth(addr, "new", "").await;
        let (_again, known_answer) = connect_and_auth(addr, "known", "").await;

        assert_eq!(new_answer, registry_unavailable(), "a new id");
        assert!(
            state.db.get_system("new").unwrap().is_none(),
            "nothing added"
        );
        assert_eq!(known_answer.unwrap()["type"], "auth_ok", "a known id");
    }

    /// RFC 0007 §4: a registration unit that panics (here on a poisoned database mutex) is
    /// answered too, rather than closing the handshake with no answer.
    #[tokio::test]
    async fn a_registration_that_panics_answers_registry_unavailable() {
        let (state, _dir) = temp_state();
        let addr = serve_push(state.clone(), "").await;
        state.db.poison_for_test();

        let (_ws, answer) = connect_and_auth(addr, "sys-poisoned", "").await;

        assert_eq!(answer, registry_unavailable());
    }

    /// RFC 0007 §4: a known push id whose row no read can map keeps its row: its name and its
    /// points. A lookup that fails no longer counts as "not registered".
    #[tokio::test]
    async fn registration_never_replaces_a_known_row() {
        let (state, dir) = temp_state();
        let addr = serve_push(state.clone(), "").await;
        let (_first, answer) = connect_and_auth(addr, "sys-kept", "").await;
        assert_eq!(answer.unwrap()["type"], "auth_ok", "registered first");
        let raw = raw_connection(&dir);
        raw.execute_batch(
            "UPDATE systems SET name = 'web-01', poll_interval_secs = -1 WHERE id = 'sys-kept';
             INSERT INTO metrics (system_id, metric, value, timestamp)
                 VALUES ('sys-kept', 'cpu', 1.0, 100);",
        )
        .unwrap();

        let (_again, answer) = connect_and_auth(addr, "sys-kept", "").await;

        assert_eq!(answer.unwrap()["type"], "auth_ok");
        let row: (String, i64) = raw
            .query_row(
                "SELECT name, (SELECT COUNT(*) FROM metrics WHERE system_id = systems.id)
                 FROM systems WHERE id = 'sys-kept'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, ("web-01".to_string(), 1), "name and points kept");
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

    // ── RFC 0006: connection deadlines, bounded sends, size limits ──────────────────────

    use config::{PushSocketLimits, PushToken};
    use std::time::Duration;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    /// The production configuration with some values overridden.
    fn config_with(change: impl FnOnce(&mut PushConfig)) -> PushConfig {
        let mut config = PushConfig::production(PushAuth::Open);
        change(&mut config);
        config
    }

    /// Small limits that still fit an auth message and a handshake answer.
    fn small_limits() -> PushSocketLimits {
        PushSocketLimits::new(64 * 1024, 1024, 8 * 1024).unwrap()
    }

    fn frame_bytes(hostname: &str) -> Vec<u8> {
        rmp_serde::to_vec(&sample_frame(hostname)).unwrap()
    }

    /// Reads until the hub closes the connection. Returns the messages seen before the close,
    /// or `None` if the connection is still open after `within`.
    async fn messages_until_closed(
        ws: &mut AgentSocket,
        within: Duration,
    ) -> Option<Vec<WsMessage>> {
        use futures_util::StreamExt;
        tokio::time::timeout(within, async {
            let mut seen = Vec::new();
            while let Some(Ok(msg)) = ws.next().await {
                if matches!(msg, WsMessage::Close(_)) {
                    seen.push(msg);
                    // Never answer it: only the hub dropping the socket counts as closed, so a
                    // hub that waits for the peer's Close can't pass.
                    use tokio::io::AsyncReadExt;
                    let tokio_tungstenite::MaybeTlsStream::Plain(tcp) = ws.get_mut() else {
                        panic!("test sockets are plain TCP");
                    };
                    let mut buf = [0u8; 64];
                    while let Ok(read) = tcp.read(&mut buf).await {
                        if read == 0 {
                            break;
                        }
                    }
                    break;
                }
                seen.push(msg);
            }
            seen
        })
        .await
        .ok()
    }

    /// Connects and authenticates as `system_id` on an open hub; panics unless `auth_ok`.
    async fn connect_authenticated(addr: std::net::SocketAddr, system_id: &str) -> AgentSocket {
        let (ws, answer) = connect_and_auth(addr, system_id, "").await;
        assert_eq!(
            answer,
            Some(serde_json::json!({"type": "auth_ok"})),
            "handshake"
        );
        ws
    }

    #[tokio::test]
    async fn a_client_that_sends_nothing_after_the_upgrade_is_answered_handshake_timeout() {
        use futures_util::StreamExt;
        let (state, _dir) = temp_state();
        let config = config_with(|c| c.handshake_timeout = Duration::from_millis(200));
        let addr = serve_push_with(state, config).await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/push"))
            .await
            .unwrap();

        let answer = tokio::time::timeout(Duration::from_secs(1), ws.next()).await;

        // With the late-auth test, whose auth arrives 1.5 s into a 3 s deadline, no fixed
        // deadline passes both.
        assert!(
            answer.is_ok(),
            "no answer within 1 s of a 200 ms handshake deadline"
        );
        let answer = match answer.unwrap() {
            Some(Ok(WsMessage::Text(text))) => {
                serde_json::from_str::<serde_json::Value>(&text).ok()
            }
            _ => None,
        };
        assert_eq!(
            answer,
            Some(serde_json::json!({"type": "auth_error", "message": "handshake timeout"}))
        );
        let closed = messages_until_closed(&mut ws, Duration::from_secs(3)).await;
        assert!(
            closed.is_some(),
            "the connection ends after the timeout answer"
        );
    }

    #[tokio::test]
    async fn an_auth_message_that_arrives_late_but_inside_the_handshake_deadline_is_accepted() {
        use futures_util::{SinkExt, StreamExt};
        let (state, _dir) = temp_state();
        let config = config_with(|c| c.handshake_timeout = Duration::from_secs(3));
        let addr = serve_push_with(state, config).await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/push"))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let auth = serde_json::json!({"type": "auth", "system_id": "sys-late", "token": ""});

        ws.send(WsMessage::Text(auth.to_string())).await.unwrap();

        let answer = match tokio::time::timeout(Duration::from_secs(3), ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                serde_json::from_str::<serde_json::Value>(&text).ok()
            }
            _ => None,
        };
        assert_eq!(answer, Some(serde_json::json!({"type": "auth_ok"})));
    }

    /// The idle deadline wraps each receive, never the whole connection: a live agent that
    /// only pings is never cut off on a schedule. Every deadline is short and the client pings
    /// for longer than the quiet test's 3 s window, so no fixed lifetime, multiplied idle
    /// deadline or connection-wide handshake deadline passes both.
    #[tokio::test]
    async fn an_authenticated_client_that_only_pings_stays_connected_across_idle_deadlines() {
        use futures_util::SinkExt;
        let (state, _dir) = temp_state();
        let config = config_with(|c| {
            c.handshake_timeout = Duration::from_millis(300);
            c.idle_timeout = Duration::from_millis(300);
            c.oversize_linger = Duration::from_millis(300);
        });
        let addr = serve_push_with(state.clone(), config).await;
        let mut ws = connect_authenticated(addr, "sys-pinging").await;

        for _ in 0..35 {
            let sent = ws.send(WsMessage::Ping(Vec::new())).await;
            assert!(
                sent.is_ok(),
                "the hub closed a connection that kept pinging"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        ws.send(WsMessage::Binary(frame_bytes("pinging-host")))
            .await
            .unwrap();

        // Not wait_until_hub_caught_up: the earlier pings' pongs are still unread.
        let stored = eventually(|| {
            state
                .live_metrics
                .read()
                .unwrap()
                .contains_key("sys-pinging")
        })
        .await;
        assert!(
            stored,
            "the connection outlived many idle deadlines and still ingests"
        );
        let sys = state.db.get_system("sys-pinging").unwrap().unwrap();
        assert_eq!(sys.status, SystemStatus::Online);
    }

    /// Every message resets the idle deadline, not only pings: a data frame (even one that
    /// doesn't decode) and a pong keep a connection alive too.
    #[tokio::test]
    async fn an_authenticated_client_that_only_sends_frames_or_pongs_stays_connected() {
        use futures_util::SinkExt;
        let cases = [
            ("data frames", WsMessage::Binary(vec![0])),
            ("pongs", WsMessage::Pong(Vec::new())),
        ];
        for (name, keep_alive) in cases {
            let (state, _dir) = temp_state();
            let config = config_with(|c| {
                c.handshake_timeout = Duration::from_millis(300);
                c.idle_timeout = Duration::from_millis(300);
                c.oversize_linger = Duration::from_millis(300);
            });
            let addr = serve_push_with(state.clone(), config).await;
            let mut ws = connect_authenticated(addr, "sys-alive").await;

            for _ in 0..35 {
                let sent = ws.send(keep_alive.clone()).await;
                assert!(sent.is_ok(), "{name}: the hub closed a live connection");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            ws.send(WsMessage::Binary(frame_bytes("alive-host")))
                .await
                .unwrap();

            let stored =
                eventually(|| state.live_metrics.read().unwrap().contains_key("sys-alive")).await;
            assert!(
                stored,
                "{name}: the connection outlived many idle deadlines"
            );
        }
    }

    #[tokio::test]
    async fn an_authenticated_client_that_goes_quiet_is_disconnected_and_marked_offline() {
        // Windows that don't overlap, so no fixed idle deadline passes both rows.
        let cases = [
            (
                "short idle",
                Duration::from_millis(300),
                Duration::ZERO,
                Duration::from_millis(1200),
            ),
            (
                "long idle",
                Duration::from_millis(2500),
                Duration::from_millis(1800),
                Duration::from_secs(3),
            ),
        ];
        for (name, idle, still_open_for, then_closed_within) in cases {
            let (state, _dir) = temp_state();
            let config = config_with(|c| c.idle_timeout = idle);
            let addr = serve_push_with(state.clone(), config).await;
            let mut ws = connect_authenticated(addr, "sys-quiet").await;

            if !still_open_for.is_zero() {
                let early = messages_until_closed(&mut ws, still_open_for).await;
                assert!(early.is_none(), "{name}: closed before the idle deadline");
            }
            let closed = messages_until_closed(&mut ws, then_closed_within).await;

            assert!(
                closed.is_some(),
                "{name}: still open after the idle deadline"
            );
            let offline = eventually(|| {
                state.db.get_system("sys-quiet").unwrap().unwrap().status == SystemStatus::Offline
            })
            .await;
            assert!(
                offline,
                "{name}: an idle cut marks the system offline like any disconnect"
            );
        }
    }

    /// Guards the production limits: tungstenite panics after the 101 if the write buffer
    /// isn't below its cap, and a frame of about 300 KB, well within the 512 KiB limit, must
    /// be accepted (it would not be under bytes-for-KiB or halved limits).
    #[tokio::test]
    async fn a_client_on_the_production_limits_authenticates_and_has_a_large_frame_stored() {
        use futures_util::SinkExt;
        let (state, _dir) = temp_state();
        let addr = serve_push_with(state.clone(), PushConfig::production(PushAuth::Open)).await;
        let mut ws = connect_authenticated(addr, "sys-production").await;
        // Padded with processes, which the hub doesn't store, so the frame stays cheap.
        let mut frame = sample_frame("production-host");
        frame.top_processes = (0..1000)
            .map(|pid| MirroredProcess {
                pid,
                name: "p".repeat(290),
                cpu_usage: 0.0,
                memory_usage_display: "0 B".into(),
                memory_percent: 0.0,
            })
            .collect();
        let bytes = rmp_serde::to_vec(&frame).unwrap();
        assert!(
            bytes.len() > 256 * 1024 && bytes.len() < 512 * 1024,
            "frame is {} bytes",
            bytes.len()
        );

        ws.send(WsMessage::Binary(bytes)).await.unwrap();

        let stored = eventually(|| {
            state
                .live_metrics
                .read()
                .unwrap()
                .contains_key("sys-production")
        })
        .await;
        assert!(stored, "a 300 KB frame is within the production limits");
    }

    /// A peer that pings but never reads must not stall the hub, and the hub must not buffer
    /// its pongs without limit. tungstenite parks one pong once the write buffer is at its
    /// cap and replaces it with each newer one, so a capped hub returns pongs with a gap; an
    /// uncapped one returns every pong, in order.
    #[tokio::test]
    async fn a_client_that_pings_without_reading_gets_capped_pongs_and_keeps_a_usable_connection() {
        use futures_util::{SinkExt, StreamExt};
        const FLOOD: u64 = 20_000;
        let (state, _dir) = temp_state();
        // Kernel buffers (doubled by Linux) well below the injected 32 KiB cap, and a flood of
        // 10-byte pongs far past 2 x (both buffers) + the cap.
        let listener_socket = tokio::net::TcpSocket::new_v4().unwrap();
        listener_socket.set_send_buffer_size(4096).unwrap();
        listener_socket
            .bind("127.0.0.1:0".parse().unwrap())
            .unwrap();
        let listener = listener_socket.listen(16).unwrap();
        let limits = PushSocketLimits::new(64 * 1024, 1024, 32 * 1024).unwrap();
        let addr = serve_push_on(listener, state.clone(), config_with(|c| c.limits = limits));
        let client_socket = tokio::net::TcpSocket::new_v4().unwrap();
        client_socket.set_recv_buffer_size(4096).unwrap();
        let stream = client_socket.connect(addr).await.unwrap();
        let (mut ws, _) = tokio_tungstenite::client_async(format!("ws://{addr}/api/push"), stream)
            .await
            .unwrap();
        let auth = serde_json::json!({"type": "auth", "system_id": "sys-flood", "token": ""});
        ws.send(WsMessage::Text(auth.to_string())).await.unwrap();
        let answer = ws.next().await;
        assert!(
            matches!(answer, Some(Ok(WsMessage::Text(_)))),
            "handshake: {answer:?}"
        );

        let flooded = tokio::time::timeout(Duration::from_secs(10), async {
            for seq in 0..FLOOD {
                ws.send(WsMessage::Ping(seq.to_be_bytes().to_vec()))
                    .await
                    .unwrap();
            }
        })
        .await;
        assert!(
            flooded.is_ok(),
            "the hub stopped reading while its pongs went unread"
        );

        ws.send(WsMessage::Binary(frame_bytes("flood-host")))
            .await
            .unwrap();
        let stored =
            eventually(|| state.live_metrics.read().unwrap().contains_key("sys-flood")).await;
        assert!(stored, "a frame after the flood is still ingested");

        // Keep pinging while draining, so the hub retries its parked pong, until the pong of a
        // drain ping arrives.
        let mut seqs = Vec::new();
        let drained = tokio::time::timeout(Duration::from_secs(10), async {
            for next in FLOOD.. {
                ws.send(WsMessage::Ping(next.to_be_bytes().to_vec()))
                    .await
                    .unwrap();
                while let Ok(Some(Ok(msg))) =
                    tokio::time::timeout(Duration::from_millis(20), ws.next()).await
                {
                    if let WsMessage::Pong(data) = msg {
                        seqs.push(u64::from_be_bytes(data.try_into().unwrap()));
                    }
                }
                if seqs.iter().any(|&seq| seq >= FLOOD) {
                    break;
                }
            }
        })
        .await;
        assert!(drained.is_ok(), "no drain pong came back");
        let gap = seqs.windows(2).any(|pair| pair[1] > pair[0] + 1);
        assert!(
            gap,
            "all {} pongs came back in order, so the hub buffered them without a cap",
            seqs.len()
        );
        // The in-order run is what the cap and the kernel buffers held: at most the cap plus
        // twice both (doubled) socket buffers, in 10-byte pongs. A larger cap shows up here.
        let in_order = seqs
            .iter()
            .enumerate()
            .take_while(|(i, seq)| **seq == *i as u64)
            .count();
        assert!(
            in_order * 10 <= 32 * 1024 + 2 * (2 * 4096 + 2 * 4096),
            "{in_order} pongs came back in order, more than a 32 KiB cap allows"
        );
    }

    /// Writes a masked binary frame header declaring `len` payload bytes, and no payload.
    async fn send_bare_frame_header(ws: &mut AgentSocket, len: u64) {
        use tokio::io::AsyncWriteExt;
        let tokio_tungstenite::MaybeTlsStream::Plain(tcp) = ws.get_mut() else {
            panic!("test sockets are plain TCP");
        };
        let mut header = vec![0x82, 0x80 | 127];
        header.extend_from_slice(&len.to_be_bytes());
        header.extend_from_slice(&[1, 2, 3, 4]);
        tcp.write_all(&header).await.unwrap();
        tcp.flush().await.unwrap();
    }

    #[tokio::test]
    async fn a_frame_header_over_the_limit_closes_the_connection_before_any_deadline() {
        let (state, _dir) = temp_state();
        let config = config_with(|c| {
            c.limits = small_limits();
            c.oversize_linger = Duration::from_millis(200);
        });
        let addr = serve_push_with(state, config).await;
        let mut ws = connect_authenticated(addr, "sys-header").await;

        send_bare_frame_header(&mut ws, 64 * 1024 + 1).await;

        let closed = messages_until_closed(&mut ws, Duration::from_secs(3)).await;
        assert!(
            closed.is_some(),
            "still open, waiting for a payload the limit forbids"
        );
    }

    #[tokio::test]
    async fn a_message_fragmented_past_the_limit_closes_the_connection_before_any_deadline() {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::protocol::frame::{
            Frame,
            coding::{Data, OpCode},
        };
        let (state, _dir) = temp_state();
        let config = config_with(|c| {
            c.limits = small_limits();
            c.oversize_linger = Duration::from_millis(200);
        });
        let addr = serve_push_with(state, config).await;
        let mut ws = connect_authenticated(addr, "sys-fragments").await;

        // Each fragment is within the 64 KiB limit; together they are over it.
        let first = Frame::message(vec![0; 40 * 1024], OpCode::Data(Data::Binary), false);
        // Not final: only tungstenite's per-fragment accounting can see this message.
        let last = Frame::message(vec![0; 40 * 1024], OpCode::Data(Data::Continue), false);
        ws.send(WsMessage::Frame(first)).await.unwrap();
        ws.send(WsMessage::Frame(last)).await.unwrap();

        let closed = messages_until_closed(&mut ws, Duration::from_secs(3)).await;
        assert!(
            closed.is_some(),
            "still open after a message over the limit"
        );
    }

    #[tokio::test]
    async fn an_oversize_auth_message_is_dropped_at_once_without_an_answer_or_a_registration() {
        use futures_util::SinkExt;
        let (state, _dir) = temp_state();
        // The linger as long as the deadlines: only an immediate drop closes within 3 s.
        let config = config_with(|c| {
            c.limits = small_limits();
            c.oversize_linger = c.handshake_timeout;
        });
        let addr = serve_push_with(state.clone(), config).await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/push"))
            .await
            .unwrap();
        let padded = serde_json::json!({
            "type": "auth",
            "system_id": "sys-padded",
            "token": "",
            "pad": "a".repeat(64 * 1024),
        });

        ws.send(WsMessage::Text(padded.to_string())).await.unwrap();

        let seen = messages_until_closed(&mut ws, Duration::from_secs(3)).await;
        assert!(
            seen.is_some(),
            "an oversize auth message was not dropped at once"
        );
        assert!(
            !seen
                .unwrap()
                .iter()
                .any(|msg| matches!(msg, WsMessage::Text(_) | WsMessage::Close(_))),
            "an oversize client gets no answer and no Close frame"
        );
        assert!(state.db.get_system("sys-padded").unwrap().is_none());
    }

    #[tokio::test]
    async fn an_oversize_frame_after_auth_is_held_unread_for_the_linger_then_closed() {
        use futures_util::SinkExt;
        // Windows that don't overlap, so no fixed wait passes both rows.
        let cases = [
            (
                "short linger",
                Duration::from_millis(400),
                Duration::from_millis(200),
                Duration::from_millis(1200),
            ),
            (
                "long linger",
                Duration::from_millis(2500),
                Duration::from_millis(1800),
                Duration::from_secs(3),
            ),
        ];
        for (name, linger, still_open_for, then_closed_within) in cases {
            let (state, _dir) = temp_state();
            let config = config_with(|c| {
                c.limits = small_limits();
                c.oversize_linger = linger;
            });
            let addr = serve_push_with(state, config).await;
            let mut ws = connect_authenticated(addr, "sys-linger").await;

            ws.send(WsMessage::Binary(vec![0; 64 * 1024 + 1]))
                .await
                .unwrap();

            let early = messages_until_closed(&mut ws, still_open_for).await;
            assert!(early.is_none(), "{name}: closed before the linger ended");
            let closed = messages_until_closed(&mut ws, then_closed_within).await;
            assert!(closed.is_some(), "{name}: still open after the linger");
            assert!(
                !closed
                    .unwrap()
                    .iter()
                    .any(|msg| matches!(msg, WsMessage::Text(_) | WsMessage::Close(_))),
                "{name}: an oversize client gets no answer and no Close frame"
            );
        }
    }

    #[tokio::test]
    async fn a_message_exactly_at_the_limit_is_accepted_and_the_connection_goes_on() {
        use futures_util::SinkExt;
        let (state, _dir) = temp_state();
        let config = config_with(|c| {
            c.limits = small_limits();
            c.oversize_linger = Duration::from_secs(5);
        });
        let addr = serve_push_with(state.clone(), config).await;
        let mut ws = connect_authenticated(addr, "sys-at-limit").await;

        // Not a valid frame, so it is dropped after decoding, but it is within the limit.
        ws.send(WsMessage::Binary(vec![0; 64 * 1024]))
            .await
            .unwrap();
        ws.send(WsMessage::Binary(frame_bytes("at-limit-host")))
            .await
            .unwrap();

        let stored = eventually(|| {
            state
                .live_metrics
                .read()
                .unwrap()
                .contains_key("sys-at-limit")
        })
        .await;
        assert!(stored, "a message of exactly the limit was refused");
    }

    /// Characterisation: when the handshake answer itself fails, the registered system still
    /// ends offline. Another connection holds the database exclusively, so registration
    /// waits until after the client has reset, and the answer is written to a dead socket.
    #[tokio::test]
    async fn a_failed_handshake_answer_still_leaves_its_system_offline() {
        use futures_util::SinkExt;
        let (state, dir) = temp_state();
        let addr = serve_push_with(state.clone(), PushConfig::production(PushAuth::Open)).await;
        let blocker = rusqlite::Connection::open(dir.path().join("test.db")).unwrap();
        blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/push"))
            .await
            .unwrap();
        let auth = serde_json::json!({"type": "auth", "system_id": "sys-reset", "token": ""});
        ws.send(WsMessage::Text(auth.to_string())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        // A zero linger makes the drop a TCP reset, without reading the answer.
        let tokio_tungstenite::MaybeTlsStream::Plain(tcp) = ws.get_ref() else {
            panic!("test sockets are plain TCP");
        };
        tcp.set_zero_linger().unwrap();
        drop(ws);
        tokio::time::sleep(Duration::from_millis(100)).await;
        blocker.execute_batch("COMMIT").unwrap();

        let offline = eventually(|| {
            state
                .db
                .get_system("sys-reset")
                .unwrap()
                .is_some_and(|sys| sys.status == SystemStatus::Offline)
        })
        .await;
        assert!(offline, "registered, answer failed, and marked offline");
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
                .update_system_info(system_id, preset_os, preset_hostname, None, None, None)
                .unwrap();

            push_frames_then_disconnect(&state, ws, system_id, &[sample_frame("host1")]).await;

            let sys = state.db.get_system(system_id).unwrap().unwrap();
            assert_eq!(sys.hostname.as_deref(), Some(hostname), "{name}: hostname");
            assert_eq!(sys.os.as_deref(), Some(os), "{name}: os");
        }
    }

    /// RFC 0014 §8: the memory capacity follows every frame, while the rest of the system's
    /// info keeps its fill-once rule.
    #[tokio::test]
    async fn push_frames_refresh_the_memory_capacity_and_keep_the_systems_info() {
        let system_id = "0123456789abcdef";
        let (state, _dir) = temp_state();
        let addr = serve_push(state.clone(), "").await;
        let (ws, _) = connect_and_auth(addr, system_id, "").await;
        state
            .db
            .update_system_info(
                system_id,
                Some("Preset OS"),
                Some("preset-host"),
                Some("preset-kernel"),
                Some("preset-cpu"),
                Some(64),
            )
            .unwrap();
        state
            .db
            .update_memory_capacity(
                system_id,
                &MemoryCapacity::fixture("31.0 GB", 33_285_996_544),
            )
            .unwrap();
        let mut resized = sample_frame("host1");
        resized.memory_total_display = "512.0 MB".into();
        resized.memory_total_bytes = 536_870_912;
        // A frame whose display is blank carries no capacity: the stored one stays.
        let mut blank = sample_frame("host1");
        blank.memory_total_display = String::new();
        blank.memory_total_bytes = 1;

        push_frames_then_disconnect(
            &state,
            ws,
            system_id,
            &[sample_frame("host1"), resized, blank],
        )
        .await;

        let sys = state.db.get_system(system_id).unwrap().unwrap();
        assert_eq!(
            (sys.total_memory_display.as_deref(), sys.total_memory_bytes),
            (Some("512.0 MB"), Some(536_870_912)),
            "the last whole capacity, not the host's, the first frame's or a blank one"
        );
        assert_eq!(
            (
                sys.os.as_deref(),
                sys.hostname.as_deref(),
                sys.kernel.as_deref(),
                sys.cpu_model.as_deref(),
                sys.cpu_cores,
            ),
            (
                Some("Preset OS"),
                Some("preset-host"),
                Some("preset-kernel"),
                Some("preset-cpu"),
                Some(64)
            ),
            "the rest of the system info keeps its fill-once rule"
        );
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

    mod applications {
        use super::*;
        use crate::applications::RoundId;
        use futures_util::SinkExt;
        use std::collections::BTreeMap;

        const RUN: &str = "6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b";

        type Report = (String, String, Option<String>, BTreeMap<String, f64>);

        /// Mirrors the agent's application frame: positional, like every push frame.
        fn application_frame(kind: &str, seq: u64, heap: f64) -> Vec<u8> {
            let orders: Report = (
                "orders".into(),
                "up".into(),
                Some("2.4.1".into()),
                BTreeMap::from([("heap_used_bytes".into(), heap)]),
            );
            rmp_serde::to_vec(&(kind, RUN, seq, 15u64, vec![orders])).unwrap()
        }

        fn frame(seq: u64) -> Vec<u8> {
            application_frame("applications.v1", seq, 300.0)
        }

        fn up_points(state: &AppState, system: &str) -> Vec<crate::models::MetricPoint> {
            state
                .db
                .get_metrics(system, "app:orders:up", 1000, None)
                .unwrap()
        }

        fn shown_seq(state: &AppState, system: &str) -> Option<u64> {
            let live = state.live_applications.read().unwrap();
            live.get(system)?
                .shown
                .as_ref()
                .map(|held| held.round.id().seq())
        }

        fn now_secs() -> u64 {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        }

        async fn send(ws: &mut AgentSocket, bytes: Vec<u8>) {
            ws.send(WsMessage::Binary(bytes)).await.unwrap();
        }

        #[tokio::test]
        async fn an_application_frame_is_stored_at_hub_time_and_shown_beside_snapshots() {
            let (state, _dir) = temp_state();
            let addr = serve_push(state.clone(), "").await;
            let mut ws = connect_authenticated(addr, "sys-app").await;
            let before = now_secs();
            send(&mut ws, frame_bytes("host")).await; // agent clock: 1_700_000_000
            send(&mut ws, frame(1)).await;
            wait_until_hub_caught_up(&mut ws).await;
            let after = now_secs();

            let points = up_points(&state, "sys-app");
            assert_eq!(points.len(), 1, "one up point");
            assert!(
                // Slack for a host clock that steps; the agent's clock says 1_700_000_000.
                (before.saturating_sub(30)..=after + 30).contains(&points[0].timestamp),
                "stored at the hub's time, not the agent's: {}",
                points[0].timestamp
            );
            assert_eq!(points[0].value, 1.0);
            let heap = state
                .db
                .get_metrics("sys-app", "app:orders:heap_used_bytes", 10, None)
                .unwrap();
            assert_eq!(heap.iter().map(|p| p.value).collect::<Vec<_>>(), [300.0]);
            assert_eq!(shown_seq(&state, "sys-app"), Some(1));
            assert!(
                state.live_metrics.read().unwrap().contains_key("sys-app"),
                "the snapshot on the same connection is stored too"
            );
        }

        #[tokio::test]
        async fn a_reconnect_re_sending_the_same_round_stores_nothing_and_it_stays_shown() {
            let (state, _dir) = temp_state();
            let addr = serve_push(state.clone(), "").await;
            let mut first = connect_authenticated(addr, "sys-app").await;
            send(&mut first, frame(1)).await;
            wait_until_hub_caught_up(&mut first).await;
            drop(first);
            let offline = eventually(|| {
                state.db.get_system("sys-app").unwrap().map(|s| s.status)
                    == Some(SystemStatus::Offline)
            })
            .await;
            assert!(offline, "the first connection ended");
            assert_eq!(
                shown_seq(&state, "sys-app"),
                Some(1),
                "a disconnect evicts nothing"
            );

            let mut second = connect_authenticated(addr, "sys-app").await;
            send(&mut second, frame(1)).await;
            send(&mut second, frame(2)).await;
            wait_until_hub_caught_up(&mut second).await;
            assert_eq!(
                up_points(&state, "sys-app").len(),
                2,
                "rounds 1 and 2, once each"
            );
            assert_eq!(shown_seq(&state, "sys-app"), Some(2));
        }

        #[tokio::test]
        async fn a_repeated_id_with_other_content_or_a_lower_seq_is_still_stored() {
            let (state, _dir) = temp_state();
            let addr = serve_push(state.clone(), "").await;
            // One connection per frame, so pacing can't be what drops one.
            // (case, frame, heap values stored after it)
            let cases: [(&str, Vec<u8>, &[f32]); 4] = [
                (
                    "seq 5",
                    application_frame("applications.v1", 5, 300.0),
                    &[300.0],
                ),
                (
                    "seq 5 again, other content",
                    application_frame("applications.v1", 5, 301.0),
                    &[300.0, 301.0],
                ),
                (
                    "a lower seq after it: no seq ordering",
                    application_frame("applications.v1", 1, 302.0),
                    &[300.0, 301.0, 302.0],
                ),
                (
                    "an exact re-send of seq 5: the only duplicate",
                    application_frame("applications.v1", 5, 300.0),
                    &[300.0, 301.0, 302.0],
                ),
            ];
            for (case, bytes, expected) in cases {
                let mut ws = connect_authenticated(addr, "sys-app").await;
                send(&mut ws, bytes).await;
                wait_until_hub_caught_up(&mut ws).await;
                drop(ws);
                let heap: Vec<f32> = state
                    .db
                    .get_metrics("sys-app", "app:orders:heap_used_bytes", 10, None)
                    .unwrap()
                    .iter()
                    .map(|p| p.value)
                    .collect();
                assert_eq!(heap, expected, "case: {case}");
            }
            assert_eq!(
                shown_seq(&state, "sys-app"),
                Some(1),
                "the last stored round is shown"
            );
        }

        #[tokio::test]
        async fn a_connections_pace_refills_one_round_per_8_seconds() {
            let (state, _dir) = temp_state();
            let addr = serve_push(state.clone(), "").await;
            let mut ws = connect_authenticated(addr, "sys-app").await;
            for seq in 1..=3 {
                send(&mut ws, frame(seq)).await;
            }
            wait_until_hub_caught_up(&mut ws).await;
            assert_eq!(up_points(&state, "sys-app").len(), 2, "the burst is two");
            tokio::time::sleep(Duration::from_millis(8_200)).await;
            send(&mut ws, frame(4)).await;
            wait_until_hub_caught_up(&mut ws).await;
            assert_eq!(
                up_points(&state, "sys-app").len(),
                3,
                "one token refilled after 8 s"
            );
            assert_eq!(shown_seq(&state, "sys-app"), Some(4));
        }

        #[tokio::test]
        async fn a_third_round_within_8_s_is_dropped_but_another_connections_is_not() {
            let (state, _dir) = temp_state();
            let addr = serve_push(state.clone(), "").await;
            let mut hostile = connect_authenticated(addr, "sys-app").await;
            for seq in 1..=3 {
                send(&mut hostile, frame(seq)).await;
            }
            wait_until_hub_caught_up(&mut hostile).await;
            assert_eq!(up_points(&state, "sys-app").len(), 2, "the burst is two");
            assert_eq!(shown_seq(&state, "sys-app"), Some(2));

            let mut honest = connect_authenticated(addr, "sys-app").await;
            send(&mut honest, frame(4)).await;
            wait_until_hub_caught_up(&mut honest).await;
            assert_eq!(up_points(&state, "sys-app").len(), 3, "its own pace");
            assert_eq!(shown_seq(&state, "sys-app"), Some(4));
            drop(hostile);
        }

        #[tokio::test]
        async fn a_refused_application_frame_is_dropped_and_the_connection_goes_on() {
            let (state, _dir) = temp_state();
            let addr = serve_push(state.clone(), "").await;
            let mut ws = connect_authenticated(addr, "sys-app").await;
            send(&mut ws, application_frame("applications.v2", 1, 300.0)).await;
            send(&mut ws, application_frame("applications.v1", 1, f64::NAN)).await;
            wait_until_hub_caught_up(&mut ws).await;
            let points = up_points(&state, "sys-app");
            assert_eq!(points.len(), 1, "only the v1 frame is stored");
            let heap = state
                .db
                .get_metrics("sys-app", "app:orders:heap_used_bytes", 10, None)
                .unwrap();
            assert!(heap.is_empty(), "a NaN gauge is skipped, not stored");
        }

        #[tokio::test]
        async fn a_round_for_a_deleted_system_ends_the_connection_and_leaves_no_entry() {
            let (state, _dir) = temp_state();
            let addr = serve_push(state.clone(), "").await;
            let mut ws = connect_authenticated(addr, "sys-gone").await;
            state.db.delete_system("sys-gone").unwrap();
            send(&mut ws, frame(1)).await;
            let closed = messages_until_closed(&mut ws, Duration::from_secs(5)).await;
            assert!(closed.is_some(), "the hub ends the connection");
            assert!(up_points(&state, "sys-gone").is_empty());
            assert!(
                !state
                    .live_applications
                    .read()
                    .unwrap()
                    .contains_key("sys-gone"),
                "no entry is recreated for a gone system"
            );
        }

        #[test]
        fn a_connections_first_refused_frame_is_first_and_the_rest_repeat() {
            use RefusedFrame::{First, Repeat};
            let mut rounds = ConnectionRounds::new(Instant::now());
            let refused: Vec<_> = (0..4).map(|_| rounds.note_refusal()).collect();
            assert_eq!(refused, [First, Repeat, Repeat, Repeat]);
            assert_eq!(rounds.refusals, 4, "every refusal is counted");
            let mut another = ConnectionRounds::new(Instant::now());
            assert_eq!(another.note_refusal(), First, "per connection");
        }

        #[test]
        fn the_test_run_id_is_a_uuid() {
            assert!(RoundId::parse(RUN, 1).is_ok());
        }
    }
}
