# RFC 0006: Push Receiver Resource Limits and Fail-Closed Configuration

- Status: Draft
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-25
- Affects: `system-hub`
- Depends on: RFC 0005 (`SystemIdError`, and the 255-byte system id limit), which lands first

## Motivation

RFC 0003 hardened the push handshake's comparison and parsing. It deferred several gaps in the
receiver (`system-hub/src/push.rs`), listed in `docs/ARCHITECTURE.md` § Open architectural
questions. Each lets one client make the hub hold resources without limit, or leaves the push
endpoint open when the operator meant to close it. When `HUB_PUSH_TOKEN` is unset, which is
the README's basic start, that client can be anyone.

1. **Connections live forever (API4).** `handshake` awaits the first message with no
   deadline, and `receive_frames` awaits every later one with no deadline. A client that
   upgrades and sends nothing, or authenticates and then goes quiet, keeps a task, its socket
   and its buffers alive until TCP gives up.
2. **Message size isn't limited (API4).** The receiver uses tungstenite 0.24's defaults: a
   64 MiB message and a 16 MiB frame, for the handshake text and for data frames alike.
3. **Per-frame work isn't bounded (API4).** Each disk in a frame becomes one `insert_metric`
   call: an INSERT, a SELECT and a DELETE under the hub's single SQLite mutex. It also becomes
   one entry in `live_metrics`. Even a 1 MiB frame decodes into more than 100,000 minimal disk
   entries. Frames can also arrive back to back, with nothing enforcing a rate.
4. **Auto-registration is unbounded (API4).** Every accepted handshake with an unseen id
   inserts a permanent, enabled `systems` row, which the poller visits every 30 s.
5. **A deleted system keeps ingesting.** Deleting a system removes its rows, but its open push
   connection keeps storing metric rows and refreshing its `live_metrics` entry. Only the
   `systems` row is gone, and `live_metrics` entries are never removed.
6. **A non-UTF-8 `HUB_PUSH_TOKEN` fails open (A05 / API8).** `PushToken::from_env` treats
   `Err(VarError::NotUnicode)` like an unset variable, so the operator sets a token and gets
   none.

## Proposed design

The rules below are domain values with pure constructors and predicates. The adapter reads
the clock, the socket and SQLite, and passes plain values in.

### 1. Deadlines

- **Handshake:** the first message must arrive within `HANDSHAKE_TIMEOUT = 10 s` of the
  upgrade. Otherwise the hub answers `auth_error` / `handshake timeout` and closes the socket.
  The agent sends its auth message as soon as it connects.
- **Idle connection:** after the handshake, any message must arrive within
  `IDLE_TIMEOUT = 90 s` of the previous one, or the hub closes the socket and marks the system
  offline. A data frame, a ping or a pong all count. Every shipped agent pings every 30 s,
  whatever its push interval (`src/push.rs`), so a live agent never comes close to this.
- Both are `tokio::time::timeout` around `socket.recv()` in the adapter.

### 2. Message size

`push_handler` sets both `max_message_size` and `max_frame_size` to
`MAX_PUSH_MESSAGE_BYTES = 1 MiB`. Both are needed:

- The frame limit is checked on the frame header, before the payload is buffered.
- The message limit is checked on the reassembled message.

With only the message limit, the hub would buffer up to 16 MiB before refusing. A current
agent's frame is a few KB: fixed fields, at most 10 processes, and one small entry per disk.

tungstenite answers an oversize message with an error, not a close frame. The hub logs it at
`warn`, in `handshake` and in `receive_frames` alike, and drops the connection. It sends no
`auth_error`, because a client that is sending oversize messages isn't reading answers.

### 3. Frame limits

These rules are checked after decoding and before any SQLite or cache work. A frame that
breaks one is dropped and logged at `warn`. The connection stays open.

- **Disks per frame:** at most `MAX_DISKS_PER_FRAME = 1024`. A frame with more is refused
  whole, not truncated, so no system shows a partial set of disks. sysinfo already leaves out
  tmpfs, `/proc`, `/sys` and `/run`. A host with a few hundred container overlay mounts still
  fits.
- **Frame pacing:** a frame that arrives less than `MIN_FRAME_SPACING = 1 s` after the last
  accepted frame on the same connection is dropped. The agent clamps its push interval to at
  least 2 s. The only frames it could send closer together are catch-up ticks after a stall,
  and those carry nothing new.

With both rules, one connection costs at most about 1,029 metric inserts per second.
Connection count stays unbounded (see Security implications).

### 4. Bounded auto-registration

A **push system** is a system the hub registered from a push handshake. Its url is the
constant `PUSH_SYSTEM_URL = "push://"`, which replaces the literal in `register_if_new`. The
registry admits a new push system only while there are fewer push systems than the **push
registry limit**:

```rust
/// The most push systems the hub will auto-register. Never zero.
pub struct PushRegistryLimit(NonZeroUsize);

impl PushRegistryLimit {
    pub const DEFAULT: usize = 1000;
    /// Whether a registry holding `push_systems` push systems may register one more.
    pub fn admits(&self, push_systems: usize) -> bool;
}
```

`Database` gets one method that holds its connection guard across the existence check, the
count and the insert, and asks the domain rule in between:

```rust
pub enum Registration { AlreadyRegistered, Registered, RegistryFull }

pub fn register_push_system(
    &self,
    system: &SystemInfo,
    admits: impl FnOnce(usize) -> bool,
) -> Result<Registration, rusqlite::Error>;
```

- The count is `SELECT COUNT(*) FROM systems WHERE url = ?1`, with `PUSH_SYSTEM_URL` bound.
  The rule itself stays in `admits`, not in SQL.
- One guard covers all three steps, so there is no race. The limit is exact, and an id racing
  itself gets `AlreadyRegistered`, never a spurious `RegistryFull`.
- A known id is unaffected.
- An unseen id beyond the limit gets `auth_error` / `registry full`. The refusal is logged at
  `warn` with the refused id in `Debug` form. The id is authenticated, at most 255 bytes, and
  escaped by `Debug`, so the operator can see which agent is locked out.

The operator frees a slot by deleting a push system (the dashboard's "delete offline" does
this for every offline one) or by raising the limit.

### 5. Deleted systems stop ingesting

- `ingest_frame` reads the system's row first. If the row is gone, the frame is dropped, the
  hub logs at `info` that the system was deleted while connected, and the connection is
  closed. When the agent reconnects, the handshake registers it again, subject to the limit.
- `DELETE /api/systems/:id` also removes the system's `live_metrics` entry, for push systems
  and polled systems alike.

### 6. Fail-closed configuration

Both variables are parsed once at startup, in `main`. A value that is set but invalid makes
the hub refuse to start: it logs the variable's name and the reason at `error`, never the
value, and exits non-zero.

| Variable | Unset or empty | Valid | Refuses to start |
|---|---|---|---|
| `HUB_PUSH_TOKEN` | push auth disabled, startup `warn` (as today) | `PushAuth::Required(token)` | not UTF-8 |
| `HUB_MAX_PUSH_SYSTEMS` | `PushRegistryLimit::DEFAULT` (1000) | a whole number ≥ 1 | `0`, negative, not a number, overflows `usize`, not UTF-8 |

```rust
enum PushAuth { Open, Required(PushToken) }        // replaces Option<PushToken>
enum PushConfigError { TokenNotUnicode, LimitNotUnicode, LimitZero, LimitNotANumber }
```

An empty value means unset because compose files commonly write `VAR=` for that. Startup is
where configuration is verified (`CLAUDE.md`), and an operator who set a value must never get
a more open hub because the value was malformed.

### Handshake outcomes

`authenticate` stays a pure function of the frame and the token, returning
`Result<SystemId, HandshakeRejection>` as today. The adapter has its own outcome for what only
the socket or the registry can decide:

```rust
enum Refusal {
    Rejected(HandshakeRejection), // shape, token, system id (wire messages unchanged)
    Timeout,                      // "handshake timeout"
    RegistryFull,                 // "registry full"
}
```

## Domain impact

- **Ingestion** (`push.rs`): deadlines, size limits, frame limits and the `Refusal` outcome.
  The context-map row gains the frame rules.
- **Fleet Registry** (`models.rs`, `db.rs`): `PushRegistryLimit`, `PUSH_SYSTEM_URL`, and
  `Database::register_push_system`.
- **Glossary:**
  - adds **push system**: a system registered from a push handshake, whose url is `push://`;
  - adds **push registry limit**: the most push systems the hub will auto-register
    (`PushRegistryLimit`);
  - changes **push token**: a set token is `PushAuth::Required`, and a non-UTF-8 value refuses
    startup.
- **Published contracts:** the push frame's shape is untouched. What the hub accepts
  narrows. The handshake gains two `auth_error` messages, `handshake timeout` and
  `registry full`. Mixed-version fleets:
  - *Any agent, new hub:* agents send auth immediately, ping every 30 s, push at most every
    2 s, and send frames of a few KB, so no current or older agent trips a deadline or a
    limit. An agent refused with `registry full` treats it like any `auth_error` and retries
    every 5 s.
  - *Agents whose id changes* (a container recreated without `/etc/machine-id`, which then
    falls back to its hostname, or the random-UUID fallback) leave an offline push system
    behind at every change. Nothing expires those rows, so churn can fill the limit. The
    refusal log names the id, and "delete offline" frees the slots. Expiring stale push
    systems is recorded as an open question.
  - *Old hub:* unchanged until upgraded.

## Alternatives considered

- **Do nothing.** Each gap is a one-client resource exhaustion or a silent auth bypass.
- **An unlimited registry default.** No behaviour change on upgrade, but the default stays
  unbounded. The owner chose 1000.
- **Truncate a frame's disks instead of refusing the frame.** A host would silently show a
  partial set of disks.
- **The registry rule as one SQL statement** (`INSERT … SELECT … WHERE (SELECT COUNT(*) …) < ?`).
  It would put a business rule in an SQL string (`CLAUDE.md`), and it can't tell "full" from
  "registered meanwhile".
- **A global connection limit** (for example `tower::limit::ConcurrencyLimitLayer`). It's
  complementary, and it belongs with the general rate-limiting gap.
- **Pre-registration only** (only ids an operator registered may push). This is the strongest
  control, but it needs an API that accepts a chosen id. It belongs with per-system push
  credentials.
- **Refuse pushes instead of refusing to start on bad configuration.** It would hide the
  mistake behind agent-side errors.
- **Treat an empty value as invalid.** It breaks compose files that write `VAR=` to mean
  unset.

## Security implications

- **API4 Unrestricted Resource Consumption:** the purpose of the RFC. Per connection, it
  bounds idle time, message size, disks per frame and frame rate. Push systems are bounded by
  the limit. What stays unbounded:
  - The number of connections. A client can open many and keep each alive with a ping every
    89 s. This belongs with rate limiting.
  - Rows the REST API creates. The cap counts push systems only. `POST /api/systems` stays an
    unbounded registration path, and `PUT /api/systems/:id` can rewrite a push system's url
    so it no longer counts. The hub has no client authentication, so **the cap holds only
    where push clients can't reach the REST API** (for example, a proxy that exposes only
    `/api/push` to agents).
  - Stored metric rows over time. Pruning is keyed on the frame's client-chosen `timestamp`,
    and a mount name that appears once is never pruned again. This is recorded as an open
    question in the retention area.
- **A05 / API8 Misconfiguration:** a non-UTF-8 token, or a malformed limit, now refuses
  startup instead of silently opening the hub.
- **A07 / API2 Authentication:** the token comparison is unchanged: constant-time, and checked
  before the id. The deadline and the size limits don't depend on content, so they leak
  nothing about the token. `registry full` comes after authentication, so only an
  authenticated client learns that the registry is full.
- **A01 / API1:** unchanged. The id stays self-asserted.
- **A09 Logging:** every refusal, drop and size error is logged at `warn`. Registry refusals
  name the id in `Debug` form. Configuration errors name the variable, never its value.
  Under a flood, log volume is part of the rate-limiting gap.
- **A03 Injection:** the count query is a fixed string with a bound parameter.
- **API10 Unsafe Consumption:** push frames are bounded before decoding, and checked before
  any storage work.
- **Agent side (out of scope):** the agent has the same fail-open on a non-UTF-8
  `SYSTEM_AGENT_TOKEN` (`src/auth.rs`). It's recorded as an open question.
- A02, A04, A06 (no dependency change), A08, A10 / API7, API3, API5, API6, API9 (no new
  route): not touched.

## Testing plan

TDD, per `CLAUDE.md`. `red-test-adversary` attacks every red test, and `rosette-auditor` runs
on the diff.

- **Pure, table-driven:**
  - The configuration parser, with one row per cell of the table in §6, including empty, `0`,
    `-1`, `1,000`, `18446744073709551616` and non-UTF-8.
  - `PushRegistryLimit::admits`: below the limit, at it, and above it.
  - The disk rule: 1024 and 1025 disks.
  - The pacing rule: 0.999 s, 1 s, and no previous frame.
- **`Database::register_push_system`** (temp SQLite):
  - A limit of 1 with one polled `http://` system present admits an unseen push id. This
    shows that only push systems count.
  - A limit of 1 with one push system present refuses an unseen id and inserts nothing, but
    returns `AlreadyRegistered` for the known id.
- **Real server** (`axum::serve` on an ephemeral port, as the push tests do today). The
  deadlines, size limit and registry limit are injected through `router_with_config`, which
  replaces `router_with_token`, so no test waits 10 s or touches the environment.
  - A client that upgrades and sends nothing is answered `handshake timeout`, and the hub
    closes the socket.
  - An authenticated client that goes quiet past the idle deadline is closed, and the system
    is marked offline.
  - Frame and message size:
    - A frame header declaring limit + 1 bytes, with no payload, gets the connection closed.
    - A message fragmented into frames that are each within the limit but together exceed it
      gets the connection closed.
    - An oversize handshake text gets the connection closed, and nothing is registered.
  - A frame with 1025 disks stores nothing. The next valid frame is stored.
  - Two frames 10 ms apart store once.
  - A full registry answers `registry full` to an unseen id.
  - Deleting a system while it's connected closes the connection. It also removes the
    system's `live_metrics` entry. Nothing more is stored for its id.

## Impact on `docs/ARCHITECTURE.md`

- § Domain model:
  - Context map: Ingestion row (the frame rules) and Fleet Registry row (the limit,
    `register_push_system`).
  - Glossary: **push system** and **push registry limit** (new); **push token** (changed).
- § Trust boundaries, Agent → Hub (push): deadlines, size and frame limits, the registry
  limit, deleted systems, and fail-closed configuration.
- § Testing architecture: `router_with_config` replaces `router_with_token`.
- § Open architectural questions:
  - Removed: the non-UTF-8 hub token, the handshake timeout, and unbounded auto-registration.
  - Rewritten: frame size, now bounded.
  - Added:
    - expiring stale push systems;
    - the client-chosen `timestamp` driving pruning, and one-off mount names never pruned;
    - REST-created systems as an unbounded registration path;
    - the agent's non-UTF-8 `SYSTEM_AGENT_TOKEN` failing open.
  - Kept: rate limiting and per-system credentials.
- `README.md`: the `HUB_PUSH_TOKEN` row (non-UTF-8 refuses to start), a new
  `HUB_MAX_PUSH_SYSTEMS` row, the new `auth_error` messages, and the limits in the push
  protocol section.
- `.env.example`: a commented `HUB_MAX_PUSH_SYSTEMS`. `rfcs/README.md`: the index row.

## Rollout / migration notes

Hub-only. No schema change. Land after RFC 0005.

- A hub that ran with a non-UTF-8 `HUB_PUSH_TOKEN`, or that gets a malformed
  `HUB_MAX_PUSH_SYSTEMS`, refuses to start after the upgrade. That is the intended outcome, and
  the log names the variable.
- A registry that already holds more push systems than the limit keeps all of them. It refuses
  only new ids until it's back under the limit.
- A fleet of more than 1000 push agents must set `HUB_MAX_PUSH_SYSTEMS` before upgrading, or
  its newest agents are refused with `registry full`.
