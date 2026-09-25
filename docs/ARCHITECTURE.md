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
  model, default rules for CPU/memory/disk warning and critical. Each uninterrupted breach is
  one alert incident with a stable incident id (agent run + sequence), so the hub can store
  one record per incident.
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
- Persists time-series metrics (`metrics` table) and alert history, one record per alert
  incident (`alerts` table) to SQLite, with per-system per-metric retention (`metric_retention` table, default
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

## Domain model

This section is the context map and glossary that `CLAUDE.md`'s Domain-Driven Design rules
refer to. It describes where each module sits today, including where the code doesn't yet
match those rules (listed under Open architectural questions below).

### Bounded contexts

| Context | Crate | Domain core | Adapters (I/O) | Owns |
|---|---|---|---|---|
| **Host Telemetry** | agent | `models.rs`, `state.rs` (`MetricsHistory` ring buffer; `AppState` is wiring, and `AppState::new` mints the agent run) | `collectors/*` (sysinfo; `dpkg`/`rpm`/`pacman`/`apk`, `systemctl`, `docker`, `ss` shell-outs) | the snapshot of one host and its recent history |
| **Alerting** | agent | `alerts.rs` (`AlertRule`, `AlertMetric`, `AlertOperator`, `AlertSeverity`, `AgentRun`, `IncidentId`, the per-rule `Breach` state, `AlertManager::evaluate` and `replace_rules`) | `routes/api.rs` alert endpoints, `routes/sse.rs` and `routes/ws.rs` alert streams | deciding when a metric breaches a rule, for how long, and cooldown; the identity of each alert incident |
| **Agent Access** | agent | — | `auth.rs`, `routes/*` | who may read the agent's API |
| **Telemetry Publishing** | agent | — | `push.rs` (WS client, agent-side `PushPayload`) | sending snapshots to a hub |
| **Fleet Registry** | hub | `models.rs` (`SystemInfo`, `SystemStatus`, `SystemId` and the default-name rule) | `db.rs` `systems` table, `routes/api.rs` system CRUD | which systems exist, their config and last-known status |
| **Ingestion** | hub | — | `collector.rs` (HTTP poll), `push.rs` (WS receiver: handshake parsing via `authenticate` / `PushToken` / `HandshakeRejection`, hub-side `PushPayload`) | turning agent output into hub metrics, alerts and status |
| **Fleet History** | hub | `models.rs` (`MetricSnapshot`, `AlertRecord`, `HubSummary`) | `db.rs` `metrics` / `alerts` / `metric_retention` tables, `state.rs` live cache, `routes/*` | stored time series, alert history, retention |

**Published contracts between contexts** (both sides must change together, and a
mixed-version fleet must keep working):

- *Push handshake*: Telemetry Publishing → Ingestion. JSON text messages: the agent sends
  `{"type":"auth",…}` (hub-side `AuthMessage`), and the hub answers `auth_ok`/`auth_error`
  (agent-side `HubMessage`).
- *Push frame*: Telemetry Publishing → Ingestion. A binary MessagePack `PushPayload`,
  declared independently in `src/push.rs` and `system-hub/src/push.rs`. The agent encodes
  it with `rmp_serde::to_vec`, which is positional: structs become arrays with no field
  names, so field *order* is the contract. The hub silently drops frames that fail to
  decode.
- *Poll responses*: the agent's `/api/system` and `/api/alerts` JSON → Ingestion
  (`collector.rs`). `/api/alerts` is read field by field from untyped `serde_json::Value`.
  Each active alert's `id` is its incident id, the same on every tick of the incident, and
  `fired_at` is the tick the incident became active. The hub treats the id as an opaque
  string.

### Glossary (ubiquitous language)

| Term | Meaning | In code |
|---|---|---|
| **agent** | the `system-agent` process running on one monitored host | `system-agent` crate |
| **hub** | the `system-hub` process aggregating many agents | `system-hub` crate |
| **system** | a monitored host *as the hub knows it*: registry entry, config, status | `SystemInfo`, `systems` table |
| **system id** | the non-empty identifier an agent presents in the push handshake; the hub uses it as the system's primary key | `SystemId` (hub) |
| **default system name** | the name the hub gives a newly pushed system until its first snapshot supplies a hostname: the longest prefix of the system id that is at most 8 bytes and ends on a character boundary | `SystemId::default_name`, `SystemId::is_default_name` |
| **system status** | the hub's view of whether a system is reachable: online / offline / unknown | `SystemStatus` |
| **snapshot** | one point-in-time reading of a host's CPU, memory, swap, disks, network, processes, etc. | `SystemSnapshot` (agent), `MetricSnapshot` (hub), `PushPayload` (wire) |
| **metric point** | one timestamped value of one metric | `MetricPoint` (both crates) |
| **history** | the agent's in-memory ring buffer of recent metric points (3600 per series) | `MetricsHistory` |
| **alert rule** | a metric, an operator, a threshold, a duration and a cooldown | `AlertRule` |
| **agent run** | one lifetime of the agent process, identified by a random UUID minted at startup | `AgentRun` |
| **tick** | one evaluation of every enabled alert rule against one snapshot, every 2 s | `AlertManager::evaluate` |
| **metric readings** | the values of one snapshot that alert rules read on a tick (CPU, memory, swap, disks, load, core count) | `Readings` |
| **alert incident** | one uninterrupted breach of one alert rule. It becomes active on the first tick the breach has lasted the rule's duration, and ends on the first tick the rule no longer breaches, when the rule set is replaced, or when the agent restarts | `Incident`, held by `Breach::Active` |
| **incident id** | identifies one alert incident: `<agent run>-<sequence>`, where the sequence counts the run's incidents from 1 and never rewinds. Every active alert of the incident carries it | `IncidentId` |
| **active alert** | the report, on one tick, of an alert incident that is active | `ActiveAlert` |
| **notification** | an active alert's announcement (the agent's `🚨 ALERT` log line), made when the incident is active and the rule's cooldown since its last notification has elapsed. The cooldown spans incidents | `Report::Notify`, the return value of `evaluate` |
| **severity** | how serious an alert is (info / warning / critical). The hub stores it as a free `String`, defaulting to `"warning"` | `AlertSeverity` (agent), `AlertRecord.severity` (hub) |
| **alert record** | the hub's stored copy of one alert incident, keyed by `<system id>_<incident id>` and inserted with `INSERT OR IGNORE`, so it keeps the values first seen | `AlertRecord`, `alerts` table |
| **retention** | how long the hub keeps metric points per system per metric | `metric_retention` table |
| **poll** | the hub fetching a system's snapshot over HTTP on the system's interval | `collector.rs` |
| **push** | an agent streaming snapshots to the hub over WebSocket + MessagePack | `push.rs` (both crates) |
| **push token** | the shared secret (`HUB_PUSH_TOKEN`) an agent must present in the push handshake when one is configured | `PushToken` (hub) |
| **push handshake** | the JSON text exchange that authenticates a push connection | `AuthMessage` (hub), `HubMessage` (agent) |
| **push frame** | one binary, positional MessagePack snapshot message on the push connection | `PushPayload` (both crates) |

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
  any data frame is accepted. The hub reads `HUB_PUSH_TOKEN` once, when `push::router` is
  built (`PushToken::from_env`). If it is unset, empty or not valid unicode, push auth is
  disabled and startup logs a warning. `push::authenticate` parses the handshake into a
  `SystemId` or a typed `HandshakeRejection`, checking the shape first, then the token
  (in constant time via `subtle::ConstantTimeEq`, as on the agent), then that the id is
  non-empty. So an unauthenticated client can't probe id validation. Each rejection is logged
  at `warn` with its variant, never the token. Neither `PushToken` nor the `AuthMessage` DTO
  implements `Debug`. The id itself is self-asserted: any token holder can push as any
  system id (see Open architectural questions). See
  `rfcs/0003-hub-push-handshake-hardening.md`.
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
- **Hub API → hub dashboard**: every string the dashboard renders is untrusted. Names, URLs,
  OS fields, disk mount points, alert messages and severities come from agent JSON or push
  frames, and system ids are self-asserted in the push handshake, so any agent, anyone who
  can register or re-point a system, and any push-token holder controls them. The dashboard
  therefore builds its DOM with `createElement` + `textContent` and never assembles HTML from
  data. Ids reach click handlers through closures and URLs through `encodeURIComponent`,
  never through inline `onclick` markup. Values used as CSS classes are checked against a
  fixed allowlist: an unknown system status renders as `unknown`, and an unknown severity
  gets no severity class. Numbers are type-checked before formatting or use in styles: a
  card metric or disk percentage that isn't a number renders as `—`, and a core count that
  isn't one is left out. The hub serves no Content-Security-Policy yet, so this rendering
  rule is the only XSS control.

## Storage

SQLite database `system-hub/system-hub.db`, auto-created on first run, migrations applied at
startup (`db.rs`).

| Table | Contents |
|---|---|
| `systems` | Registered agents: id, name, URL, token, status, OS info, poll interval |
| `metrics` | Time-series rows: system_id, metric name (`cpu`, `memory`, `swap`, `load1`, `load5`, `disk:{mount}`), value, timestamp |
| `alerts` | Alert history, one record per alert incident, with acknowledge support |
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
- **`system-hub`'s push receiver** (`push.rs`): the handshake is parsed by the pure
  `authenticate` function, which a table-driven unit test covers without a server. The
  real-server tests inject the push token through `router_with_token` instead of setting
  `HUB_PUSH_TOKEN`, so they run in parallel with no `unsafe` env mutation. After closing the
  socket they wait for the offline marking, which is ordered after every frame's ingestion.
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
- Domain and wire shapes are the same structs: `models.rs` in both crates derives serde on
  the types the rest of the code treats as the domain model, so there is no anti-corruption
  layer between the API/push/SQLite shapes and domain logic.
- `AlertManager::evaluate` takes nine positional arguments (seven metrics, `cpu_cores`,
  `now_secs`) under `#[allow(clippy::too_many_arguments)]`. It packs them into a private
  `Readings` value object at once; retiring the exception only needs `Readings` made public
  and the one production caller (`collectors/mod.rs::background_collector`) changed. It was
  kept because touching that caller obliges fixing its blocking `sysinfo` collection on the
  runtime (RFC 0004).
- Agents older than RFC 0004 still report `ongoing_rule_<index>` ids, which collide across
  incidents, so the hub drops their later incidents until they are upgraded. Hub databases
  may still hold stale `<system>_ongoing_rule_N` rows.
- Neither binary sends a Content-Security-Policy (or any security headers) with its
  dashboard. The hub dashboard's only XSS control is its rendering rule (see Trust
  boundaries). A `script-src 'self'` policy would first need the inline `<script>` moved to
  a file and the static `onclick` attributes replaced by listeners.
- A system id of `.` or `..` escapes its path segment in the dashboard's URLs:
  `encodeURIComponent` leaves dots alone, and the URL parser removes dot segments.
  `SystemId` only rejects the empty id, and push ids are self-asserted. With id `.`, the
  history fetch `/api/systems/./history` lands on `GET /api/systems/history`, which returns
  the record of a system whose id is `history`, and the system's own record and delete go to
  `/api/systems/`. With id `..`, the requests go to `/api/` and `/api/history`, which no
  route serves. So such a system can't be opened or deleted from the dashboard, and a `.`
  system shows another system's data in place of its history. The fix is for `SystemId` to
  reject dot segments.
- The hub's `alerts` table has no retention. With one record per alert incident, a flapping
  rule adds a record for each incident a poll sees.
- An agent alert without an `id` gets a random id on the hub (`collector.rs`), so it becomes a
  new record on every poll.
- An alert rule with no disk reading (a named mount point missing from the snapshot, or no
  disks reported at all) reads `0.0`. A mount that briefly disappears ends a `Gt` incident,
  which returns as a new record; for `Lt`/`Lte` it opens a phantom incident.
- Replacing the alert rule set ends every incident, including those of rules the new set leaves
  unchanged: they come back one duration later under new ids. Per-rule state is keyed by
  position because rules have no ids.
- Most blocking work is not offloaded. The collectors' and push client's
  `std::process::Command` shell-outs (`dpkg-query`, `rpm`, `pacman`, `apk`, `systemctl`,
  `docker`, `ss`, `lsb_release`, `hostname`) run on the async runtime. The hub's synchronous
  `rusqlite` calls hold a `std::sync::Mutex` from async code everywhere except the push
  receiver, which runs registration, frame ingestion and offline marking in
  `spawn_blocking`.
- Retention policy (look up `retention_secs`, fall back to 86400, delete older rows) is
  decided inside `Database::insert_metric`, in the SQL adapter, rather than in the domain.
- The push system id is self-asserted (API1). The hub trusts whatever `system_id` the
  handshake presents, and `GET /api/systems` lists every id without auth. So anyone holding
  the single shared push token, or anyone at all when it is unset, can push as any
  registered system, a polled one included: inject metrics, trigger the rename to hostname,
  or force it offline on disconnect. Fixing this needs per-system push credentials.
- Push auto-registration is unbounded (API4). Every handshake with an unseen system id
  inserts a permanent, enabled `systems` row, and the poller visits every enabled row every
  30 s. There is also no cap on handshake or frame size.
- Push-registered systems (`url: "push://"`) are also polled. reqwest rejects the scheme,
  so the poller marks them offline, and they flap between online and offline.
- A non-UTF-8 `HUB_PUSH_TOKEN` disables push auth (fails open), like an unset one.
- The push handshake waits for the first message with no timeout. A client that upgrades
  and never sends anything holds a connection task open indefinitely (API4).
- The snapshot → metric mapping (metric names `cpu`, `memory`, `swap`, `load1`, `load5`,
  `disk:<mount>`) is written twice: once in `push::metric_points` over the push DTO, and
  once in `collector.rs` over the poll DTO. It belongs in one Fleet History domain function.
- The Fleet Registry rules applied on each push frame are decided inside the Ingestion
  adapter (`push::update_registry`). Those rules are: refill system info while its
  hostname or OS is missing, and mark the system online. Only the default-name rule lives
  in the domain.
- Rejected push handshakes are logged without the peer's address. The hub isn't served with
  connect info, and behind a reverse proxy it would need a forwarded-header policy.
- The agent doesn't treat a missing handshake answer as a failure. Its handshake check has
  no branch for it, so it enters its push loop, which returns `Ok(())` as soon as the
  socket closes, and `main` reconnects without backoff.
- Ingestion reads agent alerts from untyped `serde_json::Value` inside `collector.rs` and
  discards `insert_alert` errors (`let _ =`). Parsing, domain mapping and storage are
  interleaved in one poll function.
