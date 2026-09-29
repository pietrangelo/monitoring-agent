# RFC 0016: The Current Push Connection, and the Agent's Id Computed Once

- Status: Draft
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-29 (revised the same day for the first `rfc-adversary` pass; see Review)
- Affects: `system-hub` (a push connection's end, a disconnection sweep, the poller), and
  `system-agent` (its push system id, its `Dockerfile`)
- **Split from RFC 0008 §5 and §7 (owner's decision, recorded in RFC 0007's header):** this RFC
  ships on today's SQLite hub, as a pull request of its own, before the release that replaces
  SQLite. RFC 0008 keeps the push registry limit, which needs RFC 0011's catalog. RFC 0010
  carries this RFC's rules into its `LiveStatus` (§8).
- Depends on (all Implemented):
  - RFC 0005: the hub's `SystemId` rule, which the agent's id now mirrors (§6).
  - RFC 0006: the 90 s idle deadline (pings count), and `handle_push`'s exits.
  - RFC 0007: `end_connection` (the offline marking and the live metrics eviction, in one unit
    of blocking work), `SnapshotStored::SystemGone`, `on_blocking_pool`, and the lock order
    (the database mutex, then the live state).

## Motivation

1. **A connection that is no longer current marks its system offline.** Every exit of
   `handle_push` after registration runs `end_connection`, whichever connection it is. An
   agent that reconnects (connection B) while its old connection A is half-open is marked
   offline when A reaches its 90 s idle deadline, and B's live metrics are evicted. Both stay
   so until B's next frame: one push interval, which the operator sets (`PUSH_INTERVAL`, 2 s by
   default, with no maximum). ARCHITECTURE.md records the eviction half as waiting for this RFC.
2. **Push systems are polled.** The poller visits every enabled row every 30 s, `push://` rows
   included. reqwest refuses the scheme, so each tick marks every push system offline with a
   `builder error`, and its next frame marks it online again: every push system flaps on every
   tick. Each of those polls first builds a reqwest client, which RFC 0007 measured at about
   0.36 s of CPU: 1,000 push systems kept every core of a 4-core hub busy. The flapping is also,
   by accident, the only thing that marks offline a push system whose agent is gone after a hub
   restart (the hub has no graceful shutdown, so no connection's end ran), or whose offline
   write failed. Stopping the polls needs a replacement for that.
3. **The agent's id isn't stable where it matters, and isn't checked.** `get_persistent_id`
   runs once per process (in `spawn_push_client`, on the blocking pool), and reads
   `/etc/machine-id`, then `/var/lib/dbus/machine-id`, then runs the `hostname` binary, then
   draws a random UUID.
   - In the agent's image (`debian:bookworm-slim`) there is no machine id, so the id is the
     container's hostname, which Docker sets to the container id. Every re-creation of the
     container (an image upgrade, `docker compose up` after a change) registers a new system
     and leaves the old one offline for good. An image with no `hostname` binary gets a new
     random UUID on every restart. Nothing lets an operator keep the id.
   - The id is never checked against the hub's rule (RFC 0005). A machine-id file over 255
     bytes is refused at every handshake, every 5 s, forever, and the agent's log never says
     why at startup.
   - Running the `hostname` binary is a shell-out for a value the kernel publishes in
     `/proc/sys/kernel/hostname`.

## Proposed design

### 1. Push system

On today's hub a **push system** is a system whose `url` is exactly `push://`: push
registration (`push/ingest.rs::register_if_new`) is what writes that sentinel. The Fleet
Registry parses it once, into a value, instead of comparing strings at each use:

```rust
/// Where a system's snapshots come from (registry.rs).
pub enum SystemSource { Push, Poll }

impl SystemSource {
    /// `Push` for the `push://` sentinel (`PUSH_URL`), `Poll` for any other url.
    pub fn of(url: &str) -> Self;
}
```

`register_if_new` writes `PUSH_URL`, the same constant `of` compares with. `POST
/api/systems` can't store the sentinel (it trims trailing slashes, so `push://` is stored as
`push:`, a polled url), but `PUT` stores a url verbatim, as it does today. A row it turns into a
push system is treated as one: not polled, and swept (§4). RFC 0011 replaces the sentinel with a
stored `Source` that neither can set.

### 2. Connection numbers and the current connection

```rust
/// Identifies one accepted push connection within one hub process. Never persisted: numbers
/// only compare connections of one process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionNumber(u64);

/// Which open connection is current for each push system (presence.rs). Pure: no lock, no
/// clock read, no I/O.
#[derive(Default)]
pub struct PushPresence { /* next: u64, current: HashMap<String, ConnectionNumber> */ }

pub enum Ending {
    /// The ending connection was current: its entry is removed, and the caller marks the
    /// system offline and evicts its live metrics.
    Current,
    /// Another connection is current, or none is: nothing changes.
    NotCurrent,
}

impl PushPresence {
    /// A handshake accepted: takes the next number and makes it current.
    pub fn accept(&mut self, system_id: &SystemId) -> ConnectionNumber;
    /// A snapshot frame on `connection`: that connection becomes current.
    pub fn claim(&mut self, system_id: &SystemId, connection: ConnectionNumber);
    /// `connection` ended.
    pub fn end(&mut self, system_id: &SystemId, connection: ConnectionNumber) -> Ending;
    /// What the disconnection sweep does with one push system (§4).
    pub fn sweep(&self, system_id: &str, status: &SystemStatus, up_for: Duration) -> Sweep;
}
```

- **Numbers.** `accept` takes the next number from one hub-wide counter, inside `PushPresence`.
  It wraps on overflow (at a million connections a second, after 584,000 years).
- **The handshake makes its connection current.** Registration (`register_if_new`) and
  `accept` run in the handshake's one unit of blocking work, under the presence lock, after
  the id is authenticated, and `accept` runs only once registration succeeded. So a refused
  handshake never leaves an entry behind.
- **A snapshot frame claims currency.** Every snapshot frame that passed the edge
  (`SnapshotFrame::try_from`) runs `claim` in its unit of blocking work, just before its store.
  So the current connection is the one whose snapshot claimed last, or, before any, the one
  accepted last. Application frames and pings don't claim: they write no status.
- **Only the current connection's end marks offline.** `end_connection` takes the presence
  lock and calls `end`. `Current`: it marks the system offline and evicts its live metrics, as
  RFC 0007 §4 does today, under that lock. `NotCurrent`: it does neither, and its
  `Push client disconnected` line says a newer connection is current. This covers every exit
  of `handle_push` after registration: an `auth_ok` that couldn't be sent, the socket's end,
  the idle deadline, an oversize message, a failed unit of work, and `SystemGone`.
  - A host that reconnected within its old connection's 90 s idle deadline isn't marked
    offline, nor emptied of live metrics, when the old connection times out.
  - **Newest exits first.** If the new connection ends while an older one still delivers
    snapshots (two agents presenting one id, or a half-open socket that recovers), the new one
    marks offline and evicts, and the older one's next snapshot claims currency and marks it
    online again. Its own end later marks offline.
- **Every accepted connection ends, even by a panic.** `handle_push` holds a guard from the
  moment `accept` returns its number. On every normal exit the task awaits `end_connection`
  and disarms the guard. If the task unwinds instead (a panic), the guard's `Drop` hands
  `end_connection` to the blocking pool (through `Handle::try_current`, so a runtime already
  shutting down is skipped rather than panicked on). So an entry exists exactly while its
  connection's task runs, and a connection's task runs at most 90 s past its last message
  (RFC 0006): an **entry means an open, live connection.** The one exception is a task dropped
  between the accept and its guard, which only a runtime shutdown can do, and the process
  then exits.
- **The rule, and what it doesn't promise.** The last claim decides currency. A store's
  status write lands after its claim, outside the presence lock, so while two connections of
  one id both deliver snapshots, a store claimed earlier can land after a later end: the
  system then reads online with no live metrics, or offline while one connection is open,
  until the next claim. With one connection delivering at a time, which is every honest
  agent's case, the outcomes are exactly those above:
  - a claim that takes the lock before an older connection's end makes that end `NotCurrent`;
  - a claim that takes it after the end waits for the end's offline write, and its store then
    marks the system online;
  - the sweep's check and write happen under the lock, so no claim falls between them.

  Lock order: **presence, then the database mutex, then the live state.** Nothing takes the
  presence lock while holding the database mutex or a live lock; the lock is taken only inside
  units on the blocking pool, never on a runtime thread. Holding it across a database write can
  make a frame's `claim` wait for one write (the database mutex already serialises every
  store). A poisoned presence lock is recovered (`PoisonError::into_inner`), as the live locks
  are: `PushPresence` holds no invariant a panic elsewhere could break.
- **Memory.** One entry per open push connection that is current: at most the open push
  connections (which RFC 0006 leaves unbounded, see Security), each an id of at most 255 bytes
  and 8 bytes more.
- **A deleted system**'s entry is left to its connection: that connection's next snapshot
  gets `SystemGone`, its end is `Current`, and it removes the entry (its offline write finds no
  row). `DELETE` itself stays as RFC 0007 left it.

### 3. The poller visits polled systems only

`registry::enabled_systems` becomes `polled_systems`: the enabled systems whose source is
`Poll`, in the registry's order. A push system is never polled, so it no longer flaps, and no
poll builds a reqwest client for it.

### 4. The disconnection sweep

A push system with no current connection is **disconnected**. Its current connection's end has
already marked it offline, unless that write failed, or the hub restarted (no connection's end
ran), or `PUT` made it a push system. The sweep marks those offline.

Every 30 s, a task started by `main` (`push::start_disconnection_sweep`) runs one unit on the
blocking pool:

1. a narrow registry read, `Database::system_sources`: `SELECT id, url, status FROM systems`,
   three `TEXT NOT NULL` columns, so no row can fail to map (`status` maps any unknown text to
   `Unknown`, as `list_systems` does). It reads no token. A failed read skips the pass with one
   `warn`;
2. for each row whose `SystemSource::of(url)` is `Push`, whatever its `enabled` (push ingestion
   ignores `enabled`, so the sweep does too), it takes the presence lock and asks `sweep`:

| Stored status | Current connection | Hub up for | Outcome |
|---|---|---|---|
| `Offline` | any | any | `Leave`: already offline |
| `Online` or `Unknown` | yes | any | `Leave`: an open, live connection |
| `Online` or `Unknown` | none | under 120 s | `Leave`: its agent may still be reconnecting |
| `Online` or `Unknown` | none | 120 s or more | `MarkOffline(NotConnected)`, or `MarkOffline(InvalidSystemId)` when the id breaks `SystemId` |

`MarkOffline` writes, under the presence lock, `status = offline`, `last_seen = ""` and
`last_error` = `push disconnected` (`NotConnected`, the text a connection's end writes) or
`invalid system id` (`InvalidSystemId`, the text today's poll writes for such rows). The live
metrics are left, as a failed poll leaves a polled system's.

- **Time.** `up_for` is the monotonic time since the sweep task started, read once per pass,
  in the adapter. **120 s** is RFC 0010 §10's restart rule; every shipped agent retries every
  5 s, so an agent that is running reconnects well within it.
- **What it catches:** a failed offline write (within 30 s); after a restart, every push system
  whose agent doesn't come back (120 s to 150 s after the hub starts; today's first poll
  marked it offline at once, then flapped); rows `PUT` made push systems; and rows stored
  before RFC 0005 whose id breaks `SystemId`, which no handshake can make current.
- **What it doesn't:** a connection that stays open delivering pings but no snapshot. An
  agent pushing every 300 s is such a connection between its snapshots, and it is online.
  Online means connected; a live connection's liveness is RFC 0006's idle deadline.
- **Races.** Status is read before the per-row lock. A row read `Online` whose current
  connection ends in between is written offline again with the same text; a row read
  `Offline` that a claim marks online in between is left.
- **Cost.** One read every 30 s, and one `UPDATE` per push system found disconnected, once (an
  offline row is skipped). After a restart with 1,000 agents gone, one pass makes 1,000 writes
  in turn on one blocking thread.
- **Logging (A09).** One `info` line per pass that marked anything, with the count; the first
  failed write of a pass at `warn` with the error, and the count of failures. No id is logged
  per system.

### 5. What doesn't change

The push frame, the application frame, the handshake's messages, the SQLite schema, the REST
API and the SSE summary's shape. `update_system_status` stays the offline writer. The poller's
handling of polled systems, including its `invalid system id` marking, is unchanged. A push
system is marked offline by its current connection's end at once, as today; RFC 0006's idle
deadline still ends a connection that sends nothing, pings included, within 90 s.

### 6. The agent's id, computed once, before the runtime

`start` reads `PUSH_TO` once (`std::env::var`, `Ok` meaning push is on, exactly as
`spawn_push_client` decides today) after the configuration parses it already does, and only
then resolves the push system id, before the Tokio runtime exists (so `std::fs` is fine there).
The push task receives the hub URL and the id by value and reuses the id on every reconnect;
`get_persistent_id` is removed.

```rust
/// A push system id the hub will accept: the hub's `SystemId` rule, after trimming.
pub struct AgentId(String);

pub enum AgentIdRule { Empty, TooLong, DotSegment }

impl TryFrom<&str> for AgentId { type Error = AgentIdRule; }

/// Where the id came from, for the startup log line.
pub enum IdSource { IdFile, MachineId, DbusMachineId, HostName, Random }

/// Why `SYSTEM_AGENT_ID_FILE` refused startup.
pub enum AgentIdError {
    /// Not an absolute path (exit 78).
    NotAbsolute,
    /// The file isn't UTF-8 (exit 78).
    NotUtf8,
    /// The file holds an id that breaks the rule (exit 78).
    Invalid(AgentIdRule),
    /// The file exists but couldn't be read (exit 1).
    Unreadable(std::io::Error),
    /// The file was missing and couldn't be written (exit 1).
    Unwritable(std::io::Error),
}
```

**The rule mirrors the hub's `SystemId`** (RFC 0005): after trimming ASCII whitespace, not
empty, at most 255 bytes, not `.` or `..`.

**The sources, in order** (`resolve_push_id`, over injected paths, so tests never touch `/etc`
or `/proc`):

1. **`SYSTEM_AGENT_ID_FILE`** (`std::env::var_os`; empty means unset), when the file exists:
   its content is the id. A value that isn't an absolute path, a file that isn't UTF-8 or
   breaks the rule, **refuses startup** with exit code 78 (`EX_CONFIG`), naming the variable
   and the reason, never the path or the content. A file that exists but can't be read refuses
   startup with exit code 1, naming the variable and the I/O error.
2. `/etc/machine-id`, then `/var/lib/dbus/machine-id`, as today.
3. The **host name**, from `/proc/sys/kernel/hostname`: the kernel's nodename, which is what
   `gethostname(2)` and so the `hostname` binary return, converted lossily as today's
   `from_utf8_lossy` does, so an agent that uses it keeps its id, 64-byte and non-UTF-8 names
   included. No binary is run.
4. A new random UUID (v4).

A source in 2–4 that is missing, unreadable or breaks the rule (after the lossy conversion) is
skipped, as an empty one is today.

**When `SYSTEM_AGENT_ID_FILE` is set and the file is missing,** the id resolved by 2–4 is
written there, so it is the id from then on, whatever the machine id, the host name or the
container becomes. With `std` only:

- a temporary file `.<name>.<uuid>.tmp` is created in the same directory (`create_new`, so it
  never replaces anything), written, and `sync_all`ed;
- `std::fs::hard_link` links it to the file's name: `link(2)` never replaces an existing file,
  so the file appears whole or not at all, never empty. The temporary name is then removed,
  and the directory is `fsync`ed; a failed directory `fsync` is logged at `warn` and startup
  goes on (the file is written; a crash before the kernel flushes it can only lose it, and the
  next start then writes it again);
- if another process created the file first (`hard_link` answers `AlreadyExists`), the agent
  removes its temporary file and reads that file instead (step 1's rules);
- any other failure (`Unwritable`: a missing or read-only directory, a full disk, a filesystem
  without hard links) removes the temporary file if it was created, and refuses startup with
  exit code 1. The operator asked for a stable id, and a silent fallback would register a new
  system on the next re-creation. A crash can leave a stray `.tmp` file, which nothing reads.

The write captures the id the agent has today, so turning the file on doesn't re-register the
system: an agent in the image keeps its container-hostname id, now across re-creations.

**Unset or empty,** nothing is written. When step 4 is reached, the agent logs one `warn`: the
id lives for this process only, and `SYSTEM_AGENT_ID_FILE` would keep it.

**Logging.** One `info` line at startup names the source (`IdSource`). The push client's
`Push authenticated` line prints the id in `Debug` form from now on (today it's `Display`), so
an id file holding a control character can't forge a log line.

**The image gets the directory, not the variable.** The `Dockerfile` adds
`RUN install -d -o agent -g agent /var/lib/system-agent`, so a named volume mounted there
starts owned by the agent's user (Docker copies an image directory's ownership into an empty
named volume; a bind mount needs the operator's `chown`, and rootless Podman its `:U` option).
The image and `docker-compose.yml` set no `SYSTEM_AGENT_ID_FILE`; the README gives the Compose
lines that turn it on.

### 7. Mixed versions

No wire contract changes, so every agent and hub pair keeps working.

- An agent that already had a machine id or a host name keeps its id.
- An agent with neither (a random UUID per restart before) still draws one per process, and
  now says so.
- An agent whose machine-id file broke the hub's rule (refused forever before) now presents
  the next source.

### 8. Relation to the redb release

RFC 0010 §10 keeps a `LiveStatus` per system, with a `connection` field and a restart rule of
its own (`Unknown`, then `Offline` after 120 s). When it lands, it carries this RFC's rules
forward: `accept`, `claim` and `end` decide its `connection` field; the disconnection sweep
marks offline a push system with no current connection 120 s after the store opens; and
`SystemSource` becomes RFC 0011's stored `Source`. A push system's `last_contact` there stays a
display value (the last frame), not a liveness rule: a connection that pushes every 300 s is
online. Nothing here is persisted, so nothing needs migrating.

## Domain impact

- **Fleet Registry:** `SystemSource`, `PUSH_URL` and `polled_systems` (`registry.rs`);
  `PushPresence`, `ConnectionNumber`, `Ending`, `Sweep`, `OfflineReason` and
  `RECONNECT_GRACE` (new `presence.rs`, pure).
- **Ingestion:** the handshake's unit registers and accepts; a snapshot frame's unit claims,
  then stores; `end_connection` acts only on `Ending::Current`; the connection's guard; the
  disconnection sweep (`push/sweep.rs`); `Database::system_sources`; the poller's filter.
- **Telemetry Publishing (agent):** `AgentId`, `AgentIdRule`, `IdSource`, `AgentIdError`,
  `resolve_push_id` (`push/identity.rs`); `StartupError::PushId`.
- **Glossary:**
  - **push system**: a system registered by a push handshake (`url` = `push://` until RFC
    0011), never polled;
  - **polled system**: any other system;
  - **connection number**: the in-memory number that identifies one accepted push
    connection;
  - **current connection**: of a push system's open connections, the one whose snapshot frame
    claimed last, or, before any, the one accepted last. Only its end marks the system
    offline;
  - **disconnected push system**: one with no current connection. The disconnection sweep
    marks it offline once the hub has run for 120 s;
  - **system status** changes: a push system is marked offline by its current connection's
    end, or by the disconnection sweep; polled systems as today;
  - **push system id** (agent): the id the agent presents in the handshake, resolved once per
    process.
- **Published contracts:** none changes. The handshake's answers, the frames and the poll
  responses are byte-identical.

## Alternatives considered

- **Only the latest-accepted connection is current** (RFC 0008's first revision). If the
  newest connection ended first, the system went offline while the older one still delivered
  snapshots, and stayed offline. Snapshots claiming currency fix both orders.
- **A silence rule over the last contact** (this RFC's first draft: offline 180 s after the
  last snapshot or handshake). An agent with `PUSH_INTERVAL` of 180 s or more, which is legal,
  would flap between snapshots. Counting pings as contact would fix that, at a blocking unit per
  ping (pings aren't budgeted, so it would need its own rate limit), and would still only catch
  what the guard catches: a task that died without its end.
- **Keep the connection number in the database** (a `push_connection` column). A schema
  change for a value that means nothing across processes.
- **Claim inside the store's transaction**, under the database mutex, so no new lock exists.
  The end's check and its offline write would then need one database critical section with the
  live-metrics eviction inside it: a new `Database` method taking a closure that decides for
  the Fleet Registry. The presence lock keeps the decision in pure code.
- **Hold the presence lock across a frame's store** too. It closes the two-deliverer window of
  §2 only until the next claim, as claiming first does, and holds the lock for every store.
- **Keep polling push systems, and skip only their offline marking.** Still one reqwest client
  per push system per tick (the CPU cost of Motivation 2), for nothing.
- **Sweep from the poller's tick**, sharing its `list_systems` read. That read fails whole on
  one unmappable row (a negative `poll_interval_secs` stored by an older hub), and its last-read
  fallback would re-mark the same systems every pass. The sweep's own read can't fail to map.
- **Iterate only the presence entries.** A push system with no entry (after a restart, or after
  a failed offline write) would stay online for good: exactly the gap the polls filled by
  accident.
- **No grace after a restart.** Every push system would read offline for the seconds its agent
  takes to reconnect, on every hub restart.
- **A default `SYSTEM_AGENT_ID_FILE`** (`/var/lib/system-agent/id`, RFC 0008's previous
  revision). Existing deployments have no volume there, so every start would write the id
  into the container's own layer, lost with it.
- **Write the file only when step 4 is reached** (RFC 0008's last revision). A host name is
  practically always there, so step 4 is practically never reached and the file would never be
  written: a container's id would still change with each re-creation.
- **Write a fresh UUID into a missing file**, rather than the resolved id. Turning the file on
  would then re-register every agent once, leaving an offline twin.
- **Drop the host name source.** It would change the id of every agent that uses it today.
- **`sysinfo::System::host_name()`** for the host name. Its 64-byte buffer makes glibc refuse a
  64-byte name, and it drops a non-UTF-8 one: both would fall to a random UUID per restart,
  where today's binary gives a stable id.
- **The `tempfile` crate** for the write. It isn't built into the agent on Linux today (only
  `native-tls` on macOS depends on it), so it would add four crates for what `create_new`,
  `sync_all` and `hard_link` do.
- **`OpenOptions::create_new` on the file itself, then write.** A crash between the create and
  the write leaves an empty file, which step 1 then refuses: the agent would never start again
  without an operator.

## Security implications

- **A01 / API1 (push ids are self-asserted): unchanged.** Anyone who can push as an id (the
  push token, or anyone while it's unset) can make a connection current with a handshake, and
  its end then marks the system offline and evicts its live metrics until the agent's next
  snapshot. That is today's behaviour for every connection's end; this RFC only stops *older*
  connections from doing it. Two agents presenting one id alternate its status, as they
  already alternate its metrics.
- **API4:** the presence map holds at most one entry per open push connection (§2), and open
  connections stay unbounded as RFC 0006 recorded; this RFC adds about 270 bytes per
  connection to what each already costs. The sweep's work is one narrow read every 30 s and at
  most one write per push system, once. Not polling push systems removes the dominant
  per-system CPU cost on the hub (about 0.36 s per push system per tick).
- **A04 / API6:** a hostile client can't use the sweep against a system: it marks only systems
  with no open connection, and holding one open keeps a system online, which pushing as the id
  already allows.
- **A05 / API8:** a relative path, or an id file that isn't UTF-8 or breaks the rule, refuses
  the agent's startup (exit 78); an unreadable or unwritable one refuses it too (exit 1),
  rather than pushing under a per-process id. The file is created with the process umask; the
  id isn't a secret. The image's new directory is owned by the agent's user and empty.
- **A09:** the sweep logs a count per pass, its first failed write, and a failed read; a
  superseded connection's end says so on its disconnect line; the agent logs its id's source at
  startup, and the id in `Debug` form when it authenticates, so a control character can't
  forge a line. No token is logged anywhere.
- **A03:** the new read has no parameter; the offline write is `update_system_status`, with
  bound parameters. The new `last_error` texts are constants.
- **A06:** no new dependency.
- **A08:** the id file is written through a temporary file and a no-clobber link; only someone
  with write access to its directory can place one.
- **A02, A07 / API2, API3, API5, A10 / API7:** not touched. The handshake's token check is
  unchanged (`PushToken::accepts`, constant time).
- **API9:** `SYSTEM_AGENT_ID_FILE` goes into the README's agent table; the status rules into
  README's push section and ARCHITECTURE.md.
- **API10:** the hub consumes push frames as before.

## Testing plan

TDD, per `CLAUDE.md`, with `red-test-adversary` and `rosette-auditor`.

- **Pure (hub):**
  - `SystemSource::of` over `push://`, `push:`, `http://…`, `https://…`, `PUSH://`,
    `push://x` and `""`;
  - `polled_systems`: disabled systems and push systems left out, the registry's order kept;
  - `accept`: increasing numbers, each current;
  - `end` as a table: the current connection ends (`Current`, entry removed); an older one
    ends (`NotCurrent`, entry kept); an unknown system (`NotCurrent`);
  - claiming as a sequence, **newest exits first**: A accepted, A claims, B accepted, B ends
    (`Current`), A claims, A ends (`Current`);
  - `sweep` as the §4 table: each status, with and without a current connection, at 119 s and
    at exactly 120 s, and a disconnected row whose id breaks `SystemId` (`InvalidSystemId`).
- **Hub adapters (real server, temp database).** A test that asserts something *didn't*
  happen first waits for an event that comes after it would have: the hub closing the old
  socket (`handle_push` returns after `end_connection`), or the poller's third poll of a
  counted polled system.
  - a host whose new connection B delivers a snapshot, then whose old connection A ends
    (closed by the agent, and, with a short injected idle deadline, timed out): once the hub has
    closed A, still `Online`, live metrics kept, nothing sent on B after A's close;
  - the current connection's end: `Offline`, `push disconnected`, live metrics evicted (the
    RFC 0007 §4 test, kept);
  - newest exits first, over two sockets: B ends → `Offline`; A's snapshot → `Online`;
    A ends → `Offline`;
  - one table over the handshake: registered → a current connection exists; `registry
    unavailable` → none;
  - the guard: a connection task that panics after its accept (through a test seam) still
    marks its system offline;
  - the sweep, run once with an injected `up_for`: a disconnected `Online` push row → `Offline`
    with `push disconnected`; a connected one untouched; a disabled push row swept; a row with
    id `..` → `invalid system id`; a polled row never touched; an `Offline` row keeps its
    `last_error`; before 120 s nothing written; a database also holding a row with a negative
    `poll_interval_secs` still swept;
  - `system_sources` over rows `list_systems` can't map;
  - the poller, at a period of 500 ms over an enabled `Online` push row and a counted polled
    system: after the polled system's third poll, the push row is still `Online` (today the
    first tick marks it offline).
- **Agent** (`resolve_push_id` over temp directories under `std::env::temp_dir`, with injected
  paths for `/etc/machine-id`, `/var/lib/dbus/machine-id` and `/proc/sys/kernel/hostname`):
  - `AgentId` as a table, the same inputs as the hub's `SystemId` table, plus trimming;
  - each source in order, each skipped when missing, empty or breaking the rule; a 64-byte host
    name, and a non-UTF-8 one (lossily converted, as today);
  - `SYSTEM_AGENT_ID_FILE` set and missing: the resolved id is written, and a second
    resolution with a different host name reads the file's;
  - set and existing: the file wins over a machine id;
  - set and relative, set and invalid, set and not UTF-8, set in a missing directory: each the
    right `AgentIdError`; `StartupError::PushId`'s exit codes (78 and 1) in the existing table;
  - the file created between the check and the link: `AlreadyExists` reads the other file's
    id, never overwrites it, and leaves no temporary file;
  - unset, with no machine id and no host name: `IdSource::Random`.
  - **the binary** (`tests/startup.rs`): a relative `SYSTEM_AGENT_ID_FILE` refuses startup with
    78 before anything starts (no connection reaches `PUSH_TO`), naming the variable and not
    the path; a missing one in a temp directory is written at startup, with an id that follows
    the rule.
  - "computed once" has no runtime test: the push task receives an `AgentId` by value, and no
    function that resolves one is reachable from it.

## Impact on `docs/ARCHITECTURE.md`

- § Components (agent): the id resolved in `start`; the configuration and exit-code paragraph
  (78 also for `SYSTEM_AGENT_ID_FILE`'s refused values, 1 for its I/O failures). (Hub): the
  disconnection sweep; push systems are not polled; live metrics evicted by the current
  connection's end.
- § Domain model: the Fleet Registry row (`presence.rs`, `SystemSource`, `polled_systems`),
  the Ingestion row (the sweep, the handshake and frame units, the guard, `end_connection`,
  `system_sources`), the agent's Telemetry Publishing row (`push/identity.rs`); the glossary
  terms of Domain impact, and the **live metrics**, **last seen** and **system status**
  entries; the lock order (presence, database, live).
- § Trust boundaries, Agent → Hub (push): only the current connection's end marks offline.
- § Testing architecture: the barrier rule for tests that assert nothing happened; the
  agent's new startup cases.
- § Open architectural questions:
  - removed: push systems being polled and flapping; "any connection presenting an id evicts";
    the `hostname` shell-out; the 1,000 `push://` rows keeping every core busy (the per-poll
    client cost stays, for polled systems);
  - changed: RFC 0005's stored-id entry (the sweep, not the poller, marks those rows offline);
    "push auto-registration is unbounded" (no longer polled; still unbounded, RFC 0008);
  - added: after a restart, a push system whose agent is gone reads online for up to 150 s;
    `PUT` can still write the `push://` sentinel (RFC 0011); a connection that pings but never
    sends a snapshot keeps its system online.
- `README.md`: the `SYSTEM_AGENT_ID_FILE` row and its Compose lines; the startup paragraph's
  list of variables parsed first and its exit codes; the handshake paragraph's id sources; the
  deadlines paragraph's offline marking (by the current connection, and the sweep).
- RFC 0010 §10: its sweep is the disconnection sweep (a push system with no current
  connection, 120 s after open), not a silence rule over `last_contact`.

## Rollout / migration notes

- No schema change and no wire change: hub and agent upgrade in either order.
- After the hub upgrade, push systems stop flapping at once. A push system whose agent is gone
  stays online up to 150 s after the hub starts, then goes offline for good.
- Agents keep their ids. Those that drew a random UUID per restart keep doing so until they
  set `SYSTEM_AGENT_ID_FILE`.
- To keep an agent's id across container re-creations, mount a volume on
  `/var/lib/system-agent` and set `SYSTEM_AGENT_ID_FILE=/var/lib/system-agent/id`. The first
  start writes the current id there.

## Review

`rfc-adversary`, first pass, on the first draft:

| Finding | Verdict | Resolution |
|---|---|---|
| A: a silence rule over the last contact makes an agent with `PUSH_INTERVAL` ≥ 180 s flap | CONFIRMED | the silence rule is gone: the sweep marks only push systems with **no current connection**; a guard ends every accepted connection, a panic included, so an entry means an open, live connection (§2, §4) |
| B: the recommended id-file path isn't writable in the image | CONFIRMED | the `Dockerfile` creates `/var/lib/system-agent` owned by `agent`; the README gives the Compose lines (§6, Rollout) |
| C: `list_systems` fails whole on one unmappable row; the sweep's read failure is unspecified | CONFIRMED | the sweep's own read of three `TEXT NOT NULL` columns; a failed read skips the pass with a `warn`; a test with a negative `poll_interval_secs` row (§4) |
| D: "nothing happened" tests with nothing to wait on | PLAUSIBLE | each names its barrier: the hub's close of the old socket, the third poll of a counted system; the handshake cases in one table (Testing plan) |
| E: `sysinfo::System::host_name()` differs from the `hostname` binary (64 bytes, non-UTF-8) | CONFIRMED | `/proc/sys/kernel/hostname`, converted lossily as today; both cases in the tests (§6) |
| F: disabled push rows in the sweep | PLAUSIBLE | swept whatever `enabled`, as push ingestion ignores it; a test row (§4) |
| G: ARCHITECTURE/README impact incomplete; exit 78 "used for nothing else"; RFC 0010's contact rule | CONFIRMED | the sections listed; I/O failures exit 1, refused values 78; RFC 0010 §10 amended (Impact) |
| H: `tempfile` isn't built on Linux; the `std` alternative was a straw man | CONFIRMED | `std` only: `create_new`, `sync_all`, `hard_link`; the alternatives corrected (§6) |
| I: `POST` can't store `push://` | CONFIRMED | `PUT` only; `push:` → `Poll` in the tests (§1) |
| J: the directory `fsync` of a bare relative name | CONFIRMED | `SYSTEM_AGENT_ID_FILE` must be absolute (exit 78); a failed directory `fsync` is a `warn` (§6) |
| K: the `invalid system id` diagnostic lost | CONFIRMED | the sweep writes it for such rows (§4) |
| L: the agent logs the id in `Display` form on every authentication | CONFIRMED | `Debug` form (§6, A09) |
| M: claim-before-store overstated for two deliverers | PLAUSIBLE | restated: the last claim decides currency; the two-deliverer window stated (§2) |
| N: `PUSH_TO` read twice; a non-UTF-8 `SYSTEM_AGENT_ID_FILE`; where resolution sits | PLAUSIBLE | `PUSH_TO` read once in `start`, as today's `var`; `var_os` for the id file; resolution after the other parses (§6) |
| the presence lock's poisoning | PLAUSIBLE | recovered with `into_inner`, as the live locks (§2) |

Came closest among the rejected: the lock order, checked against every path that takes the
database mutex or a live lock; none needs the presence lock after them.
