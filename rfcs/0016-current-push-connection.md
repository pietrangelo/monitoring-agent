# RFC 0016: The Current Push Connection, and the Agent's Id Computed Once

- Status: Draft
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-29
- Affects: `system-hub` (the push connection's end, a silence sweep, the poller), and
  `system-agent` (its push system id)
- **Split from RFC 0008 §5 and §7 (owner's decision, recorded in RFC 0007's header):** this RFC
  ships on today's SQLite hub, as a pull request of its own, before the release that replaces
  SQLite. RFC 0008 keeps the push registry limit, which needs RFC 0011's catalog. RFC 0010
  carries this RFC's rules into its `LiveStatus` (§8).
- Depends on (all Implemented):
  - RFC 0005: the hub's `SystemId` rule, which the agent's id now mirrors (§6).
  - RFC 0006: the 90 s idle deadline, and `handle_push`'s exits.
  - RFC 0007: `end_connection` (the offline marking and the live metrics eviction, in one unit
    of blocking work), `SnapshotStored::SystemGone`, `on_blocking_pool`, and the lock order
    (the database mutex, then the live state).

## Motivation

1. **A connection that is no longer current marks its system offline.** Every exit of
   `handle_push` after registration runs `end_connection`, whichever connection it is. An
   agent that reconnects (connection B) while its old connection A is half-open is marked
   offline when A reaches its 90 s idle deadline, and B's live metrics are evicted. Both stay
   so until B's next frame: one push interval, which the operator sets (`PUSH_INTERVAL`, 2 s by
   default). ARCHITECTURE.md records the eviction half as waiting for this RFC.
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
   - Running the `hostname` binary is a shell-out that `sysinfo` (already a dependency) makes
     unnecessary: `System::host_name()` calls `gethostname(2)`, which is what the binary
     prints.

## Proposed design

### 1. Push system

On today's hub a **push system** is a system whose `url` is exactly `push://`: push
registration (`push/ingest.rs::register_if_new`) is what writes that sentinel. The Fleet
Registry parses it once, into a value, instead of comparing strings at each use:

```rust
/// Where a system's snapshots come from (registry.rs).
pub enum SystemSource { Push, Poll }

impl SystemSource {
    /// `Push` for the `push://` sentinel, `Poll` for any other url.
    pub fn of(system: &SystemInfo) -> Self;
}
```

`POST` and `PUT /api/systems` can still write `push://` into a url, as they can today; RFC
0011 replaces the sentinel with a stored `Source` that neither can set. A row they turn into a
push system is treated as one: not polled, and swept (§4).

### 2. Connection numbers and the current connection

```rust
/// Identifies one accepted push connection within one hub process. Never persisted: numbers
/// only compare connections of one process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionNumber(u64);

/// Every push system's current connection, and when the hub last heard from it. Pure: the
/// caller passes the monotonic `now`; no lock, no clock read, no I/O (presence.rs).
#[derive(Default)]
pub struct PushPresence { /* next: u64, current: HashMap<String, Presence> */ }

struct Presence { connection: ConnectionNumber, last_contact: Instant }

pub enum Ending {
    /// The ending connection was current: its entry is removed, and the caller marks the
    /// system offline and evicts its live metrics.
    Current,
    /// Another connection is current, or none is: nothing changes.
    NotCurrent,
}

impl PushPresence {
    /// A handshake accepted at `now`: takes the next number and makes it current.
    pub fn accept(&mut self, system_id: &SystemId, now: Instant) -> ConnectionNumber;
    /// A snapshot frame on `connection` at `now`: that connection becomes current.
    pub fn claim(&mut self, system_id: &SystemId, connection: ConnectionNumber, now: Instant);
    /// `connection` ended.
    pub fn end(&mut self, system_id: &SystemId, connection: ConnectionNumber) -> Ending;
    /// Whether the silence sweep marks this push system offline (§4).
    pub fn is_silent(&self, system_id: &str, status: &SystemStatus, started: Instant,
                     now: Instant) -> bool;
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
  So the current connection is the one that last delivered a snapshot, or, before any, the one
  accepted last. Application frames don't claim: they write no status.
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
- **Why the order holds.** One `Mutex<PushPresence>` in `AppState`, taken only on the blocking
  pool, never on a runtime thread. The handshake holds it across registration and `accept`;
  `end_connection` and the sweep hold it across their check and their offline write; a frame
  holds it only for `claim`, then stores after releasing it. So:
  - a frame that claims before an older connection's end makes that end `NotCurrent`;
  - a frame that claims after the end waits for the end's offline write, and its store then
    marks the system online;
  - the sweep sees the contact of every claim that took the lock before it.

  Lock order: **presence, then the database mutex, then the live state.** Nothing takes the
  presence lock while holding the database mutex or a live lock. Holding the presence lock
  across a database write can make a frame's `claim` wait for one write (the database mutex
  already serialises every store, so no frame waits longer than it does today).
- **Memory.** An entry exists for a system while its current connection is open: the
  current connection's end removes it. So entries are at most the open push connections (which
  RFC 0006 leaves unbounded, see Security), plus one per `handle_push` task that panicked
  before its end, each an id of at most 255 bytes and 24 bytes more.
- **A deleted system**'s entry is left to its connection: that connection's next snapshot
  gets `SystemGone`, its end is `Current`, and it removes the entry (its offline write finds no
  row). `DELETE` itself stays as RFC 0007 left it.

### 3. The poller visits polled systems only

`registry::enabled_systems` becomes `polled_systems`: the enabled systems whose source is
`Poll`, in the registry's order. A push system is never polled, so it no longer flaps, and no
poll builds a reqwest client for it.

### 4. The silence sweep

A push system that the hub hasn't heard from in **180 s** (twice the idle deadline) is
**silent**. Every 30 s, a task started by `main` (`push::start_silence_sweep`) reads the
registry on the blocking pool (`list_systems`, as the poller does) and, for each push system,
takes the presence lock and asks `is_silent`:

| Stored status | Presence entry | Silent when |
|---|---|---|
| `Offline` | any | never: it's already offline |
| `Online` or `Unknown` | present | `now − last_contact ≥ 180 s` |
| `Online` or `Unknown` | none | `now − started ≥ 180 s`, where `started` is when the sweep task started |

A silent system is marked offline (`status = offline`, `last_seen = ""`, `last_error = "no
push frame for 180 s"`, the same shape as a disconnect), under the presence lock. The entry
stays: the connection, if any, is still open, and its end removes it. Its live metrics stay
beside the offline status, as a failed poll leaves a polled system's.

- **Time.** The sweep reads the monotonic clock once per pass and passes it in; `is_silent`
  saturates (a contact after `now` is not silent). The contact is the handshake's `accept` or a
  snapshot frame's `claim`; pings and application frames are not contact.
- **What it catches:** a current connection that stays open without delivering a snapshot
  (pings alone keep it past the idle deadline); a `handle_push` task that panicked before its
  end; an offline write that failed; and, after a restart, every push system whose agent
  doesn't come back. After a restart the hub keeps showing such a system online for 180 s to
  210 s, where today's first poll marked it offline at once and then flapped.
- **Rows stored before RFC 0005** with an id that breaks `SystemId` are push systems too. No
  handshake can make them current, so the sweep marks them offline 180 s after the hub starts;
  today it is the poll's `invalid system id` marking.
- **Cost.** One registry read every 30 s, and one `UPDATE` per system found silent (each
  marked once: an offline row is skipped). After a restart with 1,000 agents gone, one pass
  makes 1,000 writes, one after another, on one blocking thread.
- **Logging (A09).** One `info` line per pass that marked anything, with the count; the first
  failed write of a pass at `warn` with the error, and the count of failures. No id is logged
  per system.
- **Latency.** A connection's end marks offline at once. A silent open connection is marked
  offline 180 s to 210 s after its last contact. RFC 0006's idle deadline (90 s) still ends a
  connection that sends nothing at all, pings included.

### 5. What doesn't change

The push frame, the application frame, the handshake's messages, the SQLite schema, the REST
API and the SSE summary's shape. `update_system_status` stays the offline writer. The poller's
handling of polled systems, including its `invalid system id` marking, is unchanged.

### 6. The agent's id, computed once, before the runtime

The agent resolves its push system id **once, in `start`** (synchronous, before the Tokio
runtime exists, so `std::fs` is fine there), and only when `PUSH_TO` is set. The push task
receives the id by value and reuses it on every reconnect; `get_persistent_id` is removed.

```rust
/// A push system id the hub will accept: the hub's `SystemId` rule, after trimming.
pub struct AgentId(String);

pub enum AgentIdRule { Empty, TooLong, DotSegment }

impl TryFrom<&str> for AgentId { type Error = AgentIdRule; }

/// Where the id came from, for the startup log line (never the id itself).
pub enum IdSource { IdFile, MachineId, DbusMachineId, HostName, Random }

pub enum AgentIdError {
    /// `SYSTEM_AGENT_ID_FILE` exists but couldn't be read, or isn't UTF-8.
    Unreadable(std::io::Error),
    /// `SYSTEM_AGENT_ID_FILE` holds an id that breaks the rule.
    Invalid(AgentIdRule),
    /// `SYSTEM_AGENT_ID_FILE` was missing and couldn't be written.
    Unwritable(std::io::Error),
}
```

**The rule mirrors the hub's `SystemId`** (RFC 0005): after trimming ASCII whitespace, not
empty, at most 255 bytes, not `.` or `..`.

**The sources, in order** (`resolve_push_id`, over injected paths and an injected host name
function, so tests never touch `/etc`):

1. **`SYSTEM_AGENT_ID_FILE`**, when set and not empty, and the file exists: its content is
   the id. A file that can't be read, isn't UTF-8 (`Unreadable`), or breaks the rule
   (`Invalid`) **refuses startup** with exit code 78 (`EX_CONFIG`, as the agent's other
   configuration errors), naming the variable and the reason, never the content.
2. `/etc/machine-id`, then `/var/lib/dbus/machine-id`, as today.
3. The **host name**, from `sysinfo::System::host_name()` (`gethostname(2)`): the value the
   `hostname` binary prints, so an agent that uses it keeps its id. No binary is run.
4. A new random UUID (v4).

A source in 2–4 that is missing, unreadable or breaks the rule is skipped, as an empty one is
today.

**When `SYSTEM_AGENT_ID_FILE` is set and the file is missing,** the id resolved by 2–4 is
written there, so it is the id from then on, whatever the machine id, the host name or the
container becomes:

- written through `tempfile::NamedTempFile::new_in` (the file's directory), `write_all`,
  `sync_all`, then `persist_noclobber`, which never replaces an existing file; then the
  directory is `fsync`ed. A crash leaves either no file or a whole one, never an empty one;
- if another process created the file first (`persist_noclobber` answers `AlreadyExists`),
  the agent reads that file instead (step 1's rules);
- any other failure (`Unwritable`: a missing or read-only directory, a full disk) refuses
  startup with exit code 78. The operator asked for a stable id, and a silent fallback would
  register a new system on the next re-creation.

The write captures the id the agent has today, so turning the file on doesn't re-register the
system: an agent in the image keeps its container-hostname id, now across re-creations.

**Unset or empty,** nothing is written. When step 4 is reached, the agent logs one `warn`: the
id lives for this process only, and `SYSTEM_AGENT_ID_FILE` would keep it.

**Logging.** One `info` line at startup names the source (`IdSource`), never the id.

**Container images are unchanged.** The Dockerfile and `docker-compose.yml` set no
`SYSTEM_AGENT_ID_FILE`: the image has no volume for it. The README tells an operator who wants
a stable id to mount one and set the variable.

`tempfile` (3.27.0, already in the agent's lockfile through `native-tls`) becomes a direct
dependency.

### 7. Mixed versions

No wire contract changes, so every agent and hub pair keeps working.

- An agent that already had a machine id or a `hostname` binary keeps its id.
- An agent with neither (a random UUID per restart before) now presents its host name: one
  more new system, once, then a stable one.
- An agent whose machine-id file broke the hub's rule (refused forever before) now presents
  the next source.

### 8. Relation to the redb release

RFC 0010 §10 keeps a `LiveStatus` per system, with the connection number of RFC 0008's former
§5, and a restart rule of its own (`Unknown`, then `Offline` after 120 s). When it lands, it
carries this RFC's rules forward: `accept`, `claim` and `end` decide its `connection` field,
the silence rule becomes its staleness sweep over `last_contact`, and `SystemSource` becomes
RFC 0011's stored `Source`. Nothing here is persisted, so nothing needs migrating.

## Domain impact

- **Fleet Registry:** `SystemSource` and `polled_systems` (`registry.rs`); `PushPresence`,
  `ConnectionNumber`, `Ending`, `SILENCE_BOUND` and `is_silent` (new `presence.rs`, pure).
- **Ingestion:** the handshake's unit registers and accepts; a snapshot frame's unit claims,
  then stores; `end_connection` acts only on `Ending::Current`; the silence sweep
  (`push/sweep.rs`); the poller's filter.
- **Telemetry Publishing (agent):** `AgentId`, `AgentIdRule`, `IdSource`, `AgentIdError`,
  `resolve_push_id` (`push/identity.rs`); `StartupError::PushId`, exit code 78.
- **Glossary:**
  - **push system**: a system registered by a push handshake (`url` = `push://` until RFC
    0011), never polled;
  - **polled system**: any other system;
  - **connection number**: the in-memory number that identifies one accepted push
    connection;
  - **current connection**: of a push system's open connections, the one that last delivered
    a snapshot frame, or, before any, the one accepted last. Only its end marks the system
    offline;
  - **silent push system**: a push system with no contact (handshake or snapshot frame) for
    180 s, or none since the hub started 180 s ago. The silence sweep marks it offline;
  - **system status** changes: a push system is marked offline by its current connection's
    end, or by the silence sweep; polled systems as today;
  - **push system id** (agent): the id the agent presents in the handshake, resolved once per
    process.
- **Published contracts:** none changes. The handshake's answers, the frames and the poll
  responses are byte-identical.

## Alternatives considered

- **Only the latest-accepted connection is current** (RFC 0008's first revision). If the
  newest connection ended first, the system went offline while the older one still delivered
  snapshots, and stayed offline. Snapshots claiming currency fix both orders.
- **Keep the connection number in the database** (a `push_connection` column). A schema
  change for a value that means nothing across processes.
- **Claim inside the store's transaction**, under the database mutex, so no new lock exists.
  The end's check and its offline write would then need one database critical section with the
  live-metrics eviction inside it: a new `Database` method taking a closure that decides for
  the Fleet Registry. The presence lock keeps the decision in pure code, at the cost of one
  lock and one stated order.
- **Hold the presence lock across a frame's store** too. Correct, but every frame would hold
  it for its whole store; claiming before the store gives the same outcomes (§2).
- **Keep polling push systems, and skip only their offline marking.** Still one reqwest client
  per push system per tick (the CPU cost of Motivation 2), for nothing.
- **Sweep from the poller's tick**, sharing its registry read. It would tie the sweep's period
  and its last-read fallback to polling; a separate task costs one more read every 30 s.
- **Iterate only the presence entries.** Cheaper, but a push system with no entry (after a
  restart, or after a failed offline write) would stay online for good: exactly the gap the
  polls filled by accident.
- **No grace after a restart** (mark every push system with no entry offline on the first
  pass). Every push system would read offline for the few seconds its agent takes to
  reconnect, on every hub restart. 180 s is the bound the sweep uses anyway.
- **A default `SYSTEM_AGENT_ID_FILE`** (`/var/lib/system-agent/id`, RFC 0008's previous
  revision). The image has no writable volume there, so every start would warn or fail.
- **Write the file only when step 4 is reached** (RFC 0008's last revision). On Linux
  `gethostname` always answers, so step 4 is practically never reached and the file would
  never be written: a container's id would still change with each re-creation.
- **Write a fresh UUID into a missing file**, rather than the resolved id. Turning the file on
  would then re-register every agent once, leaving an offline twin.
- **Drop the host name source.** It would change the id of every agent that uses it today.
- **Write the file with `std` only** (`OpenOptions::create_new`, then write). A crash between
  the create and the write leaves an empty file, which step 1 then refuses: the agent would
  never start again without an operator. `tempfile`'s write-then-link has no such window.

## Security implications

- **A01 / API1 (push ids are self-asserted): unchanged.** Anyone who can push as an id (the
  push token, or anyone while it's unset) can make a connection current with a handshake, and
  its end then marks the system offline and evicts its live metrics until the agent's next
  snapshot. That is today's behaviour for every connection's end; this RFC only stops *older*
  connections from doing it. Two agents presenting one id alternate its status, as they
  already alternate its metrics.
- **API4:** the presence map holds at most one entry per open push connection (§2), and open
  connections stay unbounded as RFC 0006 recorded; this RFC adds about 280 bytes per
  connection to what each already costs. The sweep's work is one read and at most one write
  per push system per 30 s. Not polling push systems removes the dominant per-system CPU cost
  on the hub (about 0.36 s per push system per tick).
- **A04 / API6:** a hostile client can't use the sweep to mark a system offline: it only marks
  systems without contact, and any contact defers it.
- **A05 / API8:** an unreadable, invalid or unwritable `SYSTEM_AGENT_ID_FILE` refuses the
  agent's startup (exit 78), rather than pushing under a per-process id. The file is created
  `0600` by `tempfile`, owned by the agent's user; the id isn't a secret.
- **A09:** the sweep logs a count per pass and its first failed write; a superseded
  connection's end says so on its disconnect line; the agent logs its id's source, never the
  id, and one `warn` when the id lives for one process only. No token is logged anywhere.
- **A03:** no new SQL: `update_system_status`'s bound parameters. The new `last_error` text is
  a constant.
- **A06:** `tempfile` becomes a direct dependency of the agent, at the version already locked;
  `cargo audit` runs if it is installed.
- **A08:** the id file is written through a temporary file and a no-clobber link; nobody but
  the operator's volume can place one.
- **A02, A07 / API2, API3, API5, A10 / API7:** not touched. The handshake's token check is
  unchanged (`PushToken::accepts`, constant time).
- **API9:** `SYSTEM_AGENT_ID_FILE` goes into the README's agent table; the status rules into
  ARCHITECTURE.md.
- **API10:** the hub consumes push frames as before.

## Testing plan

TDD, per `CLAUDE.md`, with `red-test-adversary` and `rosette-auditor`.

- **Pure (hub):**
  - `SystemSource::of` over `push://`, `http://…`, `https://…`, `PUSH://`, `push://x` and `""`;
  - `polled_systems`: disabled systems and push systems left out, the registry's order kept;
  - `accept`: increasing numbers, each current;
  - `end` as a table: the current connection ends (`Current`, entry removed); an older one
    ends (`NotCurrent`, entry kept); an unknown system (`NotCurrent`);
  - claiming as a sequence, **newest exits first**: A accepted, A claims, B accepted, B ends
    (`Current`), A claims, A ends (`Current`);
  - `is_silent` as a table: `Offline` never; `Online` and `Unknown` with a contact exactly
    180 s old and one second younger; no entry with the sweep started exactly 180 s ago and
    one second later; a contact after `now`.
- **Hub adapters (real server, temp database):**
  - a host whose new connection delivers a snapshot, then whose old connection ends (closed,
    and, with a short injected idle deadline, timed out): still `Online`, live metrics kept;
  - the current connection's end: `Offline`, `push disconnected`, live metrics evicted (the
    RFC 0007 §4 test, kept);
  - newest exits first, over two sockets: B ends → `Offline`; A's snapshot → `Online`;
    A ends → `Offline`;
  - a handshake that registers nothing (`registry unavailable`) leaves no presence entry;
  - the sweep, run once with injected instants: a connected system whose last snapshot is
    180 s old → `Offline` with its `last_error`; a fresh one untouched; a push system with no
    entry, before and after 180 s since `started`; a polled system never touched; an
    already-offline row keeps its `last_error`;
  - the poller, one tick at a short period over a database holding an enabled `Online` push
    row: the row is still `Online` after the tick (today the tick marks it offline).
- **Agent** (`resolve_push_id` over temp directories and an injected host name function):
  - `AgentId` as a table, the same inputs as the hub's `SystemId` table, plus trimming;
  - each source in order, each skipped when missing, empty or breaking the rule;
  - `SYSTEM_AGENT_ID_FILE` set and missing: the resolved id is written, and a second
    resolution with a different host name reads the file's;
  - set and existing: the file wins over a machine id;
  - set and invalid, set and not UTF-8, set in a missing directory: each the right
    `AgentIdError`; `StartupError::PushId`'s exit code is 78 (the existing table);
  - the file created between the check and the link: `persist_noclobber`'s `AlreadyExists`
    path reads the other file's id, and never overwrites it;
  - unset, with no machine id and no host name: `IdSource::Random`.
  - "computed once" has no runtime test: the push task receives an `AgentId` by value, and no
    function that resolves one is reachable from it.

## Impact on `docs/ARCHITECTURE.md`

- § Components (hub): the silence sweep; push systems are not polled. (Agent): the id resolved
  in `start`.
- § Domain model: the Fleet Registry row (`presence.rs`, `SystemSource`, `polled_systems`),
  the Ingestion row (the sweep, the handshake and frame units, `end_connection`), the agent's
  Telemetry Publishing row (`push/identity.rs`); the glossary terms of Domain impact; the lock
  order (presence, database, live).
- § Trust boundaries, Agent → Hub (push): only the current connection's end marks offline.
- § Open architectural questions:
  - removed: push systems being polled and flapping; "any connection presenting an id evicts";
    the `hostname` shell-out; the 1,000 `push://` rows keeping every core busy (the per-poll
    client cost stays, for polled systems);
  - changed: RFC 0005's stored-id entry (the sweep, not the poller, marks those rows offline);
    "push auto-registration is unbounded" (no longer polled; still unbounded, RFC 0008);
  - added: after a restart, a push system whose agent is gone reads online for up to 210 s;
    `POST`/`PUT` can still write the `push://` sentinel (RFC 0011).
- `README.md`: the `SYSTEM_AGENT_ID_FILE` row, and how a push system goes offline.

## Rollout / migration notes

- No schema change and no wire change: hub and agent upgrade in either order.
- After the hub upgrade, push systems stop flapping at once. A push system whose agent is gone
  stays online up to 210 s after the hub starts, then goes offline for good.
- Agents keep their ids, except those that drew a random UUID per restart, which register one
  last time under their host name.
- To keep an agent's id across container re-creations, mount a volume and set
  `SYSTEM_AGENT_ID_FILE` to a path on it (`/var/lib/system-agent/id`, say). The first start
  writes the current id there.
