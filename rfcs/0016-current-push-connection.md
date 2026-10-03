# RFC 0016: The Current Push Connection, and the Agent's Id Computed Once

- Status: Draft
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-29 (revised the same day for two `rfc-adversary` passes; a third pass's findings are open; see Review)
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

/// Held by one connection's task for as long as the task runs, and dropped with it, a panic's
/// unwinding included. Not `Clone`. The presence entry keeps only a `Weak` of it.
pub struct ConnectionLease(Arc<ConnectionNumber>);

/// Which open connection is current for each push system (presence.rs). Pure: no lock, no
/// clock read, no I/O.
#[derive(Default)]
pub struct PushPresence { /* next: u64, current: HashMap<String, Current> */ }

/// A system's current connection: its number, whether its task still runs, and whether a
/// snapshot on it has claimed currency.
struct Current { number: ConnectionNumber, task: Weak<ConnectionNumber>, claimed: bool }

pub enum Ending {
    /// The ending connection was current: its entry is removed, and the caller marks the
    /// system offline and evicts its live metrics.
    Current,
    /// Another connection is current, or none is: nothing changes.
    NotCurrent,
}

impl PushPresence {
    /// A handshake accepted: takes the next number, and makes it current only when the system
    /// has no live current connection.
    pub fn accept(&mut self, system_id: &SystemId) -> ConnectionLease;
    /// A snapshot frame on `lease`'s connection: that connection becomes current.
    pub fn claim(&mut self, system_id: &SystemId, lease: &ConnectionLease);
    /// `lease`'s connection ended. Takes the lease by value: nothing can claim with it after.
    pub fn end(&mut self, system_id: &SystemId, lease: ConnectionLease) -> Ending;
    /// What the disconnection sweep does with one push system (§4).
    pub fn sweep(&mut self, system_id: &str, status: &SystemStatus, up_for: Duration) -> Sweep;
}
```

- **Numbers.** `accept` takes the next number from one hub-wide counter, inside `PushPresence`.
  It wraps on overflow (at a million connections a second, after 584,000 years).
- **A live current connection** is an entry whose `Weak` still upgrades: its task hasn't
  returned or unwound. An entry whose task is gone counts as none.
- **Only a push system has a current connection.** Registration (`register_if_new`) reads the
  row's `url` with its existence check, in the same statement, and returns
  `Registration::New` or `Registration::Known(SystemSource)`. A new id is registered as a push
  system with status **`Unknown`**, not `Online`: it reads online only once a snapshot is
  stored, so a new host whose frames never decode never reads online. When the id belongs to a
  **polled** system (`Known(Poll)`), the handshake is still accepted, as today, but `accept` is
  not called: the connection holds no lease, its snapshots are stored but claim nothing, and its
  end marks nothing offline and evicts nothing. So a push-token holder who presents a polled
  system's id can still inject snapshots (A01, as today) but can no longer keep it offline.
- **The handshake makes its connection current only when there is none.** Registration
  and `accept` run in the handshake's one unit of blocking work, under the
  presence lock, after the id is authenticated, and `accept` runs only once registration
  succeeded, so a refused handshake never leaves an entry behind. When the system already has
  a live current connection, the new one is accepted but not current: **between open
  connections, only a snapshot moves currency.** So a connection that never sends a snapshot
  can't take currency from one that does, and its end changes nothing.
- **A snapshot frame claims currency.** Every snapshot frame that passed the edge
  (`SnapshotFrame::try_from`) on a connection that holds a lease runs `claim` in its unit of
  blocking work, just before its store, which also marks the entry `claimed`.
  So the current connection is the one whose snapshot claimed last, or, before any, the first
  one accepted while none was current. Application frames and pings don't claim: they write no
  status. A reconnecting agent claims with its first tick (immediate, then every 2 s by
  default), long before its old connection's 90 s idle deadline.
- **Only the current connection's end marks offline.** `end_connection` takes the presence
  lock and calls `end` with the connection's lease, by value (a connection without a lease,
  on a polled system, ends with no write). `Current`: it marks the system offline and evicts its live metrics, as
  RFC 0007 §4 does today, under that lock. `NotCurrent`: it does neither, and its
  `Push client disconnected` line says another connection is current. This covers every exit
  of `handle_push` after registration: an `auth_ok` that couldn't be sent, the socket's end,
  the idle deadline, an oversize message, a failed unit of work, and `SystemGone`.
  - A host that reconnected, and whose new connection delivered a snapshot, isn't marked
    offline, nor emptied of live metrics, when its old connection times out.
  - A host whose current connection died half-open is marked offline at its idle deadline,
    whatever other connection presents its id without sending snapshots.
  - **Newest exits first.** If the connection that claimed last ends while an older one still
    delivers snapshots (two agents presenting one id, or a half-open socket that recovers), it
    marks offline and evicts, and the older one's next snapshot claims currency and marks it
    online again. Its own end later marks offline.
- **A task that panics** drops its lease while unwinding: nothing is spawned or locked in a
  destructor. Its entry then counts as no current connection, so the next sweep pass (within
  30 s) marks the system offline and removes the entry (§4). A normal exit still runs
  `end_connection` at once. A task stays alive past its socket only while it awaits: a unit of
  blocking work (queued behind the database mutex or the blocking pool, with no deadline of its
  own), the oversize linger (30 s), or a send (5 s). **An entry means the task runs,** not that
  its socket is live; RFC 0006's 90 s idle deadline bounds a task that is reading.
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
- **Memory.** One entry per push system with a current connection: at most the open push
  connections (which RFC 0006 leaves unbounded, see Security), plus the entries of panicked
  tasks until the next sweep, each an id of at most 255 bytes and 16 bytes more; a lease's
  allocation is 24 bytes, freed with the last of the task and the entry.
- **A deleted system**'s entry is left to its connection: that connection's next snapshot
  gets `SystemGone`, its end is `Current`, and it removes the entry (its offline write finds no
  row). `DELETE` itself stays as RFC 0007 left it.

### 3. The poller visits polled systems only

`registry::enabled_systems` becomes `polled_systems`: the enabled systems whose source is
`Poll`, in the registry's order. A push system is never polled, so it no longer flaps, and no
poll builds a reqwest client for it.

### 4. Startup, and the disconnection sweep

**At startup, a push system's status is unknown.** The hub has no graceful shutdown, so the
`Online` a push row holds was written by the previous process, and says nothing about now.
Before it builds `AppState` (so before the first SSE summary is built, and before any
handshake), `main` runs one statement on the blocking pool,
`Database::reset_status(PUSH_URL, Online, Unknown)`: `UPDATE systems SET status = ?3 WHERE
url = ?1 AND status = ?2`, all three bound, `last_seen` and `last_error` kept. A push system
reads `online` again only when a snapshot from its agent is stored. A failed reset is logged at
`warn`, and the hub serves anyway. A rolled-back hub reads `unknown` as it always has.

A push system with no live current connection is **disconnected**. Its current connection's
end has already marked it offline, unless that write failed, or its task panicked, or the hub
restarted (no connection's end ran), or `PUT` made it a push system. The sweep marks those
offline.

Every 30 s, a task started by `main` (`push::start_disconnection_sweep`) runs one unit on the
blocking pool:

1. a narrow registry read, `Database::system_sources`: `SELECT id, url, status FROM systems`.
   Each row maps on its own: a row whose `id` isn't text (`id TEXT PRIMARY KEY` admits a NULL
   or a BLOB written by hand) is skipped and counted, and `status` maps any unknown text to
   `Unknown`, as `list_systems` does. It reads no token. A failed read skips the pass with one
   `warn`; skipped rows are counted in that pass's `warn`;
2. for each row whose `SystemSource::of(url)` is `Push`, whatever its `enabled` (push ingestion
   ignores `enabled`, so the sweep does too), it takes the presence lock and asks `sweep`:

| Stored status | Live current connection | Hub up for | Outcome |
|---|---|---|---|
| `Offline` | any | any | `Leave`: already offline |
| `Online` | yes, and it has claimed | any | `Leave` |
| `Online` | yes, but it never claimed | under 120 s | `Leave` |
| `Online` | yes, but it never claimed | 120 s or more | `MarkOffline(NotConnected)`: an `Online` with no snapshot behind the current connection is stale (a dead entry replaced by a connection that only pings, or an end whose offline write failed) |
| `Unknown` | yes | any | `Leave` |
| `Online` or `Unknown` | none | under 120 s | `Leave`: its agent may still be reconnecting |
| `Online` or `Unknown` | none | 120 s or more | `MarkOffline(NotConnected)`, or `MarkOffline(InvalidSystemId)` when the id breaks `SystemId` |

`sweep` also removes an entry whose task is gone. `MarkOffline` writes, under the presence
lock, `status = offline`, `last_seen = ""` and `last_error` = `push disconnected`
(`NotConnected`, the text a connection's end writes) or `invalid system id`
(`InvalidSystemId`, the text today's poll writes for such rows). The live metrics are left, as
a failed poll leaves a polled system's.

- **Time.** `up_for` is the monotonic time since the sweep task started (after `AppState::new`,
  before the bind), read once per pass, in the adapter. **120 s** is RFC 0010 §10's restart
  rule. Every shipped agent retries every 5 s once it notices the hub is gone; after a hub
  *process* restart it notices at once (the socket is reset). After a hub host reboot or a
  partition, an agent with a dead socket notices only when TCP gives up on it, which can take
  longer than 120 s: such a system reads offline until its agent reconnects.
- **What it catches:** a failed offline write and a panicked task (within 30 s); after a
  restart, every push system whose agent doesn't come back (`unknown` from the start, offline
  120 s to 150 s after it; today's first poll marked it offline at once, then flapped); rows
  `PUT` made push systems; and rows stored before RFC 0005 whose id breaks `SystemId`, which no
  handshake can make current.
- **What it doesn't:** a live current connection that never gets a snapshot stored, while its
  system reads `Unknown` (the row above marks an `Online` one offline). An agent
  pushing every 300 s is such a connection between its snapshots, and it is online. A
  connection whose snapshots never store (frames the hub can't decode, a failing database)
  keeps the status its last stored snapshot left: after a restart, `unknown`, never `online`.
  So a hub upgraded before its agents, whose frames it can't decode, shows them `unknown`,
  and logs it (below).
- **Undecodable frames are counted.** Today a binary message that decodes neither as a
  snapshot nor as an application frame is dropped without a word. It is now counted on the
  connection, the first one logged at `warn` and the count at `info` when the connection ends,
  as refused frames already are (`warn_first`, `log_counts`).
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
API and the SSE summary's shape. `update_system_status` stays the offline writer (the startup
reset is the one new status write, §4). The poller's
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
  removes its temporary file and reads that file once more, and that read is final: any
  failure there, `NotFound` included (a dangling symlink at the file's name), is `Unreadable`,
  never another write;
- any other failure (`Unwritable`: a missing or read-only directory, a full disk) removes the
  temporary file if it was created, and refuses startup with exit code 1. The operator asked
  for a stable id, and a silent fallback would register a new system on the next re-creation.
  A `link(2)` refused with `EPERM` or `ENOTSUP` (a filesystem without hard links: vfat, some
  network and FUSE mounts) is reported as such, so the operator doesn't chase ownership. A
  crash can leave a stray `.tmp` file, which nothing reads.

The write captures the id the agent resolves at that start. On a host whose machine id or host
name doesn't change, that is the id it already had, so turning the file on doesn't re-register
it. **In a container it does, once:** adding the variable and the volume re-creates the
container, whose new host name is the one captured. The operator deletes the old system; from
then on the id survives re-creations.

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
forward: `accept`, `claim` and `end` decide its `connection` field; its `Unknown` at open is
§4's startup reset; the disconnection sweep marks offline a push system with no live current
connection 120 s after the store opens (its contact rule stays for polled systems); and
`SystemSource` becomes RFC 0011's stored `Source`. A push system's `last_contact` there stays a
display value (the last frame), not a liveness rule: a connection that pushes every 300 s is
online. Nothing here is persisted, so nothing needs migrating.

## Domain impact

- **Fleet Registry:** `SystemSource`, `PUSH_URL` and `polled_systems` (`registry.rs`);
  `PushPresence`, `ConnectionNumber`, `ConnectionLease`, `Ending`, `Sweep`, `OfflineReason`
  and `RECONNECT_GRACE` (new `presence.rs`, pure).
- **Ingestion:** the handshake's unit registers (`Registration::{New, Known(SystemSource)}`,
  a new id `Unknown`) and accepts only for a push system; a snapshot frame's unit claims,
  then stores; `end_connection` acts only on `Ending::Current`; the lease a connection's task
  holds; the startup reset and the disconnection sweep (`push/sweep.rs`);
  `Database::system_sources` and `Database::reset_status`; the undecodable-frame count; the
  poller's filter.
- **Telemetry Publishing (agent):** `AgentId`, `AgentIdRule`, `IdSource`, `AgentIdError`,
  `resolve_push_id` (`push/identity.rs`); `StartupError::PushId`.
- **Glossary:**
  - **push system**: a system registered by a push handshake (`url` = `push://` until RFC
    0011), never polled;
  - **polled system**: any other system;
  - **connection number**: the in-memory number that identifies one accepted push
    connection;
  - **current connection**: of a push system's open connections, the one whose snapshot frame
    claimed last, or, before any, the first one accepted while none was current. Only its end
    marks the system offline;
  - **disconnected push system**: one with no live current connection. The disconnection
    sweep marks it offline once the hub has run for 120 s;
  - **system status** changes: a push system is marked offline by its current connection's
    end, or by the disconnection sweep; polled systems as today;
  - **push system id** (agent): the id the agent presents in the handshake, resolved once per
    process.
- **Published contracts:** none changes. The handshake's answers, the frames and the poll
  responses are byte-identical. A new push id is registered `Unknown` instead of `Online`,
  which the dashboard shows until its first snapshot is stored.

## Alternatives considered

- **Only the latest-accepted connection is current** (RFC 0008's first revision). If the
  newest connection ended first, the system went offline while the older one still delivered
  snapshots, and stayed offline. Snapshots claiming currency fix both orders.
- **A silence rule over the last contact** (this RFC's first draft: offline 180 s after the
  last snapshot or handshake). An agent with `PUSH_INTERVAL` of 180 s or more, which is legal,
  would flap between snapshots. Counting pings as contact would fix that, at a blocking unit per
  ping (pings aren't budgeted, so it would need its own rate limit), and would still only catch
  what the lease catches: a task that died without its end.
- **Every accepted connection becomes current** (this RFC's second draft). A connection that
  only pings could then take currency from the agent's, and when the agent's connection died
  half-open its end changed nothing: the system stayed online, with frozen live metrics, for as
  long as the pinging connection lived.
- **A guard whose `Drop` runs `end_connection`** (the second draft). A destructor that runs while
  a panic unwinds must not panic, and `spawn_blocking` panics when no thread can be spawned,
  the likeliest cause of the panic in the first place: one connection's failure would abort the
  hub. The lease makes the task's end visible to the sweep without running anything in `Drop`.
- **Keep the stored `Online` across a restart.** A reconnecting agent whose frames the new hub
  can't decode would read online for good, with no stored snapshot behind it: the silent
  failure this repository fears most, made invisible on the dashboard too.
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
  would then re-register every agent once, hosts included, leaving an offline twin; the
  resolved id re-registers only containers, whose host name changes with the re-creation that
  turning the file on needs anyway.
- **Fall back to `rename` where `link(2)` isn't supported.** It loses no-clobber for a case
  (a filesystem without hard links) that a named volume on a local driver never is.
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

- **A01 / API1 (push ids are self-asserted): narrowed, not closed.** For a **polled**
  system's id, a push connection holds no lease (§2): its snapshots are stored, as today, but
  its end never marks the polled system offline. For a push system's id, anyone who can push
  as it (the push token, or anyone while it's unset) can still send snapshots as it, which
  claim currency: their connection's end then marks the system offline until the agent's next
  snapshot, and two senders alternate its status, as they already alternate its metrics. A
  connection that sends no snapshot can no longer do anything: it isn't current while the
  agent's is, so its end changes nothing. Today every connection's end marks offline.
- **API4:** the presence map holds at most one entry per open push connection (§2), and open
  connections stay unbounded as RFC 0006 recorded; this RFC adds about 270 bytes per
  connection to what each already costs. The sweep's work is one narrow read every 30 s and at
  most one write per push system, once. Not polling push systems removes the dominant
  per-system CPU cost on the hub (about 0.36 s per push system per tick).
- **A04 / API6:** a hostile client can't hide an outage by holding a connection open: a
  connection that sends no snapshot is never current while the agent's is, and when it is
  current (it came first, after the agent's end), it writes no status, so the system stays
  offline, or `unknown` after a restart. Hiding an outage takes injected snapshots, which go
  into the history as today. Nor can it use the sweep: the sweep marks only systems with no
  live current connection.
- **A05 / API8:** a relative path, or an id file that isn't UTF-8 or breaks the rule, refuses
  the agent's startup (exit 78); an unreadable or unwritable one refuses it too (exit 1),
  rather than pushing under a per-process id. The file is created with the process umask; the
  id isn't a secret. The image's new directory is owned by the agent's user and empty.
- **A09:** the sweep logs a count per pass, its first failed write, a failed read and skipped
  rows; the startup reset logs a failure; undecodable frames are counted and the first logged,
  where today they vanish; a connection whose end isn't current says so on its disconnect line;
  the agent logs its id's source at
  startup, and the id in `Debug` form when it authenticates, so a control character can't
  forge a line. No token is logged anywhere.
- **A03:** the new read has no parameter; the reset binds all three values; the offline write
  is `update_system_status`, with bound parameters. The new `last_error` texts are constants.
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
  - `accept` as a table: increasing numbers; current when the system has no entry, when its
    entry's lease was dropped, and not when a live connection is current;
  - `end` as a table: the current connection ends (`Current`, entry removed); another one
    ends (`NotCurrent`, entry kept); an unknown system (`NotCurrent`);
  - claiming as a sequence: A accepted and claims; B accepted (not current); A ends
    (`Current`); B claims (current); and **newest exits first**: A claims, B claims, B ends
    (`Current`), A claims, A ends (`Current`);
  - `sweep` as the §4 table: each status, with a live current connection, with none, and with
    one whose lease was dropped (removed, and swept), at 119 s and at exactly 120 s, and a
    disconnected row whose id breaks `SystemId` (`InvalidSystemId`).
- **Hub adapters (real server, temp database).** A test that asserts something *didn't*
  happen first waits for an event that comes after it would have: the hub closing the socket
  (`handle_push` returns after `end_connection`), or the poller's third poll of a counted
  polled system. Rows marked *characterisation* pin behaviour today's code already has, so
  `red-test-adversary` attacks them in mutation mode rather than as red tests.
  - reconnect: A delivers a snapshot, B is accepted and delivers one, then A ends (closed by
    the agent, and, with a short injected idle deadline, timed out): once the hub has closed A,
    still `Online`, live metrics kept, nothing sent on B after A's close;
  - **a connection that only pings can't mask an outage:** A delivers a snapshot; B is
    accepted and sends nothing but pings; A closes: once the hub has closed A, `Offline`, live
    metrics evicted; B closes: still `Offline` (today: `Offline` too, so the first half is
    characterisation and the red half is the reconnect row above);
  - the current connection's end: `Offline`, `push disconnected`, live metrics evicted
    (characterisation: the RFC 0007 §4 test, kept);
  - newest exits first, over two sockets: A and B each deliver a snapshot (B last); B ends →
    `Offline`; A's snapshot → `Online`; A ends → `Offline` (characterisation);
  - one table over the handshake, observed through the sweep at `up_for` ≥ 120 s: registered
    and open → a new row is `Unknown` and stays so; `registry unavailable` → no entry, so a
    planted `Online` row with that id is swept; a polled system's id → accepted, and its
    close leaves the polled row `Online` (barrier: the hub's close of the socket);
  - `push_handshake_with_the_configured_token_is_accepted_and_registers_system` asserts the
    new row is `Unknown` (today `Online`);
  - a lease dropped by a panicking task (a test seam that panics in the connection's task after
    its accept): the next sweep marks the system `Offline`, and the hub still serves;
  - the startup reset: `Online` push rows become `Unknown` with `last_seen` and `last_error`
    kept; `Offline` push rows and `Online` polled rows untouched;
  - **a restart with undecodable frames:** a planted `Online` push row, the reset, then a
    connection for it that sends only binary messages the hub can't decode; while the
    connection is still open, `wait_until_hub_caught_up`, then the row is exactly `Unknown`
    (the reset's value: the close's offline write hasn't run). The count is checked
    separately, through the connection's counts, not through a log line;
  - `sweep` rows for `Online` with a live current connection that never claimed: `Leave` at
    119 s, `MarkOffline(NotConnected)` at 120 s;
  - **the binary** (`tests/`): plant an `online` `push://` row, start the hub, and after its
    "listening" line `GET /api/systems/<id>` returns `unknown`, so `main` runs the reset
    before serving. The sweep's start is not tested at the binary level: its 120 s grace
    can't be injected into the binary;
  - the sweep, run once with an injected `up_for`: a disconnected `Online` and `Unknown` push
    row → `Offline` with `push disconnected`; a connected one untouched; a disabled push row
    swept; a row with id `..` → `invalid system id`; a polled row never touched; an `Offline`
    row keeps its `last_error`; before 120 s nothing written; a database also holding a row
    with a negative `poll_interval_secs` still swept;
  - `system_sources` over rows `list_systems` can't map, and a row with a NULL id (skipped and
    counted);
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
    id, never overwrites it, and leaves no temporary file; a dangling symlink at the file's
    name: `Unreadable` after one link attempt, no loop;
  - unset, with no machine id and no host name: `IdSource::Random`.
  - **the binary** (`tests/startup.rs`): a relative `SYSTEM_AGENT_ID_FILE` refuses startup with
    78 before anything starts (no connection reaches `PUSH_TO`), naming the variable and not
    the path; a missing one in a temp directory is written at startup, with an id that follows
    the rule.
  - the `Push authenticated` line renders the id in `Debug` form (an id holding a newline is
    printed escaped), through the function that formats it;
  - "computed once" has no runtime test: the push task receives an `AgentId` by value, and no
    function that resolves one is reachable from it.

## Impact on `docs/ARCHITECTURE.md`

- § Components (agent): the id resolved in `start`; the configuration and exit-code paragraph
  (78 also for `SYSTEM_AGENT_ID_FILE`'s refused values, 1 for its I/O failures). (Hub): the
  startup reset and the disconnection sweep; push systems are not polled; live metrics evicted
  by the current connection's end; undecodable frames counted.
- § Domain model: the Fleet Registry row (`presence.rs`, `SystemSource`, `polled_systems`),
  the Ingestion row (the reset, the sweep, the handshake and frame units, the lease,
  `end_connection`, `system_sources`, `reset_status`), the agent's Telemetry Publishing row (`push/identity.rs`); the glossary
  terms of Domain impact, and the **live metrics**, **last seen** and **system status**
  entries; the lock order (presence, database, live).
- § Trust boundaries, Agent → Hub (push): only the current connection's end marks offline.
- § Testing architecture: the barrier rule for tests that assert nothing happened; the
  agent's new startup cases; the hub's binary reset test.
- § Data flow and § Domain model (the push frame contract): "the hub silently drops frames
  that fail to decode" becomes "drops, counts and logs (first at `warn`, the count at the
  close)". `CLAUDE.md` says the same in its DDD section; that file is the owner's, so the
  change is proposed to the owner, not made by this RFC. The glossary's *system status*:
  "a new push id is registered `Unknown`", not online.
- § Open architectural questions:
  - removed: push systems being polled and flapping; "any connection presenting an id evicts";
    the `hostname` shell-out; the 1,000 `push://` rows keeping every core busy (the per-poll
    client cost stays, for polled systems);
  - changed: RFC 0005's stored-id entry (the sweep, not the poller, marks those rows offline);
    "push auto-registration is unbounded" (no longer polled; still unbounded, RFC 0008);
  - added: after a restart, a push system whose agent is gone reads `unknown` for up to 150 s,
    then offline; `PUT` can still write the `push://` sentinel (RFC 0011); a live current
    connection whose snapshots never store keeps the status its last stored snapshot left;
    a task stuck in a unit of blocking work keeps its entry past its socket's death.
- `README.md`: push systems read `unknown` after every hub restart and at registration,
  until their first snapshot is stored; the `SYSTEM_AGENT_ID_FILE` row and its Compose lines; the startup paragraph's
  list of variables parsed first and its exit codes; the handshake paragraph's id sources; the
  deadlines paragraph's offline marking (by the current connection, and the sweep).
- RFC 0010 §10: `Unknown` at open is this RFC's startup reset; a push system goes offline only
  through its current connection's end or the disconnection sweep (no live current connection,
  120 s after open); its contact rule stays for polled systems.

## Rollout / migration notes

- No schema change and no wire change: hub and agent upgrade in either order.
- After the hub upgrade, push systems stop flapping at once. At every hub start, push systems
  read `unknown` until their agent's first snapshot is stored; one whose agent is gone goes
  offline 120 s to 150 s after the start.
- Agents keep their ids. Those that drew a random UUID per restart keep doing so until they
  set `SYSTEM_AGENT_ID_FILE`.
- Turning `SYSTEM_AGENT_ID_FILE` on for a container re-creates it, so it registers once more
  under its new host name; delete the old system. On a host it keeps the id it had.
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

`rfc-adversary`, second pass, on the revision above:

| Finding | Verdict | Resolution |
|---|---|---|
| a handshake takes currency from a live connection, so a connection that only pings hides an outage | CONFIRMED | `accept` makes a connection current only when none is live; between open connections only a snapshot moves currency; a test row (§2, Testing plan) |
| after a restart, a system whose frames never store reads online for good | CONFIRMED | the startup reset: push rows `Online` → `Unknown` before serving; undecodable frames counted and logged; a test row (§4) |
| RFC 0010 §10 carries two restart rules | CONFIRMED | 0010 §10 amended: `Unknown` at open, push systems offline only by the end or the sweep, the contact rule for polled systems (§8, Impact) |
| turning the id file on in a container re-registers it | CONFIRMED | stated, with the operator's step, in §6 and Rollout; the fresh-UUID alternative's reason restated |
| the guard's `Drop` can abort the hub by panicking in `spawn_blocking` while unwinding | PLAUSIBLE | no guard: the task holds a lease whose `Weak` the entry keeps; the sweep sees a dead task (§2, §4) |
| a green "red" test; no test for the `Debug`-form id; no test for the two confirmed attacks; the handshake table's observable | CONFIRMED | characterisation rows labelled; the `Debug` row; the ping-only and undecodable-restart rows; the handshake table observed through the sweep (Testing plan) |
| a NULL or BLOB id fails the sweep's read every pass | PLAUSIBLE | each row maps on its own; skipped rows counted in the pass's `warn` (§4) |
| a dangling symlink at the id path loops | PLAUSIBLE | the read after `AlreadyExists` is final; `NotFound` there is `Unreadable`; a test row (§6) |
| a filesystem without hard links exits 1 reading as a permission error | PLAUSIBLE | reported as a filesystem without hard links; the `rename` fallback rejected (§6, Alternatives) |
| the liveness bounds overstated | CONFIRMED | reworded: an entry means the task runs; a hub host reboot can take agents past the grace (§2, §4) |

Came closest among the rejected: the lease's premise that a panicking task drops its locals,
checked against axum's `on_upgrade` (`tokio::spawn`) and tokio's harness (the future is dropped
inside its panic guard).

`rfc-adversary`, third pass, on the second revision (5fe04f6). All resolved in the third
revision, each by the fix the pass proposed; the red tests in 6842329 and 0fe18c2 follow them
(`end` takes the lease, the new `sweep` rows, the `Unknown` registration):

| Finding | Verdict | Cheapest fix proposed (adopted) |
|---|---|---|
| a new id registers `Online` with no snapshot (`push/ingest.rs:184`), so the startup reset only covers rows that exist at startup, and a new host whose frames never decode reads online for good | CONFIRMED | `register_if_new` registers `Unknown`; flip the handshake-table row, the assertion in `push_handshake_with_the_configured_token_is_accepted_and_registers_system` and ARCHITECTURE's "registered online at its handshake" |
| the undecodable-after-restart test's barrier (the `info` line at the close) comes before `end_connection`'s offline write, so it passes without the reset; the in-crate harness captures no logs | CONFIRMED | after the undecodable frames, `wait_until_hub_caught_up` while the connection is open, then assert exactly `Unknown`; check the count separately |
| RFC 0010 §10 still flushes `connection: Option<u64>` in `LiveStatus` and sweeps on "no current connection", not "no live current connection" | CONFIRMED | take `connection` out of the flushed `LiveStatus`, keep the in-memory `PushPresence` with its liveness, say "live current connection", and add the presence lock to 0010's lock order (or keep registration outside it) |
| A01 overclaims: a handshake that presents a polled system's id becomes current, and its close writes `Offline` and evicts live metrics, so anyone with the push token (or anyone, when it is unset) keeps a polled system offline | CONFIRMED | the handshake unit reads the `url` with the existence check and never makes a connection current for a `Poll` source; otherwise restate A01 |
| nothing tests that `main` runs the reset before serving, or starts the sweep | CONFIRMED | one binary test: plant an `online` `push://` row, start the hub, after "listening" `GET /api/systems/<id>` returns `unknown`; state that the sweep's start is untested at binary level (the 120 s grace can't be injected) |
| RFC 0008 §5 still paraphrases the rejected currency rule | CONFIRMED | point to 0016 §2 instead of paraphrasing it |
| the impact list misses ARCHITECTURE's "silently drops" (§ Data flow, § Domain model), `CLAUDE.md`'s same sentence (the owner's to change), and the README note on `unknown` after each restart | CONFIRMED (minor) | add these sections to Impact |
| a dead entry, or an end whose offline write fails, lets a connection that only pings take currency and freeze a stale `online` | PLAUSIBLE | record in `Current` whether the connection has claimed; the sweep treats `Online` + a live current connection that never claimed as disconnected; units carry `(ConnectionNumber, Weak)`, and `end` takes the lease by value |
| the first SSE summary is built before the reset, so pre-restart `online` shows for up to 5 s | PLAUSIBLE | run the reset before `AppState::new` |

Came closest among the rejected: the honest reconnect that hasn't claimed yet. `end` holds the
presence lock across its offline write, and the agent's first tick is immediate, so the result
is a short offline blip.
