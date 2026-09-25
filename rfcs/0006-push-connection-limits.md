# RFC 0006: Push Connection Limits and Fail-Closed Token

- Status: Draft
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-25
- Affects: `system-hub`
- Depends on: RFC 0005 (implemented)
- Related: RFC 0007 (push ingestion cost and live state) and RFC 0008 (push registry limit
  and push system lifecycle). All three come from one earlier draft of this RFC, which three
  `rfc-adversary` passes showed was spread across three bounded contexts (see git history).
  This RFC keeps the part that belongs to Ingestion's connection handling.

## Motivation

The push receiver (`system-hub/src/push.rs`) puts no limit on how long a connection lives or
how much it may send. It also opens the push endpoint when the operator set a token that
isn't UTF-8. When `HUB_PUSH_TOKEN` is unset, which is the README's basic start, the client
doing this can be anyone.

1. **No deadlines (API4).** `handshake` waits for the first message with no deadline, and
   `receive_frames` waits for each later one with no deadline. A client that upgrades and
   sends nothing, or authenticates and then goes quiet, keeps a task and its socket alive
   until TCP gives up.
2. **An unbounded send (API4).** `receive_frames` answers each ping with an explicit
   `Message::Pong` and awaits the send until it has flushed. A peer that pings but never reads
   leaves that await pending forever, outside any deadline.
3. **No size limits (API4).** The receiver uses tungstenite 0.24's defaults:
   - a 64 MiB message and a 16 MiB frame, for the handshake text and for data frames alike;
   - an unlimited write buffer (`max_write_buffer_size = usize::MAX`).
4. **A non-UTF-8 `HUB_PUSH_TOKEN` fails open (A05 / API8).** `PushToken::from_env` treats
   `Err(VarError::NotUnicode)` like an unset variable, so the operator sets a token and gets
   none.
5. **Raw ids in logs (A09).** `Push client authenticated: {}` and similar lines print the
   self-asserted id with `Display`, so a client can forge log lines with `\n` or terminal
   escapes.

## Proposed design

### 1. Deadlines

- **Handshake:** the first message must arrive within `HANDSHAKE_TIMEOUT = 10 s` of the
  upgrade. Otherwise the hub answers `auth_error` / `handshake timeout`, and the connection
  ends. The agent sends its auth message as soon as it connects.
- **Idle:** after the handshake, any message must arrive within `IDLE_TIMEOUT = 90 s` of the
  previous one. Otherwise the connection ends and the system is marked offline, as on any
  disconnect. A data frame, a ping or a pong all count. Every shipped agent pings every 30 s
  whatever its push interval (`src/push.rs`, unchanged since the first commit).
- Both deadlines are a `tokio::time::timeout` around `socket.recv()`. That's cancel-safe,
  because a partial frame stays in tungstenite's codec. The timeout wraps each receive, never
  the whole connection, so a live agent is never cut off on a schedule.

### 2. Bounded sends

- **The explicit pong goes.** tungstenite already queues a pong when it reads a ping. The
  explicit one replaces it (so one pong goes out today, not two), and it's the only send
  after the handshake. Without it, the hub never awaits a send after `auth_ok`.
- **Handshake answers** (`auth_ok` and every `auth_error`) are sent under
  `SEND_TIMEOUT = 5 s`. If the send fails or times out after the system was registered, the
  hub marks it offline before the connection ends. Today that path returns without marking
  it.
- **Buffered pongs are capped.** A peer that pings but never reads makes tungstenite buffer
  pongs. Once `max_write_buffer_size` is reached, tungstenite parks one pong and replaces it
  with each newer one. So the hub holds at most `max_write_buffer_size` plus one pong per
  connection, and the connection stays usable. (The shipped agent never reads after the
  handshake, and that stays harmless: it's bounded now.)
- A connection ends by dropping the socket, never by waiting on a Close frame.

### 3. Size limits as one checked value

```rust
/// The WebSocket limits of one push connection. tungstenite panics at upgrade unless
/// `max_write_buffer_size > write_buffer_size`, so the constructor enforces it.
pub struct PushSocketLimits { /* private fields */ }

impl PushSocketLimits {
    pub const fn new(
        max_message_bytes: usize,
        write_buffer_bytes: usize,
        max_write_buffer_bytes: usize,
    ) -> Self; // panics unless max_message_bytes > 0 && max_write_buffer_bytes > write_buffer_bytes

    /// Checked at compile time: an invalid production value fails the build.
    pub const PRODUCTION: Self = Self::new(512 * 1024, 8 * 1024, 64 * 1024);

    fn apply(&self, upgrade: WebSocketUpgrade) -> WebSocketUpgrade; // sets all four values
}
```

- **Message and frame size:** both are `max_message_bytes = 512 KiB`. The frame limit is
  checked on the frame header, before the payload is buffered. The message limit is checked
  on the reassembled message, so a fragmented oversize message is refused too. 512 KiB leaves
  room for the largest frame RFC 0007's disk rule accepts: 1024 disks with 256-byte mount
  points, about 290 KB. A host above about 4,000 typical overlay mounts, at about 122 bytes
  each, would still exceed it (see Rollout).
- **Write buffer:** `write_buffer_bytes = 8 KiB` and `max_write_buffer_bytes = 64 KiB`. The
  hub writes only handshake answers and pongs.
- **An oversize message** makes tungstenite return an error, not a close frame. The hub logs
  it at `warn`, naming the system id in `Debug` form once the handshake has authenticated it,
  and drops the connection. It sends no `auth_error`, because a client that sends oversize
  messages isn't reading answers.

Because the constant is built by a `const fn` that asserts its invariant, `PRODUCTION` can't
hold values that would panic at upgrade. A real-server test also connects with `PRODUCTION`,
so the tested path is the shipped one.

### 4. Fail-closed token

`HUB_PUSH_TOKEN` is parsed in `main`, **before** `Database::new` and `start_collectors`, into
a push-auth policy instead of an `Option<PushToken>`:

```rust
enum PushAuth { Open, Required(PushToken) }

enum PushAuthError { NotUnicode }

impl PushAuth {
    fn from_env(value: Result<String, VarError>) -> Result<Self, PushAuthError>;
}
```

| Value | Outcome |
|---|---|
| unset or empty | `PushAuth::Open`, with the startup `warn` (as today) |
| valid UTF-8 | `PushAuth::Required(token)` |
| not UTF-8 | the hub refuses to start: `error` log naming the variable, never its value, and a non-zero exit |

An empty value means unset because compose files commonly write `VAR=` for that. Startup is
where configuration is verified (`CLAUDE.md`). An operator who set a token must never get an
open endpoint because it was malformed. `main` returns `ExitCode`, so this doesn't need a
panic.

### 5. Ids in logs

Every log line that names a system id prints it in `Debug` form, which quotes and escapes it:
`Push client authenticated`, `Push client disconnected`, `Push work for … failed`, and the new
ones. RFC 0005 already keeps a refused id out of the logs entirely. An id logged here has
passed `SystemId`, so it's at most 255 bytes, and `Debug` makes it inert.

### Configuration for tests

`router_with_config(state, PushConfig)` replaces `router_with_token`. `PushConfig` carries
`PushAuth`, `PushSocketLimits` and the two deadlines. `router(state, auth)` uses the
production values. Tests inject small deadlines and limits, so none of them waits 10 s or
touches the environment.

## Domain impact

- **Ingestion** (`push.rs`): the deadlines, bounded sends, `PushSocketLimits`, `PushAuth`, and
  Debug-form ids. The context-map row gains them.
- **Glossary:** **push token** changes: a set token is `PushAuth::Required`, and a non-UTF-8
  value refuses startup. There are no new domain terms. `PushSocketLimits` is adapter
  configuration, not domain vocabulary.
- **Published contracts:** the push frame's shape is untouched. The handshake gains one
  `auth_error` message, `handshake timeout`. Mixed-version fleets:
  - *Any agent, new hub:* every shipped agent sends auth immediately, pings every 30 s, and
    sends frames far below 512 KiB. So none trips a deadline or a limit, except a host with
    thousands of disks (see Rollout). A refused agent treats every `auth_error` alike and
    retries every 5 s.
  - *Old hub:* unchanged until upgraded.

## Alternatives considered

- **One timeout around the whole connection.** It would disconnect every agent on a
  schedule.
- **Close a connection whose write buffer is full.** tungstenite doesn't surface that on read,
  so the hub would have to send its own pings to detect it. A capped buffer is enough.
- **Check the limits only in tests.** The panic happens at upgrade in production. Only a
  compile-time check makes the shipped values safe.
- **Refuse pushes instead of refusing to start** on a non-UTF-8 token. It hides the mistake
  behind agent-side errors.
- **Treat an empty token as invalid.** It breaks compose files that write `VAR=` to mean
  unset.

## Security implications

- **API4:** bounds handshake time, idle time, sends, buffered writes, message and frame size.
  Still unbounded:
  - the number of connections (the rate-limiting gap);
  - per-frame ingestion work and live state (RFC 0007);
  - registration (RFC 0008).
- **A05 / API8:** a non-UTF-8 token now refuses startup.
- **A07 / API2:** the token comparison is unchanged: constant-time, and before the id. The
  deadline and size checks don't depend on content, so they leak nothing about the token.
- **A09:** every id is logged in `Debug` form, which closes the log-forging path. The
  configuration error names the variable, never the value.
- **API10:** frames are bounded before decoding.
- **A01 / API1:** unchanged. The id stays self-asserted.
- A02, A03, A04, A06 (no dependency change), A08, A10 / API7, API3, API5, API6, API9: not
  touched.

## Testing plan

TDD, per `CLAUDE.md`. `red-test-adversary` attacks every red test, and `rosette-auditor` runs
on the diff.

- **Pure:**
  - `PushAuth::from_env` over unset, empty, valid and `NotUnicode`.
  - `PushSocketLimits::new` accepts a valid set and panics on `max_write <= write` and on a
    zero message size (`#[should_panic]`, one test per rule).
- **Real server** (`axum::serve` on an ephemeral port, as today), through
  `router_with_config`:
  - **Production limits:** a client connecting with `PushSocketLimits::PRODUCTION`
    authenticates and has a frame stored. This guards the upgrade path.
  - **Handshake deadline:** a client that upgrades and sends nothing is answered
    `handshake timeout`, and the connection ends.
  - **Idle deadline:**
    - a client that authenticates and then **only pings**, at a third of the idle deadline,
      stays open for three deadlines;
    - a client that goes quiet past the deadline is closed and marked offline.
  - **Buffered pongs:** a client that sends many pings without reading, with a small injected
    write buffer, still has a frame sent afterwards stored.
  - **Size limits:**
    - a frame header declaring limit + 1 bytes, with no payload, closes the connection;
    - a message fragmented into frames within the limit, but over it in total, closes the
      connection;
    - an oversize handshake text closes the connection, and nothing is registered.
  - **Answer send failure:** a client that sends auth and then resets without reading leaves
    its system marked offline.
- The existing push tests move to `router_with_config`, and their assertions don't change.

## Impact on `docs/ARCHITECTURE.md`

- § Domain model: the Ingestion context-map row, and the **push token** glossary entry.
- § Trust boundaries, Agent → Hub (push): the deadlines, bounded sends, size limits, the
  fail-closed token, and Debug-form ids.
- § Testing architecture: `router_with_config` replaces `router_with_token`.
- § Open architectural questions:
  - Removed: the non-UTF-8 hub token, and the handshake timeout.
  - Rewritten: the frame-size entry (message size is bounded; per-frame work is RFC 0007).
  - Added: the agent never reads after the handshake (harmless now that pongs are capped).
- `README.md`: the `HUB_PUSH_TOKEN` row (non-UTF-8 refuses to start), `handshake timeout` in
  the `auth_error` table, and the limits in the push protocol section.

## Rollout / migration notes

Hub-only. No schema change. Rolling back is safe.

- A hub that ran with a non-UTF-8 `HUB_PUSH_TOKEN` refuses to start after the upgrade. Its
  log names the variable.
- A host whose frame exceeds 512 KiB (roughly 4,000 typical overlay mounts, or fewer if its
  mount paths are long) has each frame refused and its connection dropped. It then reconnects
  every push interval. The `warn` names the system. Until RFC 0007's disk rule lands, the only
  fix is a larger limit, which is a constant. This case is unlikely, but it's visible.
