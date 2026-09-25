# RFC 0006: Push Receiver Resource Limits and Fail-Closed Configuration

- Status: Draft
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-25
- Affects: `system-hub`
- Depends on: RFC 0005 (`SystemIdError`, and the 255-byte system id limit), which has landed

## Motivation

RFC 0003 hardened the push handshake's comparison and parsing. It deferred several gaps in the
receiver (`system-hub/src/push.rs`), listed in `docs/ARCHITECTURE.md` § Open architectural
questions. Each lets one client make the hub hold resources without limit, or leaves the push
endpoint open when the operator meant to close it. When `HUB_PUSH_TOKEN` is unset, which is
the README's basic start, that client can be anyone.

1. **Connections live forever (API4).** `handshake` waits for the first message with no
   deadline, and `receive_frames` waits for each later one with no deadline. The hub also
   answers every ping with an explicit pong. It awaits that send until it has flushed, so
   when the peer never reads, the task hangs in `send`. Either way, a task, its socket and
   its buffers stay alive until TCP gives up.
2. **Message size isn't limited (API4).** The receiver uses tungstenite 0.24's defaults:
   - a 64 MiB message and a 16 MiB frame, for the handshake text and for data frames alike;
   - an unlimited write buffer.
3. **Per-frame work isn't bounded (API4).** Each disk in a frame becomes one `insert_metric`
   call: an INSERT, a SELECT and a DELETE, each write committing on its own, under the hub's
   single SQLite mutex. That mutex is also taken by REST and SSE handlers. Each disk also
   becomes one entry in `live_metrics`, with a mount point of any length. A 1 MiB frame can
   carry more than 100,000 minimal disk entries, and frames can arrive back to back.
   Measured with the hub's schema and statements: a 5-disk frame costs 15.4 ms of serial
   SQLite work, and a 1024-disk frame 1,206 ms.
4. **Auto-registration is unbounded (API4).** Every accepted handshake with an unseen id
   inserts a permanent, enabled `systems` row, and the poller visits every enabled row every
   30 s. Push systems are polled too. Their url is `push://`, which reqwest rejects, so each
   poll marks them offline until their next frame. That's why "delete offline" in the
   dashboard can delete a live push system.
5. **A deleted system's live state survives.** Deleting a system removes its rows, and the
   bundled SQLite enforces foreign keys, so its open push connection can't store metric rows
   any more. The insert fails, and the error is discarded. But the connection keeps
   refreshing the system's `live_metrics` entry, and nothing ever removes one: not a delete,
   and not a disconnect. The SSE summary sends every entry to every dashboard every 5 s.
6. **A non-UTF-8 `HUB_PUSH_TOKEN` fails open (A05 / API8).** `PushToken::from_env` treats
   `Err(VarError::NotUnicode)` like an unset variable, so the operator sets a token and gets
   none.
7. **Raw ids reach the logs (A09).** `Push client authenticated: {id}` and similar lines print
   the self-asserted id with `Display`, so a client can forge log lines with `\n` or terminal
   escapes.

## Proposed design

The rules below are domain values with pure constructors and predicates. The adapter reads
the clock, the socket and SQLite, and passes plain values in.

### 1. Deadlines, and no unbounded sends

- **Handshake:** the first message must arrive within `HANDSHAKE_TIMEOUT = 10 s` of the
  upgrade. Otherwise the hub answers `auth_error` / `handshake timeout`, and the connection
  ends. The agent sends its auth message as soon as it connects.
- **Idle connection:** after the handshake, any message must arrive within
  `IDLE_TIMEOUT = 90 s` of the previous one. Otherwise the connection ends and the system is
  marked offline. A data frame, a ping or a pong all count. Every shipped agent pings every
  30 s whatever its push interval (`src/push.rs`, unchanged since the first commit), so a live
  agent never comes close.
- **No send without a bound:**
  - The explicit pong goes. tungstenite already answers every ping, so today the hub sends
    two pongs per ping.
  - `max_write_buffer_size` is set to `MAX_PUSH_WRITE_BUFFER_BYTES = 64 KiB`, so a peer that
    never reads fills the buffer. The next write then fails, and the connection ends.
  - Handshake answers are sent under `SEND_TIMEOUT = 5 s`.
  - A connection ends by dropping the socket, never by waiting on a Close frame.
- Deadlines are `tokio::time::timeout` in the adapter. Every await on the socket has one.

The shipped agent never reads after the handshake, so tungstenite's automatic pongs collect
in its receive buffer. Once that buffer is full, which takes weeks at two bytes every 30 s,
the hub's write buffer fills and the connection ends. The agent then reconnects. That is
self-healing, and it's recorded as an agent-side open question.

### 2. Message size

`push_handler` sets both `max_message_size` and `max_frame_size` to
`MAX_PUSH_MESSAGE_BYTES = 256 KiB`. Both are needed:

- The frame limit is checked on the frame header, before the payload is buffered.
- The message limit is checked on the reassembled message, so a fragmented oversize message
  is also refused.

256 KiB is twice what 1024 Docker overlay mounts need (about 123 KB). An agent's frame
carries fixed fields, at most 10 processes, and one small entry per disk.

tungstenite answers an oversize message with an error, not a close frame. The hub logs it at
`warn` (in `handshake` and in `receive_frames`) and drops the connection. It sends no
`auth_error`, because a client sending oversize messages isn't reading answers.

### 3. Frame rules

These are checked after decoding, before any SQLite or cache work.

- **Disks:** a frame's disk list is used only if it has at most `MAX_DISKS_PER_FRAME = 1024`
  entries, and every mount point is at most `MAX_MOUNT_POINT_BYTES = 256` bytes with no
  control characters. Otherwise the frame's scalar metrics are still stored and its disks
  are skipped, logged at `warn` once per connection. A host with too many disks keeps its
  CPU, memory and load, and shows no disks rather than a partial set. sysinfo leaves out
  tmpfs, `/proc`, `/sys` and `/run`, but not Docker overlays, ZFS datasets or CSI mounts. A
  host with more than 1024 of those is the case this rule changes (see Rollout).
- **Pacing:** a frame read less than `MIN_FRAME_SPACING = 1 s` after the last accepted frame
  on the same connection is dropped. The agent pushes at most every 2 s, and a catch-up tick
  after an agent-side stall is a near-duplicate. The hub measures time when it reads, so
  frames that were buffered during a hub-side stall are read back to back and all but the
  first are dropped. Those samples are already late, and dropping them is accepted.

### 4. One transaction per frame

`Database` gets `store_snapshot(system_id, points, timestamp) -> Result<Stored, Error>`, with
`Stored::{Stored, SystemGone}`:

- it holds the connection guard for the whole frame;
- inside one transaction, it checks that the system's row exists, then inserts every metric
  point and applies retention exactly as `insert_metric` does today;
- it returns `SystemGone`, without writing anything, when the row is missing.

Measured with the hub's schema, statements and SQLite defaults: a 5-disk frame drops from
15.4 ms to **1.7 ms**, and a 1024-disk frame from 1,206 ms to **23.9 ms**. Every legitimate
agent gets cheaper about ninefold. At the worst frame and the 1 s spacing, one connection
holds the mutex about 2.4 % of the time. Retention stays in SQL for now. That is an existing
open question, and this method doesn't make it worse.

### 5. Bounded auto-registration

A **push system** is a system registered from a push handshake. Its url is the constant
`PUSH_SYSTEM_URL = "push://"`, which replaces the literal in `register_if_new`.

- **Registry limit.**

  ```rust
  /// The most push systems the hub will auto-register. Never zero.
  pub struct PushRegistryLimit(NonZeroUsize);

  impl PushRegistryLimit {
      pub const DEFAULT: usize = 1000;
      /// Whether a registry holding `push_systems` push systems may register one more.
      pub fn admits(&self, push_systems: usize) -> bool;
  }

  pub enum Registration { AlreadyRegistered, Registered, RegistryFull }

  pub fn register_push_system(      // on Database
      &self,
      system: &SystemInfo,
      admits: impl FnOnce(usize) -> bool,
  ) -> Result<Registration, rusqlite::Error>;
  ```

  - `register_push_system` holds the single connection guard across the existence check,
    `SELECT COUNT(*) FROM systems WHERE url = ?1` (with `PUSH_SYSTEM_URL` bound) and the
    insert. It calls `admits` in between.
  - It must not call `get_system` or `insert_system`, because a std `Mutex` isn't reentrant.
  - The limit is exact, and an id racing itself gets `AlreadyRegistered`.
  - `on_blocking_pool` becomes generic over its return value, so it can carry the
    `Registration` back.
- **Handshake outcomes.**
  - A known id is unaffected.
  - An unseen id beyond the limit is answered `auth_error` / `registry full`.
  - A database error is answered `auth_error` / `registry unavailable`. The agent then takes
    its 5 s backoff instead of reconnecting at once, which is what it does on a missing answer.
- **Logging.** The first refusal after the registry fills is logged at `warn`, once, until it
  has room again (an `AtomicBool` in the app state). Each refused id is logged at `debug`.
- **Push systems aren't polled.** `start_collectors` skips rows whose url is
  `PUSH_SYSTEM_URL`, so a live push system is no longer marked offline by a failed poll, and
  "delete offline" deletes only systems that really are offline. No push connection survives
  a restart, so at startup the hub marks every push system offline. Each is marked online
  again by its first frame. Without this, a push system that was online at shutdown would
  stay online forever. That includes a stored id that breaks the RFC 0005 rule, which can
  never push again.

Operators free a slot by deleting an offline push system, or they raise the limit. Deleting a
push system that is still connected is undone within seconds: its connection ends (§6), and
the agent reconnects and registers again.

### 6. A deleted system's connection ends

- A frame whose `store_snapshot` returns `SystemGone` is dropped, and the connection ends.
  This is logged at `info`.
- A frame whose `store_snapshot` fails is dropped and logged at `warn`, and the connection
  stays open. A database error isn't a deletion.
- When a push connection ends, the hub removes the system's `live_metrics` entry after marking
  it offline. This runs after the connection's last ingest, so a frame racing a delete can't
  leave a ghost entry behind.
- `DELETE /api/systems/:id` also removes the system's `live_metrics` entry, for push and
  polled systems alike.

### 7. Fail-closed configuration

Both variables are parsed in `main` **before** `Database::new` and `start_collectors`. A value
that is set but invalid makes the hub refuse to start. It logs the variable's name and the
reason at `error`, never the value, and exits non-zero.

| Variable | Unset or empty | Valid | Refuses to start |
|---|---|---|---|
| `HUB_PUSH_TOKEN` | push auth disabled, startup `warn` (as today) | `PushAuth::Required(token)` | not UTF-8 |
| `HUB_MAX_PUSH_SYSTEMS` | `PushRegistryLimit::DEFAULT` (1000) | a whole number from 1 to `usize::MAX` | not UTF-8, `0`, negative or overflowing, anything else |

```rust
enum PushAuth { Open, Required(PushToken) }        // replaces Option<PushToken>
enum PushConfigError {
    TokenNotUnicode,
    LimitNotUnicode,
    LimitZero,
    LimitOutOfRange,   // negative, or larger than usize::MAX
    LimitNotANumber,
}
```

An empty value means unset because compose files commonly write `VAR=` for that. Startup is
where configuration is verified (`CLAUDE.md`).

### 8. Ids in logs

Every log line that names a system id prints it in `Debug` form, which quotes and escapes it.
That covers the existing lines (`Push client authenticated`, `Push client disconnected`,
`Push work for … failed`) and the new ones. RFC 0005 already keeps a refused id out of the
logs entirely. An id logged here has passed `SystemId`, so it is at most 255 bytes, and
`Debug` makes it inert.

### Adapter outcome

`authenticate` stays a pure function returning `Result<SystemId, HandshakeRejection>`. The
adapter has its own outcome for what only the socket or the registry can decide:

```rust
enum Refusal {
    Rejected(HandshakeRejection), // shape, token, system id (wire messages unchanged)
    Timeout,                      // "handshake timeout"
    RegistryFull,                 // "registry full"
    RegistryUnavailable,          // "registry unavailable"
}
```

## Domain impact

- **Ingestion** (`push.rs`): the deadlines, the size limits, the frame rules and `Refusal`.
  Also the context-map row.
- **Fleet Registry** (`models.rs`, `db.rs`, `collector.rs`): `PushRegistryLimit`,
  `PUSH_SYSTEM_URL`, `register_push_system`, startup offline marking, and the poller skipping
  push systems.
- **Fleet History** (`db.rs`): `store_snapshot` writes one frame in one transaction.
- **Glossary:**
  - adds **push system**: a system registered from a push handshake, with url `push://`. It is
    never polled, and the hub marks it offline at startup;
  - adds **push registry limit**: the most push systems the hub will auto-register
    (`PushRegistryLimit`);
  - changes **push token**: a set token is `PushAuth::Required`, and a non-UTF-8 value refuses
    startup.
- **Published contracts:** the push frame's shape is untouched, but what the hub accepts
  narrows. The handshake gains three `auth_error` messages: `handshake timeout`,
  `registry full` and `registry unavailable`. Mixed-version fleets:
  - *Any agent, new hub:* agents send auth immediately, ping every 30 s, push at most every
    2 s, and send frames far below 256 KiB. So no current or older agent trips a deadline or a
    limit, except a host with more than 1024 disks, which keeps its scalars. A refused agent
    treats every `auth_error` alike and retries every 5 s.
  - *Agents whose id changes* (a container recreated without `/etc/machine-id`, which then
    falls back to its hostname, or the random-UUID fallback) leave an offline push system
    behind at each change. Nothing expires those rows, so churn fills the limit over time. The
    registry-full `warn` and "delete offline" handle it. Expiring stale push systems is
    recorded as an open question.
  - *Old hub:* unchanged until upgraded. It ignores `HUB_MAX_PUSH_SYSTEMS`.

## Alternatives considered

- **Do nothing.** Each gap is a one-client resource exhaustion or a silent auth bypass.
- **An unlimited registry default.** No behaviour change on upgrade, but the default stays
  unbounded. The owner chose 1000.
- **Refuse a whole frame with too many disks.** The host would go dark, when today it works.
- **Truncate the disk list.** The host would silently show a partial set of disks.
- **The registry rule as one SQL statement** (`INSERT … SELECT … WHERE (SELECT COUNT(*) …) < ?`).
  That puts a business rule in an SQL string (`CLAUDE.md`), and it can't tell "full" from
  "registered meanwhile".
- **Record how a system was registered in its own column,** instead of recognising a push
  system by its `push://` url. That is cleaner, because `PUT` can't rewrite it by accident.
  But it's a schema change with a backfill. `PUSH_SYSTEM_URL` stays the marker until a schema
  RFC needs the column anyway.
- **Pace frames by the frame's own `timestamp`.** It's chosen by the client, so it protects
  nothing.
- **A global connection limit** (for example `tower::limit::ConcurrencyLimitLayer`). It's
  complementary, and it belongs with the general rate-limiting gap.
- **Pre-registration only** (only ids an operator registered may push). This is the strongest
  control. But it needs an API that accepts a chosen id, so it belongs with per-system push
  credentials.
- **Refuse pushes instead of refusing to start on bad configuration.** It hides the mistake
  behind agent-side errors.
- **Treat an empty value as invalid.** It breaks compose files that write `VAR=` to mean
  unset.

## Security implications

- **API4 Unrestricted Resource Consumption.** This is the purpose of the RFC. Per connection,
  it bounds:
  - handshake and idle time;
  - buffered sends;
  - message size;
  - disks per frame, and mount point size;
  - frame rate.

  Per frame, one transaction bounds SQLite work to about 24 ms at worst. What stays unbounded
  or merely bounded:
  - **Connections.** A client can open many and keep each alive with a ping every 89 s. Each
    one it keeps sending frames costs at most about 24 ms of mutex time per second. About 40
    such connections saturate the mutex, and so the dashboard. This belongs with rate
    limiting.
  - **Live state.** At most one `live_metrics` entry per connected system, each bounded by a
    frame (256 KiB). An attacker holding `limit` connections pins about
    `1000 × 256 KiB ≈ 250 MiB`. SSE clones and serialises that for each subscriber every 5 s
    (no control characters, so escaping at most doubles it). Setting a token removes this for
    anyone who doesn't hold it.
  - **REST-created rows.** The limit counts push systems only. `POST /api/systems` stays an
    unbounded registration path, and `PUT /api/systems/:id` can rewrite a push system's url
    so it stops counting. The hub has no client authentication, so **the limit holds only
    where push clients can't reach the REST API** (for example, a proxy that exposes only
    `/api/push` to agents).
  - **Stored metric rows over time.** Pruning is keyed on the frame's client-chosen
    `timestamp`, and a mount name that appears once is never pruned again. Recorded as an
    open question.
- **A04 / API6: onboarding lock-out, accepted.** Registration is permanent and happens before
  `auth_ok`. So whoever can reach the push endpoint (every token holder, or anyone while the
  token is unset) can fill the registry with 1000 handshakes. From then on, every new or
  recreated host gets `registry full`. The attacker refills slots faster than agents retry,
  so deleting offline systems loses that race. The remedies are to set `HUB_PUSH_TOKEN`, raise
  the limit, and keep `/api/push` off networks the fleet doesn't need. The trade is accepted:
  unbounded rows cost every operator, while the lock-out needs an attacker who already has
  push access.
- **A05 / API8 Misconfiguration:** a non-UTF-8 token or a malformed limit now refuses startup.
- **A07 / API2 Authentication:** the token comparison is unchanged: constant-time, and
  checked before the id. The deadline and size checks don't depend on content, so they leak
  nothing about the token. `registry full` and `registry unavailable` come after
  authentication, so only an authenticated client learns about the registry.
- **A09 Logging:**
  - every refusal, skipped disk list, drop and size error is logged, with the per-connection
    and registry-full rules above keeping a steady state quiet;
  - every id is logged in `Debug` form, which closes the log-forging path in today's lines;
  - configuration errors name the variable, never its value.
- **A01 / API1:** unchanged. The id stays self-asserted.
- **A03 Injection:** the count query is a fixed string with a bound parameter.
- **API10 Unsafe Consumption:** frames are bounded before decoding and checked before any
  storage.
- **Agent side (out of scope):** the agent fails open on a non-UTF-8 `SYSTEM_AGENT_TOKEN`
  (`src/auth.rs`), and it never reads after the handshake. Both are recorded as open questions.
- A02, A06 (no dependency change), A08, A10 / API7, API3, API5, API9 (no new route): not
  touched.

## Testing plan

TDD, per `CLAUDE.md`. `red-test-adversary` attacks every red test, and `rosette-auditor` runs
on the diff.

- **Pure, table-driven:**
  - The configuration parser, one row per cell of the table in §7: unset, empty, `1`,
    `1000`, `0`, `-1`, `1,000`, `18446744073709551616` and non-UTF-8, each with its
    `PushConfigError`.
  - `PushRegistryLimit::admits`: below the limit, at it, and above it.
  - The disk rule:
    - 1024 and 1025 disks;
    - a 256-byte and a 257-byte mount point;
    - a mount point containing `\n`.
  - The pacing rule: 0.999 s, 1 s, and no previous frame.
- **`Database`** (temp SQLite):
  - `register_push_system`:
    - a limit of 1 with one polled `http://` system present admits an unseen push id, so only
      push systems count;
    - a limit of 1 with one push system present refuses an unseen id, inserts nothing, and
      returns `AlreadyRegistered` for the known id.
  - `store_snapshot`:
    - it writes every point of a frame;
    - it returns `SystemGone` and writes nothing when the row is missing;
    - a failure part-way leaves no partial frame.
  - Startup offline marking touches push systems only.
- **Collector:** `start_collectors`' selection skips a push system and polls an `http://`
  one.
- **Real server** (`axum::serve` on an ephemeral port, as the push tests do today). Every
  limit, deadline, the spacing and the write-buffer size are injected through
  `router_with_config`, which replaces `router_with_token`. So no test waits 10 s, and the
  existing two-frame tests run with spacing 0.
  - A client that upgrades and sends nothing is answered `handshake timeout`, and the
    connection ends.
  - A client that authenticates and then **only pings**, at a third of the idle deadline,
    stays open for three deadlines. The same client going quiet past the deadline is closed
    and marked offline. This rules out one timeout wrapped around the whole connection.
  - A client that pings but never reads is closed once the write buffer fills.
  - Frame and message size:
    - a frame header declaring limit + 1 bytes, with no payload, closes the connection;
    - a message fragmented into frames within the limit but together over it closes the
      connection;
    - an oversize handshake text closes the connection, and nothing is registered.
  - A frame with 1025 disks stores its scalars and no disks.
  - Pacing:
    - two frames 10 ms apart (spacing 1 s) store once;
    - two frames that are the spacing apart both store.
  - A full registry answers `registry full` to an unseen id. A registry whose `systems` table
    has been dropped answers `registry unavailable`.
  - Deleting a connected system ends its connection and removes its `live_metrics` entry. A
    connection that ends for any reason removes the entry.

## Impact on `docs/ARCHITECTURE.md`

- § Components (the push receiver): registration is bounded, and push systems aren't polled.
- § Domain model:
  - context map: the Ingestion and Fleet Registry rows;
  - glossary: **push system** and **push registry limit** are new, and **push token**
    changes.
- § Trust boundaries, Agent → Hub (push): the deadlines, send bounds, size and frame limits,
  the registry limit, deleted systems, logging, and the fail-closed configuration.
- § Testing architecture: `router_with_config` replaces `router_with_token`.
- § Open architectural questions:
  - Removed:
    - the non-UTF-8 hub token;
    - the handshake timeout;
    - unbounded auto-registration;
    - push systems being polled;
    - the offline-marking note from RFC 0005's stored-id entry, since push systems are now
      marked offline at startup.
  - Rewritten: frame size (now bounded).
  - Added:
    - expiring stale push systems;
    - the client-chosen `timestamp` driving pruning, and one-off mount names never pruned;
    - REST-created systems as an unbounded registration path;
    - the connection count;
    - the agent's non-UTF-8 `SYSTEM_AGENT_TOKEN`;
    - the agent never reading after the handshake.
  - Kept: rate limiting and per-system credentials.
- `README.md`:
  - the `HUB_PUSH_TOKEN` row (non-UTF-8 refuses to start), plus a new `HUB_MAX_PUSH_SYSTEMS`
    row;
  - the new `auth_error` messages and the limits, in the push protocol section;
  - "they appear automatically" (up to the limit);
  - the `DELETE /api/systems/{id}` row, which now also ends a push connection.
- `docker-compose.yml`: `HUB_MAX_PUSH_SYSTEMS: ${HUB_MAX_PUSH_SYSTEMS:-}` on the hub (empty
  means the default). `.env.example`: a commented `HUB_MAX_PUSH_SYSTEMS`.
- `rfcs/README.md`: the index row.

## Rollout / migration notes

Hub-only. No schema change. Rolling back is safe: an old hub ignores the new variable, and
reads the same rows.

- **Configuration:** a hub that ran with a non-UTF-8 `HUB_PUSH_TOKEN`, or that gets a
  malformed `HUB_MAX_PUSH_SYSTEMS`, refuses to start after the upgrade. The log names the
  variable.
- **Registry limit:** the limit counts every row whose url is `push://`, live or stale. To see
  where a hub stands before upgrading, run
  `sqlite3 system-hub.db "SELECT COUNT(*) FROM systems WHERE url = 'push://';"`. A hub at or
  over 1000 keeps every row, but refuses new ids until it's under the limit. Delete stale
  push systems first, or set `HUB_MAX_PUSH_SYSTEMS`.
- **Hosts with more than 1024 disks:** they keep their CPU, memory and load, but show no
  disks. The hub logs a `warn` once per connection that names the system.
- **Status after upgrade:** push systems start offline, and turn online with their first
  frame, within one push interval.

## Review status

Three `rfc-adversary` passes so far. The third pass left these findings open. They must be
addressed before this RFC can be `Accepted`:

- **CONFIRMED: the write-buffer settings would panic.** tungstenite asserts
  `max_write_buffer_size > write_buffer_size`, and the default `write_buffer_size` is 128 KiB.
  With `max_write_buffer_size = 64 KiB`, every upgrade would panic before `handle_push` runs.
  Fix: set both values, in a value object whose constructor enforces the order, and add a
  real-server test that uses the production defaults.
- **CONFIRMED: a full write buffer never ends a connection.** tungstenite parks an overflowing
  pong and replaces it with the next one, so `recv` never errors. The hub also sends one pong
  per ping today, not two. Fix: state the guarantee as "buffered pongs are capped per
  connection", drop the self-healing paragraph, and replace the "closed once the buffer fills"
  test.
- **CONFIRMED: the limits disagree about disk-heavy hosts.** 1024 disks with 256-byte mount
  points make a frame of about 290 KB, over the 256 KiB message limit. A host with more than
  about 2,100 overlay mounts loses its connection, not just its disks. Fix: raise the message
  limit to 512 KiB, and put the byte threshold in Rollout.
- **CONFIRMED: the cost figures miss per-frame work.** Each frame also runs an autocommit
  status update and `refresh_cache`, a full `systems` scan whose result only the poller reads.
  Fix: fold the status update into `store_snapshot`, drop the per-frame refresh, and restate
  the figures end to end, with the storage they were measured on.
- **CONFIRMED: SSE multiplies live state.** It clones the map and serialises it once per
  subscriber, making about four copies (about 1.3–1.6 GiB per subscriber per tick at the worst
  case), and subscribers are unbounded. Fix: serialise once per tick into a shared `Arc<str>`,
  or state the real worst case.
- **CONFIRMED: the README's deployments break the limit's precondition.** Its patterns expose
  REST and push together. Fix: document an nginx block that exposes only `/api/push` to agent
  networks, or call the limit advisory where REST is reachable.
- **CONFIRMED: the disk rule is in the push adapter only.** The poll path still stores every
  disk. Fix: make it a Fleet History domain function that both paths call.
- **PLAUSIBLE: a local FUSE mount can hide every disk.** A mount point with a tab, or longer
  than 256 bytes, drops the whole list. Fix: drop only the invalid entries.
- **PLAUSIBLE: some exits leave a push system online.** Exits after registration (`auth_ok`
  failing to send, a `JoinError`) skip the offline marking. Fix: every exit after registration
  marks the system offline and evicts its live entry, and `JoinError` becomes
  `RegistryUnavailable`.
- **PLAUSIBLE: a racing delete can leave a ghost entry.** It stays until the next frame. Fix:
  write the live entry inside `store_snapshot`'s critical section.

The design now spans three contexts: Ingestion (the connection limits), Fleet History (per-frame
cost and live state) and Fleet Registry (the registry limit and push system lifecycle). A
proposed split into three RFCs that can be accepted and implemented separately is waiting on
the owner's decision.
