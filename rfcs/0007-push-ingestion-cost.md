# RFC 0007: One Snapshot Rule, One Transaction, One Serialisation

- Status: Draft
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-25
- Affects: `system-hub`
- Depends on: RFC 0006 (the message size bounds a frame before this RFC's rules run)
- Related: RFC 0008. Split from an earlier, wider draft of RFC 0006 (see git history). This RFC
  keeps the part that belongs to Fleet History: what a snapshot costs to store, and the live
  state it leaves behind.

## Motivation

Once RFC 0006 bounds a connection, what one frame, or one poll, costs the hub is still
unbounded, and so is what it leaves in memory:

1. **Per-point commits.** Each metric point is one `insert_metric` call: an INSERT, a SELECT
   and a DELETE, each write committing on its own. It runs under the hub's single SQLite
   mutex, which REST and SSE handlers also take from async code. Each frame also runs
   `update_system_status`, another autocommit write.
2. **A scan per frame that nobody reads.** Each push frame calls `refresh_cache()`, which runs
   `list_systems()`: a full `systems` scan with `ORDER BY name`. The only reader of
   `systems_cache` is the poller, and it refreshes the cache itself right before reading.
3. **Unbounded disks.** A snapshot can carry any number of disks, with mount points of any
   length, and each disk becomes a metric row and a `live_metrics` entry. Push
   (`push::metric_points`) and poll (`collector::store_metrics`) each map a snapshot to metric
   points in their own code. That duplication is already an open architectural question.
4. **Frames arrive back to back.** Nothing limits how fast a connection sends frames.
5. **Live state never shrinks.** `live_metrics` entries are removed neither on disconnect nor
   on delete. A deleted system's connection keeps refreshing its entry (its metric inserts
   fail, since the bundled SQLite enforces foreign keys, and the error is discarded).
6. **SSE copies everything per subscriber.** Every 5 s, each `/api/stream/summary` subscriber:
   - clones the whole `live_metrics` map;
   - builds a `serde_json::Value` tree from it;
   - serialises that to a string;
   - copies the string into its event.

   Subscribers are unauthenticated and unbounded.

Measured with the hub's schema, statements and SQLite defaults, on this project's development
container (overlay filesystem), with the script in the Appendix:

| Frame | Today (autocommit) | One transaction |
|---|---|---|
| 5 disks (10 points) | 15.4 ms | 1.7 ms |
| 1024 disks | 1,206 ms | 23.9 ms |

The status update, and a `list_systems` over 1000 rows, come on top of each "today" figure.

## Proposed design

### 1. One snapshot rule, for push and poll

A Fleet History domain value and one pure function replace the two mappings:

```rust
/// What one snapshot contributes to a system's history, whichever path it arrived by.
pub struct SnapshotReading { cpu: f32, memory: f32, swap: f32, load1: f64, load5: f64, disks: Vec<DiskReading> }

pub struct HistoryPoints {
    pub points: Vec<(MetricName, f32)>,     // cpu, memory, swap, load1, load5, disk:<mount>
    pub disks: DiskOutcome,
}

pub enum DiskOutcome {
    Kept,
    SomeDropped { dropped: usize },          // invalid mount points dropped one by one
    AllSkipped { reported: usize },          // too many disks: none kept
}

pub fn history_points(reading: &SnapshotReading) -> HistoryPoints;
```

- **Too many disks:** a snapshot with more than `MAX_DISKS = 1024` disks keeps its scalars and
  skips all its disks. The host shows no disks rather than an arbitrary subset.
- **Invalid mount points:** a disk whose mount point is longer than
  `MAX_MOUNT_POINT_BYTES = 256` bytes, or contains a control character, is dropped on its own.
  sysinfo turns `/proc/mounts` escapes such as `\011` back into real tabs. Dropping one bad
  entry means a user who can create a FUSE mount can hide only that mount, not the host's
  disks.
- The push DTO and the poll DTO each convert into `SnapshotReading` at their edge. Both
  adapters then call `history_points`. The adapter logs the outcome at `warn`, once per
  connection for push and once per system per hub run for poll.

### 2. One transaction per snapshot

`Database::store_snapshot` holds the connection guard for one snapshot and runs it in one
transaction:

```rust
pub enum Stored { Stored, SystemGone }

pub fn store_snapshot(
    &self,
    system_id: &SystemId,           // polled systems' ids pass SystemId too (UUIDs)
    points: &HistoryPoints,
    timestamp: u64,
    status: StatusUpdate,           // online, with uptime, as update_system_status does now
    on_stored: impl FnOnce(),       // runs before the guard drops (see §4)
) -> Result<Stored, rusqlite::Error>;
```

- If the system's row is missing, it writes nothing, skips `on_stored`, and returns
  `SystemGone`.
- Otherwise it inserts every point, applies retention exactly as `insert_metric` does today,
  updates the status, and calls `on_stored`.
- Retention stays in SQL. That is an existing open question, and this method doesn't make it
  worse.

The push path's per-frame `refresh_cache()` goes. The cache is refreshed where the registry
actually changes: at registration, at the system-info fill and rename in `update_registry`,
and by the REST handlers, as today. The poller keeps refreshing before it reads.

### 3. Frame pacing (push)

A frame read less than `MIN_FRAME_SPACING = 1 s` after the last accepted frame on the same
connection is dropped. The agent pushes at most every 2 s, and a catch-up tick after an
agent-side stall is a near-duplicate. The hub measures time when it reads, so frames buffered
during a hub-side stall are read back to back and all but the first are dropped. Those
samples are already late, and dropping them is accepted. The spacing is injected in tests,
and the existing two-frame tests run with spacing 0.

### 4. Live state that follows the registry

- **The live entry is written inside the store.** `on_stored` writes the `live_metrics`
  entry while the database guard is still held. Lock order is always database, then live
  state. No code holds the live lock while it takes the database mutex: SSE and the REST
  handlers take them one after the other. A delete (row removed, then entry evicted) can
  therefore never leave an entry behind for a system it removed.
- **`SystemGone` ends the push connection**, logged at `info` with the id in `Debug` form. A
  store error is logged at `warn`, and the connection stays open.
- **Disconnects and deletes evict.** When a push connection ends, the hub removes the system's
  entry after marking the system offline. `DELETE /api/systems/:id` removes it too, for push
  and polled systems alike.

### 5. One serialisation per tick

A single background task builds the summary every 5 s, serialises it once into an `Arc<str>`,
and publishes it on a `tokio::sync::watch` channel. Each `/api/stream/summary` subscriber sends
that same string. So the per-tick cost is one clone and one serialisation, whatever the number
of subscribers. The SSE wire format doesn't change.

## Domain impact

- **Fleet History:** gains `SnapshotReading`, `history_points`, `DiskOutcome` and the disk
  rules. This closes the open question about the snapshot → metric mapping being written
  twice.
- **Ingestion** (`push.rs`, `collector.rs`): both convert their DTO into `SnapshotReading`,
  store through `store_snapshot`, and log outcomes. Push adds pacing.
- **Glossary:**
  - adds **snapshot reading**: the values one snapshot contributes to history
    (`SnapshotReading`);
  - adds **history points**: the metric points it becomes (`HistoryPoints`);
  - **metric point** is unchanged.
- **Published contracts:** none change. The push frame, the poll response and the SSE event
  keep their shapes. What the hub stores narrows only for snapshots that break the disk
  rules. Mixed-version fleets are unaffected, apart from hosts with more than 1024 disks (see
  Rollout).

## Alternatives considered

- **Keep two mappings and add the rule to push only.** The same host would show no disks when
  it pushes and every disk when it's polled, and the duplication would grow.
- **Refuse a whole snapshot with too many disks.** The host would go dark.
- **Truncate the disk list.** The host would silently show an arbitrary subset.
- **Drop all disks when one mount point is invalid.** A local FUSE mount could hide every disk.
- **Batch several frames per transaction.** It would delay live state, and one frame is
  already cheap.
- **Cap SSE subscribers** instead of sharing one serialisation. It's complementary, and it
  belongs with rate limiting.

## Security implications

- **API4:**
  - Per snapshot, SQLite work is bounded by at most 1024 disks, and one transaction costs at
    most about 24 ms.
  - Per push connection, the rate is bounded by pacing, so at most about 24 ms of mutex time
    per second.
  - Live state is bounded per system (by the frame size in RFC 0006), is evicted with the
    connection, and is serialised once per tick.

  Still unbounded:
  - the number of connections (rate limiting);
  - registration (RFC 0008);
  - a fleet's aggregate load at the default 2 s interval. At 1000 agents that's 500 frames/s
    at about 1.7 ms or more each, close to saturating the mutex. The measurement shows the
    ceiling moving from about 130 agents today to about 1000.
- **A09:** skipped and dropped disks are logged, and ids are logged in `Debug` form (RFC 0006).
- **API10:** push frames and poll responses go through the same rules.
- **A03:** `store_snapshot` uses fixed statements with bound parameters.
- Everything else is unchanged.

## Testing plan

TDD, per `CLAUDE.md`, with `red-test-adversary` and `rosette-auditor`.

- **`history_points`, table-driven:**
  - 0, 1024 and 1025 disks;
  - mount points of 256 and 257 bytes, and ones containing `\t` and `\n`;
  - a mix of valid and invalid mount points, where only the invalid ones are dropped;
  - the scalar points and their names (the existing mapping, now pinned once).
- **`store_snapshot`** (temp SQLite):
  - it writes every point and the status in one transaction;
  - it returns `SystemGone` and writes nothing (and doesn't call `on_stored`) when the row is
    missing;
  - a failure part-way leaves nothing behind.
- **Pacing**, pure: 0.999 s, 1 s, and no previous frame.
- **Real server:**
  - a push frame with 1025 disks stores its scalars and no disks;
  - two frames 10 ms apart store once, and two frames the spacing apart both store;
  - deleting a connected system ends its connection and removes its live entry;
  - any connection that ends removes the entry;
  - a push frame no longer refreshes `systems_cache` unless the registry changed.
- **Poll path:** a mock agent that reports 1025 disks has its scalars stored and no disks.
- **SSE:** two subscribers receive the same payload, and the payload is serialised once per
  tick (the publisher is a pure function of the state, tested alone).

## Impact on `docs/ARCHITECTURE.md`

- § Domain model:
  - the Fleet History row and the glossary (**snapshot reading**, **history points**);
  - the Ingestion row (both paths store through `store_snapshot`).
- § Data flow: push and poll share one snapshot rule; SSE publishes one serialised summary
  per tick.
- § Trust boundaries (Agent → Hub, both paths): the disk rules.
- § Open architectural questions:
  - Removed: the duplicated snapshot → metric mapping.
  - Rewritten: the frame-size entry, now fully bounded.
  - Added:
    - the client-chosen `timestamp` driving pruning;
    - one-off mount names never pruned;
    - aggregate fleet load against the single mutex.

## Rollout / migration notes

Hub-only. No schema change. Rolling back is safe.

- Hosts with more than 1024 disks keep their CPU, memory and load, but show no disks,
  whether they push or are polled. A `warn` names them.
- Dashboards see the same SSE events. Live entries for offline push systems disappear, so
  their cards show `—` for live values, as a system without live data does today.

## Appendix: measurement script

Run with Python 3's `sqlite3` against a file on the storage being measured:

```python
import sqlite3, time
c = sqlite3.connect('bench.db', isolation_level=None)
c.executescript("""
PRAGMA foreign_keys=ON;
CREATE TABLE systems(id TEXT PRIMARY KEY);
CREATE TABLE metrics(id INTEGER PRIMARY KEY AUTOINCREMENT, system_id TEXT NOT NULL,
  metric TEXT NOT NULL, value REAL NOT NULL, timestamp INTEGER NOT NULL,
  FOREIGN KEY (system_id) REFERENCES systems(id) ON DELETE CASCADE);
CREATE INDEX idx_metrics_system_time ON metrics(system_id, metric, timestamp);
CREATE TABLE metric_retention(system_id TEXT NOT NULL, metric TEXT NOT NULL,
  retention_secs INTEGER NOT NULL DEFAULT 86400, PRIMARY KEY (system_id, metric),
  FOREIGN KEY (system_id) REFERENCES systems(id) ON DELETE CASCADE);
INSERT INTO systems VALUES ('s');
""")
def frame(ts, n, tx):
    names = ['cpu','memory','swap','load1','load5'] + \
        [f'disk:/var/lib/docker/overlay2/{i:064x}/merged' for i in range(n)]
    if tx: c.execute('BEGIN')
    for m in names:
        c.execute("INSERT INTO metrics (system_id, metric, value, timestamp) VALUES (?,?,?,?)", ('s', m, 1.0, ts))
        r = c.execute("SELECT retention_secs FROM metric_retention WHERE system_id=? AND metric=?", ('s', m)).fetchone()
        c.execute("DELETE FROM metrics WHERE system_id=? AND metric=? AND timestamp < ?", ('s', m, ts - (r[0] if r else 86400)))
    if tx: c.execute('COMMIT')
for tx in (False, True):
    for n in (5, 1024):
        reps = 3 if n == 1024 and not tx else 20
        t = time.perf_counter()
        for i in range(reps): frame(1000 + i * 2, n, tx)
        print(tx, n, (time.perf_counter() - t) / reps * 1000, "ms/frame")
```

The implementation re-measures end to end, with the status update, and records the figure here
before this RFC is marked `Implemented`.

## Review status

The first `rfc-adversary` pass left these findings. They must be addressed before this RFC can
be `Accepted`. It stays `Draft` while RFC 0006 is implemented first.

**CONFIRMED:**
- **The cost figures aren't an upper bound.** rusqlite re-prepares statements on every
  `execute`, and the appendix inserts into an empty table. Without a statement cache and with
  a table filled to the retention window, a 1024-disk snapshot costs 38–51 ms. So 1000 agents
  at 2 s still saturate the mutex. Fix: `prepare_cached` for the per-point statements and the
  status update, and bounds re-measured against a full table.
- **One non-finite value would discard the whole snapshot.** SQLite stores NaN as NULL, which
  breaks `NOT NULL`, and a timestamp above `i64::MAX` fails the same way. Fix:
  `history_points` drops non-finite values point by point and counts them.
- **Axum still copies SSE data once per subscriber** (`Event::data`). Fix: build the complete
  `event: summary` frame once per tick as `Bytes` and serve refcounted clones, or restate the
  claim.
- **The live entry's disks and bound are unstated for the poll path.** Fix: build the live
  entry from the kept disks on both paths, bound it by `MAX_DISKS × MAX_MOUNT_POINT_BYTES`,
  and bound the warn-once set.
- **The new database work runs on the async runtime.** This covers the poll store, the
  publisher's reads, and decoding before pacing. Fix: `spawn_blocking` for both, and a pacing
  check before `rmp_serde` decodes.
- **RFC 0006 can't be helped by this disk rule** (oversize frames never decode). RFC 0006 now
  says so, and this RFC's Rollout must name that frame ceiling.
- **Several planned tests pass without the behaviour.** Fix:
  - a barrier test showing `on_stored` runs before the guard drops;
  - evicting in the same blocking unit as `mark_offline`;
  - a serialisation counter;
  - rows for NaN/±inf, an empty mount point, and 1025 reported disks with ≤1024 valid.
- **The `refresh_cache` rule contradicts itself** (status changes every frame). Fix: make the
  systems cache private to the poller, and remove every refresh outside `start_collectors`.
- **Domain impact leaves out Fleet Registry,** since `store_snapshot` writes status. "History
  points" duplicates **metric point**, and the fate of `MetricSnapshot` is unstated. Fix: name
  Fleet Registry and rename the term. Remove `MetricSnapshot`, and decide whether an absent
  scalar skips its point.
- **Inventory gaps:**
  - the README (push protocol, the DELETE and SSE rows);
  - ARCHITECTURE § Storage, the Testing sync point, and three open questions;
  - the dependencies on RFC 0006's `PushConfig` and RFC 0008's generic `on_blocking_pool`;
  - Security implications written out category by category.

- **`push/mod.rs` should split before this RFC grows it.** `rosette-auditor`'s review of
  RFC 0006 put its non-test code at about 373 lines. The split belongs at `on_blocking_pool`,
  which is already the async/sync seam: the connection protocol stays in `mod.rs`, and
  everything that runs as `FnOnce(&AppState, &SystemId)` moves to `push/ingest.rs`. That
  means the DTOs, registration, frame ingestion and offline marking, which is the
  anti-corruption layer. It was deferred from RFC 0006 because this RFC rewrites exactly that
  part.

**PLAUSIBLE:**
- **SSE first-event timing.** Use `WatchStream::new` semantics and publish before serving.
- **Skipping all disks above 1024 hides `/`.** Consider dropping invalid entries, then keeping
  the first 1024 in reported order.
- **`StatusUpdate` "with uptime" doesn't fit the poll path.** Carry each adapter's `last_seen`
  unchanged.
- **Eviction triggered by a self-asserted id (A01).** Evict only the entry this connection
  wrote, using RFC 0008's connection generation.
