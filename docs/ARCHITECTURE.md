# Architecture

This document describes the system as it currently exists. It is a living document: whenever
a change alters components, data flow, protocols, schema, or trust boundaries, update the
relevant section in place rather than appending a changelog entry. See `CLAUDE.md` for the
policy on when this file must be updated, and `rfcs/` for the design rationale behind past and
proposed changes.

## Components

Two independent Rust binaries live in this repository. They are not a Cargo workspace — each
has its own `Cargo.toml` and `Cargo.lock` and is built/tested separately.

### `system-agent` (repo root, `src/`)

Runs on every monitored Linux host. Responsibilities:

- Collects local system metrics on a background loop (`collectors/`): CPU, memory, disk,
  network, processes (`system.rs`, via `sysinfo`), installed packages (`packages.rs`, shells
  out to `dpkg`/`rpm`/`pacman`/`apk`), systemd units (`services.rs`), Docker containers
  (`containers.rs`), and listening TCP ports (`ports.rs`).
- Keeps a ring-buffer history of key metrics in memory (`state.rs`, 3600 points).
- Evaluates alert rules against live metrics (`alerts.rs`) — threshold + duration + cooldown
  model, default rules for CPU/memory/disk warning and critical.
- Exposes a REST API, SSE streams, and a WebSocket endpoint (`routes/`) for local/direct
  consumption, plus serves a static single-file dashboard (`static/index.html`).
- Optionally authenticates inbound API requests via a shared bearer/API-key/query-param token
  (`auth.rs`, `SYSTEM_AGENT_TOKEN`). Auth is opt-in: if the env var is unset, the API is open.
- Optionally pushes periodic snapshots to a `system-hub` instance over WebSocket, MessagePack-
  encoded (`push.rs`), if `PUSH_TO`/`PUSH_TOKEN`/`PUSH_INTERVAL` are configured.

### `system-hub` (`system-hub/`)

Aggregates data from many `system-agent` instances. Responsibilities:

- Maintains a registry of known systems (`db.rs`, SQLite table `systems`) — name, URL, token,
  poll interval, last-known status/OS info.
- Two ingestion modes, both able to run simultaneously per fleet:
  - **HTTP poll** (`collector.rs`): hub periodically calls each registered agent's
    `/api/system` endpoint on its configured interval. Used when the hub can reach the agent
    (agent behind NAT/firewall from the hub's perspective is *not* this mode).
  - **WebSocket push** (`push.rs`): agents connect out to the hub's `/api/push` endpoint,
    authenticate with a shared token (`HUB_PUSH_TOKEN`), then stream MessagePack-encoded
    snapshots every `PUSH_INTERVAL` seconds. Agents auto-register on first successful push
    handshake — no prior entry in `systems` is required.
- Persists time-series metrics (`metrics` table) and deduplicated alert history (`alerts`
  table) to SQLite, with per-system per-metric retention (`metric_retention` table, default
  24h; old rows pruned automatically).
- Maintains an in-memory live-metrics cache (`state.rs`) for low-latency dashboard updates
  between DB writes.
- Exposes a REST API and an SSE summary stream (`routes/`), and serves a static fleet
  dashboard (`static/index.html`) that updates every 5s via SSE.

## Data flow

```
System Agent (:9090)                         System Hub (:9091)
┌───────────────────────┐                    ┌───────────────────────────┐
│ collectors/ (bg loop)  │                    │ collector.rs (HTTP poll)  │
│   → state.rs (ring     │◄──── GET ──────────│   pulls /api/system on    │
│     buffer history)    │      /api/system    │   each system's interval  │
│   → alerts.rs          │                    │                            │
│                        │                    │ push.rs (WS receiver)     │
│ routes/api.rs  (REST)  │                    │   accepts agent WS conns, │
│ routes/sse.rs  (SSE)   │                    │   auth via HUB_PUSH_TOKEN,│
│ routes/ws.rs   (WS)    │                    │   decodes MessagePack     │
│                        │                    │   frames                  │
│ push.rs (WS client) ───┼──── WS + MsgPack ──►│                            │
│   PUSH_TO/PUSH_TOKEN   │      /api/push      │ db.rs (SQLite)            │
└───────────────────────┘                    │   systems / metrics /     │
                                              │   alerts / metric_retention│
                                              │                            │
                                              │ state.rs (live cache)     │
                                              │ routes/api.rs (REST)      │
                                              │ routes/sse.rs (SSE)       │
                                              └───────────────────────────┘
```

| Mode | Direction | Protocol | Use case |
|---|---|---|---|
| HTTP Poll | Hub → Agent | REST + JSON | Agent reachable from hub; hub controls cadence |
| Push | Agent → Hub | WebSocket + MessagePack | Agent behind NAT/firewall from hub's side; lower latency, smaller payload (~50-70% smaller than equivalent JSON) |

## Trust boundaries & auth

- **Client → Agent**: optional bearer/API-key/query-token auth (`SYSTEM_AGENT_TOKEN`). Health
  check, static files, and the dashboard HTML are always unauthenticated. Auth middleware
  (`auth::require_auth`) compares the presented token to the configured one in constant time via
  `subtle::ConstantTimeEq` (`auth::tokens_match`), closing the timing side-channel a `==`
  comparison would have (see `rfcs/0001-constant-time-token-comparison.md`). Token *length* is
  still observable via timing (the comparison short-circuits on a length mismatch before the
  constant-time byte loop) — only token *content* is protected, which matches standard practice
  for this kind of fixed-secret comparison.
- **Agent → Hub (push)**: agent presents `system_id` + `token` in a JSON handshake frame before
  any data frame is accepted; hub compares against `HUB_PUSH_TOKEN`.
- **Hub → Agent (poll)**: hub sends the per-system token stored in `db.rs` (as configured via
  `POST/PUT /api/systems`) as the agent's expected auth token.
- **Client → Hub**: no auth on the hub's own REST/SSE API in the current implementation — the
  hub is assumed to sit behind a trusted network boundary or reverse proxy (see "Deployment
  patterns" in `README.md`). Adding hub-side client auth would be an architectural change
  requiring an RFC.
- **CORS**: both services currently allow any origin/method/header (`CorsLayer::new()...Any`).
  This is a known misconfiguration (see `CLAUDE.md` security section), not an intentional
  trust decision — don't treat it as load-bearing design.
- **SSRF surface**: the hub polls arbitrary URLs supplied via `POST /api/systems`; there is no
  URL validation today. Anyone who can call that endpoint can make the hub issue HTTP requests
  to arbitrary hosts reachable from the hub.

## Storage

SQLite database `system-hub/system-hub.db`, auto-created on first run, migrations applied at
startup (`db.rs`).

| Table | Contents |
|---|---|
| `systems` | Registered agents: id, name, URL, token, status, OS info, poll interval |
| `metrics` | Time-series rows: system_id, metric name (`cpu`, `memory`, `swap`, `load1`, `load5`, `disk:{mount}`), value, timestamp |
| `alerts` | Deduplicated alert history with acknowledge support |
| `metric_retention` | Per-system, per-metric retention window (default 24h); enforced by periodic pruning |

The `system-agent` has no persistent storage — its metric history is an in-memory ring buffer
(`state.rs`, 3600 points) that resets on restart.

## Testing architecture

Both crates have test coverage colocated with the code under test — `#[cfg(test)] mod tests`
blocks at the bottom of each source file, per standard Rust convention (no separate `tests/`
integration-test directories exist yet; nothing currently needs cross-file fixtures large enough
to warrant one).

- **Pure logic** (alert threshold/duration/cooldown evaluation, `ss`/`dpkg`/`rpm`/`pacman`/`apk`
  output parsing, byte/uptime formatting, ISO-8601 formatting, dynamic-SQL clause building) is
  tested directly as unit tests. Where parsing logic originally lived inline in a
  command-shelling `collect()` function or a handler closure, it was extracted into a standalone
  function first specifically to make it unit-testable without invoking real system commands.
- **HTTP route handlers** (`routes/api.rs` in both crates) are tested by building the crate's
  `Router` and driving requests through it with `tower::ServiceExt::oneshot` — no real socket is
  bound.
- **SSE and WebSocket endpoints** can't be fully exercised through `oneshot` (a WebSocket upgrade
  needs a real hyper connection's `OnUpgrade` extension, which `oneshot` doesn't provide — such
  requests get a `426 Upgrade Required` instead of `101`). Those get real-server integration
  tests instead: `axum::serve` bound to an ephemeral `127.0.0.1:0` port inside the test, driven
  from a real client (`tokio_tungstenite::connect_async` for WS, a direct request for SSE
  headers).
- **`system-hub`'s SQLite layer** (`db.rs`) and its agent-polling logic (`collector.rs`) are
  tested against real (but temporary) SQLite files via the `tempfile` crate — never against the
  real `system-hub.db`. `collector.rs`'s `poll_system` is tested against a small mock
  `system-agent`-shaped `axum::serve` instance covering the success, JSON-parse-error,
  HTTP-error, and connection-refused branches.
- `tower` (`features = ["util"]`, for `ServiceExt::oneshot`), `tempfile`, and `futures-util` (hub
  only, for WS test streams) are the test-only additions beyond what production code already
  depended on.
- No coverage-measurement tool (`cargo llvm-cov`/`tarpaulin`) is installed in this environment;
  `cargo clippy --all-targets --all-features -- -D warnings` and `cargo test` are what currently
  gate a change per `CLAUDE.md`.

## Open architectural questions / known gaps

Tracked here so they aren't rediscovered from scratch; promote any of these to an RFC
(`rfcs/`) before acting on them.

- No TLS in either binary — deployments are expected to terminate TLS at a reverse proxy
  (documented in `README.md`). Confirm this is still the intended posture before changing it.
- No rate limiting or request body size limits on either API.
- No hub-side client authentication (see Trust boundaries above).
- CORS is unconditionally permissive in both crates.
- No URL validation on hub-side system registration (SSRF surface).
