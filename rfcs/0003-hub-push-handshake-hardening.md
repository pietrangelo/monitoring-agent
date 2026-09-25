# RFC 0003: Hub Push Handshake Hardening

- Status: Implemented
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-25
- Affects: `system-hub`

## Motivation

The hub's push handshake (`system-hub/src/push.rs::handle_push`) has two defects, both
reachable by any client that can open a WebSocket to `/api/push`:

1. **Non-constant-time token comparison.** `auth.token != expected_token` compares the
   presented token with `HUB_PUSH_TOKEN` using `String`'s `PartialEq`, which returns at the
   first differing byte. That leaks the secret's content through timing — the same
   side-channel RFC 0001 closed on the agent (OWASP A02 / API2). RFC 0001 covered only
   `src/auth.rs`.
2. **Panic on short or non-ASCII system ids.** A newly seen system is named
   `auth.system_id[..8]`, and every push frame later checks `sys.name == system_id[..8]` to
   decide whether to replace that default name with the snapshot's hostname. Byte slicing
   panics when the id is shorter than 8 bytes, or when byte 8 falls inside a multi-byte
   character. The panic kills the connection task: the system is never registered and the
   socket closes without an `auth_ok`/`auth_error` answer. The agent doesn't notice: its
   handshake check in `src/push.rs` has no branch for a missing answer, so it falls through
   into its push loop. That loop ends as soon as the socket is gone and returns `Ok(())`.
   `src/main.rs` backs off only on `Err`, so the agent reconnects right away.

   This is not only an attacker's input. The agent's `get_persistent_id()` falls back to
   the host's `hostname` when `/etc/machine-id` and the dbus machine id are missing
   (containers, minimal images), and hostnames like `pi`, `web1` or `db` are shorter than
   8 bytes. Such agents cannot push to the hub at all today, and reconnect in a loop.

   When `HUB_PUSH_TOKEN` is unset, any unauthenticated client can trigger the panic. When it
   is set, only token holders can.

Doing nothing leaves a timing oracle on the push secret and a crash path that locks out
legitimate agents with short hostnames.

## Proposed design

All changes are in `system-hub`. The wire format of the handshake (`{"type":"auth",
"system_id":…, "token":…}` → `{"type":"auth_ok"}` / `{"type":"auth_error","message":…}`)
is unchanged. The push frame is untouched.

### Parse the handshake at the boundary

A pure function turns the first text message into a domain value or a typed rejection:

```rust
enum HandshakeRejection {
    NotAnAuthMessage, // malformed JSON or `type` != "auth" → "expected auth message"
    InvalidToken,     // token doesn't match HUB_PUSH_TOKEN → "invalid token"
    InvalidSystemId,  // empty system_id                   → "invalid system_id"
}

fn authenticate(frame: &str, push_token: Option<&PushToken>)
    -> Result<SystemId, HandshakeRejection>;
```

The checks run in this order: shape, then token, then system id. An unauthenticated client
therefore learns nothing about which system ids the hub accepts: a bad token always yields
`invalid token`, even if the id is also invalid.

The handler maps a `HandshakeRejection` to its wire message. The two existing messages keep
their exact text.

### `PushToken`: the configured secret, read once

```rust
struct PushToken(String); // never empty; no Debug/Display, so it can't be logged
impl PushToken {
    fn new(value: String) -> Option<Self>;  // "" → None (push auth disabled)
    fn from_env(value: Result<String, std::env::VarError>) -> Option<Self>;
    fn accepts(&self, presented: &str) -> bool; // subtle::ConstantTimeEq
}
```

`from_env` is pure, so it can be table-tested for each case: set, empty, not present, not
unicode. `router()` calls `from_env(std::env::var("HUB_PUSH_TOKEN"))`, so the line that
touches the environment does no parsing. Its only branch is this: when the result is
`None`, it logs one `tracing::warn!` at startup saying that push authentication is
disabled. It never logs the value.

The value is taken verbatim, as before: surrounding whitespace is part of the secret,
and a whitespace-only value enables push auth with that (weak) secret.

`AuthMessage`, the serde DTO of the handshake, holds the presented token, which equals the
secret on success. It loses its `Debug` derive.

`HUB_PUSH_TOKEN` is read once, when `push::router` is built at startup, instead of on every
upgrade request. An unset, empty or non-UTF-8 value means push authentication is disabled,
exactly as today (`std::env::var(...).unwrap_or_default()` treats all three as `""`). A
process's environment does not change after start unless the process itself changes it, so
reading once is observably the same. As a side effect the tests no longer need
`unsafe { std::env::set_var }`, because they can pass the token in directly.

`accepts` compares with `subtle::ConstantTimeEq`, as `src/auth.rs::tokens_match` does. As
in RFC 0001, the token's *length* stays observable through timing; its content does not.
`subtle = "2"` becomes a direct dependency of `system-hub`. The agent already depends on
it.

### `SystemId` and the default system name (Fleet Registry)

```rust
pub struct SystemId(String);           // private field, never empty
impl TryFrom<String> for SystemId { type Error = EmptySystemId; }
impl SystemId {
    pub fn as_str(&self) -> &str;
    pub fn default_name(&self) -> String;
}
```

`default_name` is the longest prefix of the id that is at most 8 bytes long and ends on a
character boundary. For every id where today's `id[..8]` does not panic, the result is the
same. So rows already stored by the hub keep matching the "is this still the default
name?" check, and systems registered before this change are still renamed to their
hostname on the next frame. For shorter ids it is the whole id, and for ids with a
multi-byte character across byte 8, it stops before that character.

Both panic sites use it. Registration names a new system `id.default_name()`. The frame
loop's rename decision becomes the pure Fleet Registry rule
`SystemId::is_default_name(&self, name: &str) -> bool`, which is table-tested.

The field defaults of a newly pushed `SystemInfo` row are not remodelled here. They stay in
the Ingestion adapter as they are today, with sentinels such as `url: "push://"`,
`last_seen: ""` and `token: ""`. That belongs with the anti-corruption-layer work on
`models.rs` that is already an open question.

### Empty system ids are rejected

An empty `system_id` gets `auth_error` / `invalid system_id`. Today an empty id panics at
`[..8]`, so it has never been registered. Rejecting it keeps that outcome and adds an
answer, and it keeps an empty string (a sentinel) out of the `systems` table's primary key.
The agent never sends an empty id: every branch of `get_persistent_id()` checks for empty.

### Handler structure

`handle_push` (170 lines) is split along its existing seams: the handshake (receive, then
`authenticate`, then register if new, then answer), the frame loop, and ingesting one
frame. Frame ingestion keeps its current behaviour. Its decode failures, discarded DB
errors and the `live_metrics` lock `unwrap()` are out of scope, and stay listed in
`docs/ARCHITECTURE.md` § Open architectural questions.

The synchronous SQLite work these functions do (registration, frame ingestion, offline
marking) runs in `tokio::task::spawn_blocking` rather than on the async runtime, following
CLAUDE.md's rule to fix blocking calls in code you touch. The connection task awaits each
unit before it reads the next WebSocket message. So a connection has at most one unit in
flight, frames are ingested in arrival order, and the offline marking runs only after the
last frame's ingestion has finished. A late `Online` therefore can't overwrite `Offline`.
If a unit panics (for example on a poisoned lock), its `JoinError` ends the frame loop and
is logged once at `error`. The offline marking still runs. Each rejection is logged with
`tracing::warn!`, naming the rejection variant but never the presented token.

### Deferred after review

The `rosette-auditor` pass left these AT-RISK findings open. Each is a deliberate
deferral, recorded here, and each is also listed in `docs/ARCHITECTURE.md` § Open
architectural questions:

- **Snapshot → metric mapping.** Push (`push::metric_points`) and poll (`collector.rs`)
  each hard-code the metric names over their own wire DTOs. They should share one Fleet
  History domain function over a domain snapshot value. That work touches `collector.rs`
  and the models' DTO/domain split, which is beyond this RFC. `metric_points` is covered
  only through the real-server ingestion test until then.
- **Registry rules in `update_registry`.** "Refill system info while its hostname or OS is
  missing" and "a frame marks the system online" are still decided inline next to the DB
  calls. Only the default-name rule moved into the domain (`SystemId::is_default_name`).
- **`AuthMessage` as a validated DTO.** It deserializes `type` as a `String` and a missing
  token as `""`. A serde-tagged enum with `token: Option<String>` would parse rather than
  validate. It is safe today because `PushToken` is never empty, so `""` never matches.
- **`EmptySystemId`** has no `Display`/`Error` implementation yet. Add one when a second
  `SystemId` constructor site (routes, DB rows) needs it in an error chain.

## Domain impact

- **Bounded contexts:** Ingestion (`push.rs`, the handshake adapter) and Fleet Registry,
  which gains `SystemId` and the default-name rule in `system-hub/src/models.rs`, outside
  any serde DTO.
- **Glossary:** adds **push token**, the shared secret (`HUB_PUSH_TOKEN`) an agent must
  present in the push handshake when one is configured (`PushToken`). Adds **system id**, the non-empty identifier an agent presents in the push
  handshake, which the hub uses as the system's primary key (`SystemId`). Adds **default
  system name**, the name the hub gives a newly pushed system until its first snapshot
  supplies a hostname (`SystemId::default_name`).
- **Published contract (push handshake):** the message shapes are unchanged. The set of
  handshakes that get an answer grows:

  | Presented `system_id` | Before | After |
  |---|---|---|
  | ≥ 8 bytes, char boundary at byte 8 | `auth_ok` | `auth_ok` (same default name) |
  | 1–7 bytes, or multi-byte char across byte 8 | task panics, socket closes, no answer | `auth_ok` |
  | empty | task panics, socket closes, no answer | `auth_error` / `invalid system_id` |

- **Mixed-version fleet:** the change is hub-only. Old and new agents behave the same
  against a new hub, except that short-id agents can now connect. An old agent treats any
  `auth_error` message text as a failure (`src/push.rs` only logs `msg.message`), so the
  new message text is safe. A new hub reads existing `systems` rows unchanged.

## Alternatives considered

- **Fix only the two lines (`get(..8)` / `==` → `ct_eq`)** without a pure `authenticate`.
  Smaller, but the handshake would stay untestable without a live server and an `unsafe`
  env mutation, and the default-name rule would stay duplicated in two places.
- **Take the first 8 characters instead of 8 bytes.** This changes the default name of
  existing systems whose ids have multi-byte characters before byte 8. Their stored names
  would stop matching, so they would never be renamed to their hostname. Rejected in favour
  of the byte-compatible floor.
- **Validate system ids further** (charset, max length). This would reject ids that hosts
  send today, such as non-ASCII hostnames, which is a real behaviour change for a
  mixed-version fleet. Deferred. A max length belongs to API4 hardening together with frame
  size limits.
- **Fail closed on a non-UTF-8 `HUB_PUSH_TOKEN`.** Arguably safer, but it changes startup
  behaviour for a misconfiguration that is out of this RFC's scope. Recorded in
  `docs/ARCHITECTURE.md` § Open architectural questions instead.

## Security implications

- **A01 Broken Access Control / API1 Broken Object Level Authorization:** touched, but not
  fixed. The handshake is where the hub decides which system a connection may write to, and
  it trusts whatever `system_id` the client asserts. `GET /api/systems` is unauthenticated
  and lists every id. So anyone holding the single shared push token can push as any listed
  system, including a polled system's UUID: inject metrics, trigger a rename, or force it
  offline. When the token is unset, anyone can. This RFC neither widens nor narrows that.
  Binding ids to per-system credentials is a separate design, recorded as an open question
  in `docs/ARCHITECTURE.md` and deferred.
- **API5 Broken Function Level Authorization:** unchanged. Push auth stays optional
  (`HUB_PUSH_TOKEN` unset disables it), as documented.
- **A02 Cryptographic Failures / API2 Broken Authentication:** fixed. The token content
  timing oracle is closed. Length is still observable, as accepted in RFC 0001.
- **A03 Injection:** `system_id` still reaches SQLite only as a bound parameter. The new
  `auth_error` message is a fixed string that never echoes client input.
- **A04 Insecure Design:** the token is checked before the id, so unauthenticated clients
  can't probe id validation.
- **A05 / API8 Security Misconfiguration:** unchanged. Unset, empty or non-UTF-8
  `HUB_PUSH_TOKEN` still disables auth. The non-UTF-8 fail-open case is recorded as an open
  question.
- **A06 Vulnerable Components:** adds `subtle` 2.x, already in the agent's lockfile, widely
  used, no known advisories. Run `cargo audit` if available.
- **A09 Logging:** improved. Each rejection is logged at `warn` level with its variant, so
  online token guessing is visible. It is counted, not attributed: the hub doesn't log the
  peer address, and behind the reverse proxy the README expects, the peer would be the
  proxy anyway. That is recorded as an open question. Startup warns once when push auth is
  disabled. Neither `PushToken` nor `AuthMessage` has `Debug`/`Display`, so neither the
  configured nor the presented token can be logged by accident.
- **API4 Unrestricted Resource Consumption:** removes a crash-and-reconnect loop for short
  ids. Still open, and recorded as open questions: there is no limit on handshake or frame
  size, and no cap on auto-registered systems. Every handshake with an unseen id inserts a
  permanent, enabled `systems` row, which the poller then visits every 30 s. This RFC
  slightly widens the set of ids that register: 1–7-byte ids and multi-byte ids across
  byte 8.
- **API10 Unsafe Consumption of APIs:** improved. The agent's handshake is parsed into typed
  values before use.
- **A07, A08, A10 / API3, API6, API7, API9:** not touched. No new endpoint, no outbound
  request.

## Testing plan

- **Characterisation (backfill, mutation mode):** a real-server test in which an agent with
  a ≥ 8-byte id handshakes, sends one push frame, and disconnects. It asserts that metrics
  were stored, the live metrics cache was updated, the system was renamed from its default
  name to the snapshot's hostname, and it is marked offline after disconnect.
- **Red, pure:** table-driven `PushToken::from_env` and `SystemId::is_default_name` tests.
  A table-driven `authenticate` test with a row for each rejection variant,
  malformed JSON, the token-before-id ordering, a matching token, a missing token field,
  wrong tokens of equal and different length, and short and empty ids. A table-driven
  `SystemId::default_name` test covering an id longer than, exactly at, and shorter than
  8 bytes, a 2-byte and a 3-byte character across byte 8, and empty-id rejection. A
  `PushToken::new` test: an empty string means auth is disabled.
- **Red, bug reproduction (real server):** an agent whose id is shorter than 8 bytes gets
  `auth_ok`, is registered, and is renamed after one frame to a snapshot hostname that
  differs from the id. An agent with an empty id gets the full
  `{"type":"auth_error","message":"invalid system_id"}` answer. Today both tasks panic. The
  wrong-token server test asserts the full `invalid token` answer.
- The existing accept/reject server test keeps passing, with the token injected instead of
  set through the environment.
- The real-server helper waits for the offline marking after closing the socket. It then
  checks that the status is still offline after a short settle, so an ingestion unit
  landing after the offline marking would turn the test red.
- The two `warn!` lines (startup with push auth disabled, and one per rejection) are
  enforced by review, not by a test. Neither has logic beyond the branch that emits it.
- The constant-time property itself is not observable in a unit test. It is enforced by
  review and by using `subtle`, as in RFC 0001.

## Impact on `docs/ARCHITECTURE.md`

- § Trust boundaries, *Agent → Hub (push)*: describe the constant-time comparison, reading
  the token once at startup, and token-before-id ordering.
- § Glossary: add *push token*, *system id* and *default system name*.
- § Bounded contexts: Fleet Registry's domain core lists `SystemId`. Ingestion's adapter
  column lists the handshake parsing in `push.rs` (`authenticate`, `PushToken`,
  `HandshakeRejection`).
- `CLAUDE.md` says "there is no `spawn_blocking` in either crate", which this change makes
  false for the hub's push receiver. `CLAUDE.md` changes only with the user's approval, so
  the change summary flags the sentence for the user to update.
- § Open architectural questions: remove the push-handshake bullet and the "no
  `spawn_blocking`" claim for the hub's push path. Add non-UTF-8 `HUB_PUSH_TOKEN` failing
  open, the self-asserted system id (API1), the missing cap on auto-registration, the
  agent's fall-through on a missing handshake answer, and push-registered systems being
  polled (`push://`) and flapping offline.
- `README.md` § push protocol: replace the `"system_id":"<uuid>"` placeholder with what the
  agent really sends, and list the `invalid system_id` answer.
- § Testing architecture: mention the handshake tests if that section lists coverage per
  module.

## Rollout / migration notes

Hub-only, with no schema change and no agent change. Deploy in any order. Systems stored
before the change keep their names, and the rename-to-hostname check still matches them.

**Rollback is safe, but noisy for some systems.** Consider a system the new hub
registered whose id is shorter than 8 bytes, or has a multi-byte character across byte 8,
and whose name is still at most 8 bytes long. For example, a short-hostname agent whose id
equals its hostname. An old hub accepts its handshake, because the row exists. On every
frame it stores metrics, updates the live cache and sets the status online, then panics in
the `[..8]` rename check. The panic skips the rename, the cache refresh and, when the agent
disconnects, the offline marking. So the system keeps ingesting data, logs one panic per
frame, and stays online after its agent stops. Systems whose name is already longer than
8 bytes are unaffected, because the old check short-circuits on `name.len() <= 8`.

To silence the panics on an old hub without losing history, give those systems a name
longer than 8 bytes (in the dashboard, or with `UPDATE systems SET name = … WHERE id = …`).
Do not delete their rows. After a delete, the old hub panics again at registration, which
is the original lockout. And a stock SQLite client runs with `PRAGMA foreign_keys` off, so
the delete would orphan their `metrics`, `alerts` and `metric_retention` rows.
