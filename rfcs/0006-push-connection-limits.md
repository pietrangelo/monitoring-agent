# RFC 0006: Push Connection Limits and Fail-Closed Token

- Status: Implemented
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-25
- Affects: `system-hub`
- Depends on: RFC 0005 (implemented)
- Related:
  - RFC 0007 (push ingestion cost and live state) and RFC 0008 (push registry limit and push
    system lifecycle). All three come from one earlier draft of this RFC, which three
    `rfc-adversary` passes showed was spread across three bounded contexts (see git history).
    This RFC keeps the part that belongs to Ingestion's connection handling.
  - RFCs 0007 and 0008 extend `PushConfig` and `Refusal`, which this RFC introduces.

## Motivation

The push receiver (`system-hub/src/push.rs`) doesn't bound how long a connection lives or
how much it may send. It also opens the push endpoint when the operator set a token that
isn't UTF-8. When `HUB_PUSH_TOKEN` is unset, which is the README's basic start, the client
doing this can be anyone.

1. **No deadlines (API4).** `handshake` waits for the first message with no deadline, and
   `receive_frames` waits for each later one with no deadline. A client that upgrades and
   sends nothing, or authenticates and then goes quiet, keeps a task and its socket alive
   until TCP gives up.
2. **An unbounded send (API4).** `receive_frames` answers each ping with an explicit
   `Message::Pong` and awaits the send until it has flushed. Once the kernel buffers fill, a
   peer that pings but never reads leaves that await pending forever, outside any deadline.
3. **No size limits (API4).** The receiver uses tungstenite 0.24's defaults: a 64 MiB message
   and a 16 MiB frame, for the handshake text and data frames alike, and an unlimited write
   buffer (`max_write_buffer_size = usize::MAX`).
4. **A non-UTF-8 `HUB_PUSH_TOKEN` fails open (A05 / API8).** `PushToken::from_env` treats
   `Err(VarError::NotUnicode)` like an unset variable, so the operator sets a token and gets
   none.
5. **Raw ids in logs (A09).** `Push client authenticated: {}` and the other lines at
   `push.rs:184,189,250` print the self-asserted id with `Display`. The locked
   tracing-subscriber already escapes ESC, BEL and other control characters, but not `\n` or
   `\r`, so a client can forge log lines. It can also slip in bidi and zero-width characters.

## Proposed design

### 1. Deadlines

- **Handshake:** the first message must arrive within `HANDSHAKE_TIMEOUT = 10 s` of the
  upgrade. Otherwise the hub answers `auth_error` / `handshake timeout`, and the connection
  ends.
- **Idle:** after the handshake, some message must arrive within `IDLE_TIMEOUT = 90 s` of the
  previous one. Otherwise the connection ends, and the system is marked offline as on any
  disconnect. A data frame, a ping or a pong all count.
- Both deadlines are a `tokio::time::timeout` around `socket.recv()`. That's cancel-safe,
  because a partial frame stays in tungstenite's codec. The timeout wraps each receive, never
  the whole connection.
- **This is a published contract.** A push client must send its auth message within 10 s,
  and after that some message at least every 90 s. Pings count, and the hub answers them
  automatically. Every shipped agent sends auth as soon as it connects and pings every 30 s,
  whatever its push interval (`src/push.rs`, unchanged since the first commit). The README's
  push protocol section and ARCHITECTURE's published-contracts entry state these deadlines.

### 2. Bounded sends, and one exit

- **The explicit pong goes.** When tungstenite reads a ping, it queues a pong. The explicit
  pong replaces that one, so today one pong goes out, not two. It is also the only send after
  the handshake. Without it, the hub never awaits a send after answering the handshake.
- **The handshake answer, whatever happens to it, leads to one exit.** `handshake` returns
  the registered id whether or not its answer could be sent. If the send failed,
  `handle_push` skips `receive_frames` and falls through to the one `mark_offline` it already
  runs on every disconnect.
  - Today a failed answer is ignored, and the next `recv` on the broken socket ends the loop,
    which marks the system offline. So the only exit after registration that skips the
    marking is the `JoinError` in `register_if_new`, which RFC 0008 handles.
  - The answer is sent under `SEND_TIMEOUT = 5 s`. That is defence in depth: the answer is the
    first write, at most about 60 bytes, into an empty send buffer, so no test can make it
    block. It stays untested, and says so.
- **Buffered pongs are capped.** A peer that pings but never reads makes tungstenite buffer
  its automatic pongs. Once `max_write_buffer_size` is reached, tungstenite parks one pong and
  replaces it with each newer one, and it never errors on read. So the hub holds at most that
  many bytes plus one pong per connection, and the connection stays usable. The shipped agent
  never reads after the handshake, and that stays harmless, because the buffer is now
  bounded.
- A connection ends by dropping the socket, never by waiting on a Close frame.

### 3. Size limits as one checked value

```rust
/// The WebSocket limits of one push connection. tungstenite panics at upgrade unless
/// `max_write_buffer_size > write_buffer_size`, so construction checks it.
pub struct PushSocketLimits { /* private fields */ }

pub enum PushSocketLimitsError { ZeroMessageSize, WriteBufferNotBelowMax }

impl PushSocketLimits {
    pub const fn new(max_message: usize, write_buffer: usize, max_write_buffer: usize)
        -> Result<Self, PushSocketLimitsError>;

    pub const PRODUCTION: Self = match Self::new(512 * 1024, 8 * 1024, 64 * 1024) {
        Ok(limits) => limits,
        Err(_) => panic!("invalid PushSocketLimits::PRODUCTION"),
    };

    /// Sets all four limits on the upgrade: max message, max frame, write buffer, max write
    /// buffer. axum offers only scalar setters, so this is the one place they are called.
    pub fn apply(&self, upgrade: WebSocketUpgrade) -> WebSocketUpgrade;
}

// Forces evaluation at `cargo check` / clippy time, not only at codegen.
const _: PushSocketLimits = PushSocketLimits::PRODUCTION;
```

- `apply` is the only code that calls axum's four setters. axum keeps the resulting config
  private, so the buffered-pong test (below) proves the write-buffer cap end to end. That test
  goes red if `apply` forgets `max_write_buffer_size`, which would leave it at `usize::MAX`.
- `new` checks only what would panic or refuse everything: a zero message size, and a write
  buffer not below its cap. Limits a test injects must still fit an auth message and a
  handshake answer. That is the test's responsibility, and it's noted where they're built.
- **Message and frame size:** both 512 KiB. The frame limit is checked on the frame header,
  before any payload is buffered. The message limit is checked on the reassembled message, so
  a fragmented oversize message is refused too. 512 KiB fits the largest frame RFC 0007's disk
  rule accepts (1024 disks with 256-byte mount points, about 290 KB). A push host above about
  4,000 typical Docker overlay mounts would still exceed it (see Rollout).
- **Write buffer:** 8 KiB, capped at 64 KiB. The hub writes only handshake answers and pongs.
- **An oversize message:** tungstenite returns an error, not a close frame. Then:
  - **Before authentication:** the hub drops the socket at once and logs at `warn`. A
    shipped agent's auth message is about 100 bytes, so only a misbehaving client gets
    here, and it gets no answer.
  - **After `auth_ok`:** the hub logs at `warn` with the system id in `Debug` form, then
    **lingers for `OVERSIZE_LINGER = 30 s` without reading**, and then drops the socket.
    Shipped agents reconnect with no backoff after a dropped connection: their push loop
    returns `Ok` on a failed send, and `main` sleeps only on `Err`. Such an agent never reads
    after the handshake, so it finds out about the drop only through a send that fails after
    it. Holding the connection for the linger therefore spaces its reconnects to 30 s or more,
    where an immediate drop would let it loop as fast as the round trip allows.
  - **Cost:** one linger holds, per authenticated connection, a task and a socket for 30 s.
    It also holds up to about `2 × max_message + max_write_buffer` (about 1.1 MiB) of
    tungstenite buffers, plus the kernel receive queue. That's two: a message rejected at its
    second fragment keeps the first fragment and the unread second one.

### 4. Fail-closed token, and a `main` without panics

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
| not UTF-8 | refuse to start: an `error` log naming the variable, never its value, and a non-zero exit, before any file is created |

An empty value means unset because compose files commonly write `VAR=` for that. `main`
returns `ExitCode`. Since this RFC touches `main`, its other panics go too:

- a database that won't open;
- a port that won't bind;
- a server that fails.

Each becomes an `error` log and a non-zero exit, as `CLAUDE.md` asks for code it touches.

### 5. Ids in logs

Every log line that names a system id prints it in `Debug` form, which escapes `\n`, `\r`,
bidi overrides and zero-width characters. RFC 0005 already keeps a refused id out of the logs.
A logged id has passed `SystemId`, so it's at most 255 bytes.

### Configuration and outcomes

```rust
/// Everything a push router needs. RFCs 0007 and 0008 add fields.
pub struct PushConfig {
    pub auth: PushAuth,
    pub limits: PushSocketLimits,
    pub handshake_timeout: Duration,
    pub idle_timeout: Duration,
    pub oversize_linger: Duration,
}

/// Why a handshake got no `auth_ok`. RFC 0008 adds registry outcomes.
enum Refusal {
    Rejected(HandshakeRejection), // shape, token, system id (wire messages unchanged)
    Timeout,                      // "handshake timeout"
}

/// How a handshake ended, so `handle_push` has one exit per case.
enum Handshake {
    /// Registered; `answer` says whether `auth_ok` was delivered. `Failed` skips
    /// `receive_frames` and goes straight to the one `mark_offline`.
    Authenticated { id: SystemId, answer: Answer },
    /// Answered with an `auth_error`; nothing was registered.
    Refused(Refusal),
    /// The socket ended, errored, sent an oversize message, or sent a first message that
    /// isn't text (binary, ping or pong) before any auth message: no answer, nothing
    /// registered.
    Closed,
}

enum Answer { Delivered, Failed }
```

`router_with_config(state, PushConfig)` replaces `router_with_token`, and `router(state,
auth)` uses the production values.

## Domain impact

- **Ingestion** (`push.rs`): the deadlines, bounded sends, the single exit, the linger,
  `PushSocketLimits`, `PushAuth`, `Refusal`, and Debug-form ids. The context-map row gains
  these.
- **Glossary:**
  - **push token**: a set token is `PushAuth::Required`, and a non-UTF-8 value refuses
    startup;
  - **system status**: a push system is marked offline when its connection ends, and a
    connection ends at the latest 90 s after its last message, or 30 s after an oversize
    one.

  `PushSocketLimits` is adapter configuration, not domain vocabulary.
- **Published contracts:** the push frame's shape is untouched. The handshake gains
  `auth_error` / `handshake timeout`, and the connection gains its two deadlines (§1). Both
  are documented. Mixed-version fleets:
  - *Any agent, new hub:* every shipped agent sends auth immediately, pings every 30 s, and
    sends frames far below 512 KiB. So none trips a deadline or a limit, except a host with
    thousands of Docker overlay mounts (see Rollout).
  - *Refused agents:* they back off 5 s after an `auth_error`. After a dropped connection they
    back off only as long as the linger or the idle cut makes them wait.
  - *Old hub:* unchanged until upgraded.

## Alternatives considered

- **One timeout around the whole connection.** It would disconnect every agent on a schedule.
- **Close a connection whose write buffer is full.** tungstenite doesn't surface that on read,
  so the hub would have to send its own pings to find out. A capped buffer is enough.
- **A `const fn` that panics directly.** It works too, but `CLAUDE.md` prefers typed errors.
  With a `Result`, the tests are a table over the error variants.
- **Drop an oversize connection at once after auth too.** Shipped agents would reconnect in a
  tight loop. Before auth there is no agent to protect, so it is dropped at once.
- **Refuse pushes instead of refusing to start** on a non-UTF-8 token. It hides the mistake
  behind agent-side errors.
- **Treat an empty token as invalid.** It breaks compose files that write `VAR=` to mean
  unset.

## Security implications

- **API4.** This RFC bounds, once a WebSocket is upgraded:
  - handshake time and idle time;
  - sends and buffered writes;
  - message and frame size;
  - the reconnect rate of an authenticated host whose frames are too big, at the stated cost
    of one linger per such connection.

  Still unbounded:
  - the number of connections (the rate-limiting gap);
  - **connections that never finish their HTTP request.** `axum::serve` gives hyper no timer,
    so hyper's 30 s header-read timeout is off for every route. The fix is a hyper-util server
    with `TokioTimer`, which touches every route and needs its own RFC, so it's recorded as an
    open question;
  - per-frame ingestion work and live state (RFC 0007);
  - registration (RFC 0008).
- **A05 / API8:** a non-UTF-8 token refuses startup, before any file is created.
- **A07 / API2:** the token comparison is unchanged: constant-time, and before the id. The
  deadline and size checks don't depend on content, so they leak nothing about the token.
- **A09:** ids are logged in `Debug` form, and the configuration error names the variable,
  never its value.
- **API9:** the connection deadlines become part of the documented push protocol.
- **API10:** frames are bounded before they are decoded.
- **A01 / API1:** unchanged. The id stays self-asserted.
- A02, A03, A04, A06 (no dependency change), A08, A10 / API7, API3, API5, API6: not touched.

## Testing plan

TDD, per `CLAUDE.md`. `red-test-adversary` attacks every red test, and `rosette-auditor` runs
on the diff.

- **Pure, table-driven:**
  - `PushAuth::from_env` over unset, empty, valid and `NotUnicode`.
  - `PushSocketLimits::new`:
    - a valid set;
    - `ZeroMessageSize`;
    - `WriteBufferNotBelowMax`, with equal and with greater write buffers;
- **Binary** (a new `system-hub/tests/` integration test running
  `env!("CARGO_BIN_EXE_system-hub")` in a temp dir, `#[cfg(unix)]`):
  - The hub is spawned with `tokio::process::Command` and `kill_on_drop(true)`, under a 10 s
    timeout, with `RUST_LOG` removed from its environment. The first assertion is that it
    **exited**. On today's code it serves forever, so the red run fails on that assertion
    instead of hanging.
  - `HUB_PUSH_TOKEN` is `b"leak-marker-\xff"`, set through `OsStrExt::from_bytes`. The hub:
    - exits non-zero;
    - names `HUB_PUSH_TOKEN` in its output (tracing writes to **stdout**);
    - never shows `leak-marker` in stdout or stderr;
    - creates no `system-hub.db`.
- **Real server** (`axum::serve` on an ephemeral port), through `router_with_config`:
  - **Production limits:** a client on `PushSocketLimits::PRODUCTION` authenticates and has a
    frame stored. If the upgrade panicked, this goes red.
  - **Handshake deadline:** a client that upgrades and sends nothing gets `handshake timeout`.
  - **Idle deadline:**
    - a client that authenticates and then only pings, at a third of the idle deadline, stays
      open for three deadlines;
    - the same client going quiet is closed and marked offline.
  - **Buffered pongs:** the client's receive buffer and the listener's send buffer are
    shrunk (`TcpSocket::set_recv_buffer_size` and `set_send_buffer_size`). The ping flood is
    sized from them, and each ping carries a sequence number. The client pings without
    reading, then sends a frame, which must still be stored. It then reads the pongs, which
    must show a **gap**: at least one ping's pong is missing while a later one arrives. Only a
    capped buffer does that, because tungstenite parks one pong and replaces it with each
    newer one. An uncapped buffer returns every pong in order.
    - To collect the parked pong, the client keeps sending pings numbered N, N + 1, … while it
      reads, until a pong numbered N or higher arrives. The gap is then asserted over
      everything received.
    - The test runs on the current-thread runtime.
    - The effective (kernel-doubled) send and receive buffers are well below the injected
      `max_write_buffer`, and the flood is larger than `2 × (both buffers) + max_write_buffer`.
    - `red-test-adversary` runs it in mutation mode against today's explicit-pong loop and
      against an `apply` that forgets the cap. Both must go red.
  - **Size limits**, with **long** deadlines, asserting the close arrives well before them:
    - a frame header declaring limit + 1 bytes, with no payload;
    - a message fragmented into frames within the limit but over it in total;
    - a **valid auth message padded** past the limit, with an injected `oversize_linger` as
      long as the deadlines: the close arrives well before it (no linger before auth), with
      no `auth_error`, and nothing is registered.
  - **Linger:** after `auth_ok` and an oversize frame, the socket stays open without reading
    for the injected linger, then closes.
  - **Answer send failure** (characterisation, in mutation mode): a client that sends auth
    and resets without reading leaves its system marked offline.
- Existing push tests move to `router_with_config` with production-length deadlines, and
  their assertions don't change.

## Impact on `docs/ARCHITECTURE.md`

- § Domain model:
  - the Ingestion context-map row;
  - the **push token** and **system status** glossary entries;
  - the published-contracts entries for the push handshake (the two deadlines) and the push
    frame (at most 512 KiB).
- § Trust boundaries, Agent → Hub (push): the deadlines, bounded sends, the single exit,
  size limits, the linger, the fail-closed token and Debug-form ids.
- § Testing architecture: `router_with_config` replaces `router_with_token`, and
  `system-hub/tests/` runs the binary.
- § Open architectural questions:
  - Removed: the non-UTF-8 hub token; the handshake timeout.
  - Rewritten: the frame-size entry (message size is bounded; per-frame work is RFC 0007).
  - Added:
    - no hyper timer under `axum::serve` (requests that never finish);
    - the agent never reads after the handshake (harmless now that pongs are capped).
  - Edited: the existing entry "main reconnects without backoff" notes that the linger spaces
    reconnects after an oversize drop.
- `README.md`:
  - the `HUB_PUSH_TOKEN` row (non-UTF-8 refuses to start);
  - in the push protocol section: `handshake timeout` in the `auth_error` table; the two
    deadlines (pings count); the 512 KiB limit; and what an oversize client sees. That is no
    answer and no Close frame: before auth an immediate drop, after auth 30 s of silence. Then
    the connection closes, with a reset if unsent data is still queued.
  - the `expected auth message` row: it applies to a first *text* message that isn't a valid
    auth message. A first message that isn't text gets no answer.

## Rollout / migration notes

Hub-only. No schema change. Rolling back is safe.

- A hub that ran with a non-UTF-8 `HUB_PUSH_TOKEN` refuses to start after the upgrade. Its log
  names the variable.
- A push host whose frame exceeds 512 KiB (roughly 4,000 typical Docker overlay mounts, or
  fewer with long mount paths) never has a frame stored. It reconnects about every 30 s, with a
  `warn` naming the system each time. RFC 0007's disk rule can't help here, because the frame
  is refused before it is decoded. The fix is a larger limit, which is a constant.
