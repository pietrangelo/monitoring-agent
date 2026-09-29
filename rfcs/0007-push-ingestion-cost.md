# RFC 0007: One Snapshot Rule, One Transaction, One Serialisation

- Status: Accepted
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-25 (revised 2026-09-29 for the first five `rfc-adversary` passes, against the
  hub as RFCs 0006, 0009, 0014 and 0015 left it)
- Affects: `system-hub`
- Depends on (both Implemented):
  - RFC 0006: the 512 KiB message limit bounds a frame before this RFC's rules run.
    `PushConfig` gains a field here, and `push::on_blocking_pool` runs the push path's storage
    work, as it does today.
  - RFC 0009: `Database::store_round`, whose guarded-store shape `store_snapshot` follows, and
    `SourcePace`, whose token bucket §3's decode budget shares. RFC 0009's header names this
    RFC's `MIN_FRAME_SPACING`, for snapshot frames only. §3 replaces that spacing with a budget
    every binary message spends before it's decoded. A round's admission stays RFC 0009's
    source pace alone.
- **Order (owner's decision):** this RFC ships first, as one pull request on today's SQLite hub.
  - RFC 0016 (planned: the current-connection lifecycle and the agent's id, split from RFC 0008
    §5 and §7) comes after it, also on SQLite. It narrows §4's eviction to the current
    connection.
  - RFC 0010 (Draft, the redb release with RFCs 0008, 0011 and 0012) supersedes §2 once
    implemented, carries §2's cache rule forward, and maps snapshots through §1. It stamps
    points at hub time, so it drops §1's `SnapshotTime` refusal (§1).
  - Nothing here depends on RFC 0008, 0010 or 0016.
- Related: split from an earlier, wider draft of RFC 0006 (see git history). This RFC keeps the
  part that belongs to Fleet History: what a snapshot costs to store, and the live state it
  leaves behind.

## Motivation

Once RFC 0006 bounds a connection, what one frame, or one poll, costs the hub is still
unbounded, and so is what it leaves in memory:

1. **Per-point commits.** Each metric point is one `insert_metric` call: an INSERT, a SELECT and
   a DELETE.
   - The INSERT and the DELETE each commit on their own, and every statement is prepared anew
     (rusqlite's `Connection::execute` doesn't cache).
   - Each frame and each poll also runs `update_system_status`, another commit.
   - All of it runs under the hub's single SQLite mutex, which REST and SSE handlers also take
     from async code. The poller's `store_metrics` runs on the async runtime itself.
2. **A scan per frame that nobody reads.** Each push frame, and each push disconnect, calls
   `refresh_cache()`: a `list_systems()` scan of `systems`, `ORDER BY name`, under the mutex.
   The only reader of `systems_cache` is the poller, and it refreshes the cache itself before
   each 30 s tick.
3. **Unbounded, unchecked disks.** A snapshot can carry any number of disks, with mount points
   of any length, and each disk becomes a metric row and an entry in `live_metrics`. The two
   paths map a snapshot in their own code (`push/mod.rs::metric_points`,
   `collector/mod.rs::store_metrics`), which is already an open architectural question, and
   they disagree:
   - push stores a NaN as it comes. SQLite binds it as NULL, `NOT NULL` refuses it, and the
     error is discarded;
   - poll stores `0.0` for a value the agent didn't report, and `disk:` for a disk without a
     mount point;
   - the poller reads `/api/system` with no size cap.
4. **Frames arrive back to back.** Nothing limits how fast a connection sends binary messages,
   and each one is decoded in full, on a runtime worker, before anything else happens. That
   holds for application frames too: RFC 0009's source pace gates a round's store, after its
   decode, `TryFrom` and digest, and a frame that fails to decode or is refused spends
   nothing.
5. **Live state never shrinks.** `live_metrics` entries are removed neither on disconnect nor
   on delete: `DELETE /api/systems/:id` removes only `live_applications`. A deleted system's
   push connection keeps refreshing its entry. Its metric inserts fail, since the bundled
   SQLite enforces foreign keys, and the error is discarded. A poll racing a delete recreates
   the entry the same way.
6. **SSE repeats everything per subscriber.** Every 5 s, each `/api/stream/summary`
   subscriber:
   - runs `list_systems()` and `count_active_alerts()` under the database mutex, on the async
     runtime;
   - clones the whole `live_metrics` map, every disk of every system;
   - builds a `serde_json::Value` tree from it, and serialises that to a string;
   - copies the string into its event.

   Subscribers are unauthenticated and unbounded.

Measured with the hub's schema, statements and SQLite defaults, on a table filled to the 24 h
retention window (the Appendix has the script, the environment and more rows):

| Frame | Today | One transaction (§2) |
|---|---|---|
| 5 disks (10 points) | 97 ms | 5.7 ms |
| 1024 disks (1029 points) | 9.9 s | 41 ms (48 ms without the statement cache) |

- "Today" is each point's three statements, the status update, `update_registry`'s
  `get_system` read and `refresh_cache`'s scan of 1,000 systems, every statement prepared
  anew.
- "One transaction" is §2: the row check, every point and the status in one transaction
  through cached statements, then the `get_system` read.
- On the measured disk one commit costs about 4.6 ms, and it dominates: today's 5-disk frame
  is 21 commits. So at the default 2 s interval, about 20 pushing agents saturate the mutex
  today, and about 350 with §2.

## Proposed design

### 1. One snapshot rule, for push and poll

A new Fleet History module, `system-hub/src/snapshot.rs`, holds the rule as pure code. Each
path's DTO converts into a `ReportedSnapshot` at its edge: what the agent reported, with no
rule applied. The rule turns that into a `Snapshot`, which holds only what the hub keeps.

```rust
/// A snapshot as an adapter read it, before the snapshot rule. `None`: not reported.
pub struct ReportedSnapshot {
    pub cpu: Option<f32>,
    pub memory: Option<f32>,
    pub swap: Option<f32>,
    pub load1: Option<f64>,
    pub load5: Option<f64>,
    pub disks: Vec<ReportedDisk>,            // in reported order
}

pub struct ReportedDisk {
    pub mount_point: Option<String>,
    pub usage_percent: Option<f32>,
}

/// A snapshot as the hub keeps it. History and live metrics are both built from it.
pub struct Snapshot {
    scalars: Vec<(Scalar, f32)>,             // finite, at most one per scalar
    disks: Vec<(MountPoint, f32)>,           // finite, at most MAX_DISKS, in reported order
}

pub enum Scalar { Cpu, Memory, Swap, Load1, Load5 }  // metric names cpu, memory, swap, load1, load5

pub struct MountPoint(String);               // 1..=256 bytes, no control character

/// What the snapshot rule left out of one snapshot, by reason.
pub struct LeftOut {
    pub not_reported: usize,
    pub not_finite: usize,
    pub invalid_mount_point: usize,
    pub over_disk_limit: usize,
}

pub const MAX_DISKS: usize = 1024;
pub const MAX_MOUNT_POINT_BYTES: usize = 256;

pub fn snapshot_rule(reported: ReportedSnapshot) -> (Snapshot, LeftOut);

impl Snapshot {
    /// The snapshot's metric points: cpu, memory, swap, load1, load5, then disk:<mount point>.
    pub fn metric_points(&self) -> impl Iterator<Item = (String, f32)> + '_;
}
```

The rule, value by value:
- **Scalars.** A value that isn't reported, or isn't finite, is left out on its own and
  counted. Loads are converted to `f32` first, so a load too large for `f32` counts as not
  finite. Nothing becomes `0.0`.
- **Mount points.** A disk whose mount point is absent, empty, longer than 256 bytes, or holds a
  control character (`char::is_control`: C0, DEL and C1) is left out on its own. sysinfo turns
  `/proc/mounts` escapes such as `\011` and `\012` back into real tabs and newlines. One bad
  entry hides only that mount, never the host's other disks.
- **Disk usage.** A disk whose usage isn't reported, or isn't finite, is left out on its own.
- **Too many disks.** Of the disks that pass, the first 1024 in reported order are kept, and the
  rest are counted. sysinfo reports disks in `/proc/mounts` order, so `/` and the mounts made at
  boot come first, and mounts a local user adds later can't push them out.
- The rule maps exactly what both paths map today. RFC 0014's steal time and reading sources
  don't reach the hub's history: the push frame doesn't carry them, and the poller doesn't read
  them.

**The snapshot time** is separate: `SnapshotTime`, at most `i64::MAX` seconds (SQLite's
integer), built with `TryFrom<u64>`.
- Push stamps a snapshot with the frame's `timestamp`, the agent's clock, as today. A frame
  whose timestamp is above `i64::MAX` is refused whole (`SnapshotRefusal::TimestampOutOfRange`).
  Today every INSERT of such a frame fails.
- Poll stamps it with the hub's clock at the poll, as today.
- The refusal exists only because push snapshots carry the agent's clock. RFC 0010 stamps
  every point at hub time and stores no agent timestamp, so it drops the refusal together
  with the agent's clock. Until then, the stored time is the frame's.

**The edges.**
- The push DTO (`PushPayload`) converts in `push/ingest.rs` (§6). Every scalar is reported.
- The poll DTO (`AgentResponse`) converts in `collector/mod.rs`. Each `Option` stays `None`.
  The poller parses each system's id into `SystemId`, as its applications poll already does.
  `POST` ids are UUIDs, so only a push row stored before RFC 0005 can fail, and its `push://`
  poll fails before it gets here.
- `MetricSnapshot`, `DiskSnapshot` and `ProcessSnapshot` leave `models.rs`. Their only user
  was the poller's `store_metrics`, and the hub never read the top processes.
- **The poll body is capped at 4 MiB** (`MAX_SYSTEM_BODY`), read with the applications poll's
  `read_capped`. A larger body marks the system offline with `last_error` `body over 4 MiB`,
  as a body that isn't JSON does today.
  - 4 MiB is eight times the push message limit. A disk costs about 2.4 times more in the
    agent's JSON than in the push frame, so every snapshot a push frame can carry fits, with
    room for the answer's processes and networks.

**Logging.** A snapshot with anything left out is logged with the counts by reason and the
system id in `Debug` form, never a mount point. The level is a pure decision:

```rust
/// How one snapshot's left-out values are logged.
pub enum LeftOutLog { Nothing, Warn, Debug }

/// Decides from when this system last warned, and returns the warning time the system's
/// next live metrics keep: `previous` unchanged, or `now` after a `Warn`.
pub fn left_out_log(previous: Option<Instant>, left_out: &LeftOut, now: Instant)
    -> (LeftOutLog, Option<Instant>);
```

- Nothing left out: `Nothing`, and `previous` is kept.
- Something left out: `Warn` at most once an hour per system (`hourly_warning`), and the
  time becomes `now`. Otherwise `Debug`, and `previous` is kept.
- The time lives in the system's live metrics (§4), and every new entry takes it from this
  function, never from a default. So a host that leaves something out in every snapshot
  warns once an hour, not every 2 s.
- The hourly rule is RFC 0009's `refused_poll`, moved into a context-free pure module,
  `system-hub/src/hourly_warning.rs` (beside `token_bucket.rs`, §3), as `hourly_warning`, so
  both warnings use one function and `snapshot.rs` imports nothing from an adapter.
- A push system's live metrics end with its connection, so each push connection can warn once.

### 2. One transaction per snapshot

```rust
/// What `Database::store_snapshot` did with a snapshot: `R` is what `on_stored` returned.
pub enum SnapshotStored<R> { Stored(R), SystemGone }

impl Database {
    /// Stores a snapshot and writes the system's status in one transaction, holding the
    /// connection mutex throughout. Never calls another `Database` method (the mutex isn't
    /// reentrant).
    pub fn store_snapshot<R>(
        &self,
        system_id: &SystemId,
        snapshot: Snapshot,
        time: SnapshotTime,
        status: StatusUpdate,                   // decided by the Fleet Registry (below)
        on_stored: impl FnOnce(Snapshot) -> R,  // after the commit, before the guard drops
    ) -> Result<SnapshotStored<R>, rusqlite::Error>;
}
```

Holding the guard, it:
1. checks the system's row with `system_exists`, as `store_round` does. If the row is missing,
   it writes nothing, doesn't call `on_stored`, and returns `SystemGone`;
2. opens one transaction. For each metric point it runs three statements: the INSERT at
   `time`, the retention SELECT, and a **capped prune** of the metric's older points, past the
   retention the pure rule below gives (see "The prune is capped");
3. writes the `StatusUpdate` it was given, with `update_system_status`'s UPDATE;
4. commits, then hands the snapshot to `on_stored`, which moves it into the system's live
   metrics (§4), and returns what `on_stored` returned.

**The status is the registry's decision, not the store's.** `registry.rs` (Fleet Registry)
gains a pure value, which `store_snapshot` writes as given:

```rust
/// What a system's `last_seen` column shows. Two meanings today, until RFC 0010's contact time.
pub enum LastSeen {
    Uptime(UptimeDisplay),  // push: the frame's `uptime_display`, if valid
    PolledAt(String),       // poll: the poll's ISO time, built by the hub
    Unchanged,              // push, with an invalid display: the column keeps its value
}

/// An agent's uptime as it displays it ("3d 4h 5m"), under the display rule.
pub struct UptimeDisplay(String);        // TryFrom<String>

/// The display rule, for text an agent may send anew in every snapshot: 1..=64 bytes, no
/// control character. `UptimeDisplay` and `MemoryCapacity`'s display both apply it.
pub const MAX_DISPLAY_BYTES: usize = 64;

/// One write of a system's status: the status, when it was last seen, and its error.
pub struct StatusUpdate { status: SystemStatus, last_seen: LastSeen, error: Option<String> }

impl StatusUpdate {
    /// A stored snapshot: online, seen as given, no error.
    pub fn after_snapshot(last_seen: LastSeen) -> Self;
}
```

- The offline markings keep `update_system_status` as it is. They are out of this RFC's
  reach, and RFC 0016 reworks the push one.
- **`last_seen` keeps its two meanings,** now named: push writes the frame's
  `uptime_display`, and poll writes the poll's ISO time, as today. RFC 0010 replaces both with
  a contact time.
- **The push display is bounded.** Today `uptime_display` is bounded only by the 512 KiB
  message, is written into `systems.last_seen` on every frame, and reaches every SSE summary
  through `list_systems`. The push edge parses it into `UptimeDisplay`; one that is empty,
  longer than 64 bytes, or holds a control character becomes `LastSeen::Unchanged`, which the
  store writes as `last_seen = COALESCE(?, last_seen)` with `NULL` bound, so the column keeps
  its previous value. It is logged at `debug`: it is display text, and the agent's own
  formatter never produces one.
- **So is the memory capacity's display.** `update_registry` writes the frame's memory
  capacity whenever it differs from the stored one, so a sender alternating two displays
  rewrites it on every frame, and `total_memory_display` reaches every SSE summary through
  `list_systems`, as `last_seen` does.
  - `MemoryCapacity::reported` applies the display rule, on both paths. A display that is
    empty (as today), longer than 64 bytes, or holds a control character means no capacity
    was reported, so the stored one is kept: `memory_capacity_refresh` never clears it.
  - `MemoryCapacity::reported` also refuses bytes above `i64::MAX`, SQLite's integer, as
    `SnapshotTime` refuses a timestamp. rusqlite binds a `u64` above it as an error, so today
    such a report is never stored: every frame, and every poll, finds it different again,
    retries the write, and logs a `warn` when it fails. Now it is no report, and the stored
    capacity is kept. An agent's real totals are far below 2^63.
  - `MemoryCapacity::stored` goes through `reported`, so a capacity an older hub stored with a
    display the rule refuses counts as nothing stored, and the agent's next valid report
    replaces it.
  - The agent's `format_bytes` gives at most 10 bytes ("16384.0 PB" at `u64::MAX`), and has
    built this display since the agent's first version.
  - The system info strings (`hostname`, `os`, `kernel`, `cpu_model`, and the name the
    rename takes from the hostname) stay unbounded (Security, API4).

**Retention is a pure rule, read without failing.** Today `insert_metric` turns any error of
the retention SELECT into 24 h (`unwrap_or(86400)`), including a negative, hand-set
`retention_secs` that doesn't convert to `u64`. Inside one transaction such an error would roll
back the whole snapshot, so:
- the SELECT reads the column with `get_ref`: an INTEGER is `Some(i64)`, and no row or a value
  of any other type is `None`;
- `snapshot_retention(stored: Option<i64>) -> u64`, pure, in `snapshot.rs`: `None` or a
  negative value is 86,400 s, as today and as `app:*` pruning already treats negative rows;
  otherwise the stored value;
- so no `metric_retention` row can fail a store. A SELECT that fails any other way means the
  database failed, below.

**The prune is capped.** Today's DELETE removes every expired point of the series at once, and
how many that is depends on the gap since the series' last point, not on the snapshot. After
a 24 h gap (an agent that was off, a hub that was down, or a far-future push `timestamp`) the
first snapshot expires the series' whole window: 43,200 rows per series at a 2 s interval.
Inside one transaction that would hold the mutex for seconds per series (measured by the third
`rfc-adversary` pass: 8.5 s for 100 series), and the rollback journal would grow with the
expired history. So:
- each point's prune deletes at most `PRUNE_PER_POINT = 16` of the series' oldest expired
  rows, with the statement shape `prune_in_batches` already uses:
  `DELETE FROM metrics WHERE rowid IN (SELECT rowid FROM metrics WHERE system_id = ?1 AND
  metric = ?2 AND timestamp < ?3 ORDER BY timestamp LIMIT ?4)`, served by
  `idx_metrics_system_time`;
- in the steady state one row expires per series per snapshot, so the cap never binds;
- a backlog drains by up to 15 rows per snapshot beyond the steady state: a 24 h window
  expired at once is gone within about 1.6 h of 2 s snapshots. Meanwhile the series keeps
  points older than its retention, as a series that stops keeps them today;
- the worst snapshot prunes 1029 × 16 ≈ 16,500 rows, a bounded cost: 136–147 ms at 1024
  disks against 45–51 ms steady, as the fourth `rfc-adversary` pass measured it (Appendix).
  Only a series draining a backlog pays it. The implementation re-measures it end to end;
- a series that stops isn't pruned by this at all, as today (the existing open question).

- **Every statement goes through `prepare_cached`.** The hub then caches about eight
  statements, within rusqlite's default of 16.
- **Nothing in a snapshot, or in a retention row, can fail a statement:** §1 removed
  non-finite values and out-of-range times, and the retention read above can't fail to
  convert. So an error means the database failed: the transaction rolls back when dropped,
  `on_stored` isn't called, and the error is returned.
- `insert_metric` loses its last caller and goes. Its tests move to `store_snapshot`.
- The retention DELETE stays in SQL, inside the transaction. Only the fallback moves out,
  into `snapshot_retention`, which narrows the existing open question on retention.
- Both paths call `store_snapshot` on the blocking pool, awaited:
  - push through `push::on_blocking_pool`, as today;
  - poll through `tokio::task::spawn_blocking`, as its memory capacity refresh already does.

**The registry fill** follows in the same blocking unit, only on `Stored`:
- push: `update_registry` keeps its `get_system` read and its writes, rare for an honest agent
  (system info while the hostname or OS is missing, a changed memory capacity, the rename from
  the default name). A sender can change the capacity on every frame, at one commit each (API4).
  It
  no longer writes the status: `StatusUpdate::after_snapshot(LastSeen::Uptime(…))` went into
  the store.
- poll: `record_system_info` moves into the unit. The poll's offline marking runs on the
  blocking pool too.

**No systems cache.** `systems_cache`, `refresh_cache` and `get_enabled_systems` leave
`AppState`.
- The poller reads the registry with `list_systems` on the blocking pool at each 30 s tick,
  and polls the enabled systems (a pure filter, `enabled_systems`).
  - A tick whose read fails logs it at `warn` and polls the systems of the last read that
    succeeded, kept in the poller's own loop. Today's cache keeps its previous list the same
    way when a refresh fails (`AppState::refresh_cache`).
  - One row that doesn't map fails `list_systems` for every row. §4 names how such a row
    gets stored.
- No push frame, disconnect or REST handler refreshes anything. The status changes with every
  frame, so a cache refreshed on registry writes would be refreshed on every frame again, for
  one reader that reads the registry every 30 s anyway.
- RFC 0010 carries this rule forward.

### 3. The decode budget (push)

Every binary message a push connection sends spends one token of the connection's **decode
budget** before it is decoded, whatever its kind: 3 tokens, then one more per
`DECODE_REFILL` = 1 s.

- The connection reads the time the message was read (monotonic clock), and takes a token:
  - none left: the message is dropped without being decoded;
  - otherwise: the message is decoded, as today: as a snapshot frame, else as an application
    frame (RFC 0009).
- So a message that fails to decode, a frame refused by `TryFrom` or by §1, and an
  application frame RFC 0009's source pace then refuses all spend a token. A client sending
  near-valid messages of either kind pays for at most three full decodes, then one a second.
- **The decode order stays today's.** The budget is spent before either decode, so the order
  no longer matters to the bound. The previous revision decoded application frames first and
  spaced only snapshot frames, which left an application-shaped message that fails late
  decoded in full, unpaced.
- **A round's admission doesn't change.** RFC 0009's source pace still alone decides whether a
  decoded round is stored. The budget only decides whether a message is decoded at all.
- **The rule is pure.** `DecodeBudget` is a token bucket, like RFC 0009's `SourcePace`. The
  implementation moves `SourcePace`'s bucket arithmetic into one pure `TokenBucket`
  (`token_bucket.rs`, with the capacity and the period as parameters) instead of copying it,
  as a refactor under `SourcePace`'s existing tests. A zero period refills the bucket at every
  take, so it never refuses. The budget lives in the receive loop's state, beside
  `ConnectionRounds`.
- **Honest agents fit it.**
  - An agent sends a snapshot at most once per 2 s tick, each published snapshot once, by
    snapshot seq. It sends a round at most once per scrape interval, at least 10 s. That's at
    most 0.6 messages a second, under the refill of one.
  - Its densest burst is right after the handshake: the current round, the first snapshot
    (the first tick fires at once), and a round published just after. Three messages, the
    budget's capacity. Otherwise a round lands at most next to one snapshot.
  - That holds for every shipped agent: the 2 s floor on the push tick has been there since
    the agent's first version, and agents older than RFC 0009 send no rounds.
- Messages dropped by the budget are counted, and the count is logged at `info` when the
  connection ends, beside its refused application frames. A refused snapshot frame is logged
  as a refused application frame is: the first per connection at `warn`, the rest at `debug`.
- `PushConfig` gains `decode_refill`. Production uses 1 s, and the existing tests that send
  several frames use 0. The capacity, `DECODE_BURST` = 3, is a constant.
- **A hub-side stall.** The bucket refills while the connection waits, but holds at most 3
  tokens. So once a stall has buffered more than three messages (about 5 s of an agent's
  traffic), those past the third are read back to back and dropped. Those samples are
  already late. A dropped round is replaced by the agent's next one; until then the
  dashboard shows the previous round.
- A dropped message has still been read off the socket, up to RFC 0006's 512 KiB. Reading
  it can't be avoided without closing the connection, which would drop an honest agent's
  buffered frames too (Security, API4).

### 4. Live metrics that follow the registry

`live_metrics` keeps its name and its key, and its value becomes the domain value, shared:
`RwLock<HashMap<String, Arc<LiveMetrics>>>`.

```rust
/// A system's latest snapshot, as the snapshot rule kept it.
pub struct LiveMetrics {
    pub snapshot: Snapshot,
    pub time: SnapshotTime,
    /// When this system's left-out values were last logged at `warn` (§1).
    pub left_out_warned_at: Option<Instant>,
}
```

- **The kept values, on both paths.** The dashboard's live values and disks are the
  snapshot's kept values. A value left out is `null` on the wire, as a NaN already serialises
  today (`serde_json` writes non-finite floats as `null`).
- **Bounded.** At most 1024 disks, each a mount point of at most 256 bytes and an `f32`: about
  300 KB per system at most. The warning time lives in the entry, so it is bounded and evicted
  with it.
- **Written inside the store.** `on_stored` moves the snapshot into the system's entry after
  the commit, while the database guard is still held. Under the live write lock it reads the
  previous entry's warning time, decides with `left_out_log` (§1), and inserts
  `Arc::new(LiveMetrics { … })` with the time that function returned. It returns the
  `LeftOutLog` for the adapter to log, and the replaced entry.
  - Lock order is always database, then live state, as in `store_round`.
  - The live lock is taken with `unwrap_or_else(PoisonError::into_inner)`, as in
    `store_round`. So a panic elsewhere under the live lock can't become a panic under the
    database guard, which would poison the database mutex.
  - **Nothing under the live lock grows with a snapshot's disks.** A writer does one map
    insert, moving the snapshot and allocating one `Arc`. The replaced or removed `Arc` leaves
    the lock as a return value and is dropped after both locks are released, by the adapter.
    A reader (§5) copies each key and each `Arc`: one small allocation per system, never a
    disk. So the publisher's read can't stretch a store's hold of the database mutex by more
    than a copy of the map's pointers.
  - No code takes the database mutex while it holds the live lock. The SSE publisher (§5), the
    delete handler and the offline marking take them one after the other.
- **A delete can't leave an entry behind.** `DELETE /api/systems/:id` removes the row, then the
  entry, beside `live_applications`. A store that takes the guard after the row is gone gets
  `SystemGone` and writes no entry. A store that took it before wrote its entry before the
  delete's eviction.
- **`SystemGone` ends the push connection**, logged at `info` with the id in `Debug` form, as a
  round's `SystemGone` already does. On poll it ends that poll, at `debug`.
  - **A connected push agent then registers the system again, at once.** Its push loop sees
    the hang-up, returns, and reconnects with no delay. The handshake's `register_if_new`
    inserts the row again under the default name, and the next frame renames it. The system
    reappears within about 2 s, with none of its history. To remove a pushing system, stop
    its agent first, then delete it.
  - The hub drops the socket only after the old connection's `end_connection` has run
    (`handle_push` owns the socket until then), so the reconnect can't race that offline
    marking and eviction.
  - Today only an agent sending application frames is disconnected this way (RFC 0009's round
    `SystemGone`). A snapshot-only agent keeps its connection, so a deleted system comes back
    whenever that agent next reconnects, and until then its frames recreate its live metrics.
    This RFC makes both kinds of agent behave alike, at once. Refusing re-registration of a
    deleted id would need an identity the push id can't give (the standing API1 risk), so it
    is left to an RFC on per-system push credentials.
- **Registration can't replace a row.** Today `register_if_new` reads the row with
  `get_system(id).ok().flatten()`, so a lookup that fails counts as "not registered", and
  `insert_system`'s `INSERT OR REPLACE` then replaces the known row. The bundled SQLite
  enforces foreign keys, so the replace cascade-deletes the system's metrics, alerts and
  retention rows and resets its name, and the agent gets `auth_ok` with nothing logged.
  - The fourth `rfc-adversary` pass measured it: 104,000 metric rows to 0.
  - A lookup fails for good when a column doesn't map. The unauthenticated
    `PUT /api/systems/:id` stores a `poll_interval_secs` above `i64::MAX` as a negative
    number (`v as i64`), and no read maps that back to `u64`. From then on the `PUT` itself
    answers 500 for that row, since it reads the row first, so only `DELETE` removes it.
  - Refusing that value at the `PUT` edge is a bug fix of its own. It lands in this RFC's pull
    request as its own first commit, with its own red test (decided in the Review). The
    body's `poll_interval_secs` parses into a `PollInterval` newtype (`TryFrom<u64>`, at most
    `i64::MAX`, the column's range), refused with 422, and `update_system_config` binds its
    value, so the `v as i64` cast goes. After
    this RFC the poller reads `list_systems` at every tick, and one such row fails it for
    every system. It wouldn't repair rows already stored, so registration mustn't depend on a
    row mapping either way.
  - So `register_if_new` registers through one `Database` method in `db/mod.rs`,
    `insert_system_if_absent`, under one guard, taken as every `Database` method takes it
    (`self.conn.lock().unwrap()`), so a poisoned mutex panics (see the `JoinError` below):
    - it checks the row with `system_exists`, which reads no column. A known id is `Ok`,
      even when `get_system` can't map its row;
    - only an absent id is inserted, with `INSERT … ON CONFLICT(id) DO NOTHING`, never a
      REPLACE. So whatever a check returns, no registration can replace a row;
    - a failed check (the database can't be read) or a failed insert (it can't be written)
      returns the `rusqlite::Error`.
  - **For a known id, nothing but the check runs:** no INSERT, not even one that would insert
    nothing, and no write transaction. An INSERT on a read-only database file fails even when
    it would insert nothing (measured on SQLite 3.45.1). So the upsert alone, or one
    `INSERT … SELECT … WHERE NOT EXISTS`, would refuse every known agent. With the check
    first, a known agent on a hub whose writes fail stays connected, and its store errors are
    counted (below).
  - A row `get_system` can't map is still stored to, since `store_snapshot` checks only that
    the row exists. `update_registry`'s read of that row fails, so its registry fill is
    skipped, as today.
  - `insert_system` keeps its `INSERT OR REPLACE` for `POST`, which inserts a new UUID.
- **A failed registration answers `registry unavailable`.** Today `register_if_new` discards
  its insert's error (`let _ = app.db.insert_system(…)`) and the handshake answers `auth_ok`.
  With `SystemGone` now ending snapshot connections, a database that can't insert (full disk,
  read-only file) would put every new or deleted push agent into a reconnect loop with no
  delay, one cycle per snapshot. So:
  - `register_if_new` returns `insert_system_if_absent`'s result: `Ok` when the row exists or
    was inserted, the `rusqlite::Error` otherwise;
  - the handshake answers an error with `auth_error` / `registry unavailable`
    (`Refusal::RegistryUnavailable`), logged at `warn`, with the id in `Debug` form and the
    error kind, never the token;
  - **it answers a registration unit that panicked the same way.** Today a `JoinError` from
    `on_blocking_pool` closes the handshake with no answer (`Handshake::Closed`). Every
    shipped agent treats a handshake that ends without an answer as accepted (only a
    received `auth_error` is an error), finds the socket closed, and reconnects with no
    delay. The likeliest panic is a poisoned database mutex (`self.conn.lock().unwrap()`),
    which fails every registration after it. `on_blocking_pool` has already logged the
    `JoinError` at `error`. `Handshake::Closed` keeps only the socket's own endings, and its
    doc comment stops naming RFC 0008;
  - **registration panics on a poisoned mutex, as every store does.** Recovering the guard
    (`PoisonError::into_inner`) would answer `auth_ok` to an agent whose every frame then
    panics its store, and each such frame ends the connection with no delay before the
    reconnect. Mapping the poison to an error would be paced too, but only this one method
    would do it, and the `JoinError` answer, which covers any other panic in the unit, would
    have no test that reaches it. So `Database` keeps one lock convention, and a test pins it
    for `insert_system_if_absent` (Testing plan);
  - the agent treats every `auth_error` alike and retries after 5 s (`src/main.rs`,
    `PushError::Connection`). Every shipped agent does that. So **every registration failure
    is paced by the agent's 5 s retry**, whether the database returned an error or the unit
    panicked;
  - a frame whose unit panics still ends its connection (`IngestStop::WorkFailed`), and the
    agent reconnects at once. On a poisoned mutex, the reconnect's registration then panics
    too and gets `registry unavailable`, so that loop is paced as well;
  - RFC 0008 reuses this answer and variant for its store errors and its `JoinError`, and
    adds only `RegistryFull`, so the message and variant are defined here once. RFCs 0006 and
    0008 say so.
- **Store errors are rate-limited.** A failing database (a full disk, a read-only file) fails
  every snapshot.
  - Push: the connection stays open. The first store error per connection is logged at `warn`,
    the rest at `debug`, and the count at `info` when the connection ends, as refused
    application frames already are.
  - Poll: one `warn` per system per poll, as today's "couldn't mark online" warning, which
    the store's status write replaces. So a failing database logs no more than it does today.
    A fleet-wide bound belongs with rate limiting.
- **A push connection's end evicts.** The offline marking becomes one blocking unit,
  `end_connection` in `push/ingest.rs`: it marks the system offline, then removes its entry.
- **A polled system keeps its entry until it is deleted.** A failed poll marks it offline and
  leaves its last live values beside the offline status, as today.
- **Which connection evicts.** The push system id is self-asserted, so any connection
  presenting an id evicts that id's entry when it ends, even one another connection wrote.
  - The same end already marks the system offline today (the standing API1 risk), and the
    honest connection's next frame rewrites the entry within 2 s.
  - RFC 0016 restricts both the offline marking and this eviction to the system's current
    connection. This RFC adds nothing for it to undo.

### 5. One serialisation per tick

- **One publisher.** `routes/sse.rs::start_publisher`, started in `main` beside the collectors,
  builds the summary every 5 s on the blocking pool:
  - `list_systems` and `count_active_alerts`;
  - a copy of the live metrics' keys and `Arc`s, taken under the read lock (§4), and
    converted to the wire shape after the lock is released;
  - one `serde_json::to_string`, into an `Arc<str>`.
- It publishes that on a `tokio::sync::watch` channel whose sender `AppState` holds, with
  `Sender::send_replace`, which stores the summary whether or not anyone subscribes.
  - `Sender::send` would not do: it stores nothing while no receiver exists (tokio 1.52), and
    no receiver exists while no dashboard is open. A dashboard opened after an idle spell would
    then get the startup summary, or the last one sent while someone watched.
  - The publisher builds every 5 s whether or not anyone subscribes, so a new subscriber's
    first summary is at most one tick old.
- `AppState::new` builds the first summary, and `main` runs it on the blocking pool before the
  hub serves, so the channel never holds an empty value.
- **Each subscriber** streams `WatchStream::new(receiver)`. It gets the current summary at once,
  as today's first interval tick gives it, then each new one. A slow subscriber skips to the
  latest summary instead of queueing.
- **Each event** is `Event::default().data(&*summary).event("summary")`, so its bytes are
  today's: `data: <json>\nevent: summary\n\n`.
- **The cost per tick** is two database reads, one copy of the live metrics' pointers and one
  serialisation, whatever the number of subscribers. Each subscriber adds one copy of the
  serialised summary: axum's `Event::data` copies it into the event's own buffer.
  - Sharing one refcounted `Bytes` would save that copy. But it means writing SSE framing and
    keep-alive by hand instead of using axum's `Sse`, and `CLAUDE.md` asks us not to hand-roll
    what a dependency already does correctly. So the copy stays.
- **The wire format doesn't change:** the same `summary` event and JSON document. The live
  metrics' wire shape (`cpu_percent`, `memory_percent`, `load_one`, `disks`, `updated_at`)
  becomes a DTO in `routes/sse.rs`, built from `LiveMetrics`.
- If a tick's blocking task fails, the publisher logs it at `error` and keeps the previous
  summary.

### 6. The split of `push/mod.rs`, and of `db.rs`

Both files are past the Rosette's 500 lines before this RFC grows them. Each is split first, as
a pure refactor, with the existing tests green before and after.

- **`push/mod.rs`** has 616 lines before its tests, 506 of them code (not blank or comment), and
  2,439 lines in all. The split is at `on_blocking_pool`, which is already the async/sync seam.
  - `push/mod.rs` keeps the connection protocol, about 340 lines of code: the router, the
    handshake (`authenticate`, `HandshakeRejection`, `Refusal`, `Handshake`, `Answer`), the
    deadlines, the receive loop, `ConnectionRounds`, `IngestStop`, `on_blocking_pool`, and then
    §3's decode budget and the connection's counts (too-soon messages, store errors).
  - `push/ingest.rs` takes everything that runs as `FnOnce(&AppState, &SystemId)`, about 165
    lines of code. That's the anti-corruption layer: the snapshot DTOs (`PushPayload`,
    `DiskItem`, `ProcessItem`), `register_if_new`, the frame ingestion (`ingest_frame`,
    `update_registry`) and the offline marking. This RFC then adds the conversion into
    `ReportedSnapshot`, and `end_connection`, there.
  - The DTO tests (the golden snapshot frame, the two-kinds test, the MessagePack decode) move
    with the DTOs. The real-server tests stay in `mod.rs`.
- **`db.rs`** has 524 lines of code before its tests. Its Metrics section, about 170 lines of
  code, moves to `db/history.rs`: `insert_metric`, `store_round`, the prune statements and
  methods, `get_metrics` and `RoundStored`.
  - `db/history.rs` is a child module, so it keeps the one `conn` mutex.
  - `db/mod.rs` keeps the connection, the migration, and the `systems` and `alerts` tables.
    `system_exists` reads `systems`, so it stays there too. `store_round` and
    `store_snapshot` call it through `super`, and so does §4's `insert_system_if_absent`,
    which lands in `db/mod.rs`.
  - That's the Fleet History / Fleet Registry boundary. `store_snapshot` lands in
    `db/history.rs`, and writes the `StatusUpdate` the registry decided there, inside its
    transaction. The statement is the registry's table's, the decision `registry.rs`'s.

## Domain impact

- **Fleet History:**
  - `snapshot.rs`: `ReportedSnapshot`, `ReportedDisk`, `Snapshot`, `Scalar`, `MountPoint`,
    `SnapshotTime`, `LeftOut`, `snapshot_rule`, `MAX_DISKS`, `MAX_MOUNT_POINT_BYTES`,
    `LeftOutLog`, `left_out_log`, `snapshot_retention`;
  - `db/history.rs`, with `store_snapshot` and `SnapshotStored`;
  - `LiveMetrics` holding a `Snapshot`, shared as `Arc<LiveMetrics>`, and the SSE publisher
    with its wire DTO;
  - `SourcePace` keeps its behaviour, over the shared `TokenBucket` (§3);
  - `MetricSnapshot`, `DiskSnapshot`, `ProcessSnapshot` and `insert_metric` go.

  This closes the open question about the snapshot → metric mapping being written twice, and
  moves the retention fallback out of the SQL adapter.
- **Fleet Registry:**
  - `registry.rs` gains `LastSeen`, `UptimeDisplay`, `MAX_DISPLAY_BYTES` and `StatusUpdate`
    (with `after_snapshot`): marking a system online after a stored snapshot becomes a pure
    registry decision, which `store_snapshot` writes in the snapshot's transaction.
    `update_system_status` keeps only the offline markings;
  - `MemoryCapacity::reported` applies the display rule, as `UptimeDisplay` does, and refuses
    bytes above `i64::MAX`, as `SnapshotTime` refuses a timestamp;
  - `db/mod.rs` gains `insert_system_if_absent`: a check that reads no column, then an insert
    that can't replace a row. `insert_system` keeps its REPLACE for `POST` alone;
  - the systems cache leaves `AppState`, and the poller reads the registry each tick,
    keeping its last good read when one fails.
- **Ingestion:**
  - `push/ingest.rs` (§6);
  - both DTOs convert into `ReportedSnapshot`, and store through `store_snapshot` on the
    blocking pool;
  - the decode budget (`DecodeBudget`, `DECODE_BURST`, `DECODE_REFILL`,
    `PushConfig::decode_refill`); the decode order is unchanged;
  - `SnapshotRefusal` and `end_connection`;
  - the poll's `MAX_SYSTEM_BODY`;
  - `register_if_new` registering through `insert_system_if_absent` and returning its result,
    and `Refusal::RegistryUnavailable` (`auth_error` / `registry unavailable`), answered for
    a returned error and for a registration unit that panicked alike.
- **No context:** two pure modules with no domain term of their own:
  - `token_bucket.rs` (`TokenBucket`, `Refill`), a value both `SourcePace` and `DecodeBudget`
    wrap;
  - `hourly_warning.rs` (`hourly_warning`, RFC 0009's `refused_poll` moved), used by
    `left_out_log` (Fleet History) and the applications poll (Ingestion).
- **Glossary:**
  - adds **snapshot rule**: how the hub turns a snapshot into what it keeps, the same on both
    paths: `cpu`, `memory`, `swap`, `load1` and `load5` (`Scalar`), and one point per disk. A
    value that isn't reported or isn't finite, a disk with an invalid mount point, and disks
    past the first 1024 valid ones are each left out on their own, and counted
    (`snapshot_rule`, `Snapshot`, `LeftOut`, `left_out_log`);
  - adds **mount point**: where a disk is mounted, as the hub keeps it: 1 to 256 bytes, with no
    control character (`MountPoint`);
  - adds **decode budget**: a token bucket per push connection that every binary message
    spends before it is decoded, whatever its kind: 3 messages, then one per second, on the
    monotonic clock. A round's admission is still its source pace (`DecodeBudget`,
    `DECODE_BURST`, `DECODE_REFILL`);
  - adds **live metrics**: a system's latest snapshot as the snapshot rule kept it, held in
    memory for the dashboard. Written with the snapshot's store, and removed when the system
    is deleted or its push connection ends (`LiveMetrics`, `AppState::live_metrics`);
  - adds **last seen**: what the hub shows as when a system was last seen: the uptime its
    last push frame reported (at most 64 bytes, no control character, else the previous
    value is kept), or the time of its last successful poll. Two meanings, until RFC 0010's
    contact time (`LastSeen`, `UptimeDisplay`);
  - changes **push handshake**: a *refusal* also covers `registry unavailable`, answered when
    registering a new push id fails or panics (`Refusal::RegistryUnavailable`);
  - changes **system status**'s "In code": `StatusUpdate`, one write of a system's status,
    its last seen and its error;
  - changes **memory capacity**: a display over 64 bytes, or holding a control character, is
    no report, as an empty one already isn't, and neither are bytes above 2^63 − 1, which
    SQLite can't hold. The stored capacity is then kept (`MemoryCapacity`,
    `MAX_DISPLAY_BYTES`);
  - changes **retention**: a negative (hand-set) row counts as none for snapshot metrics
    too, as it already does for `app:*` metrics, and so does a snapshot metric's row that
    isn't an integer (`snapshot_retention`);
  - changes **snapshot**'s "In code": `ReportedSnapshot` and `Snapshot` (hub, before and after
    the snapshot rule) replace `MetricSnapshot`, and `SnapshotTime` is a snapshot's time;
  - **metric point** is unchanged. A snapshot's metric points are `Snapshot::metric_points`,
    and no new term names them.
- **Published contracts:**
  - the push frame is unchanged: the hub decodes the same 21 elements, in the same order of
    kinds, and the decode budget drops messages only past what an honest agent sends (§3);
  - the push handshake gains one `auth_error` message, `registry unavailable`, answered when
    the hub can't check or register a push id: a database error, or a registration that
    panicked. Every shipped agent retries any `auth_error` after 5 s, where today it takes a
    handshake closed without an answer as accepted and reconnects at once;
  - the poll response is unchanged, and a body over 4 MiB is refused;
  - the SSE event is unchanged.

  What the hub stores narrows only for snapshots the rule leaves something out of.
  Mixed-version fleets are unaffected.

## Alternatives considered

- **Keep two mappings and add the rule to push only.** The same host would show different
  disks when it pushes and when it's polled, and the duplication would grow.
- **Refuse a whole snapshot with too many disks.** The host would go dark.
- **Skip every disk past 1024** (the first revision). A local user who can create 1025 mounts
  would hide `/`, which is the attack the per-entry mount point rule exists to stop. Keeping the
  first 1024 in reported order keeps the boot-time mounts.
- **Drop all disks when one mount point is invalid.** A local FUSE mount could hide every disk.
- **Store `0.0` for a value that isn't reported** (the poller today). It's a sentinel: a chart
  can't tell it from an idle host.
- **Batch several frames per transaction.** It would delay live metrics. RFC 0010's group
  commit does this, with a store built for it.
- **WAL mode, or `synchronous=NORMAL`.** Either would cut the commit cost that dominates the
  measurement. But they change durability and the files on disk, for a store RFC 0010 replaces.
- **Pace after decoding** (the first revision). A near-valid frame that fails on its last field
  would cost a full decode, and never count as accepted.
- **Decode application frames first, and space only snapshot frames** (the second revision).
  An application-shaped message that fails on its last element, or is refused by `TryFrom`,
  still cost a full decode at wire rate, since RFC 0009's pace gates only the store.
- **One spacing for every binary message,** with no burst. A round sent next to a snapshot,
  as an agent does after every handshake and at a random phase afterwards, would lose one of
  the two.
- **Classify frames by their MessagePack array header** before decoding, and pace each kind.
  It needs `rmp` as a direct dependency and a header read by hand, and `rmp_serde` also
  accepts a struct encoded as a map, which a header check would silently stop accepting. One
  budget over every message bounds decoding with neither.
- **Close the connection when the budget runs out.** It would stop reading the dropped
  messages, but an honest agent's frames buffered by a hub-side stall would end its
  connection too.
- **Keep a push connection open on `SystemGone`,** as today for snapshot frames. The deleted
  system would come back whenever the agent next reconnected, days later or never, and until
  then its frames would store nothing. Ending the connection makes the outcome immediate and
  the same for both kinds of frame (§4).
- **Keep a `Receiver` in `AppState`** so `Sender::send` always stores. `send_replace` says the
  same without a receiver nobody reads.
- **Keep `LiveMetrics` by value,** and clone it for the publisher. The clone is deep, up to
  1,025 allocations per system, and a store waits for it with the database mutex held.
- **Share one `Bytes` SSE frame.** See §5.
- **Cap SSE subscribers** instead of sharing one serialisation. It's complementary, and it
  belongs with rate limiting.
- **Keep the systems cache, and refresh it on registry writes.** The status is a registry write
  on every frame (§2).
- **Keep `store_snapshot` deciding the status** from each adapter's `last_seen` string (the
  previous revision). The registry's rule would sit in the SQL adapter, untestable apart from
  SQLite.
- **Evict only the entry this connection wrote,** with a connection number. That's RFC 0016's
  mechanism, and it has to decide offline marking too. Half of it here would give the two rules
  different ideas of which connection is current.
- **Keep registration's `get_system` lookup, and return its error.** A known row that doesn't
  map would then be refused `registry unavailable` every 5 s, for good.
- **Register with the upsert alone,** and no check. It can't replace a row either, but an INSERT
  on a read-only database file fails even when it would insert nothing, so every known agent
  would be refused too, instead of staying connected while its store errors are counted.
- **Leave a registration `JoinError` unanswered until RFC 0008.** Until then, every push agent
  would reconnect with no delay while registration panics.
- **Recover a poisoned guard in registration** (`PoisonError::into_inner`). The agent would be
  accepted, then every frame's store would panic and end its connection, with no delay before
  the reconnect (§4).
- **Leave a memory capacity's bytes above `i64::MAX` to a bug fix of its own.** They predate
  this RFC (RFC 0014 step 7), but §2 makes `MemoryCapacity::reported` the one check of a
  reported capacity, and one condition there keeps a report SQLite can't store from being
  retried, and logged, on every frame.
- **Bound only `last_seen`, and list the memory capacity's display as unbounded.** A sender can
  make both change on every frame, so both cost the same.
- **Bound the system info strings too.** They're display text as well, but no formatter of
  the agent's bounds them. It sends the CPU's brand string, the OS's pretty name from
  `/etc/os-release` and the kernel release as it reads them, so a 64-byte rule could refuse
  an honest value. Unlike the two displays, push writes them once per registration, though
  poll writes them on every poll. They're listed as unbounded (API4).

## Security implications

OWASP Top 10 (2021):
- **A01 Broken Access Control:** unchanged. Push ids stay self-asserted. Any connection
  presenting an id evicts that id's live metrics when it ends, as its end already marks the
  system offline (§4). RFC 0016 narrows both. No route is added. Deleting a system can't keep
  it out while its agent runs: the agent re-registers at once (§4), as anyone holding the
  push token already could at any time.
- **A02 Cryptographic Failures:** not touched. The push token comparison
  (`PushToken::accepts`) is unchanged.
- **A03 Injection:** `store_snapshot` runs fixed statements with bound parameters. A metric
  name (`disk:<mount point>`) is a bound value, never SQL text. The dashboard is unchanged:
  mount points still render as text, and are now at most 256 bytes with no control character.
  The implementation runs `xss.mjs` anyway, since live values can now be `null` in more cases.
- **A04 Insecure Design:** every rule is a bound: disks, mount points, the two display strings
  a frame can change each time, the memory capacity's bytes, the decode budget, the poll body.
  What a rule leaves out is counted and logged, never lost silently. One value, or one
  hand-set retention row, can no longer break a whole transaction.
- **A05 Security Misconfiguration:** no new setting: the bounds are constants. CORS isn't
  widened.
- **A06 Vulnerable Components:** no dependency change. `WatchStream` comes with
  `tokio-stream`'s `sync` feature, which is already enabled. `cargo audit` runs if the lockfile
  changes.
- **A07 Identification and Authentication Failures:** unchanged. The handshake runs before any
  of this.
- **A08 Software and Data Integrity Failures:** a snapshot is stored whole, or not at all. A
  frame whose time can't be stored is refused whole. The push frame's contract is unchanged.
  Registration can no longer replace a known system's row. Today an unauthenticated `PUT` can
  leave a row `get_system` can't map, and the agent's next handshake then wipes the system's
  history (§4).
- **A09 Logging and Monitoring Failures:**
  - left-out values: at `warn` at most once an hour per system, with the counts by reason;
  - refused snapshot frames: the first per connection at `warn`;
  - messages dropped by the decode budget: counted, and logged when the connection ends;
  - `SystemGone` at `info`;
  - store errors: on push, the first per connection at `warn` and a count when it ends; on
    poll, one `warn` per system per poll, today's rate. A failing database doesn't flood the
    log it may share a disk with;
  - a failed push registration, whether the database returned an error or the unit panicked:
    `warn`, one per handshake, plus `on_blocking_pool`'s `error` for a panic. Every one is
    paced by the agent's 5 s retry after `registry unavailable`. Today an error isn't logged,
    and a panic gets no answer, so the agent reconnects with no delay and the hub logs an
    `error` each time;
  - ids in `Debug` form, and mount points never logged.

  A client that reconnects often gets one left-out `warn` per connection, as with RFC 0009's
  refused rounds.
- **A10 SSRF:** the poll body cap limits what a registered URL can make the hub buffer. The
  SSRF surface itself, any URL registered, is unchanged.

OWASP API Security Top 10 (2023):
- **API1 Broken Object Level Authorization:** as A01.
- **API2 Broken Authentication:** unchanged.
- **API3 Broken Object Property Level Authorization:** no new body is accepted, and the SSE
  event exposes the same properties. The `PUT /api/systems/{id}` body's `poll_interval_secs`
  is now parsed into `PollInterval` (at most `i64::MAX`), so a value no read can map back is
  refused at the edge instead of stored.
- **API4 Unrestricted Resource Consumption:** the point of this RFC.
  - Per snapshot: at most 1029 metric points, in one transaction, 41 ms at 1024 disks on a full
    table in the steady state (Appendix). The prune inside it is capped at 16 rows per point
    (§2), so a snapshot after a long gap, or with a far-future push `timestamp`, can't expire a
    whole window under the mutex: at most about 16,500 rows, about 145 ms at 1024 disks, three
    times the steady state (Appendix). Only a series draining a backlog pays that, and at
    15 rows per snapshot beyond the steady state it always drains.
  - Per push system: of the text a frame can change every time, `last_seen` and the memory
    capacity's display each hold at most 64 bytes (§2), instead of up to 512 KiB copied into
    every SSE summary.
  - Per push connection: at most three decodes at once, then one per second, of any binary
    message, spent before the decode. So at most one snapshot store per decode: about 150 ms
    of mutex time in a burst, then about 50 ms per second. That's the store (41 ms at 1024
    disks), plus the registry fill's read and at most two commits (a changed memory
    capacity, and the rename while the name is the default one). While a 1024-disk host
    drains a backlog, it's about three times that.
  - Per poll: a body of at most 4 MiB.
  - Live metrics: about 300 KB per system at most, evicted with the push connection or the
    system.
  - SSE: the per-tick work doesn't grow with subscribers, and each subscriber costs one copy of
    the summary per tick.

  Still unbounded:
  - the number of push connections, and so their total rate (rate limiting). About 20 to 25
    connections sending 1024-disk frames every second saturate the mutex, or about 7 while
    each drains a backlog;
  - registration (RFC 0008), and so the number of live metrics entries;
  - the system info strings: `hostname`, `os`, `kernel`, `cpu_model`, and the name the rename
    takes from the hostname. They're bounded only by the 512 KiB frame on push, or by the
    4 MiB body on poll, and they reach every SSE summary. Push writes them once per
    registration, and poll writes them on every poll (§2, Alternatives);
  - SSE subscribers;
  - the bytes a connection sends: a message the budget drops has still been read off the
    socket, up to 512 KiB, at wire rate (§3);
  - a fleet's aggregate load. At 2 s, about 350 agents with 5 disks saturate the mutex on the
    measured disk, against about 20 today. The commit dominates, so faster storage raises both,
    and RFC 0010 replaces the store.
- **API5 Broken Function Level Authorization:** not touched: no route or role changes.
- **API6 Unrestricted Access to Sensitive Business Flows:** not touched.
- **API7 SSRF:** as A10.
- **API8 Security Misconfiguration:** as A05.
- **API9 Improper Inventory Management:** no endpoint is added or removed. The README gains the
  `PUT` row's refusal of a `poll_interval_secs` above `i64::MAX` (422), the snapshot rule, the decode budget, the poll body cap, what `DELETE` does to a connected push
  agent and to live metrics, what the SSE stream does, and a corrected project tree (see
  Impact on `README.md`).
- **API10 Unsafe Consumption of APIs:** push frames and poll answers go through one rule, at the
  edge. Non-finite values, invalid mount points and out-of-range times can't reach SQL. The
  uptime and memory capacity displays are bounded at the same edge, and a memory capacity's
  bytes above `i64::MAX` are no report, so no report fails its write on every frame. The poll
  body is capped,
  as the applications body already is. A `metric_retention` row is read as a value that can't
  fail to convert (§2). Registration's check reads no column of a stored row, so a row that
  doesn't map can't make it replace the row (§4).

## Testing plan

TDD, per `CLAUDE.md`, with `red-test-adversary` and `rosette-auditor`.

- **The splits** (§6) are pure refactors: the existing tests pass before and after, changed
  only in their paths.
- **Characterisation first**, before any behaviour changes: today's SSE event bytes for a fixed
  state (`data: …\nevent: summary\n\n`), and its JSON document.
- **`snapshot_rule`**, table-driven (`cases = [(name, input, expected)]`). Each row checks the
  kept values, their order, and the `LeftOut` counts:
  - 0, 1, 1024 and 1025 valid disks. At 1025, the first 1024 are kept and one is counted over
    the limit;
  - 1025 reported disks of which 2 are invalid: all 1023 valid ones kept, none over the limit;
  - mount points of 256 bytes, and of 257 bytes where a 2-byte character crosses the bound;
    empty and absent ones; ones holding `\t`, `\n`, DEL, and NEL (a C1 control);
  - NaN, +∞ and −∞ for each scalar and for a disk's usage; a load of `1e300` (finite as `f64`,
    infinite as `f32`); each scalar not reported. Each is left out alone, and the rest kept;
  - the metric names and their order: `cpu`, `memory`, `swap`, `load1`, `load5`, then
    `disk:<mount point>`.
- **Pure rules**, each table-driven:
  - `SnapshotTime::try_from`: 0, `i64::MAX`, `i64::MAX + 1`, `u64::MAX`;
  - `DecodeBudget` (over `TokenBucket`): a new budget takes 3 and then refuses; after it
    empties, 0.999 s refuses and exactly 1 s takes one; 2.5 s gives two and keeps the half;
    a long quiet spell banks no more than 3; a zero refill never refuses. `SourcePace`'s
    existing table stays green, unchanged;
  - `left_out_log`: nothing left out, with and without a previous time (`Nothing`, the time
    kept); something left out with no previous time (`Warn`, `now`), 59 min 59 s after one
    (`Debug`, the time kept), and exactly 1 h after (`Warn`, `now`);
  - `snapshot_retention`: `None`, 0, 3,600, `i64::MAX`, −1 and `i64::MIN`. `None` and the
    negatives give 86,400;
  - `StatusUpdate::after_snapshot`: online, the `LastSeen` given (each variant), no error;
  - `UptimeDisplay::try_from`: `3d 4h 5m`, 64 and 65 bytes (a 2-byte character across the
    bound), empty, and holding `\n` or NEL. Only the first two are accepted;
  - `MemoryCapacity::reported`, its existing table plus the same display rows: 64 bytes is a
    report, while 65 bytes (a 2-byte character across the bound), `\n` and NEL are none. Bytes
    of `i64::MAX` are a report, and `i64::MAX + 1` and `u64::MAX` are none. And
    `memory_capacity_refresh` over a stored 65-byte display and a valid report writes the
    report, while over a stored valid capacity and bytes of `i64::MAX + 1` it writes nothing;
  - the connection's store-error count: `First`, then `Repeat`, as `note_refusal`;
  - `hourly_warning`: moved with its existing table;
  - `enabled_systems`: enabled and disabled systems.
- **`store_snapshot`** (a temporary SQLite file):
  - it writes a row per point at `time`, and the `StatusUpdate` it was given: online, its
    `last_seen` verbatim, and a previous `last_error` cleared;
  - retention, one row each: a `metric_retention` row of 3,600 deletes a point 3,601 s older
    and keeps one 3,599 s older; no row, a row of −1 and a row holding the text `abc` each
    fall back to 24 h. None of them fails the store;
  - **the warning time carries over**, through `store_snapshot` with the push path's own
    `on_stored`: two snapshots with left-out values, 1 s apart, give `Warn` and then `Debug`,
    and the second entry keeps the first's warning time. A third with nothing left out gives
    `Nothing` and keeps it too. An `on_stored` that started each entry from `None` would warn
    every time;
  - `on_stored` returns the entry it replaced (`Arc::ptr_eq` with the one read before), so
    it is dropped outside the locks;
  - `SystemGone`: nothing written, and `on_stored` never called;
  - **`on_stored` runs after the commit and before the guard drops.** Inside it,
    `db.conn.try_lock()` fails, as in RFC 0009's `both_closures_run_under_the_database_mutex`,
    and a second connection to the same file already sees the points;
  - **the prune is capped:** a series filled with 100 expired points, then one snapshot:
    exactly 16 are deleted, and a second snapshot deletes 16 more. With 10 expired points one
    snapshot deletes all 10. The oldest go first;
  - `LastSeen::Unchanged` leaves a stored `last_seen` as it was, while the status is written;
  - **a failure part-way leaves nothing.** A trigger aborts one chosen metric's INSERT, with
    valid points on either side, as in `a_failing_insert_rolls_the_whole_round_back`. No point
    and no status change remain.
  - The statement cache can't be observed from a test. The end-to-end measurement covers it.
- **The `PUT` poll interval fix** (its own first commit, red first):
  - `PollInterval::try_from`, table-driven: 1, 10, `i64::MAX` accepted; `i64::MAX + 1` and
    `u64::MAX` refused;
  - through the `Router` (`oneshot`): `PUT /api/systems/{id}` with `poll_interval_secs`
    `u64::MAX` answers 422, and the row is unchanged and still readable by `get_system` and
    `list_systems`; with 30 it answers 200 and stores 30;
  - `update_system_config` binds `PollInterval`'s value, so the `v as i64` cast at `db.rs`
    goes.
- **`insert_system_if_absent`** (a temporary SQLite file):
  - a known id, with metric points, a name that isn't the default one, and a
    `poll_interval_secs` of −1 written through a second connection with raw SQL (as
    `push/mod.rs`'s tests already plant rows), so `get_system` fails: `Ok`, and the row, its
    name and its points are as they were;
  - a new id is inserted, under the default name;
  - with a `BEFORE INSERT` trigger that fails every `systems` INSERT: a new id returns the
    error, and no row is added;
  - **no INSERT runs for a known id:** with a recording `BEFORE INSERT` trigger on `systems`
    (it inserts `NEW.id` into a probe table), a known id is `Ok` and leaves the probe empty.
    An upsert, or an INSERT tried first with `system_exists` as a fallback on error, leaves
    one probe row for it and fails this row (measured by the sixth pass);
  - **a known id is `Ok` while writes are refused:** with `PRAGMA query_only = ON` on the test's connection
    (through `db.conn`): a known id is `Ok`, and a new id returns the error. `query_only`
    refuses every INSERT, even one that inserts nothing, as a read-only file does, and also a
    `BEGIN IMMEDIATE`, which a read-only file accepts. So it is the stricter of the two, as
    §4's "no write transaction" asks. The upsert alone and an
    `INSERT … SELECT … WHERE NOT EXISTS` both fail it for the known id (measured on SQLite
    3.45.1);
  - **a poisoned mutex panics:** with the mutex poisoned by a `#[cfg(test)]` method on
    `Database` (a `std::thread::scope` thread that panics while holding the guard),
    `catch_unwind` around `register_if_new`, the function `on_blocking_pool` runs, returns
    `Err`. A `register_if_new` that caught the unwind and returned it as an error fails it. So the real-server row
    below reaches the `JoinError`, and tests its answer and nothing else.
- **`end_connection`**, called directly: afterwards the system is offline and has no live
  metrics. It is one blocking unit, so no wait can separate the two.
- **Real server (push)**, with an injected `PushConfig`:
  - one frame with 1025 disks, one 257-byte mount point and a NaN `cpu_percent` stores
    `memory`, `swap`, `load1`, `load5` and the first 1024 valid disks, and no `cpu` point. Its
    live metrics hold the same values, with `cpu_percent` `null` in the summary;
  - a frame whose timestamp is `i64::MAX + 1` stores nothing, the connection stays open, and
    the next valid frame stores;
  - with a refill of 1 s, four snapshot frames 10 ms apart store three. With a refill of
    100 ms, a fourth frame 150 ms after three stores;
  - **the budget is spent before decoding:** three garbage messages, then a valid snapshot
    frame 10 ms later, with a refill of 1 s. The snapshot isn't stored. Spending after
    decoding would store it;
  - **application-shaped messages spend it too:** three application frames `TryFrom` refuses
    (17 applications), then a valid snapshot frame 10 ms later. The snapshot isn't stored.
    The previous revision, which spaced only snapshot frames, would store it;
  - **honest neighbours pass**, with a refill of 1 s: a snapshot frame 10 ms after an
    application frame is stored, and so is one 10 ms after a refused application frame; an
    application frame 10 ms after a snapshot frame is stored; the handshake burst (a round, a
    snapshot and a second round, back to back) stores all three;
  - deleting a connected system: its next frame ends the connection, and once the system is
    offline it has no live metrics. A new handshake with the same id then registers it again,
    under its default name, with none of its old points: what the agent's reconnect does;
  - **a failed registration answers `registry unavailable`:** with a `BEFORE INSERT` trigger
    that fails `systems` INSERTs, a handshake with a new id gets `auth_error` /
    `registry unavailable`, and a known id still gets `auth_ok`;
  - **a registration that panics answers it too:** with the database mutex poisoned through
    the same `#[cfg(test)]` method, a handshake gets `auth_error` / `registry unavailable`.
    Today's handshake closes with no answer. The `insert_system_if_absent` row pins that
    registration panics, so a handshake that left the `JoinError` as `Handshake::Closed`
    fails this row;
  - **registration never replaces a row:** a known push id whose `poll_interval_secs` is
    stored as −1 (today `PUT /api/systems/:id` with `u64::MAX` stores it) gets `auth_ok`, and
    keeps its name and its metric points. Today's lookup counts the row as absent, and the
    REPLACE wipes it;
  - a frame whose `uptime_display` and `memory_total_display` are 65 bytes each stores its
    points, keeps the previous `last_seen`, and keeps the stored memory capacity;
  - a store error keeps the connection open: with a trigger failing every snapshot INSERT, a
    frame stores nothing; once the trigger is dropped, the next frame stores;
  - any connection that ends leaves no live metrics once the system is offline (the existing
    wait for the offline marking).
- **Poll path** (the mock agent):
  - an answer with 1025 valid disks, no `cpu.usage_percent`, and a disk without `mount_point`
    stores the other scalars and the first 1024 valid disks, with no `cpu` point (not `0.0`)
    and no `disk:` point;
  - a body of exactly 4 MiB is read. A chunked body over 4 MiB is refused while it streams, and
    the system is marked offline, naming the cap;
  - a system deleted before its poll gets no points and no live metrics;
  - a system inserted after startup is polled on the next tick;
  - a tick whose `list_systems` fails still polls the systems of the last read that
    succeeded. The failure comes from a second system whose `poll_interval_secs` is stored
    as −1 after that read.
- **SSE:**
  - a new subscriber's first event arrives at once, with the interval set to 60 s;
  - **it is the published summary, not a fresh one:** a system inserted after the last publish
    is absent from a new subscriber's first event. A handler that serialised per subscriber
    would include it;
  - **a serialisation counter** (a test seam on the publisher's serialise step): three
    subscribers over two ticks make two serialisations after the first, not six;
  - **a summary published with no subscriber is kept:** with no subscriber, a system is
    inserted and one publish runs; a subscriber that then connects gets the system in its
    first event. A publisher using `send` would hand it the startup summary;
  - two subscribers receive byte-identical events, equal to the characterised bytes.
- **`DELETE /api/systems/:id`** removes the system's live metrics, for a push system and for a
  polled one.
- **End to end:** the implementation re-measures a 5-disk and a 1024-disk push frame through the
  hub, on a table filled to 24 h, and records the figures in the Appendix before this RFC is
  `Implemented`.

## Impact on `docs/ARCHITECTURE.md`

- § Components, `system-hub`: the snapshot rule on both ingestion modes, with the decode
  budget and the poll body cap; one transaction per snapshot, status included; live metrics
  written with the store, shared as `Arc`s, and evicted; one SSE summary per tick; no systems
  cache, with the poller keeping its last good read; deleting a connected push system
  disconnects its agent, which registers it again; push registration that can't replace a
  row, and answers `registry unavailable` when it fails or panics.
- § Data flow: every binary push message spends the connection's decode budget before it is
  decoded; the decode order (snapshot frame, then application frame) is unchanged.
- § Domain model:
  - the Fleet History, Fleet Registry and Ingestion rows: the new modules and types, and what
    leaves; `token_bucket.rs` and `hourly_warning.rs` beside them;
  - the glossary: **snapshot rule**, **mount point**, **decode budget**, **live metrics** and
    **last seen** added; **system status**'s and **snapshot**'s "In code", **retention**'s
    negative and non-integer rows, **push handshake**'s refusals (`registry unavailable`), and **memory capacity**'s display rule and bytes bound,
    changed;
  - the published contracts: the push frame (the decode budget, the rule, the display rule
    and the memory capacity's bytes bound), the push handshake (`registry unavailable`), and
    the poll responses (the body cap).
- § Trust boundaries: Agent → Hub (push) and Hub → Agent (poll) gain the rule, the decode
  budget and the cap.
- § Storage: a snapshot's points and its status are written in one transaction. Snapshot points
  carry the agent's clock on push and the hub's clock on poll; today's text says the agent's
  clock for both.
- § Testing architecture:
  - the push receiver: the wait for the offline marking now also covers eviction, and the
    decode refill is injected through `PushConfig`;
  - the SQLite layer: `store_snapshot` and `insert_system_if_absent`;
  - SSE: the publisher's tests.
- § Open architectural questions:
  - removed: the duplicated snapshot → metric mapping;
  - rewritten:
    - the auto-registration entry's sentence on what one frame costs, now bounded;
    - "Most blocking work is not offloaded": the poller's snapshot store, registry fill and
      offline marking, and the SSE publisher, run on the blocking pool. The alert records and
      the REST handlers still don't;
    - the Fleet Registry rules on each push frame: marking online is
      `StatusUpdate::after_snapshot` in `registry.rs`, written by `store_snapshot`; refilling
      the system info stays in `update_registry`;
    - the retention entry, which names `Database::insert_metric`: the fallback (no row, or a
      negative or non-integer one, means 24 h) is the pure `snapshot_retention`; the DELETE
      stays in SQL, inside `store_snapshot`'s transaction;
    - the "series never pruned" entry: upgrading to this RFC orphans more series (Rollout);
    - the `INSERT OR REPLACE` entry: push registration checks with `system_exists`, which
      reads no column, and inserts with `ON CONFLICT(id) DO NOTHING`, so no failed lookup can
      make it replace a row. `insert_system`'s REPLACE is left to `POST` alone, which inserts
      a new UUID;
  - added:
    - a row an older hub stored with a negative `poll_interval_secs` (its `PUT` accepted a value
      above `i64::MAX`; this RFC's `PollInterval` refuses it now) maps back through no read.
      `get_system` then fails for that row, so the `PUT` answers 500 and only `DELETE` removes
      it, and `list_systems` fails for every row: the REST list and the SSE summary fall back to
      no systems, and the poller keeps its last good read;
    - the system info strings (`hostname`, `os`, `kernel`, `cpu_model`, and the name taken
      from the hostname) are unbounded, and reach every SSE summary: push writes them once per
      registration, poll on every poll;
    - the client-chosen push `timestamp` drives pruning, so a far-future frame prunes a series'
      history;
    - a fleet's aggregate load against the single mutex, with the measured ceiling;
    - SSE subscribers are unbounded, each costing one copy of the summary per tick;
    - any connection presenting an id evicts its live metrics, until RFC 0016;
    - a deleted push system comes back while its agent runs, since nothing binds the id to
      a credential;
    - a message the decode budget drops is still read off the socket, up to 512 KiB.

## Impact on `README.md`

- The `PUT /api/systems/{id}` row: a `poll_interval_secs` above 2^63 − 1 is refused with 422,
  and nothing is stored.
- Push protocol, data frames: every binary message spends the connection's decode budget
  (3 messages, then one per second) before it is decoded; the snapshot rule (1024 disks, mount
  points, values that aren't finite); a timestamp above 2^63 − 1 refuses the frame; an
  `uptime_display` or `memory_total_display` over 64 bytes, or holding a control character,
  is ignored, and the stored value kept, as is a `memory_total_bytes` above 2^63 − 1 (both
  memory rules apply on poll too).
- The `DELETE /api/systems/{id}` row: it removes the system's live metrics too. A connected push
  agent is disconnected and registers the system again at once, with no history: stop the
  agent first.
- The `GET /api/stream/summary` row: one summary every 5 s, shared by every subscriber; a new
  subscriber gets the current one at once.
- Push protocol, `auth_error` table: `registry unavailable`, answered when the hub can't
  check or register a push id (a database error, or a registration that panicked); a known
  id's row is never replaced; the agent retries after 5 s, as for any `auth_error`.
- Polling: an `/api/system` answer over 4 MiB marks the system offline.
- Database: a snapshot's points are written in one transaction.
- The `system-hub` project tree: `snapshot.rs`, `token_bucket.rs`, `hourly_warning.rs`,
  `push/ingest.rs`, and
  `db/mod.rs` with `db/history.rs` in place of `db.rs`. It also fixes what the tree already gets
  wrong: it lists a `push/application_wire.rs` that doesn't exist (the file is
  `src/application_wire.rs`, listed too), and leaves out `registry.rs`, `clock.rs` and
  `listen.rs`.

## Implementation progress

On branch `feat/push-ingestion-cost`, continued on `claude/peaceful-einstein-acohge`, in the
order Rollout sets. Each step went through `red-test-adversary` (EVIDENCE; for §2 as an
in-process check at medium effort, with mutation runs) and the hub gate.

| Step | State | Commit |
|---|---|---|
| `PUT` poll interval fix (`PollInterval`, 422) | done | `904719e` |
| §6 split of `push/mod.rs` into `push/ingest.rs` | done | `828c060` |
| §6 split of `db.rs` into `db/mod.rs` and `db/history.rs` | done | `ebc6a4a` |
| `hourly_warning` moved into a pure module | done | `9f0ff1d` |
| §1 the snapshot rule (`snapshot.rs`) | done, not yet wired into the adapters | `0059304` |
| §2 `store_snapshot`, capped prune, `LastSeen`/`UptimeDisplay`/`StatusUpdate`, `MemoryCapacity` bounds | done, `store_snapshot` not yet wired into the adapters | `3d7d610` |
| §1/§2/§4 wiring both adapters: push and poll through the rule and `store_snapshot`, poll on the blocking pool, 4 MiB poll body, no systems cache, `insert_system_if_absent`, `registry unavailable` | next | |
| §3 decode budget (`TokenBucket`, `DecodeBudget`) | to do | |
| §4 `Arc<LiveMetrics>`, `end_connection`, eviction on delete | to do | |
| §5 SSE publisher (`watch`, `send_replace`) | to do | |
| README, ARCHITECTURE, end-to-end measurement in the Appendix, `rosette-auditor` on the whole diff, status `Implemented` | to do | |

Open items to settle on the way:
- `snapshot.rs`, `registry.rs` (`LastSeen`, `StatusUpdate`) and `db/history.rs`
  (`store_snapshot`) have temporary `#[cfg_attr(not(test), allow(dead_code))]` attributes, to
  be removed once the adapters use the items. `SnapshotTimeOutOfRange` gained one in §2: Rust
  1.98's dead-code lint flags it until the push edge converts a frame's timestamp.
- §2 added `SnapshotTime::cutoff` (the retention cutoff, never before 0), a pure rule the RFC's
  text leaves inside `store_snapshot`; it keeps the arithmetic out of the SQL adapter.
- Still to test with the wiring (Testing plan, `store_snapshot`): the warning time carrying
  over through the push path's own `on_stored`, and `on_stored` returning the replaced entry
  (`Arc::ptr_eq`). Both need §4's `LiveMetrics`.
- Removing `ORDER BY timestamp` from the capped prune survives the tests: SQLite reads the
  subquery through `idx_metrics_system_time`, already in timestamp order. The `ORDER BY` stays,
  so oldest-first is a guarantee rather than a planner choice.
- In a container without IPv6, `tests/fail_closed.rs`'s `[::]:0` case (RFC 0015) fails to bind;
  CI's runners have IPv6.
- `POST /api/systems` accepts a `poll_interval_secs` above `i64::MAX` and answers 500; it should
  parse through `PollInterval` too. A `PUT` of 0 is still stored while `POST` clamps to 5 (out
  of this RFC's scope; an open question).
- `rosette-auditor` hasn't run on any step yet.

## Rollout / migration notes

Hub-only. No schema change, and no change to the push frame or the poll response. Rolling back
is safe.

- **One pull request**, in this order: the `PUT` poll interval fix (its own commit, red
  first), the two splits (§6), the characterisation tests, then each behaviour, red first.
- Hosts with more than 1024 valid disks keep their first 1024, in mount order, whether they push
  or are polled. A `warn` names them, at most hourly.
- **The push frame ceiling stays RFC 0006's.** A push host whose frame exceeds 512 KiB (about
  4,000 typical Docker overlay mounts) still never has a frame stored. The frame is refused
  before it's decoded, so the rule never sees it. Polling such a host works: its answer fits the
  4 MiB poll cap up to about 14,000 such mounts, and the rule keeps the first 1024.
- A polled agent whose answer exceeds 4 MiB shows offline, naming the cap.
- A polled value the agent didn't report is no longer stored as `0.0`. There's no point, and
  the card shows `—`.
- Dashboards see the same SSE events, and a new subscriber's first one still comes at once. A
  push system's live metrics end with its connection, so its card shows `—` for live values, as
  a system without live data does today.
- **Deleting a pushing system now resets it rather than removing it,** for snapshot-only agents
  too: the agent is disconnected, reconnects, and the system reappears within about 2 s under
  its default name, then its hostname, with no history. Stop the agent before deleting the
  system.
- **Upgrading orphans some series.** Series the rule now drops stop receiving points: disks
  past a host's first 1024, the poller's `disk:` series (a disk without a mount point), and
  mount points over 256 bytes or holding a control character. Snapshot pruning runs only when a
  metric is inserted, so their points, up to 43,200 per series at 24 h, stay until the system
  is deleted. That is the existing "series never pruned" open question, which already holds
  every unmounted disk's series; RFC 0010 leaves the SQLite file behind. No one-off cleanup
  ships: disks past the first 1024 can't be told apart in SQL from disks that are merely
  unmounted.
- Messages a hub-side stall buffered past the budget's 3 tokens are dropped (§3): only after
  a stall of about 5 s or more.
- A memory capacity an older hub stored with a display over 64 bytes, or holding a control
  character, is replaced by the agent's next valid report (§2). A stored `last_seen` stays
  until the next valid uptime.
- A push row that no read can map (a `poll_interval_secs` stored as −1) keeps its history
  when its agent reconnects, where today the reconnect replaces it (§4).

## Appendix: measurement

Environment, on 2026-09-29:
- WSL2 (Linux 6.18), ext4 on a virtual disk, AMD Ryzen 7 7840HS;
- Python 3.12's `sqlite3`, with SQLite 3.45.1. The hub bundles 3.45.0, through
  `libsqlite3-sys` 0.28;
- the hub's schema and statements, and its defaults: rollback journal, `synchronous=FULL`, a
  2 MB page cache, foreign keys on;
- one commit costs about 4.6 ms on this disk;
- the median of 20 frames, or of 5 for today's 1024-disk frame.

| Frame | Table | Today | One transaction, uncached | One transaction, cached |
|---|---|---|---|---|
| 5 disks | one row per series | 55 ms | 6.4 ms | 5.9 ms |
| 5 disks | filled to 24 h | 97 ms | 5.7 ms | 5.7 ms |
| 1024 disks | one row per series | 5.3 s | 34 ms | 40 ms |
| 1024 disks | filled to 24 h | 9.9 s | 48 ms | 41 ms |

- The filled table holds 44.5 million rows (13.3 GiB): one system's 1029 series, a point every
  2 s for 24 h, written frame by frame as the hub writes them. Each measured frame deletes one
  row per series, as on a hub that has run for a whole window.
- The 5-disk frames touch 10 of those series, so they run against a table the size of a fleet
  of about 100 such systems.
- The worst of 20 cached frames on the filled table was 50 ms at 1024 disks, and 7.5 ms at 5.
- The capped prune's worst case (§2), as the fourth `rfc-adversary` pass measured it on the
  same machine, with the RFC's capped DELETE:
  - `EXPLAIN QUERY PLAN` shows the capped subquery served by `idx_metrics_system_time`
    (`SEARCH metrics USING COVERING INDEX … (system_id=? AND metric=? AND timestamp<?)`),
    with no temporary B-tree for its `ORDER BY`;
  - a 1024-disk snapshot draining a backlog deleted 16,464 rows in 136–147 ms, against
    45–51 ms in the steady state;
  - a 5-disk one took about 6.7 ms, against 5.9 ms.
- At this disk's commit cost, the statement cache is within the noise except on the filled
  table's 1024-disk frame.
- The previous revision's figures (15.4 ms and 1.7 ms at 5 disks, 1,206 ms and 23.9 ms at 1024)
  came from a development container's overlay filesystem, on an empty table, with Python's
  statement cache on in both columns. They aren't comparable with these.
  - RFC 0010's Motivation cited "~1.5 ms per point" from them, and now cites this figure. The figure to cite is
    today's cost per point on the measured disk, about 9.7 ms (97 ms for a 10-point frame,
    9.9 s for 1029 points, status and registry reads included). 0010 takes it when it is next
    amended.
- These figures are SQLite's cost alone, from Python. **The implementation re-measures end to
  end,** through the hub, on a table filled to 24 h, and records the figures here before this
  RFC is marked `Implemented`.

Run as `python3 bench.py bench.db 0 20` (one row per series) and
`python3 bench.py bench.db 86400 20` (filled to 24 h), on the storage being measured:

```python
import os, sqlite3, sys, time

# python3 bench.py <db> <window_secs> <frames>. Fills one system's 1029 series (5 scalars, 1024
# disks) with a point every 2 s over the window, frame by frame as the hub writes them, then
# times frames of 1024 and of 5 disks. Each frame deletes one row per series: steady state.
DB, WINDOW, FRAMES = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
SID, T0, STEP, RETENTION = '0f4e2a9c7b1d4e8f9a3c5b7d1e2f4a6c', 2_000_000_000, 2, 86400
NAMES = ['cpu', 'memory', 'swap', 'load1', 'load5'] + \
    [f'disk:/var/lib/docker/overlay2/{i:064x}/merged' for i in range(1024)]
SCHEMA = """PRAGMA foreign_keys=ON;
CREATE TABLE systems (id TEXT PRIMARY KEY, name TEXT NOT NULL, url TEXT NOT NULL,
  token TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'unknown',
  last_seen TEXT NOT NULL DEFAULT '', last_error TEXT, os TEXT, hostname TEXT, kernel TEXT,
  cpu_model TEXT, cpu_cores INTEGER, total_memory_display TEXT, total_memory_bytes INTEGER,
  poll_interval_secs INTEGER NOT NULL DEFAULT 10, enabled INTEGER NOT NULL DEFAULT 1);
CREATE TABLE metrics (id INTEGER PRIMARY KEY AUTOINCREMENT, system_id TEXT NOT NULL,
  metric TEXT NOT NULL, value REAL NOT NULL, timestamp INTEGER NOT NULL,
  FOREIGN KEY (system_id) REFERENCES systems(id) ON DELETE CASCADE);
CREATE INDEX idx_metrics_system_time ON metrics(system_id, metric, timestamp);
CREATE TABLE metric_retention (system_id TEXT NOT NULL, metric TEXT NOT NULL,
  retention_secs INTEGER NOT NULL DEFAULT 86400, PRIMARY KEY (system_id, metric),
  FOREIGN KEY (system_id) REFERENCES systems(id) ON DELETE CASCADE);"""

def fill():
    if os.path.exists(DB): os.remove(DB)
    c = sqlite3.connect(DB, isolation_level=None)
    c.executescript('PRAGMA cache_size=-4000000;' + SCHEMA)  # a big cache for the fill only
    c.execute('BEGIN')
    c.executemany('INSERT INTO systems (id, name, url) VALUES (?,?,?)', [(SID, 'measured', 'push://')]
                  + [(f'{i:032x}', f'host-{i:04}', 'push://') for i in range(1, 1000)])
    rows = WINDOW // STEP + 1
    c.executemany('INSERT INTO metrics (system_id, metric, value, timestamp) VALUES (?,?,?,?)',
                  ((SID, m, 1.0, T0 - k * STEP) for k in range(rows - 1, -1, -1) for m in NAMES))
    c.execute('COMMIT'); c.close()
    print(f'{rows * len(NAMES):,} rows, {os.path.getsize(DB) / 2**30:.1f} GiB', flush=True)

def frame(c, ts, names, mode):
    tx = mode != 'today'
    if tx:  # store_snapshot: one transaction, the row checked first
        c.execute('BEGIN'); c.execute('SELECT EXISTS(SELECT 1 FROM systems WHERE id = ?)', (SID,)).fetchone()
    for m in names:  # insert_metric, per point
        c.execute('INSERT INTO metrics (system_id, metric, value, timestamp) VALUES (?,?,?,?)', (SID, m, 1.0, ts))
        r = c.execute('SELECT retention_secs FROM metric_retention WHERE system_id=? AND metric=?', (SID, m)).fetchone()
        c.execute('DELETE FROM metrics WHERE system_id=? AND metric=? AND timestamp < ?', (SID, m, ts - (r[0] if r else RETENTION)))
    c.execute('UPDATE systems SET status = ?, last_seen = ?, last_error = ? WHERE id = ?', ('online', '3d 4h', None, SID))
    if tx: c.execute('COMMIT')
    c.execute('SELECT * FROM systems WHERE id = ?', (SID,)).fetchall()      # update_registry's read
    if not tx: c.execute('SELECT * FROM systems ORDER BY name').fetchall()  # refresh_cache, today only

fill()
ts = T0
for disks in (1024, 5):
    # today: rusqlite's `execute` prepares each statement anew (Python: cached_statements=0).
    for mode, cache in (('today', 0), ('one tx, uncached', 0), ('one tx, cached', 128)):
        c = sqlite3.connect(DB, isolation_level=None, cached_statements=cache)  # default page cache
        c.execute('PRAGMA foreign_keys=ON')
        times = []
        for _ in range(FRAMES if mode != 'today' or disks == 5 else max(3, FRAMES // 4)):
            ts += STEP; t = time.perf_counter(); frame(c, ts, NAMES[:5 + disks], mode)
            times.append((time.perf_counter() - t) * 1000)
        c.close(); times.sort()
        print(f'{disks:5} disks, {mode:16}: median {times[len(times) // 2]:8.1f} ms, max {times[-1]:8.1f} ms', flush=True)
```

## Review

### First pass

`rfc-adversary`, first pass, on the 2026-09-25 revision. The 2026-09-29 revision resolved every
finding as follows. Where the second pass changed a resolution, its table says so.

| Finding | Verdict | Resolution |
|---|---|---|
| the cost figures aren't an upper bound (statements re-prepared, an empty table) | CONFIRMED | `prepare_cached` for every statement of the store and the status (§2); re-measured on a table filled to 24 h, with and without the statement cache, the status and the registry reads included, and the environment stated; the API4 bounds restated from the new figures; the implementation re-measures end to end (Motivation, Security, Appendix) |
| one non-finite value would discard the whole snapshot; so would a timestamp above `i64::MAX` | CONFIRMED | the snapshot rule leaves out values that aren't reported or aren't finite one by one, and counts them; `SnapshotTime` refuses a frame whose timestamp SQLite can't hold, whole (§1, §2) |
| axum still copies SSE data once per subscriber (`Event::data`) | CONFIRMED | the claim is restated: one build and one serialisation per tick, plus one copy per subscriber by `Event::data`. Sharing one `Bytes` is rejected, since it means hand-rolling SSE framing and keep-alive (§5, Alternatives) |
| the live entry's disks and bound are unstated for the poll path; the warn-once set is unbounded | CONFIRMED | live metrics hold the kept `Snapshot` on both paths, about 300 KB per system at most; the warning time lives in the entry, so it is bounded and evicted with it (§1, §4) |
| new database work runs on the async runtime (poll store, the publisher's reads, decoding before pacing) | CONFIRMED | the poll's store, registry fill and offline marking run on the blocking pool (§2); the publisher builds on the blocking pool (§5); spacing is decided before a snapshot decode, with application frames decoded first (§3) |
| RFC 0006 can't be helped by the disk rule: oversize frames never decode | CONFIRMED | the Rollout names the 512 KiB ceiling (about 4,000 overlay mounts) and polling as the fallback, within the new 4 MiB poll cap (§1, Rollout) |
| several planned tests pass without the behaviour | CONFIRMED | `on_stored` checked under the mutex and after the commit; eviction tested on `end_connection` itself, one blocking unit; a serialisation counter, and a published-not-fresh SSE test; rows for NaN and ±∞, an empty mount point, and 1025 reported disks with 1023 valid; a garbage-then-valid spacing test (Testing plan) |
| the `refresh_cache` rule contradicts itself (the status changes every frame) | CONFIRMED | the systems cache leaves `AppState`; the poller reads the registry each tick, and nothing else refreshes (§2) |
| Domain impact leaves out Fleet Registry; "history points" duplicates **metric point**; `MetricSnapshot`'s fate is unstated | CONFIRMED | Fleet Registry named (the status in the snapshot's transaction); no "history points" term: a snapshot's metric points are `Snapshot::metric_points`; the hub's domain type is `Snapshot`, under the existing **snapshot** term, with no "snapshot reading" either, which would have sat next to the agent's **metric readings**; `MetricSnapshot`, `DiskSnapshot` and `ProcessSnapshot` go; a value that isn't reported skips its point (§1, Domain impact) |
| inventory gaps: the README, ARCHITECTURE § Storage, the Testing sync point and three open questions, the dependencies on RFC 0006's `PushConfig` and RFC 0008's `on_blocking_pool`, Security not category by category | CONFIRMED | a README section; § Storage, § Testing architecture and the open questions listed; the header depends only on implemented RFCs 0006 and 0009, using today's `push::on_blocking_pool` and `tokio::task::spawn_blocking`, and nothing from RFC 0008 or 0016; Security written out category by category (header, Security, Impact sections) |
| `push/mod.rs` should split before this RFC grows it | CONFIRMED | split at `on_blocking_pool` into `push/ingest.rs`, sized against today's 506 lines of code (about 165 move); `db.rs`, at 524, splits into `db/history.rs` too; both are pure refactors landing first (§6) |
| SSE first-event timing | PLAUSIBLE | adopted: the first summary is built before the hub serves, and `WatchStream::new` hands it to a new subscriber at once (§5) |
| skipping all disks above 1024 hides `/` | PLAUSIBLE | adopted: invalid entries are dropped, then the first 1024 are kept in reported order (§1, Alternatives) |
| `StatusUpdate` "with uptime" doesn't fit the poll path | PLAUSIBLE | adopted: `store_snapshot` takes each adapter's `last_seen`, unchanged (§2) |
| eviction triggered by a self-asserted id (A01) | PLAUSIBLE | deferred to RFC 0016, which restricts both the offline marking and the eviction to the current connection. Until then the harm is stated and bounded: the next honest frame rewrites the entry, and the same end already marks the system offline (§4, Security A01) |

Also changed in the first pass's revision:
- facts re-checked against the code as RFCs 0006, 0009, 0014 and 0015 left it: the module
  paths (`push/`, `collector/`), the SSE handler's database reads, the poll path's `0.0`
  defaults and missing body cap, and the delete handler evicting only `live_applications`;
- the poll body cap (§1);
- the application-first decode order and the rename of the pacing to **frame spacing** (§3),
  both replaced by the second pass's decode budget;
- a `README.md` impact section of its own.

### Second pass

`rfc-adversary`, second pass, on the first pass's revision. This revision resolves every finding
as follows:

| Finding | Verdict | Resolution |
|---|---|---|
| spacing bounds only the snapshot decode: application-shaped messages are still decoded in full, unpaced, on a runtime worker, so §3's per-connection bound and API4 were wrong | CONFIRMED | **design changed**: the snapshot spacing becomes a **decode budget** every binary message spends before any decode (3 tokens, then one per second), so a refused or undecodable application-shaped message spends it too. The decode order goes back to today's, since the budget no longer depends on it. Honest traffic is shown to fit (at most 0.6 messages a second, a burst of 3 after the handshake). What stays unbounded, the bytes read off the socket for a dropped message, is listed. `TokenBucket` is shared with `SourcePace` rather than copied (Motivation 4, §3, Domain impact, Alternatives, API4, Testing plan) |
| SSE: `watch::Sender::send` stores nothing while no one subscribes, and no planned test catches a new dashboard getting the startup summary | CONFIRMED | the publisher uses `Sender::send_replace`; `send` and a `Receiver` kept in `AppState` are recorded as alternatives; a test row: no subscriber, a system inserted, one publish, then a subscriber whose first event has the system (§5, Alternatives, Testing plan) |
| deleting a connected push system now makes every push agent re-register within one frame, and neither README nor Rollout says so | CONFIRMED | ending the connection is kept, as RFC 0009 already does for rounds, and stated: §4 gives the sequence, and why the reconnect can't race the old connection's end; Rollout, the README `DELETE` row, A01 and a new open question say that a deleted pushing system comes back until its agent stops; the delete test now asserts that a new handshake re-registers it under the default name, with no history. Keeping the connection open is recorded as an alternative (§4, Alternatives, Security, Testing plan, Impact sections) |
| no test checks that an application frame leaves the snapshot spacing clock alone | CONFIRMED | there's no longer a snapshot-only clock: every message spends the budget by design, and its capacity of 3 holds an agent's honest neighbours. Rows: a snapshot 10 ms after an application frame, and after a refused one, is stored; an application frame 10 ms after a snapshot is stored; the handshake burst stores all three; application-shaped messages do spend the budget (Testing plan) |
| `on_stored` takes the live write lock under the database guard, so the publisher's deep clone of `live_metrics` stalls the SQLite mutex | PLAUSIBLE | adopted (**design changed**): entries are `Arc<LiveMetrics>`, and nothing under the live lock grows with a snapshot's disks. Readers copy keys and `Arc`s, and a replaced entry is returned out of the store and dropped after both locks are released. Test: `on_stored` returns the replaced `Arc` (§4, §5, Alternatives, Testing plan) |
| "nothing in a snapshot can fail a statement" ignores the retention SELECT, which reads a stored row; its error handling inside the transaction is unspecified and untested | PLAUSIBLE | adopted (**design changed**): the column is read with `get_ref`, and a pure `snapshot_retention` maps no row, a negative value or a non-integer one to 24 h, as today and as `app:*` pruning does, so no hand-set row can roll a snapshot back. Only a failing database can. Rows: 3,600, missing, −1 and text, plus the pure table; the **retention** glossary entry changes (§2, Domain impact, API10, Testing plan) |
| no test covers carrying the hourly left-out warning over; dropping it warns on every frame | CONFIRMED | the decision is the pure `left_out_log(previous, &LeftOut, now) -> (LeftOutLog, Option<Instant>)`, with a table test; a `store_snapshot` row through the push path's own `on_stored` gives `Warn`, then `Debug`, then `Nothing`, with the time kept (§1, §4, Testing plan) |
| store errors are logged at `warn` on every frame with no limit | PLAUSIBLE | adopted for push: the first per connection at `warn`, the rest at `debug`, and a count when the connection ends, as refused application frames. Poll keeps one `warn` per system per poll, which is today's rate for the "couldn't mark online" warning the store's status write replaces. A fleet-wide bound belongs with rate limiting (§4, A09, Testing plan) |
| the "mark online" registry decision moves from one adapter into another, the SQL one | PLAUSIBLE | adopted (**design changed**): `registry.rs` gains `StatusUpdate::after_snapshot(LastSeen)`, and `store_snapshot` writes the value it's given. `LastSeen` names the column's two meanings (`Uptime`, `PolledAt`), which answers the first pass's objection that a status "with uptime" didn't fit the poll path; glossary: **last seen**, and **system status**'s "In code" (§2, §6, Domain impact, Alternatives) |
| inventory gaps: ARCHITECTURE's retention entry names `insert_metric`; upgrading orphans series; the README project tree | CONFIRMED | (a) the retention open question is rewritten around `snapshot_retention`; (b) Rollout lists the orphaned series (disks past 1024, the poller's `disk:`, invalid mount points) and why no one-off cleanup ships, and the "series never pruned" open question takes them; (c) the README impact adds the tree: the new files, and its existing errors (`push/application_wire.rs` doesn't exist; `registry.rs`, `clock.rs`, `listen.rs` are missing) (Impact sections, Rollout) |
| RFC 0010 cites a figure 0007 disowns, and hub-time stamping leaves 0007's `SnapshotTime` refusal with nothing to protect | CONFIRMED | §1 and the header state that the refusal exists only while push snapshots carry the agent's clock, and that RFC 0010 drops it; the Appendix gives the per-point figure 0010 should cite (about 9.7 ms on the measured disk). 0010's Motivation now cites the new figure; its header takes the refusal's end when it is next amended (header, §1, Appendix) |

This pass changed the design (the decode budget, `Arc<LiveMetrics>`, `StatusUpdate`, the pure
retention and left-out rules), so a third `rfc-adversary` pass ran.

### Third pass

`rfc-adversary`, third pass, on the design changes of the second. This revision resolves every
finding as follows:

| Finding | Verdict | Resolution |
|---|---|---|
| in the one transaction, the retention DELETE's cost depends on how many rows the frame's time expires, not on how many points it carries: after a 24 h gap, or with a far-future push `timestamp`, one frame holds the mutex for seconds (8.5 s measured for 100 series) and the rollback journal grows with the expired history | CONFIRMED | **design changed**: each point's prune deletes at most `PRUNE_PER_POINT = 16` of the series' oldest expired rows (`rowid IN (SELECT … ORDER BY timestamp LIMIT ?)`, the shape `prune_in_batches` uses). The steady state deletes one; a backlog drains over later snapshots (a 24 h window in about 1.6 h). Refusing far-future timestamps was not adopted: it would refuse honest agents with skewed clocks in a mixed fleet, and RFC 0010 moves to hub time; the capped prune bounds the cost either way, and the open question on the client-chosen timestamp stays. Test rows: 100 expired rows, 16 per snapshot, oldest first (§2, API4, Testing plan) |
| `hourly_warning` is a pure rule placed in `state.rs`, an adapter that imports `db::Database`, and the pure `left_out_log` would call it | CONFIRMED | it moves to a context-free pure module, `hourly_warning.rs`, beside `token_bucket.rs`; Domain impact lists both under "No context" (§1, Domain impact, ARCHITECTURE and README impact) |
| `LastSeen::Uptime(String)` carries the frame's unbounded `uptime_display` into `systems.last_seen` on every frame and into every SSE summary | PLAUSIBLE | adopted (**design changed**): `UptimeDisplay`, 1..=64 bytes with no control character, parsed at the push edge; an invalid one is `LastSeen::Unchanged`, written with `COALESCE`, so the column keeps its value. Rows for the parse and for the store (§2, Domain impact, glossary, API4, Testing plan) |
| a hub whose registration insert fails answers `auth_ok`, and with `SystemGone` now ending snapshot connections every new or deleted push agent loops with no delay, contradicting A09 | PLAUSIBLE | adopted (**design changed**): `register_if_new` returns its result, and a failed insert answers `auth_error` / `registry unavailable` (`Refusal::RegistryUnavailable`), logged at `warn`; every shipped agent retries an `auth_error` after 5 s. RFC 0008 reuses the answer for its store errors. Real-server row with a trigger failing `systems` INSERTs (§4, Domain impact, A09, README impact, Testing plan) |

The closest attacks that failed: the decode budget against honest mixed-version agents (every
shipped agent fits it), and the delete → reconnect ordering with `send_replace`.

**Still open:** nothing CONFIRMED is unaddressed. This pass changed the design (the capped
prune, `UptimeDisplay`, `registry unavailable`), so a fourth `rfc-adversary` pass, limited to
those changes, runs before this RFC is `Accepted`.

### Fourth pass

`rfc-adversary`, fourth pass, on the design changes of the third. This revision resolves every
finding as follows:

| Finding | Verdict | Resolution |
|---|---|---|
| §4 rewrites `register_if_new` without saying what a failed lookup means. Today a lookup error counts as "not registered", and `insert_system`'s `INSERT OR REPLACE` then replaces a known row and cascade-deletes its history (104,000 metric rows to 0). An unauthenticated `PUT` storing a `poll_interval_secs` above `i64::MAX` makes that lookup fail for good | CONFIRMED | **design changed**: registration goes through `Database::insert_system_if_absent`, a check with `system_exists`, which reads no column, then `INSERT … ON CONFLICT(id) DO NOTHING`, never a REPLACE. A known row is `Ok` even when `get_system` can't map it, and no registration can replace a row. Only a failed check or insert answers `registry unavailable`. The suggested upsert alone was not adopted: an INSERT on a read-only file fails even when it would insert nothing, and a `BEFORE INSERT` trigger fires for a known id too (both measured on SQLite 3.45.1), so it would refuse known agents. The same row fails `list_systems` for every row, so the poller keeps its last good read, as today's cache does. `system_exists` stays in `db/mod.rs`. Rows: `insert_system_if_absent` on a known row that doesn't map, on a new id, and under a failing trigger; a real-server known id stored with −1 gets `auth_ok` and keeps its name and points; a poll tick whose read fails. ARCHITECTURE's `INSERT OR REPLACE` question is rewritten, and the `PUT` cast is added as an open question (§2, §4, §6, Domain impact, Alternatives, A08, API10, Testing plan, Impact sections, Rollout) |
| API4 says `UptimeDisplay` removes a push system's contribution of up to 512 KiB to every SSE summary, but `memory_total_display` is still unbounded, rewritten whenever it changes, and in every summary | CONFIRMED | **design changed**: `MemoryCapacity::reported` applies the same display rule (`MAX_DISPLAY_BYTES` = 64, no control character) on both paths. A display that breaks it is no report, so the stored capacity is kept, and a stored one that breaks it is replaced by the next valid report. The agent's `format_bytes` gives at most 10 bytes. The system info strings (`hostname`, `os`, `kernel`, `cpu_model`, the name taken from the hostname) are listed under API4's "Still unbounded" with their write rates: once per push registration, and every poll. Bounding them is recorded as an alternative, since no agent formatter bounds their honest length. API4's per-system claim now names both displays, and its per-connection figure names the registry fill's commits. Rows: display rows for `MemoryCapacity::reported`, a stored oversize display replaced, and a real-server frame with a 65-byte memory display (§2, Domain impact, glossary **memory capacity**, Alternatives, A04, API4, API10, Testing plan, Impact sections, Rollout) |
| a registration `JoinError` still closes the handshake with no answer. Every shipped agent treats that as accepted and then reconnects with no delay, so "paced by the agent's 5 s backoff" covers only a returned error | PLAUSIBLE | adopted (**design changed**): the handshake answers a registration `JoinError` with `auth_error` / `registry unavailable` too, now rather than in RFC 0008, and `Handshake::Closed` keeps only the socket's own endings. §4 and A09 state that every registration failure is paced by the agent's 5 s retry. On a poisoned mutex, a frame's `JoinError` still ends its connection, and the reconnect's registration then panics and is answered, so that loop is paced too. Test: a mutex poisoned through a test seam on `Database` answers `registry unavailable` (§4, Domain impact, published contracts, Alternatives, A09, Testing plan, README impact) |

The closest attacks that failed:
- the capped prune's worst case: 136–147 ms at 1024 disks, three times the steady state, paid
  only while a series drains a backlog, which it always does. The figures are now in the
  Appendix, §2 and API4;
- `UptimeDisplay` against real agents: `format_uptime` never exceeds about 24 bytes.

**Decided (was a question for the owner):** the fix for the `PUT` cast (refusing a
`poll_interval_secs` above `i64::MAX` at the edge, with its own red test) rides in this RFC's
pull request, as its own first commit. This RFC touches that handler anyway, and its poller
reads `list_systems` at every tick, which one such row fails for every system. The design
doesn't depend on it: registration no longer depends on a row mapping, and the fix doesn't
repair rows already stored.

**Still open:** nothing CONFIRMED is unaddressed. This pass changed the design, so this RFC
stays `Draft`. The changes: registration through `insert_system_if_absent`, the display rule
on the memory capacity, `registry unavailable` for a registration that panics, and the
poller's last good read. A fifth `rfc-adversary` pass, limited to those changes, must look
at:
- `insert_system_if_absent`'s check before its upsert, on a full or read-only database, and
  whether its test trigger proves no INSERT runs for a known id;
- the poller's last good read while reads keep failing: a deleted system still polled, and a
  new one never;
- the display rule on `MemoryCapacity` on both paths, including capacities an older hub
  stored;
- the `JoinError` answer: other panics in the unit, and how shipped agents handle it;
- the capped prune's figures, now cited in API4.

### Fifth pass

`rfc-adversary`, fifth pass, on the design changes of the fourth. This revision resolves every
finding as follows:

| Finding | Verdict | Resolution |
|---|---|---|
| the `BEFORE INSERT` trigger row can't show that no INSERT runs for a known id: triggers fire per row, so a zero-row `INSERT … SELECT … WHERE NOT EXISTS` passes every planned row, yet fails on the read-only file the check-first order exists for, refusing every known agent `registry unavailable` | CONFIRMED | test fixed, design unchanged. §4 now states what the check-first order already meant: for a known id nothing but the check runs, with no INSERT and no write transaction. A new `insert_system_if_absent` row runs with `PRAGMA query_only = ON`: a known id is `Ok`, and a new one returns the error. The upsert alone and the `NOT EXISTS` form both fail it (re-measured on SQLite 3.45.1). `query_only` also refuses a `BEGIN IMMEDIATE`, which a read-only file accepts, so the row is the stricter of the two. The trigger row keeps only what it can show: a new id's error, and no row inserted for a known one (§4, Testing plan) |
| `MemoryCapacity::reported` checks the display but not the bytes. rusqlite can't bind a `u64` above `i64::MAX`, so such a report is never stored and is retried, with a `warn`, on every frame and every poll. That contradicts A09 and §2's "rare writes" | CONFIRMED | **design changed**, by one condition: `reported` also refuses bytes above `i64::MAX`, as `SnapshotTime` refuses a timestamp, so such a report counts as none and the stored capacity is kept. The bug predates this RFC (RFC 0014 step 7), but §2 already makes `reported` the one check of a reported capacity, so the fix goes there. Shipping it as a bug fix of its own is recorded as an alternative. §2's registry fill no longer calls its writes rare without qualification: a sender can change the capacity on every frame, at one commit each, which API4 already counts. Rows: bytes of `i64::MAX` are a report, `i64::MAX + 1` and `u64::MAX` are none, and a refresh over a valid stored capacity writes nothing (§2, Domain impact, glossary **memory capacity**, Alternatives, A04, API10, Testing plan, README and ARCHITECTURE impact) |
| RFCs 0008 and 0006 still say RFC 0008 introduces `Refusal::RegistryUnavailable` and fixes the registration `JoinError`, which this RFC now does | CONFIRMED | factual corrections, no design change. RFC 0008's Related line, Motivation 2, §4, Domain impact, published contracts, API9 and README impact now say `Refusal` gains only `RegistryFull`, and store errors reuse RFC 0007's `registry unavailable`. RFC 0006's two mentions now name RFC 0007. §4 here says both RFCs agree, and that `Handshake::Closed`'s doc comment stops naming RFC 0008 (§4; RFCs 0006 and 0008) |
| the poisoned-mutex test reaches the `JoinError` only if `insert_system_if_absent` panics on a poisoned lock, which the RFC didn't state. Mapping the poison to an error would pass it through the returned-error path and leave `JoinError → Handshake::Closed` untested | PLAUSIBLE | adopted and pinned. §4 already depended on registration panicking on a poisoned mutex ("the reconnect's registration then panics too"), and now says so: `insert_system_if_absent` locks as every `Database` method does. Recovering the guard would accept an agent whose every store then panics, an unpaced reconnect loop. Mapping the poison to an error would be paced, but would leave the `JoinError` answer with no test that reaches it. New row: `catch_unwind` around `insert_system_if_absent` on a poisoned `Database` is `Err`, so the real-server row tests the `JoinError` mapping alone (§4, Alternatives, Testing plan) |

The closest attacks that failed:
- `insert_system_if_absent` against races, cascades and callers: one guard serialises two
  handshakes for one id, `ON CONFLICT(id) DO NOTHING` deletes nothing, so no cascade can fire,
  and `POST`, the last `INSERT OR REPLACE` caller, inserts a new UUID;
- the poller's last good read, which matches today's `AppState::refresh_cache` when a refresh
  fails, so it isn't a regression.

Also clean: the agent's `format_bytes` (at most 10 bytes); no RFC 0014 step 7 test uses a
display the rule refuses; shipped agents' handling of a missing answer and of `auth_error`;
and the capped prune's figures, which agree across §2, API4 and the Appendix.

This pass changed the design, by the bytes condition on `MemoryCapacity::reported`, so a
sixth pass ran. The other amendments
correct tests, wording and other RFCs, and state a lock convention §4 already relied on. A
sixth `rfc-adversary` pass, limited to this revision, must look at:
- the bytes bound on `MemoryCapacity::reported`, on both paths, and against capacities an
  older hub stored;
- whether the `query_only` row and the poisoned-mutex row now pin what §4 claims: no write
  for a known id, and a registration `JoinError` answered rather than closed;
- the lock convention stated for `insert_system_if_absent`, against `CLAUDE.md`'s rules on
  `unwrap`.

### Sixth pass

`rfc-adversary`, sixth pass, limited to the fifth pass's revision. Every finding is resolved in
the tests, the inventory or the glossary; none changes the design:

| Finding | Verdict | Resolution |
|---|---|---|
| the decided `PUT` fix is missing from Rollout, the Testing plan, the ARCHITECTURE and README impact, API3 and API9, and the RFC doesn't say where the refusal lives | CONFIRMED | §4 names `PollInterval` (`TryFrom<u64>`, at most `i64::MAX`, 422) and drops the `v as i64` cast; Rollout puts the fix first; the Testing plan gains its rows, and the −1 fixtures are planted through a second connection; the open question is narrowed to rows an older hub stored; API3, API9 and the README impact list the refusal (§4, Security, Testing plan, Impact sections, Rollout) |
| the `query_only` row pins "a known id is `Ok` while writes are refused", not "no INSERT runs for a known id": an INSERT-first cheat with a `system_exists` fallback passes every row | CONFIRMED | a recording `BEFORE INSERT` trigger row: a known id leaves the probe table empty, which the cheat and the upsert fail; the `query_only` row keeps catching the `NOT EXISTS` form and is renamed to what it pins (Testing plan) |
| the **push handshake** glossary entry still limits a refusal to a rejection or a timeout | CONFIRMED | Domain impact and the ARCHITECTURE § Domain model impact change **push handshake**: a refusal also covers `registry unavailable` (Domain impact, Impact on `docs/ARCHITECTURE.md`) |
| the poisoned-mutex rows pin that the lock panics, not that the registration unit does | PLAUSIBLE | adopted: `catch_unwind` wraps `register_if_new`, the function `on_blocking_pool` runs, so the real-server row can only pass through the `JoinError` arm (Testing plan) |

Held: the bytes bound on `MemoryCapacity::reported` (no older hub can have stored bytes above
`i64::MAX`), the lock convention for `insert_system_if_absent`, and the RFC 0006/0008 edits.
The closest attack was the lock convention: one of its three reasons (a `JoinError` answer
with no reachable test) is weak, since `spawn_blocking(|| panic!())` reaches it, but the
conclusion stands on the other two.

**Status:** nothing CONFIRMED is unaddressed, and this pass's amendments change no design, so
per `CLAUDE.md` no seventh pass runs. `Accepted`.
