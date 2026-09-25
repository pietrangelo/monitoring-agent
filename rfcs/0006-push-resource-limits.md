# RFC 0006: Push Receiver Resource Limits and Fail-Closed Token

- Status: Draft
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-25
- Affects: `system-hub`

## Motivation

RFC 0003 hardened the push handshake's comparison and parsing. It deferred four gaps in the
receiver (`system-hub/src/push.rs`), which are listed in `docs/ARCHITECTURE.md` § Open
architectural questions. Each lets one client make the hub hold resources without limit, or
leaves the push endpoint open when the operator meant to close it:

1. **No handshake timeout (API4).** `handshake` awaits `socket.recv()` with no deadline. A
   client that completes the WebSocket upgrade and then sends nothing keeps a connection
   task, its socket and its buffers alive until the TCP connection dies. This needs no token,
   because the token is only checked once the first message arrives.
2. **No size limits (API4).** The receiver uses the WebSocket defaults from tungstenite 0.24:
   a 64 MiB message and a 16 MiB frame. The handshake is parsed from a text message of up to
   64 MiB before any check. A data frame can carry any number of disks, and each becomes a
   metric row on every frame. A `system_id` can be any length and is stored as a primary key.
3. **Unbounded auto-registration (API4).** Every accepted handshake with an unseen id
   inserts a permanent, enabled `systems` row, and the poller visits every enabled row every
   30 s. The ids are self-asserted, so with the token unset anyone can fill the registry, and
   with it set any token holder can.
4. **A non-UTF-8 `HUB_PUSH_TOKEN` fails open (A05 / API8).** `PushToken::from_env` treats
   `Err(VarError::NotUnicode)` like an unset variable, so the push endpoint needs no token.
   The operator set a token and gets none.

## Proposed design

### 1. Handshake deadline

The first message must arrive within `HANDSHAKE_TIMEOUT = 10 s` of the upgrade. The agent
sends its auth message as soon as it connects, so 10 s leaves room for a slow link, and it
bounds how long an idle connection lives.

```rust
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.recv()).await;
// Err(Elapsed) → HandshakeRejection::Timeout, answered "handshake timeout", socket closed
```

`HandshakeRejection` gains `Timeout`, answered with `auth_error` / `handshake timeout` and
logged at `warn` like the other rejections. The deadline lives in the adapter: `authenticate`
stays a pure function of the frame, and the timeout is a property of the socket.

### 2. Size limits

- **Message and frame size:** `push_handler` configures the upgrade with
  `max_message_size(MAX_PUSH_MESSAGE_BYTES)` and `max_frame_size(MAX_PUSH_MESSAGE_BYTES)`,
  with `MAX_PUSH_MESSAGE_BYTES = 1 MiB`. A current agent frame carries fixed fields, at most
  10 processes and one entry per disk, about 150 bytes each. So 1 MiB fits several thousand
  mounts, far beyond any real host. Tungstenite closes a connection whose message exceeds the
  limit with close code 1009 (message too big). The receive loop already treats an `Err` as
  the end of the connection, then marks the system offline.
- **System id length:** `SystemId` rejects an id longer than `MAX_SYSTEM_ID_BYTES = 255`,
  with a new `SystemIdError::TooLong`, answered `invalid system_id`. Machine ids are 32
  bytes, hostnames at most 253, and UUIDs 36. This builds on RFC 0005's `SystemIdError`.
- **Disks per frame:** not capped separately. The message limit bounds them, and a per-frame
  cap would silently drop a real host's disks. The per-disk metric rows are a retention
  question, which is a separate open question.

### 3. Bounded auto-registration

The registry gets a size limit for push-registered systems, `HUB_MAX_PUSH_SYSTEMS`:

- A handshake whose id is already registered is unaffected.
- A handshake with an unseen id is accepted only while fewer than `HUB_MAX_PUSH_SYSTEMS`
  systems have the `push://` url. Otherwise it's refused with `auth_error` / `registry full`
  (a new `HandshakeRejection::RegistryFull`), logged at `warn`, and nothing is inserted.
- The count and the insert run in one unit on the blocking pool (`register_if_new`), and
  units are serialised per connection, not across connections. Two connections racing at the
  limit can exceed it by at most the number racing, which is acceptable for a resource
  bound. Enforcing it exactly needs the count and insert in one SQLite transaction, which
  `db.rs` can do (`INSERT ... SELECT ... WHERE (SELECT COUNT(*) ...) < ?`). The
  implementation uses that form if it stays a single statement.
- The limit is parsed once at startup into a `PushRegistryLimit` newtype, which is never
  zero. The **default is open for decision** (see Unresolved questions).

Operators free a slot by deleting a stale system, from the dashboard or with
`DELETE /api/systems/:id`.

### 4. Fail closed on a non-UTF-8 token

`HUB_PUSH_TOKEN` parses into a push-auth policy at startup instead of an `Option<PushToken>`:

```rust
enum PushAuth {
    /// Unset or empty: any client may push (the documented, logged opt-out).
    Open,
    /// Every handshake must present this token.
    Required(PushToken),
}

enum PushAuthError {
    /// Set, but not valid unicode, so no agent could ever present it.
    NotUnicode,
}

fn from_env(value: Result<String, VarError>) -> Result<PushAuth, PushAuthError>
```

`main` refuses to start on `PushAuthError::NotUnicode`, logging at `error` that
`HUB_PUSH_TOKEN` is not valid UTF-8, without the value, and exiting non-zero. An operator who
set a token must never get an open endpoint by accident. An unset or empty value keeps
meaning "push auth disabled", with its startup warning, because compose files commonly write
`HUB_PUSH_TOKEN=` to leave it unset.

## Domain impact

- **Ingestion** (`push.rs`): the handshake gains a deadline and two rejections
  (`Timeout`, `RegistryFull`). The size limits are adapter configuration.
- **Fleet Registry** (`models.rs`): `SystemId` gains a length rule (`SystemIdError::TooLong`).
  The registry limit is a Fleet Registry rule ("a push may register a new system only while
  there are fewer than N push systems"). It is a pure function over a count and the limit,
  with the count read in the adapter.
- **Glossary:** adds **push registry limit**, the most systems the hub will auto-register
  from push handshakes (`PushRegistryLimit`). **system id** gains "at most 255 bytes".
- **Published contracts:** the push frame is untouched. The handshake answers three new
  `auth_error` messages: `handshake timeout`, `registry full`, and `invalid system_id` for
  over-long ids. Mixed-version fleets:
  - *Old agent, new hub:* an old agent treats any `auth_error` as a failure and reconnects,
    as it does today for `invalid token`. A current agent's frames are well under 1 MiB, and
    its id is well under 255 bytes. An old agent never idles through the handshake.
  - *New hub with a full registry:* a genuinely new agent is refused until an operator
    frees a slot. That is the intended trade-off, and the answer names the cause.
  - *Existing rows* are untouched. An already-registered id longer than 255 bytes can no
    longer push. Only a hostile client could have created one, since real ids are at most
    253 bytes.

## Alternatives considered

- **Do nothing.** Each gap is a one-client resource exhaustion or a silent auth bypass.
- **An idle timeout on authenticated connections as well.** The agent pings every 30 s, so a
  timeout of a few minutes would be safe. But the gap is unauthenticated connections, and
  a token holder can already do more harm by pushing. Deferred.
- **A global connection limit** (for example `tower::limit::ConcurrencyLimitLayer` on
  `/api/push`). It bounds the number of connections, not how long each lives, so it's
  complementary. It belongs with the general rate-limiting gap.
- **Pre-registration only** (`HUB_PUSH_AUTO_REGISTER=false`, so that only ids an operator
  registered may push). This is the strongest control. But `POST /api/systems` generates
  UUIDs, so an operator can't register an agent's machine id today, and that needs an API
  change of its own. It fits the future per-system credentials RFC.
- **Register on the first valid data frame instead of at the handshake.** It filters out
  handshake-only probes, but it isn't a bound, and a hostile client sends one frame.
- **Refuse pushes instead of refusing to start on a non-UTF-8 token.** It would keep
  polling up, but it would hide the misconfiguration behind agent-side `invalid token`
  errors. Startup is where configuration is verified (`CLAUDE.md`).
- **Treat an empty `HUB_PUSH_TOKEN` as fail-closed too.** It breaks compose files that use
  `HUB_PUSH_TOKEN=` to mean unset. Kept as documented.

## Security implications

- **API4 / Unrestricted Resource Consumption:** the purpose of the RFC. It bounds idle
  handshake time, message size, id length and auto-registered rows. It doesn't add rate
  limiting or connection counts (see Alternatives).
- **A05 / API8 Security Misconfiguration:** a non-UTF-8 token now fails closed, at startup.
- **A07 / API2 Authentication:** the token comparison is unchanged: constant-time, and
  checked before the id. The deadline applies before authentication, so it can't leak
  whether a token is valid. `RegistryFull` is checked after the token and the id, so only
  an authenticated client learns that the registry is full.
- **A01 / API1:** unchanged. The id stays self-asserted. A token holder can still push as
  any existing id, and the limit doesn't help with that.
- **A09 Logging:** each new rejection is logged at `warn` with its variant, and the startup
  error doesn't include the value. A flood of `registry full` warnings is itself a signal.
  Log volume under a flood is part of the rate-limiting gap.
- **A03 Injection:** no new SQL fragment. The count query uses a fixed string and bound
  parameters.
- **API10 Unsafe Consumption:** push frames are bounded before decoding.
- A02, A04, A06 (no dependency change), A08, A10 / API7, API3, API5, API6, API9 (no new
  route): not touched.

## Testing plan

TDD, per `CLAUDE.md`. Pure parts first, then the adapter:

- `PushAuth::from_env`: a table over unset, empty, a valid token, and `NotUnicode`.
- `SystemId`: rows at 255 and 256 bytes, and a multi-byte character across the boundary.
- The registry rule: a table over counts below, at and above the limit, and an already
  registered id.
- `PushRegistryLimit` parsing: unset (the default), `0`, a negative number, not a number,
  and a valid value.
- Real-server tests (`axum::serve` on an ephemeral port, as the push tests do today). They
  inject the timeout, the size limit and the registry limit through `router_with_*`, so no
  test waits 10 s or mutates the environment:
  - A client that upgrades and sends nothing gets `handshake timeout`, and its task ends.
  - A 1 MiB + 1 byte binary message closes the connection, and the system goes offline.
  - A full registry refuses an unseen id and inserts nothing, but accepts a known one.
- `red-test-adversary` attacks each red test. `rosette-auditor` runs on the diff.

## Impact on `docs/ARCHITECTURE.md`

- § Trust boundaries, Agent → Hub (push): the handshake deadline, the size limits, the
  registry limit, and the fail-closed token.
- § Domain model: the **push registry limit** glossary entry, and the **system id** length
  rule.
- § Open architectural questions: remove the non-UTF-8, handshake timeout and
  auto-registration entries. Keep rate limiting and the per-system credentials entries.
- `README.md`: the `HUB_PUSH_TOKEN` row (non-UTF-8 refuses to start), a new
  `HUB_MAX_PUSH_SYSTEMS` row, and the new `auth_error` messages.

## Rollout / migration notes

Hub-only. No schema change. A hub that previously ran with a non-UTF-8 `HUB_PUSH_TOKEN`
refuses to start after the upgrade, which is the intended outcome, and its log names the
variable. A hub whose registry already holds more push systems than the limit keeps all of
them, and refuses only new ids until it's under the limit.

## Unresolved questions

- **The default for `HUB_MAX_PUSH_SYSTEMS`.** The options:
  - **Unlimited unless set.** No behaviour change on upgrade, but the default stays
    unbounded.
  - **A generous number (for example 1000).** Bounded by default. A fleet bigger than that
    must set the variable, or new agents are refused with `registry full`.
  - **A small number (for example 100).** Tighter, but more deployments have to configure it.

  The recommendation is **1000**. It makes the default safe, and a fleet that size already
  needs deliberate configuration. This is the operator's call, so the RFC stays `Draft`
  until it is decided.
