# RFC 0010: Hub Store on redb — Tiered Time Series and the Hub's Catalog in One Embedded Database

- Status: Draft
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-26 (rewritten on redb the same day, after three `rfc-adversary` passes on a
  custom engine; see Review)
- Affects: `system-hub`, and a new crate, `hub-store`, inside it
- Depends on: RFC 0009 (Accepted, ships first on SQLite; this RFC takes over its `app:*`
  series, its interim prune and its guarded store)
- **Prerequisite, shipped as a change of its own before this release (owner's decision):** the
  hub serves its dashboard from `HUB_STATIC_DIR`, a path baked into the image outside the data
  volume (`fix/static-outside-volume`, commit `8a3a153`). `HUB_DATA_DIR` sits in the `hub-data`
  volume, and the new dashboard (RFC 0012) must reach Compose users.
- **The release that replaces SQLite:** RFCs **0008, 0010, 0011 and 0012 ship together**, in one
  release. 0010 and 0011 land as one implementation step (the engine's generations, tombstones
  and catalog are 0011's). **RFC 0013 (the SQLite import) is Rejected: there is no migration**
  (owner's decision). A hub on this release starts with an empty store; see Rollout.
- Supersedes, once implemented: RFC 0007 §2 (one transaction per snapshot: the group commit
  batches every point anyway). **0007 §2's other rule is carried forward** explicitly: the
  per-frame `refresh_cache()` goes (§10). The `metric_retention` table goes with SQLite.

## Motivation

The hub keeps every metric point as a SQLite row, behind one `std::sync::Mutex<Connection>`.

1. **Storage cost.** A point costs one `metrics` row plus one `idx_metrics_system_time` entry,
   about 40–60 bytes, for a 4-byte value and an 8-byte timestamp. The owner's scale target is
   10,000 systems and 50,000 Spring Boot applications:
   - about 80k host series (≈ 8 per system) plus 550k application series (11 per application,
     RFC 0009), roughly 630k series;
   - snapshots every 2 s and scrape rounds every 15 s make about 6.6 billion points a day;
   - that is about 330 GB a day in SQLite.
2. **No long history.** Retention is one window per metric (default 24 h, and nothing writes
   the table). A year is out of reach, and a long-range chart reads hundreds of thousands of
   rows to draw 300 pixels.
3. **Write path.** Each point is an `INSERT`, a `SELECT` and a `DELETE`, each its own commit
   (RFC 0007 measured ~1.5 ms per point), under the mutex that async handlers also take on
   runtime workers.
4. **Pruning is incidental.** Rows are pruned only when a new point of the same series
   arrives, so series that stop are never pruned.

The owner asked for a store built for the hub alone: minimal storage even with thousands of
systems and applications, and retention set in the hub, globally and per system.

**Why redb, and not our own engine.** Three drafts of this RFC built a write-ahead log,
checkpoints, head files, block files and a recovery order of their own. Each `rfc-adversary`
pass found new ways for them to lose or duplicate data, or to wipe history after a clock
fault (Review). Those are the problems an embedded database has already solved and tested.
The owner chose to keep the store *dedicated to the hub* (its codec, tiers, rollups, retention
and catalog are ours) and to put its bytes in **redb**. Atomic, crash-safe transactions then
come from redb, and this RFC only has to state what goes into each one.

**Decisions carried by this RFC:**

| Question | Decision | By |
|---|---|---|
| Where it runs | embedded in the hub process, as a library crate behind a narrow API; no network protocol | owner |
| Engine | **redb** holds every byte; the hub's codec, tiers, rollups, retention and catalog sit on top | owner |
| Migration | **none**: a new hub starts with an empty store; the old `system-hub.db` is left untouched for the old binary (RFC 0013 Rejected) | owner |
| What it stores (with 0011) | everything: time series, the series table, the registry, alert records, retention overrides; SQLite removed | owner |
| History | tiered downsampling: raw points, 1-minute rollups, 1-hour rollups | owner |
| Sequencing | RFC 0009 ships first on SQLite | owner |
| Precision | a declared resolution per metric kind (values stored as scaled integers) | owner |
| Default tiers | raw 24 h, 1-minute 14 days, 1-hour 400 days | owner |
| Storage cap | on by default: 80% of the data volume's size, re-read at every retention pass; `HUB_STORAGE_LIMIT=none` disables it. *Behaviour change.* | owner |
| Container mounts | dropped by the hub's snapshot → points rule. *Behaviour change.* | owner |
| The static-files fix | ships first, as its own change | owner |
| RFC 0008 | joins this release | owner |
| Which container mounts | mounts **strictly under** a runtime's storage, and per-pod kubelet mounts; the runtime's own disk and CSI `globalmount`s are kept | author |
| Commit protocol | every durable commit uses redb's **2-phase commit with quick-repair**; points are durable within **1 s** (the group commit) | author |
| Clock | hub time never runs backwards (`max(system clock, last issued)`, persisted in the same transaction as the points it stamped); retention acts on a **guarded retention clock** that advances at most twice as fast as real time and never past the system clock (§2) | author |
| Panics | the default `unwind`; a panic on the store's writer thread fails the store, and the hub exits non-zero so the next start re-derives memory from redb (§6) | author |
| Escape hatches | one-shot: each echoes the value it acts on (`HUB_CLOCK_REWIND`, 0011's key reset) | author |

The retention *API* and who may call it are 0012's. Sealed tokens, systems and alert records
are 0011's.

## Proposed design

### 1. Shape: a crate on redb, in a hub workspace

- `system-hub/` becomes a Cargo workspace root with one member, `system-hub/hub-store/`. One
  `Cargo.lock`, so the versions `hub-store` is tested with are the versions the hub ships. The
  agent and the hub stay two independent top-level crates, as `CLAUDE.md` says.
  - The Docker build context stays `./system-hub`; `system-hub/Dockerfile` gains
    `COPY hub-store ./hub-store`.
  - The gate in `system-hub/` becomes `cargo fmt --all` and `cargo clippy|test|build
    --workspace`. `CLAUDE.md`'s toolchain section and CI's hub job say `--workspace`.
- `hub-store` depends on **redb** and nothing async. It has zero `unsafe`.
- The hub adapter, `system-hub/src/storage/`, is the only caller. It never holds a store call
  on a runtime worker: calls run on the blocking pool, and every write goes to the store's
  **writer thread** (§6).

**redb facts this design relies on** (redb **4.3.0**, the latest on crates.io when this was
written; paths are inside the crate):

| Fact | Where |
|---|---|
| Pure Rust, no memory mapping: pages are read with `read_exact_at` and written with `write_all_at`, and durability is `File::sync_data` | `src/tree_store/page_store/file_backend/optimized.rs:316-361` |
| Zero runtime dependencies; MIT or Apache-2.0 (compatible with the repo's AGPL-3.0); `rust-version = "1.90"` (the repo tracks 1.96) | `Cargo.toml` |
| Copy-on-write B+trees with XXH3-128 checksums in a Merkle tree; ACID transactions; MVCC readers that never block the single writer | `docs/design.md` (§ Commit strategies, line 316; § MVCC, line 384) |
| `Durability::Immediate` (durable when `commit` returns) and `Durability::None` (made durable by the next `Immediate` commit; after a crash the database is consistent at the last durable or non-durable commit) | `src/transactions.rs:378-385`; `docs/design.md:323-335` |
| **2-phase commit** (`set_two_phase_commit`): two `fsync`s per commit, recommended "when handling malicious data", because the default 1-phase commit relies on a non-cryptographic checksum an attacker who controls the data could in theory collide | `src/transactions.rs:1577`; `docs/design.md:353-383` |
| **Quick-repair** (`set_quick_repair`): each commit saves the allocator state, so recovery after a crash is "almost instant" instead of a walk of the whole database; it implies 2-phase commit | `src/transactions.rs:1581-1603` |
| A dropped write transaction aborts; dropped while **panicking**, it skips the abort, leaks its pages and marks the database as needing repair — "the leak must not outlive the process" | `src/transactions.rs:2744-2763` |
| A second process can't open the file: `DatabaseError::DatabaseAlreadyOpen` (file lock) | `src/error.rs:292-294`; `file_backend/optimized.rs` |
| Freed pages are reused, but the file only shrinks through `Database::compact`, which needs `&mut Database` with no live transaction | `src/db.rs:1124-1127` |
| 4 KiB pages; a value up to 3 GiB; a value larger than about a third of a page takes a page of its own | `page_store/header.rs:80`; `page_store/base.rs:17`; `btree_base.rs:1051-1059` |
| The page cache defaults to 1 GiB (`Builder::set_cache_size`); a database can be created from an already opened `File` (`Builder::create_file`) | `src/db.rs:2108-2118, 2242` |

**Measured**, with a throwaway crate on this project's development machine (WSL2 ext4, 16
cores), all with 2-phase commit unless stated:

| Measurement | Result |
|---|---|
| batched commits of 1,000 × 300 B values | ≈ 69 commits/s, ≈ 68k values/s (1-phase: ≈ 77/s) |
| batched commits of 10,000 × 300 B | ≈ 138k values/s |
| non-durable commits of 1,000 × 300 B | ≈ 310 commits/s |
| one commit per second of a 1 MB value (the points log, §6) | ≈ 5 ms per commit |
| rewriting 630,000 × 150 B values in one transaction (2PC + quick-repair) | 0.56–0.70 s |
| space for 1.2M values, key order **span-first** (§5) | **1.19×** the payload at 160 B and 700 B values; 1.30× at 3.2 KB (one value per page) |
| the same, key order **series-first** | 1.8–2.0× (random inserts leave half-full leaves) |
| open, clean | ≈ 2 ms (2.8 GB file) |
| open after `SIGKILL` mid-write, every commit 2PC | 3–7 ms, and the first write after it ≈ 1–4 ms, in six runs, with or without quick-repair |
| open after `SIGKILL`, the first crash after a history of 1-phase commits | 3.6 s (a full repair walk of 2.8 GB) |
| quick-repair's cost at this commit size | within noise (513 vs 518–532 commits in 5 s) |
| deleting 2.1M entries in one transaction | 6.0 s (≈ 3 µs per entry) |
| `compact()` after that delete | 19.5 s, 6.8 GB → 1.5 GB |
| file length while open vs after a clean close | a file grows in regions (8.6 GB while open for 5 GB allocated) and is trimmed at close (5.2 GB) |

The performance test (Testing plan) repeats the ones that matter at the scale target.

### 2. Series, the series table, and hub time

**Series.** A series is `(system key, generation, metric name)`:
- the **system key** is the system id as bytes, at most 255;
- the **generation** is 0011's registry generation: a system deleted and registered again gets
  a new generation, so its old series never mix with the new ones;
- the **metric name** is 1 to 261 bytes of UTF-8 with no control characters (`disk:` plus the
  256-byte mount points RFC 0007 allows, non-ASCII included). A name breaking that is refused
  at the hub edge and in the store (`Rejected::InvalidName`, counted), so no mount point an
  agent sends can reach the store unchecked.

**The series table** is two redb tables in the one database file (§5): `series` (id → record)
and `series_key` (key → id). A new series gets the next `SeriesId(u32)` from `meta/id_counter`,
**in the same transaction** as the first points that use it, so a committed point never refers
to an unknown id, and the counter can't fall behind a committed id. Ids are never reused; on
exhaustion (2³²) new series are refused and logged.

**Series caps** (§8), all counted by the store:
- **active** series (a point within the global raw retention): `HUB_MAX_SERIES` in total and
  `HUB_MAX_SERIES_PER_SYSTEM` per system. Only active series live in memory (§7);
- **interned** series (rows in `series`): at most **ten times** the per-system active cap per
  system. A per-system bound means one system rotating metric names can't starve the others,
  however long its quiet series stay interned (up to the hour retention).

A point for a series that isn't interned, or is interned but not active (a **reactivation**),
counts as new against the active caps. Refusals are `Rejected::SeriesCapReached`.

**Hub time** is the one clock every point is stamped with, in whole seconds. The agent's
timestamp is decoded but not stored, so a skewed or hostile agent clock can't reorder, backdate
or prune anything. The store's **writer thread** (§6) stamps every point, so there is one total
order and no cross-thread clock question:

```rust
/// Never runs backwards. `last_issued` is persisted in the same redb transaction as the
/// points it stamped, so a committed point is never later than the committed clock.
pub struct HubClock { last_issued: u64 }
impl HubClock {
    pub fn now(&mut self, system_now: u64) -> u64;   // max(system_now, last_issued)
}
```

- **Forward**: hub time follows the system clock at once. A forward step can't reorder
  anything, and there is no lag after a host or VM suspend.
- **Backward**: hub time holds at `last_issued` until the system clock passes it. While it
  holds, a series that already has a point at that second refuses the next one
  (`NotAfterLast`, counted). An NTP correction of seconds costs a few points; that is the
  stated price of a clock that never runs backwards. A hold longer than 300 s is logged at
  `error` once per hour with the remaining gap, and shown in `/api/storage`.
- **A far-future fault that was later corrected** holds hub time until real time catches up,
  which could be years. The operator's way out is **`HUB_CLOCK_REWIND=<last issued>`** (§8),
  one-shot: it acts only when its value equals the `last_issued` the store holds (shown in
  `/api/storage` and in the hourly `error`). At open it then, in one transaction: deletes
  every chunk whose span starts after the system clock (the misdated future), discards open
  chunks that start after it, resets `last_issued` and every series' last timestamp to the
  system clock, and logs the counts at `warn`. Misdated points inside the current span stay,
  and age out with retention. A value that doesn't match is logged at `error` and ignored.

**The guarded retention clock.** Retention never acts on hub time directly:

```rust
/// Persisted in `meta/retention_clock` by each retention pass.
pub fn retention_now(previous: u64, hub_now: u64, system_now: u64, elapsed: Duration) -> u64 {
    // At most twice as fast as the monotonic clock says time passed, plus one pass of slack,
    // and never past either clock.
    hub_now.min(system_now).min(previous + 2 * elapsed.as_secs() + PASS_INTERVAL_SECS)
}
```

- `elapsed` is the `Instant` time since the previous pass. At open there is no `Instant`
  continuity, so the first pass passes `elapsed = 0`: one pass of slack.
- **A forward clock fault** (the system clock jumps a year ahead): hub time follows at once, but
  the retention clock advances at most twice as fast as real time. If the fault is corrected ten
  minutes later, retention has run at most twenty minutes ahead of real time, and **nothing
  younger than real now minus retention minus that margin is deleted**.
- **After the correction**, the system clock is back, and `min(…, system_now)` stops the
  retention clock there, even while hub time holds in the future.
- **A hub switched off for two weeks** catches up at twice real time: the extra two weeks of
  history are deleted over the next two weeks, and the storage cap still bounds disk use
  meanwhile.
- This one pure function replaces the previous drafts' pending steps, confirmation, faults,
  `FaultId`, suspension and resume (Review). There is nothing to resume, so RFC 0012 has no
  resume route.

**`NotAfterLast`.** The store refuses a point whose timestamp isn't greater than its series'
last one, and counts it by cause:
- a held clock (above);
- two ingestion paths writing one series in the same second: the first point is kept. RFC 0012
  reserves push ids for push systems, which closes the path by which a push-token holder could
  use this to displace a polled system;
- one push connection delivering two queued snapshots in the same second (the agent's tick uses
  tokio's default `Burst`, so a stall's queued frames arrive back to back). Expected, and the
  README says so beside the counter.

### 3. Values: declared resolution and a domain range

Unchanged in substance from the previous draft. Each metric kind declares a **value scale**,
a **domain range** in its natural unit and an **encoding**. A value is stored as
`round(v / scale)`: exact down to the scale, lossy only below it.

| Kind | Scale | Domain (natural unit) | Encoding | Metrics |
|---|---|---|---|---|
| `Percent` | 0.01 | 0 ..= 1,000 % | delta | `cpu`, `memory`, `swap`, `disk:*`, `app:*:cpu_percent`, `app:*:gc_pause_percent` |
| `Load` | 0.01 | 0 ..= 100,000 | delta | `load1`, `load5` |
| `Count` | 1 | 0 ..= 2⁴⁰ | delta | `app:*:live_threads`, `app:*:db_connections_active`, `app:*:up` |
| `Monotonic` | 1 | 0 ..= 2⁴⁰ | delta-of-delta | `app:*:uptime_seconds` |
| `Bytes` | 1024 | 0 ..= 2⁶⁰ bytes | delta | `app:*:heap_used_bytes`, `app:*:heap_max_bytes` |
| `Rate` | 0.001 | 0 ..= 10⁹ /s | delta | `app:*:http_requests_per_second`, `app:*:http_server_errors_per_second` |
| `Millis` | 0.01 | 0 ..= 10⁹ ms | delta | `app:*:http_mean_latency_ms` |

- The mapping from metric name to kind is a hub domain function (Fleet History), exhaustive over
  the metric-name enum.
- **The same domain function drops container mounts** (owner's decision; the rule is the
  author's). A disk is dropped, with no point and no live-metrics entry, when its mount point:
  - has at least one component after `/var/lib/docker/`, `/var/lib/containers/`,
    `/var/lib/rancher/`, `/var/lib/k0s/`, `/var/lib/lxd/`, `/var/lib/incus/`,
    `/var/snap/docker/`, `/var/snap/microk8s/` or `/var/snap/lxd/`;
  - contains `/.local/share/containers/storage/` (rootless Podman);
  - is under `/var/lib/kubelet/pods/` (per-pod mounts, including PV mounts and
    `volume-subpaths`: the pod's uid changes on every reschedule and every Job run, so they
    churn series without end).

  **Kept:** the prefixes themselves (a dedicated disk at `/var/lib/docker` is the disk most
  likely to fill), and `/var/lib/kubelet/plugins/…/globalmount`, where each CSI volume is
  mounted once per node under a stable path. sysinfo already skips `/run/*` except
  `/run/media`. A Docker `data-root` elsewhere isn't recognised (Open questions).
- A value outside its kind's domain, or not finite, is refused at the hub edge
  (`Rejected::OutOfDomain`) and again in the store.
- Rollup accumulators sum in `i128`; with these domains they can't overflow.
- A series' kind is fixed at its first point. Changing a metric's kind later creates a new
  series, and is an RFC-level change.

### 4. Encoding

The codec is this crate's own, versioned (a leading format byte in every chunk) and
property-tested.

**Raw chunk** (one series, at most 240 points or 1 KiB encoded, sealed early at a span
boundary):

```text
header : point count (varint) · first ts (varint) · first value (zigzag varint)
ts     : delta-of-delta:  0 → 0 | 10 + 3 bits (−4..3) | 110 + 7 bits | 1110 + 12 bits | 1111 + 32 bits
value  : per the kind's encoding, a delta (or a delta of deltas), zigzagged:
         0 → 0 | 10 + 6 bits | 110 + 13 bits | 1110 + 20 bits | 11110 + 32 bits | 11111 + 64 bits
```

**Rollup chunk** (one series, one tier: at most 60 buckets for `Minute`, 24 for `Hour`):

```text
bucket : bucket-index delta-of-delta (1 bit when buckets are consecutive)
avg    : delta of the previous bucket's avg, zigzag, bit-packed as above
min    : avg − min, bit-packed unsigned        max : max − avg, bit-packed unsigned
count  : points in the bucket, varint
```

No CRC of our own: redb checksums every page (§1).

- The `10 + 3` timestamp step exists because hub-time stamping makes gaps jitter by a second or
  two. The `11110 + 32` value step keeps a large delta (a 512 MiB heap swing) at 37 bits.
- **Why these chunk sizes.** A redb entry costs about 30 bytes, and a value over about a third of
  a page takes a whole page (§1's measurement). Chunks of up to 1 KiB keep the space overhead
  at about 1.2×, while spreading each entry's cost over many points (about 0.2 B per raw point,
  0.75 B per minute bucket, 1.9 B per hour bucket).

**Budgets per kind**, asserted by the size-budget test, from deterministic generators that
model timestamps as well as values (host series every 2 s plus U(0, 0.3) s of jitter;
application series every 15 s plus a round of U(0.05, 1.0) s):

| Kind (value generator) | Raw, bytes per point | Rollup, bytes per bucket |
|---|---|---|
| `Percent`, noisy (CPU: random walk, σ = 3 points per step) | ≤ 2.8 | ≤ 7.5 |
| `Percent`, slow (memory, disk: σ = 0.05, occasional steps) | ≤ 0.9 | ≤ 3 |
| `Load` | ≤ 1.6 | ≤ 5 |
| `Bytes` (heap: sawtooth, GC drops of 10–60%) | ≤ 3.9 | ≤ 10.5 |
| `Rate`, `Millis` (log-normal around a level) | ≤ 3.9 | ≤ 10.5 |
| `Count` | ≤ 1.2 | ≤ 3 |
| `Monotonic` (uptime; deltas jitter with the timestamps) | ≤ 1.5 | ≤ 3 |
| **Weighted by the scale-target mix** (host: 8 series at 2 s; application: 11 at 15 s) | **≤ 1.8** | **≤ 6.5** |

These are *encoded* sizes. On disk each is multiplied by redb's measured overhead (§1: 1.19×
at these value sizes), which the footprint (§5) includes. The first implementation step
measures every number from the generators and records it here and in `docs/ARCHITECTURE.md`;
the test then asserts the measured value plus 10%.

### 5. Tables, tiers and retention

**One redb file**, `HUB_DATA_DIR/hub.redb`, holds everything:

| Table | Key → value | Written by |
|---|---|---|
| `meta` | `&str` → bytes: `format`, `id_counter`, `clock` (`last_issued`), `retention_clock`, `purge/cursor`; hub-owned keys under `hub/` (0008's counter, 0011's key id and generation counter) | every group commit (clock), passes |
| `series` | `SeriesId` → record (version, system key, generation, metric name, kind, last span per tier) | first point of a series; GC |
| `series_key` | len-prefixed system key ‖ generation ‖ metric name → `SeriesId` | first point; GC |
| `points_log` | batch sequence `u64` → the points of one group commit (series id, timestamp, scaled value) | every group commit; truncated as tails are flushed |
| `tails` | `(tier u8, SeriesId)` → the series' open chunk and accumulators, and its last timestamp | the rotating tail flush (§6) |
| `chunks` | `(tier u8, span start u64, SeriesId, seq u16)` → a sealed chunk | when a chunk seals |
| 0011's catalog tables | systems, alert records and their indexes, tombstones | catalog transactions |
| `retention` | len-prefixed system key → override and pending shortening (store-owned: the store enforces it) | 0012's routes, through catalog transactions |

**Key order is span-first**, so a group commit's sealed chunks land together at the right end of
each span and fill leaves densely (1.19× measured, against 1.8–2× for series-first), and
retention deletes whole contiguous key ranges. A query of one series reads, per span in its
range, one short range `(tier, span, id, ..)`.

| Tier | Content | Span | Default retention |
|---|---|---|---|
| `Raw` | every accepted point | 1 h | 24 h |
| `Minute` | 1-minute rollups | 1 day | 14 days |
| `Hour` | 1-hour rollups | 30 days | 400 days |

A chunk never crosses a span: a point in a new span seals the open chunk first.

**Footprint at the scale target with the defaults** (ceilings above × 1.19; step 1 replaces them
with measured values). Steady state holds retention plus one open span per tier:

| Tier | Per day | Kept | On disk |
|---|---|---|---|
| Raw (6.6 B points × 1.8 B × 1.19, plus 0.2 B entry cost) | ≈ 15.4 GB | 24 h + 1 h | ≈ 16 GB |
| Minute (630k series × 1,440 × (6.5 B × 1.19 + 0.75 B)) | ≈ 7.7 GB | 14 d + 1 d | ≈ 116 GB |
| Hour (630k × 24 × (6.5 B × 1.19 + 1.9 B)) | ≈ 0.15 GB | 400 d + 30 d | ≈ 64 GB |
| series table, tails, points log, catalog | | | ≈ 2 GB |
| **Total** | **≈ 23 GB of new data a day** | | **≈ 200 GB**, against ≈ 330 GB for *one day* of raw rows in SQLite |

At 100 systems (and their applications) the same policy needs about 2 GB. The honest reading:
the minute tier dominates, and an operator short of disk shortens `minute=` first.

- **Rollups are computed at ingest.** Each series has two accumulators (current minute and hour:
  `i128` sum, count, min, max). A bucket closes when the first point of a later bucket arrives.
  A **sweep** every 60 s on the writer thread closes the buckets of quiet series once hub time
  is more than one bucket length past their end. Replay (§6) doesn't need a record of sweeps:
  a bucket closed later holds the same points, since no point can arrive for a past bucket.
- **Retention policy** is one period per tier, each bounded:

| Tier | Minimum | Maximum |
|---|---|---|
| Raw | 1 h | 30 d |
| Minute | 1 d | 400 d |
| Hour | 7 d | 10 y |

  The global policy comes from `HUB_RETENTION` (§8); a system may carry an override, set through
  RFC 0012's API and stored in the store-owned `retention` table.

```rust
pub struct Policies { global: RetentionPolicy, overrides: BTreeMap<SystemKey, Override> }
pub struct Override { policy: RetentionPolicy, pending: Vec<PendingShortening> }
pub struct PendingShortening { tier: Tier, current: Duration, next: Duration, delay: Duration }
```

- **A pending shortening** (RFC 0012) is written in the same catalog transaction as its
  override. The store arms its delay on the monotonic clock, and **re-arms it with the full
  delay at every open**, so a restart can only lengthen the window. When the delay ends, the
  writer thread applies `next` in a transaction of its own.
- **The retention pass** runs every **10 minutes** of `Instant` time, on the writer thread, as
  bounded transactions of at most 50,000 deleted entries each (about 0.15 s at the measured
  3 µs per entry), so ingestion commits keep flowing between them. It:
  1. computes `retention_now` (§2) and persists it;
  2. re-reads the volume size for the storage cap;
  3. for each tier, deletes the chunks of spans that ended before `retention_now` minus the
     tier's global retention, skipping the series of systems with a longer override, then
     deletes, span by span, the chunks of systems with a shorter override. Both are pure
     functions over (span, series → system, `Policies`) that the pass executes;
  4. purges deleted generations (below);
  5. runs series GC: a series with no chunk left in any tier (its `last span per tier` all
     expired) and no tail is removed from `series` and `series_key`.
- **Erasure of deleted systems.** A deleted system's data is unreadable at once (0011). Its
  series ids are listed in the tombstone, and the pass deletes their chunks span by span
  (one range per span per series: at most 1,500 series × 40 spans), their tails and their log
  entries, recording its progress in `meta/purge/cursor` so a restart resumes it. So a deleted
  system's data is **removed from the store within one retention pass**, 10 minutes. The
  bytes of freed pages may stay in the file until the pages are reused or the file is
  compacted (below); the README says so, and 0011 states the promise in those words.
- **Compaction.** redb reuses freed pages, so the file stays near its peak size; it only
  shrinks with `compact()`, which needs the database to itself. The hub compacts **at start,
  before serving**, only when asked (`HUB_STORE_COMPACT=1`, not one-shot: it compacts only when
  reclaimable space is over 1 GiB and 25% of the file). `/api/storage` shows the reclaimable
  bytes, and the pass logs a `warn` hint once a day when they pass that threshold. Measured
  cost: about 3 s per GB of file (§1), so the README gives the expected pause.

**Storage cap.** `HUB_STORAGE_LIMIT` bounds the store's **allocated** bytes (redb's allocated
pages × page size: what a compaction would keep). Unset, it defaults to 80% of the size of the
volume holding `HUB_DATA_DIR` (`statvfs`), re-read at every pass; `none` disables it. While the
store is over the cap, each pass deletes the oldest raw span, then the oldest minute span, never
hour chunks, the open span or tails, logging each at `warn`. If the cap can't be met from those
tiers, that's logged at `error` once per hour, and ingestion goes on.

### 6. The writer thread, the group commit and durability

**One writer thread** owns every redb write transaction (redb allows one writer anyway). Callers
send it requests over a bounded channel and wait for its answer:
- `append` batches: stamped, checked against the series' state, applied to the in-memory head,
  and queued for the next group commit. The `AppendReport` (accepted, rejected with reasons)
  is answered **when the batch is applied**, not when it is durable;
- catalog transactions (0011): run in the writer's transaction, answered **after the commit
  that holds them** and after the commit hook ran (read-your-writes);
- retention, purge, GC, sweeps and the tail flush: the writer's own jobs.

A full channel makes callers wait on the blocking pool, which slows a push connection instead of
growing memory.

**The group commit.** Every `HUB_COMMIT_INTERVAL` (default **1 s**), or at once when a catalog
transaction marked `Durable` is waiting, the writer commits **one redb write transaction** with
`Durability::Immediate`, 2-phase commit and quick-repair. It holds, atomically:
- the interval's points, as one `points_log` entry;
- every chunk sealed in the interval (inserted into `chunks`, and removed from its tail);
- the next slice of the **rotating tail flush**: the tails of the next 1/900 of the active
  series, so every series' tail is written at least every 15 minutes;
- the deletion of `points_log` entries older than the oldest tail flush still needed;
- every catalog transaction applied in the interval (0011);
- `meta/clock` and `meta/id_counter`.

So **the durability window is one commit interval**: a crash of any kind, a process kill or a
power loss, loses at most the points of the last second. Catalog changes marked `Durable`
(registration, `PUT`, delete, retention, acknowledgement) are committed before their call
returns. Everything else (alert refreshes from polls, the system-info fill) rides the next
group commit.

**Recovery** at open is redb's, plus re-deriving memory:
1. redb opens the file; with quick-repair on every commit, an unclean shutdown costs
   milliseconds (§1).
2. Load `meta`, the in-memory Registry (0011), the tombstones and `Policies`.
3. Load `tails` into the head: each active series' open chunks, accumulators and last
   timestamp.
4. Replay `points_log` in sequence order, applying each point only to a series whose last
   timestamp is earlier than the point's. Points already in a tail or a sealed chunk are
   therefore skipped, and none is applied twice.
5. Run a sweep at hub now (closing buckets that are past, which gives the same buckets the live
   run would have closed).

There is no ordering between files to reconstruct: every state change a commit made is in the
one atomic redb transaction.

**Failure handling.**
- **A panic on the writer thread** leaves the in-memory head possibly ahead of what redb
  committed, and redb marks the database as needing repair (§1). The store therefore **fails
  stop**: every later call returns `StoreError::Failed`, the adapter reports it to `main`, and
  the hub logs at `error` and exits non-zero. The next start runs redb's repair and re-derives
  memory (above). Other panics (a request handler's) are unaffected: the hub keeps the default
  `unwind`. `docker-compose.yml` gains `restart: unless-stopped` on the hub, so a fail-stop
  restarts it.
- **An I/O error on commit** (`EIO`, a failed `fsync`): redb aborts the transaction. The store
  fails stop, as for a panic; retrying after a failed `fsync` can report success for lost pages.
- **`ENOSPC` or `EDQUOT` on commit**: the transaction aborts, and the writer enters **degraded
  mode**: appends are refused (`Rejected::Degraded`) and counted, queries and catalog reads
  still work, and every 60 s the writer checks free space (`statvfs`'s `f_bavail`, the space an
  unprivileged process may use: the hub runs as a non-root user). While free space is below the
  **floor** (the larger of 1 GiB and 2% of the volume), it deletes the oldest raw span, then the
  oldest minute span, in small transactions, **whatever the cap says**, logging each at `warn`.
  Deletes reuse freed pages inside the file, so they need little free disk; if even they fail,
  that is logged at `error` once a minute. Once the floor is met, it leaves degraded mode and
  logs that at `warn`. The floor check also runs before degraded mode, at every pass, so the
  store usually starts deleting before the disk is full.

**Shutdown.** `Store::close(&self)` is idempotent: it flushes every tail and commits, and
afterwards calls return `StoreError::Closed`. On SIGTERM or SIGINT the hub's `main`:
1. signals a shutdown `watch` channel: SSE streams end on it, and push connections send a Close
   frame and return **without marking their systems offline** (§10);
2. awaits `axum::serve(…).with_graceful_shutdown(…)`, raced against a **5 s drain timeout**;
3. calls `close` on the blocking pool, and exits.
`docker-compose.yml` gains `stop_grace_period: 30s`. A SIGKILL at any point is safe (the last
second is lost), just not clean.

### 7. Memory, open time and write volume

| Item | Per active series | At 630k series | At `HUB_MAX_SERIES` (1M) |
|---|---|---|---|
| open raw chunk (half full on average: host ~120 × 1.5 B, application ~120 × 2 B) | ~220 B | 140 MB | 220 MB |
| open minute and hour chunks (~30 × 6.5 B and ~12 × 6.5 B) | ~270 B | 170 MB | 270 MB |
| accumulators (`i128`), last timestamp, map and key | ~250 B | 160 MB | 250 MB |
| redb page cache (`HUB_STORE_CACHE`) | — | 256 MB | 256 MB |
| the point buffer of one commit interval (~1 MB at 76k points/s), and the writer channel | — | ~10 MB | ~10 MB |
| **Total** | | **≈ 0.75 GB** | **≈ 1 GB** |

Quiet interned series and every closed chunk live in redb, not in memory. At 100 systems the
total is about 270 MB, almost all of it the page cache, which the README says can be lowered.
The performance test measures resident memory at the scale target, and it must land within 20%
of this table, or the table is corrected.

**Open time** at the scale target: loading the tails (≈ 630k × 0.6 KB ≈ 0.4 GB: the open chunks
and accumulators of the table above) and replaying at most 15 minutes of `points_log` (≈ 70M
points), both measured by the performance test. Target:
under 30 s at the scale target, under 1 s at 100 systems.

**Write volume** at the scale target, per day, before redb's copy-on-write amplification:

| Writer | Volume |
|---|---|
| `points_log` (~12 B per point) | ~80 GB |
| rotating tail flush (630k × ~0.6 KB every 15 minutes) | ~36 GB |
| sealed chunks (raw, minute, hour) | ~23 GB |
| deletions (page rewrites of retention and purge) | ~5 GB |
| **Total** | **≈ 145 GB/day logical**, against ≈ 23 GB/day of new data kept |

Copy-on-write rewrites branch pages on each commit; the performance test measures physical
writes and records them. At small fleets the per-commit cost dominates: two `fsync`s a second
(2-phase commit), a few pages each, about 1–2 GB/day on a journalling filesystem. On SD cards
and eMMC the README advises `HUB_COMMIT_INTERVAL=10s`, accepting a 10 s durability window.

### 8. Configuration and the data directory

Parsed once in `main`, before anything opens. An invalid value refuses startup, naming the
variable, never the value:

| Variable | Meaning | Default |
|---|---|---|
| `HUB_DATA_DIR` | the hub's data directory | `./system-hub-data` (inside the image's `/app` volume) |
| `HUB_RETENTION` | global policy, e.g. `raw=24h,minute=14d,hour=400d`; omitted tiers keep their default (`events=` is 0011's) | the defaults above |
| `HUB_STORAGE_LIMIT` | cap on allocated bytes: bytes with `k`/`M`/`G`/`T` suffixes (powers of 1024), a percentage of the volume (`80%`), or `none` | `80%` |
| `HUB_MAX_SERIES` | active series in total | 1,000,000 |
| `HUB_MAX_SERIES_PER_SYSTEM` | active series per system; interned series per system are ten times this | 1,500 |
| `HUB_COMMIT_INTERVAL` | the group commit: the durability window; `100ms` to `60s` | `1s` |
| `HUB_STORE_CACHE` | redb's page cache, bytes with suffixes; at least 16 MiB | `256M` |
| `HUB_STORE_COMPACT` | `1`: compact at start if over 1 GiB and 25% reclaimable (§5) | unset |
| `HUB_CLOCK_REWIND` | the `last_issued` value `/api/storage` shows: rewinds a far-future clock at start (§2); one-shot | unset |

The admin token is 0012's, the secret key 0011's, `HUB_MAX_PUSH_SYSTEMS` 0008's, `HUB_LISTEN`
0012's, and `HUB_STATIC_DIR` the prerequisite's.

**Opening, in order**, in `main`, after configuration and before any other state:
1. the data-directory checks below;
2. create `HUB_DATA_DIR/hub.redb` if absent with mode 0600 (`OpenOptionsExt::mode`), and open
   it through `Builder::create_file`, so redb's file never takes the umask's mode;
3. redb's file lock: a second hub on the same directory gets `DatabaseAlreadyOpen` and refuses
   to start, naming the file. Everything 0011 writes beside it (the generated key) happens after
   this, so two hubs can't both generate a key.

**Data-directory safety** (A05), with no `unsafe` (`rustix`'s safe `process` and `fs` APIs for
`geteuid`, `getegid`, `getgroups` and `statvfs`), run before step 2, so no file is created in a
directory that fails:
- `HUB_DATA_DIR` is created with mode 0700 if absent;
- the hub's groups are its effective gid and its supplementary groups;
- `HUB_DATA_DIR` is refused unless it isn't a symlink, and either:
  - is owned by the hub's euid and not group- or world-writable; or
  - is owned by the hub's euid **or root**, has the setgid bit, is group-writable, and its group
    is one of the hub's groups (Kubernetes `fsGroup`, which gives a PVC's root and the
    directories the hub creates in it mode 2770 or 2775 and a supplementary gid);
  - and it is never world-writable;
- every **ancestor** must be owned by root or the hub's euid, and not group- or world-writable
  unless it has the sticky bit (OpenSSH's `StrictModes` rule), except that the **immediate
  parent** may have the `fsGroup` shape above. A Kubernetes `emptyDir` (0777, no sticky bit) is
  refused as an ancestor, and the README shows the supported volume setups;
- `.gitignore` and `system-hub/.dockerignore` gain `system-hub-data/` and `*.key`.

### 9. Store API

```rust
impl Store {
    /// Checks the directory, opens `hub.redb` (§8), re-derives memory (§6), starts the writer.
    pub fn open(data_dir: &Path, options: StoreOptions) -> Result<Store, OpenError>;

    pub fn append(&self, system: &SystemKey, generation: Generation, points: &[(MetricName, ValueKind, i64)]) -> AppendReport;
    pub fn query(&self, series: &SeriesKey, q: Query) -> Result<Series, StoreError>;
    pub fn metrics_of(&self, system: &SystemKey, generation: Generation) -> Vec<MetricName>;

    /// Runs `f` inside the writer's transaction; returns after the commit that holds it and the
    /// commit hook. `Durable` forces that commit at once.
    pub fn transact<T: Send>(&self, class: Commit, f: impl FnOnce(&mut CatalogTxn) -> Result<T, Abort> + Send) -> Result<T, StoreError>;
    /// A redb read transaction over the catalog tables (MVCC: never waits for the writer).
    pub fn read_catalog<T>(&self, f: impl FnOnce(&CatalogRead) -> T) -> Result<T, StoreError>;
    /// Called on the writer thread after each commit, in commit order, with that commit's
    /// catalog changes (0011 maintains its in-memory Registry with it).
    pub fn on_commit(&self, hook: Box<dyn Fn(&[CatalogChange]) + Send + Sync>);

    pub fn stats(&self) -> StoreStats;
    pub fn close(&self) -> Result<(), StoreError>;
}
pub enum Commit { Durable, Batched }

/// Inside `transact`: 0011's tables, plus the store-owned operations.
impl CatalogTxn {
    pub fn tombstone(&mut self, system: &SystemKey, generation: Generation);   // SystemGone from here on
    pub fn set_retention(&mut self, system: &SystemKey, change: RetentionChange) -> Result<RetentionOutcome, RetentionError>;
    // … 0011's typed-byte tables: get, range, insert, remove
}

pub struct Query { range: TimeRange, tier: Tier, order: Order, limit: Limit }
pub enum Order { Earliest, Latest }          // which end `limit` keeps; results always ascending
pub struct Limit(u16);                       // 0 ..= 10,000; 0 returns nothing, as today

pub struct AppendReport { at: u64, accepted: u16, rejected: Vec<(MetricName, Rejected)> }
pub enum Rejected { NotAfterLast, SeriesCapReached, KindMismatch, OutOfDomain, InvalidName, SystemGone, Degraded }
pub enum StoreError { Closed, Failed, Io(IoKind) }
```

- **`SystemGone`**: a `tombstone` committed in a transaction takes effect on the writer thread
  **at that commit**; a later `append` for that generation is refused there. The writer is the
  only thread that touches the head, so there is no window between the catalog and the store.
- **Queries** read committed chunks in a redb read transaction, and copy the series' unsealed
  points from the head under that series' shard lock (16 shards, taken only by the writer and
  by queries, never nested), then merge. A query never waits for a commit.
- **Lock order, in full** (with 0011 and 0010 §10): no hub lock (the Registry, `live_status`,
  `live_applications`, an admission lock) is ever held across a store call; the commit hook
  takes the Registry's write lock on the writer thread, inside no store lock; a head shard lock
  is a leaf. RFC 0009's admission lock is taken around an `append` call, and no store lock is
  held while taking it.

**`/api/storage`** is owned by this RFC. It answers `StoreStats` as JSON: allocated bytes per
tier and in total, the file size and reclaimable bytes, the cap and its source, the volume size
and free space, the degraded-mode floor; active and interned series and the caps; points per
second, the commit interval, the last commit's duration; hub time, system time, `last_issued`,
the retention clock and how far it trails hub time, any clock hold; counters per `Rejected`
reason (`NotAfterLast` split by cause), degraded mode and cap deletions. RFC 0012 adds its
refusal counters, and RFC 0008 its registry counters. It exposes no key, token or path beyond
tier names, and is an open read like the rest of the hub API.

### 10. Hub integration

`storage/` replaces `db.rs`, with 0011 in the same step.

- **`append_snapshot(system, generation, snapshot)`** replaces `store_metrics`. It maps the
  snapshot to points through the one snapshot rule (RFC 0007 §1 where implemented, else today's
  `metric_points`), including §3's container-mount and name rules. An all-`SystemGone` report
  ends the push connection.
- **`append_round(system, generation, round, decide, on_stored)`** replaces RFC 0009's
  `Database::store_round`. It clones the system's `Arc<Mutex<ApplicationAdmission>>` out of
  `live_applications`, **drops the map guard**, locks the admission, calls `decide` (the pure
  `admit`), then `append`, then `on_stored`. A report that accepted **no** point makes the
  round `NotStored(reason)`: `on_stored` isn't called, so the round isn't recorded in
  `RecentRounds`. A partly accepted round is `Stored` with its rejections counted.
- **Volatile system status stays in memory**, typed, and is written into `meta/hub/live_status`
  by the rotating flush (a slice per commit, like tails), so a restart recovers it:

  ```rust
  pub struct LiveStatus {
      generation: Generation,          // a write for another generation is ignored (0011)
      liveness: Liveness,
      last_contact: Option<u64>,       // hub time of the last successful frame or poll
      last_error: Option<String>,      // ≤ 256 bytes
      connection: Option<u64>,         // RFC 0008's connection number, push systems only
  }
  pub enum Liveness { Online, Offline { since: u64 }, Unknown }
  ```

  - `Liveness` is one enum, so "online with an offline time" can't be represented.
  - **Set only by contact and by transitions.** A successful frame or poll sets `Online` and
    `last_contact`. A failed poll, the end of the current push connection (RFC 0008 §5), and
    RFC 0008's staleness sweep set `Offline { since: now }` unless already offline. Shutdown
    touches nothing.
  - The API's `last_seen` is rendered from `last_contact` (RFC 3339), empty when there is none.
    *Behaviour change: today a push system's `last_seen` holds its agent's uptime display, and a
    failed poll updates it.*
  - At open every system has its recovered `last_contact` and `Unknown`; one not heard from
    within **120 s** becomes `Offline { since: last_contact }`, or the open time if it has none.
- **RFC 0007 §2's cache rule, carried forward.** The per-frame `refresh_cache()` goes. The
  in-memory Registry (0011) is maintained by the commit hook, so there is no cache to refresh.
- **Metric queries.** Every `limit` is **clamped** to 10,000, never refused, and `limit=0` still
  returns no points:

| Route | Behaviour |
|---|---|
| `GET /api/systems/:id/metrics?metric=&since=&until=&resolution=&limit=` | `limit` defaults to 300. Without `since`: `Order::Latest`. With `since`: `Order::Earliest` from `since`. `until` defaults to now. |
| `GET /api/systems/:id/history?limit=&since=` | CPU and memory, `Raw` tier. `since` kept, as today: without it `Order::Latest`, with it `Order::Earliest` from `since`. Ascending, as the dashboard reads today. |

  `resolution` is `raw`, `1m`, `1h` or `auto` (default). `auto` picks the finest tier that both
  covers `since` under the system's policy and fits `limit`: `Raw` if `until − since ≤ limit ×
  2 s`, else `Minute` if `≤ limit × 60 s`, else `Hour`. Without `since`, `auto` is `Raw`.
  Rollups answer `{timestamp, value, min, max}`, `value` being the average; the dashboard reads
  only `timestamp` and `value`, so it works unchanged.

## Domain impact

- **New context: Fleet Storage** (`hub-store`): series and the series table, value kinds, tiers,
  chunks and the codec, the writer thread and group commit, the points log and tails, hub time
  and the retention clock, retention enforcement and overrides, the storage cap, degraded mode,
  the data-directory checks. It knows no hub term but "system key" and "generation", and
  stores 0011's catalog values as opaque bytes.
- **Fleet History**: `storage/` replaces `db.rs`; `RetentionPolicy` parsed from `HUB_RETENTION`;
  the metric-name → kind function and the container-mount rule in the snapshot → points
  function; retention decisions as pure functions (closing "Retention policy is decided inside
  `Database::insert_metric`").
- **Fleet Registry**: volatile status becomes the typed `LiveStatus`; the rest is 0011's and
  0008's.
- **Ingestion**: `append_snapshot`, `append_round`.
- **Glossary**:
  - added: series, series table, active series, interned series, reactivation, generation, value
    kind, value scale, domain range, tier (raw, minute, hour), rollup, span, chunk, tail, points
    log, group commit, commit interval, writer thread, hub time, clock hold, clock rewind,
    retention clock, retention policy, override, pending shortening, storage cap, degraded mode
    and its floor, compaction, series cap, container mount, last contact;
  - changed: **retention** (per tier, globally and per system; no longer per metric), **metric
    point** (stamped in hub time), **system status** (the typed `LiveStatus`), **last seen**
    (the last successful contact), **snapshot** (container mounts are not metric points).
- **Published contracts**: push frames, poll responses and the handshake are untouched (RFC
  0008 adds handshake answers). Metric responses gain optional `min`/`max`; `/metrics` accepts
  `resolution` and `until`; limits are clamped; `last_seen` changes meaning; container-mount
  disks disappear.
- **Mixed-version fleet**: agents are unaffected. A hub downgrade after upgrading finds the old
  SQLite file as it was left, without anything collected since (Rollout).

## Alternatives considered

- **Our own engine** (the first three drafts): a per-shard WAL, incremental checkpoints, head and
  head-index files, immutable block files, a recovery order, a clock file with pending steps,
  faults, suspension and resume. Three `rfc-adversary` passes each found data loss, duplication
  or a history wipe in it (Review). Rejected by the owner for redb.
- **Other embedded engines.** `sled` (its own docs describe it as beta, and it keeps large
  in-memory structures), `fjall` (an LSM tree: good write amplification, but compaction runs
  in background threads we'd have to bound, and deletes are tombstones until compacted),
  `rocksdb` (C++, `unsafe` FFI, a large dependency). redb is pure Rust with no dependencies, a
  stable file format, and the smallest surface to review.
- **One redb entry per point.** ≈ 30 B of entry cost per point, twenty times the codec's size.
  Chunks of up to 1 KiB spread it.
- **Series-first keys** (`(tier, series, span)`). Measured 1.8–2× space for random inserts,
  against 1.19× span-first.
- **Keeping tails only in memory, flushing them at shutdown.** A crash would lose up to a span
  of points per series. The points log gives a 1 s window for about 80 GB/day of cheap
  sequential writes at the scale target.
- **Writing every tail at every commit.** About 630k small random updates a second at the scale
  target: most leaf pages rewritten every second. The rotating flush writes each tail every
  15 minutes instead, and the log covers the gap.
- **1-phase commit.** Slightly faster (measured ≈ 12%), but redb recommends 2-phase commit for
  data an attacker may control (agent-supplied names and values reach the file), and
  quick-repair needs it anyway.
- **`panic = "abort"` for the whole hub** (the previous draft). A handler's panic would take the
  fleet's monitoring down. Only the writer thread's panic is fatal now, which is what redb's
  repair semantics require.
- **The previous clock** (step at once, confirmation after 24 h, faults, suspension, resume,
  half-speed hold). Each piece fixed the last review's finding and opened the next one. One
  pure function bounds what retention can do on any clock.
- **Online compaction.** redb's `compact()` needs exclusive access; an online variant would
  mean closing and reopening the database under traffic. Compaction at start, on request, is
  honest about the pause.
- **A migration from SQLite** (RFC 0013). Rejected by the owner: a fresh start.
- **Keep SQLite and tune it.** Tens of bytes per point remain. The owner asked for minimal
  storage.
- **Do nothing.** RFC 0009 alone multiplies row volume on a store at its limit.

## Security implications

**OWASP Top 10 (2021)**

- **A01 Broken Access Control:** no new route writes data; retention and delete control is
  0012's. `/api/storage` is a new open read exposing sizes, counters and clock state only.
- **A02 Cryptographic Failures:** metric points aren't encrypted (owner's decision). redb's
  checksums detect corruption, not tampering. Sealed tokens are 0011's.
- **A03 Injection:** no SQL. Metric names and system keys are length-bounded, character-checked
  bytes, never interpreted.
- **A04 Insecure Design:**
  - retention acts on a clock that can run at most twice as fast as real time and never past
    the system clock, so no clock fault can wipe history;
  - the storage cap on by default, and a free-space floor that deletes before the disk fills;
  - domain and name refusal at the edge and in the store, `i128` sums;
  - atomic transactions with a stated content, and fail-stop on a writer panic or I/O error;
  - a per-system interned-series cap, so one system can't starve the rest.
- **A05 Security Misconfiguration:** directory, ancestor, ownership, group and symlink checks
  before any file is created; `hub.redb` created 0600; redb's file lock; fail-closed
  configuration; one-shot `HUB_CLOCK_REWIND`; the data directory and key ignored by git and
  Docker.
- **A06 Vulnerable Components:** `redb` (pure Rust, no dependencies) and `rustix` (`fs`,
  `process`); `proptest` as a dev-dependency. `rusqlite` and its bundled C library are removed.
  Run `cargo audit` on the hub workspace's lockfile.
- **A07:** N/A here (0012).
- **A08 Software & Data Integrity Failures:** redb's checksummed pages and atomic commits, with
  **2-phase commit** because agent-controlled data reaches the file (§1's cited attack on
  1-phase commits); our codec's format byte refuses an unknown version; decoders are total.
- **A09 Logging & Monitoring Failures:** clock holds, rewinds, retention-clock lag, cap and
  degraded deletions, series-cap refusals, fail-stops and compaction hints are logged and
  counted in `/api/storage`. No log line carries a token or a value.
- **A10 SSRF:** N/A. The store makes no network requests.

**OWASP API Security Top 10 (2023)**

- **API1:** `NotAfterLast` would let a token holder displace a polled system; RFC 0012 reserves
  push ids. Push connections sharing one self-asserted id remain the standing API1 risk.
- **API2:** N/A (0012).
- **API3:** `StoreStats` exposes counts, sizes and clock state only.
- **API4:** active and interned series caps (per system and total); `limit` clamped to 10,000;
  `auto` resolution; the bounded writer channel; the storage cap and floor.
- **API5–API7:** N/A here.
- **API8:** see A05.
- **API9:** `/metrics` gains `resolution` and `until`; `/history` keeps `since`; the clamp and
  the new `last_seen` are documented; `GET /api/storage` is new; nine variables go into the
  README.
- **API10:** ingestion paths unchanged; values checked against their kind's domain at the edge.

## Testing plan

Test-first, per `CLAUDE.md`. `hub-store` is synchronous Rust over a temporary redb file, so
nearly all of it is unit-testable. The crash tests target **our** invariants on top of redb
(redb's own atomicity is its test suite's job).

- **Codecs** (property tests with `proptest`): round-trips for 1 and 240 points, 1 and 60
  buckets; every timestamp step and value escape at its boundaries; `i64::MIN`/`MAX` steps;
  delta-of-delta for `Monotonic`; arbitrary bytes never panic nor allocate past a bound; an
  unknown format byte refused.
- **Size budget**: seeded per-kind generators with timestamp models, each asserted against its
  ceiling, and the weighted totals; plus the on-disk factor measured through redb's `stats()`
  for a day of the mix.
- **Value kinds and names**: every kind's scale, rounding and domain edges; non-finite values;
  `KindMismatch`; a 261- and a 262-byte metric name; a control character in a name
  (`InvalidName`), at the edge and in the store.
- **Container mounts**, as a table: dropped under each runtime prefix, rootless Podman, LXD and
  Incus, `/var/lib/kubelet/pods/…` (volumes and `volume-subpaths`); kept: each prefix itself,
  `/var/lib/kubelet/plugins/…/globalmount`, `/var/lib/dockerx`, a real volume under `/mnt`.
- **Rollups**: avg, min, max and count; a bucket closed by a later point; closed by the sweep
  exactly at, and one second before, one bucket length past its end; a bucket at a span
  boundary; `i128` sums at the domain's top.
- **`HubClock`** over injected times: steady; a forward step of a year (stamped at once); a
  backward step (held, never backwards, `NotAfterLast` counted by cause, the hourly `error`
  after 300 s); `last_issued` committed in the same transaction as its points (a child killed
  after a commit: the clock is at least every committed point's time).
- **`retention_now`**, as a table: steady; a +1 year system clock with 10 minutes of elapsed
  time (advances at most 20 minutes plus the slack); the system clock back after that (stops at
  the system clock while hub time holds); a first pass after a two-week downtime (one pass of
  slack, then twice real time); never past hub time or the system clock.
- **`HUB_CLOCK_REWIND`**: with the matching value, future spans deleted, open chunks starting
  in the future discarded, `last_issued` and series reset, counts logged; with any other value,
  ignored and logged; left set after it acted, ignored.
- **Durability of our invariants**, with child processes killed by `SIGKILL` at random points
  of a scripted workload (appends, seals across spans, tail rotation, a delete, a retention
  pass), then reopened:
  - every point `append` accepted before the last completed commit is returned **exactly once**
    by `query`, and nothing after the kill is half there;
  - minute and hour counts equal a recomputation from the raw points;
  - no tail and no log entry of a purged generation survives;
  - a point in both a tail and the log is applied once at replay.
- **Group commit**: points appended in one interval commit together; a `Durable` catalog
  transaction forces an immediate commit and returns after it; a `Batched` one returns after
  the next; the commit hook runs in commit order.
- **Writer failure**: a panic injected in the writer thread makes every later call
  `StoreError::Failed` and `main` exit non-zero (a child-process test); an injected I/O error on
  commit does the same.
- **Degraded mode**: an injected `ENOSPC` (a `Backend` seam over redb's `StorageBackend`, or a
  small loop-mounted filesystem in the performance suite): appends refused, deletes run until
  the floor is met, then normal; `EDQUOT` handled the same; `f_bavail` used.
- **Series table and caps**: the counter and the first point in one transaction; the per-system
  and global active caps, exactly at and one past; reactivation counted as new; the per-system
  interned cap, exactly at and one past, with other systems unaffected; GC removing a series
  once its last chunk expired.
- **Retention**: global expiry exactly at each boundary; a longer and a shorter override; a
  pending shortening before and after its delay, re-armed with the full delay after a restart;
  the pass's transactions bounded to 50,000 deletions; the purge removing a deleted system's
  chunks, tails and log entries, resuming from its cursor after a restart; tier minimums and
  maximums refused.
- **Storage cap**: the `80%` default from an injected volume size, re-read when it grows;
  deletion order; the cap unmet; `none`.
- **Compaction**: `HUB_STORE_COMPACT=1` compacts only above the threshold; the reclaimable
  figure in `/api/storage`.
- **Configuration**: parse tables for every variable of §8, including bounds, units, garbage and
  non-UTF-8.
- **Directory safety**: a symlink; group-writable without setgid (refused); setgid with a
  primary or supplementary group, owned by the hub or by root (accepted); an `emptyDir`-like
  ancestor (refused); `/tmp`-like with the sticky bit (accepted); an ancestor owned by another
  user (refused); no file created when a check fails; `hub.redb` created 0600; a second process
  refused by redb's lock.
- **Hub adapter and routes**: the metric tests of `db.rs` ported; resolution boundaries (`auto`
  without `since`; exactly at `limit × 2 s` and one second past; exactly at `limit × 60 s`;
  `since` at a tier's retention edge and one second older); `limit` 0, 10,000 and 10,001;
  `/history` with and without `since`; `append_round`'s `NotStored` and `Stored`; `LiveStatus`
  transitions, a write for another generation ignored, the 120 s rule after a restart, shutdown
  leaving statuses untouched; graceful shutdown with an open SSE client and push connection
  (a child-process test).
- **Performance** (an `#[ignore]`d test at the scale target, run and recorded in the change
  summary): ingest ≥ 100k points/s through the writer; a 24 h raw query of one series < 5 ms;
  the longest commit; open time; resident memory within 20% of §7; physical writes per day
  extrapolated from an hour; footprint per tier after a simulated day.

## Impact on `docs/ARCHITECTURE.md`

- **Components**: the hub workspace and `hub-store` on redb; `storage/` replacing `db.rs` (with
  0011); `main`'s startup order (configuration, directory checks, store open, key, serve).
- **Data flow**: the `db.rs (SQLite)` box becomes `storage/ → hub-store (writer thread) → redb`.
- **Domain model**: the Fleet Storage context; Fleet History and Fleet Registry rows; the
  glossary per Domain impact.
- **Trust boundaries**: the data directory (checks, modes, redb's lock).
- **Storage**: rewritten: the one file and its tables, key order, tiers and spans, the group
  commit and durability window, the points log and rotating tail flush, retention and the
  retention clock, the cap, the floor and compaction, memory, footprint and write volume.
- **Testing architecture**: property tests, the size budget, child-process crash tests of our
  invariants, the performance test.
- **Open questions**:
  - closes: retention inside `insert_metric`, non-pruned stale series, blocking SQLite calls on
    runtime workers, the per-frame cache refresh;
  - adds: points aren't tamper-evident; no online backup API (a copy needs the hub stopped, or
    redb's read-only open of a copy); freed pages keep old bytes until reused or compacted; a
    Docker `data-root` outside `/var/lib/docker` isn't recognised as container storage; the
    poller's fixed 30 s tick (0011).

`CLAUDE.md` changes with it: the hub is a two-member workspace, and the gate runs with
`--workspace` in `system-hub/`. `system-hub/Dockerfile` copies `hub-store/`. CI's hub job runs
the workspace gate. `docker-compose.yml` gains `restart: unless-stopped` and
`stop_grace_period: 30s` on the hub. `.gitignore` and `system-hub/.dockerignore` gain
`system-hub-data/` and `*.key`.

## Rollout / migration notes

- **Prerequisite:** the static-files fix, shipped first (header).
- **No migration (owner's decision).** The new hub starts with an empty store.
- **Upgrading** (a section of its own in the README):
  - the new hub starts with **an empty registry and no history**;
  - push agents re-register themselves on their next handshake (within seconds);
  - polled systems must be registered again, with the admin token (RFC 0012), with their URL and
    token;
  - the old `system-hub.db` is ignored. It can be kept for the old binary (a rollback runs the
    old hub on it, without anything collected since the upgrade) or deleted; while it exists
    the hub warns at every start that it holds agent tokens in plaintext (0011 §6);
  - a hub downgrade after upgrading is a rollback to the old file, as above.
- Push snapshot timestamps switch from agent time to hub time.
- **Behaviour changes:** container-runtime and per-pod mounts disappear from disk lists and
  history; the store caps itself at 80% of its volume; `last_seen` is the last successful
  contact; the hub restarts itself (Compose) after a store fail-stop.
- 0010 and 0011 are one implementation step; 0008 and 0012 follow; all four ship in one release.
- Implementation, each step through the full gate:
  1. codecs, value kinds and the size-budget test (recording measured budgets);
  2. the redb tables, the writer thread, the group commit, the points log and rotating tails,
     recovery, and the crash tests;
  3. the clock and retention clock, retention, overrides and pending shortenings, the purge,
     GC, caps, the storage cap, degraded mode, compaction, directory safety;
  4. with 0011: the hub adapter, `LiveStatus`, graceful shutdown, the routes, configuration, the
     workspace, the Dockerfile, compose and CI.
- A step that outgrows one change is split, never merged half-working.

## Review

The first three passes reviewed a custom engine. Their tables are kept as written under
**Earlier passes** at the end of this section. The **redb rewrite** maps every CONFIRMED finding
of those passes to what holds now:

| Finding (pass) | Verdict | Now |
|---|---|---|
| open chunks lived only in memory; restarts lost raw and rollup data (1) | CONFIRMED | **resolved**: tails flushed on rotation, the points log covers the gap, both in atomic commits (§6) |
| recovery duplicated sealed chunks; span close left block and head both claiming a span; `fsync` order (1, 2, 3) | CONFIRMED | **moot**: no head files, blocks or checkpoints; a seal inserts the chunk and updates the tail in one redb transaction; replay skips points not after a series' last timestamp (§6) |
| `HubClock = max(system, last)` wiped history on a forward step and stopped ingestion on a backward one (1) | CONFIRMED | **resolved differently**: hub time keeps that rule, but retention acts only on the guarded retention clock, which can't outrun real time by more than 2× nor pass the system clock (§2) |
| catalog `fsync` per frame (1) | CONFIRMED | **resolved**: volatile status in memory, flushed on rotation; the registry written on configuration changes; `Batched` catalog work rides the group commit (§6, §10) |
| deletion, `SystemGone` and lingering data (1) | CONFIRMED | **resolved**: `tombstone` in the delete transaction, effective at that commit on the writer thread; the purge removes the data within one pass (§5, §9) |
| head memory, index cache and open time (1, 2, 3) | CONFIRMED | **resolved**: memory holds active series' open chunks only; closed chunks and quiet series live in redb; budgets restated (§7) |
| one `head.chunks` shared by shards; multi-shard appends vs per-shard checkpoints (2) | CONFIRMED | **moot**: one writer thread, one transaction per commit |
| span close replay dropped rollup contributions; late-stamped points (2) | CONFIRMED | **moot**: the writer stamps and applies in one order; a sweep at open re-closes past buckets identically (§5, §6) |
| the retention guard tripped on normal steps; suspension not persisted; a fresh store started suspended (2) | CONFIRMED | **moot**: no suspension; the retention clock is persisted and needs no confirmation (§2) |
| `HubClock` lagged after a suspend (2) | CONFIRMED | **resolved**: hub time follows the system clock forward at once (§2) |
| 0010 couldn't land before 0011 (2) | CONFIRMED | **resolved**: one implementation step; one release with 0008 and 0012 (header) |
| series GC freed caps only after ~400 days; Docker churn (2, 3) | CONFIRMED | **resolved**: caps count active series; a per-system interned cap; container and per-pod mounts dropped (§2, §3) |
| graceful shutdown hung on SSE (2) | CONFIRMED | **resolved**: shutdown channel, 5 s drain, then `close` (§6) |
| `ENOSPC` torn WAL records; no operator escape (2, 3) | CONFIRMED | **moot**: redb aborts the transaction; degraded mode frees space by the floor (§6) |
| volatile status lost at restart (2) | CONFIRMED | **resolved**: `LiveStatus` flushed on rotation, 120 s rule at open (§10) |
| size budgets ignored timestamp jitter (2) | PLAUSIBLE | **resolved**: generators model timestamps (§4) |
| inventory: `/history` `since`, clamps, ignore rules (2) | CONFIRMED | **resolved** (§8, §10) |
| tests couldn't fail on ordering attacks (2) | CONFIRMED | **moot** for our own ordering (redb's transactions); crash tests target our invariants: exactly-once, rollup recomputation, purge (Testing plan) |
| a forward step not in a checkpoint vanished at restart and retention ran at the future time (3) | CONFIRMED | **moot**: the clock is committed with the points; retention can't follow a forward step faster than 2× real time (§2) |
| §2 and §6 disagreed on the series table in checkpoints; the id counter (3) | CONFIRMED | **resolved**: the series table is two redb tables; the counter and the first point share a transaction (§2) |
| the head index was neither checkpointed nor rebuilt (3) | CONFIRMED | **moot**: no head index; chunks are redb entries |
| recovery re-sealed a closed span's state (3) | CONFIRMED | **moot**: no span-close protocol; a span boundary seals chunks within the writer's transaction |
| a panic caught by the blocking pool left a torn shard (3) | CONFIRMED | **resolved**: store state changes only on the writer thread; its panic fails the store and the hub exits; redb repairs at the next start (§6) |
| a clock fault had no way out that kept history; a persistent resume switch (3) | CONFIRMED | **resolved**: no fault state; `HUB_CLOCK_REWIND` is one-shot and echoes `last_issued` (§2, §8) |
| counting only active series left the table unbounded (3) | CONFIRMED | **resolved**: a per-system interned cap (§2) |
| the default cap counted only the store and couldn't free a shared volume (3) | CONFIRMED | **resolved**: the floor deletes whatever the cap says; the volume size re-read each pass (§5, §6) |
| `last_seen` wasn't "last contact"; shutdown blanked it (3) | CONFIRMED | **resolved**: typed `LiveStatus`; shutdown touches nothing (§10) |
| the mount rule dropped the Docker disk and every PV (3) | CONFIRMED | **resolved**: strictly-under rule; per-pod mounts dropped (author's revision), `globalmount` kept (§3) |
| erasure "within 14 days" false for the hour span (3) | CONFIRMED | **resolved**: the purge removes a deleted system's data within one retention pass; freed-page bytes stated (§5) |
| rollup ceilings impossible with the encoding; footprint understated (3) | CONFIRMED | **resolved**: ceilings from the encoding, redb's measured overhead and entry cost included; footprint restated at ≈ 200 GB (§4, §5) |
| physical writes at small scale (3) | PLAUSIBLE | **resolved**: one group commit per interval, stated `fsync` rate, `HUB_COMMIT_INTERVAL` advice (§7) |
| a pending shortening only in memory until a checkpoint (3) | PLAUSIBLE | **resolved**: written with its override; re-armed at every open (§5) |
| `LOCK` had no stated directory; staging before `store/` (3) | PLAUSIBLE | **resolved**: redb's lock, taken before any other state; no staging (§8) |
| the compose volume hid `/app/static` (3) | CONFIRMED | **resolved**: the prerequisite shipped (`8a3a153`) |
| `NotAfterLast` dropped snapshots of one honest connection (3) | PLAUSIBLE | **stated**, counted by cause (§2) |
| after a resume, a later forward fault deleted everything (third pass on the split draft) | CONFIRMED | **moot**: no resume; the retention clock bound holds at all times (§2) |
| the checkpoint was the only map from series id to key (same) | CONFIRMED | **moot**: the series table is in redb, in the same file as the chunks |
| the WAL had no rollover; tombstones never dropped (same) | CONFIRMED | **moot**: `points_log` entries are deleted in the commit that makes them unneeded |
| the interned cap starved the fleet (same) | CONFIRMED | **resolved**: per-system interned cap; per-pod kubelet mounts dropped (§2, §3) |
| a segment sealed after `ENOSPC` looked like corruption (same) | CONFIRMED | **moot**: no segments of our own |
| `panic = "abort"` relied on an absent restart policy and made any panic fatal (same) | CONFIRMED | **resolved**: default `unwind`; only the writer's panic is fatal; `restart: unless-stopped` in compose (§6) |
| an N-year forward glitch misdated points for 2N years (same) | CONFIRMED | **resolved**: `HUB_CLOCK_REWIND` deletes the misdated future spans and resets the clock, one-shot (§2) |
| the weekly purge ran on `Instant` and restarted each start (same) | CONFIRMED | **resolved**: the purge runs every pass with a persisted cursor (§5) |
| two durable clock states without precedence (same) | CONFIRMED | **moot**: one `meta/clock` key, committed with the points |
| half speed didn't give each point its own second (same) | CONFIRMED | **moot**: no half speed; the hold's cost is stated (§2) |
| directory checks ran after the import and key generation (same) | CONFIRMED | **resolved**: checks run before any file is created, and the store opens before the key (§8) |
| restated budgets understated (same) | CONFIRMED | **resolved**: budgets restated for this layout; measured by the performance test (§7) |
| the key → id interning map missing from the lock order (same) | CONFIRMED | **moot**: interning happens on the writer thread in its transaction; lock order stated (§9) |
| the `fsGroup` rule refused a hub-created directory (same) | PLAUSIBLE | **resolved**: a hub-owned setgid group-writable directory with one of the hub's groups accepted (§8) |
| seam with RFC 0007's bounds (same) | CONFIRMED | **resolved**: the name rule here, `Rejected::InvalidName` (§2) |
| `LiveStatus` allowed invalid states; delete-offline on untrusted hub time (same) | CONFIRMED | **resolved**: `Liveness::Offline { since }`; RFC 0012 measures offline age on the retention clock (§10) |
| `EDQUOT` and the `statvfs` field (same) | PLAUSIBLE | **resolved**: `EDQUOT` as `ENOSPC`; `f_bavail` (§6) |

**Still open**, carried into the next `rfc-adversary` pass: whether the rotating tail flush plus
the log meets the stated open time at the scale target (to be measured), and whether redb's
copy-on-write amplification keeps the physical write volume near §7's logical figure.

**Earlier passes, as written against the custom engine.** Section references in these tables
point at that draft, not at this one; the redb rewrite above says what each finding is now.

`rfc-adversary`, first pass, on the undivided first draft (the draft that covered 0010–0013).
Every finding is recorded here, with the RFC that now holds its resolution.

| Finding | Verdict | Resolution | Held by |
|---|---|---|---|
| open chunks live only in memory: restarts lose raw points, minute and hour buckets; `close` uncallable; no graceful shutdown; the crash claim vs buffered appends | CONFIRMED | checkpoints copy the whole head, open chunks included; `Sweep` records make replay deterministic; `close(&self)` idempotent; SIGTERM graceful shutdown; `write(2)` per append; tests for each | 0010 §6 |
| recovery duplicates sealed chunks; span close leaves block and head both claiming a span; `head.chunks` not `fsync`ed; `fsync`/`ENOSPC` unspecified | CONFIRMED | explicit checkpoint and span-close order; recovery truncates `head.chunks` to the checkpointed length and discards head data of published spans; `fsync` errors fatal, `ENOSPC` degraded | 0010 §6 |
| `HubClock = max(system, last)`: a forward step wipes history, a backward step stops ingestion | CONFIRMED | wall anchor + `Instant`, 300 s step limit, bounded slew, persisted `last_issued`; retention guard with suspension and admin resume | 0010 §2, §5; resume endpoint 0012 |
| catalog `fsync` per frame; `refresh_cache` per frame; AEAD per frame | CONFIRMED | volatile status in memory; the registry written only on configuration changes; 0007 §2's cache rule carried forward; unsealed-token cache | 0010 §10; 0011 |
| delete: `SystemGone` gone, tombstone scope undefined, push recreates ids, DELETE unauthenticated, data lingers | CONFIRMED | generations and tombstones with deletion time; `SystemGone` inside the store; purge rewrite; DELETE gated by the admin token (owner) | 0011; 0010 §9; 0012 |
| catalog API lacks get/compare-and-swap/batch: lost updates, non-atomic cascades | CONFIRMED | `get`, `update(FnOnce)` and `batch` as one `fsync`ed record | 0011 |
| head memory, index cache and open time off by several times | CONFIRMED | restated budgets at 630k and 1M; sparse fence index; performance test at the target with a full head | 0010 §5, §7 |
| import: future timestamps pin series; not resumable; over-long ids; non-ASCII mounts; plaintext `system-hub.db` | CONFIRMED | future points dropped and counted; one resumable operation; skip-and-count; metric names any non-control UTF-8 ≤ 261 bytes; plaintext warning | 0013; name rule 0010 §2 |
| alert retention and cap resurface acknowledged active incidents | CONFIRMED | retention and eviction by last-seen-active; never evict what the latest poll reported | 0011 |
| `NotAfterLast` lets a token holder displace a polled system | CONFIRMED | push ids reserved for push systems (owner); `AppendReport` rejections surface to `append_round` | 0012; 0010 §10 |
| `/history` unspecified; limits uncapped | CONFIRMED | `/history` and `/metrics` specified with `Order` and a 10,000 cap; `/api/alerts` capped | 0010 §10; 0011 |
| series never leave the catalog, so the caps fill for good | CONFIRMED | series GC; caps count live series | 0010 §5 |
| scaled values up to `i64` overflow accumulator sums | PLAUSIBLE | adopted: domain ranges per kind at the edge, `i128` sums | 0010 §3 |
| data directory and admin token details: `getuid`, ancestors, delay 0–10 min, empty token, token reuse, log flood | CONFIRMED / PLAUSIBLE | uid from `/proc/self`, ancestor checks; `not_before`, empty = unset, refuse equal to the push token, rate-limited refusals | 0010 §8; 0012 |
| build and deploy inventory: Docker context, lockfile drift, relation to 0008 | CONFIRMED | a hub workspace with `hub-store` inside it; Dockerfile copy; gate rule; 0008 stated | 0010 §1, header |
| size budget can't pin 1.3 B/point; codec wastes bits on heap and uptime | CONFIRMED | seeded per-kind generators and budgets weighted by the mix; `11110+32` step; delta-of-delta for `Monotonic` | 0010 §4 |
| missing test rows (clock, `NotAfterLast`, rollups, caps, events, tombstones, import, config, resolution) | CONFIRMED | rows added in each RFC's testing plan | 0010–0013 |
| scope: one RFC across four contexts | PLAUSIBLE | split into 0010–0013 | all |

Came closest and survived: the mixed-version fleet (no wire change in either direction), and
the headline footprint arithmetic.

`rfc-adversary`, first pass on this RFC after the split. Four resolutions of the undivided
pass didn't hold as written (recovery without duplication, series GC keeping the caps free,
size budgets pinning bytes per point, `SystemGone` before 0011). Every finding was acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| one `head.chunks` per span shared by 16 shards: a checkpoint loses or duplicates sealed chunks; `fsync` before serialisation leaves recorded lengths not durable | CONFIRMED | a head file per shard per span; lengths recorded under each shard's lock, **then** every head file `fsync`ed, then the checkpoint published; recovery truncates per shard and refuses a file shorter than recorded (§6) |
| a multi-shard append against per-shard `p_s` loses acknowledged points; "stalls one shard" false | CONFIRMED | a WAL per shard; an append writes and applies one record per shard under that shard's lock, in shard order; the checkpoint copies state under the lock and encodes outside it (§6) |
| span close: replay drops rollup contributions of points in a closed raw span; late-stamped points; straddling buckets; head files of a span the checkpoint doesn't know; who owns hub time | CONFIRMED | replay applies per tier; the store owns `HubClock` and stamps under the shard lock, so no point predates a closed span; accumulators ending by the boundary are force-closed at the close; unknown head files deleted at recovery (§2, §6) |
| the retention guard trips on normal 61–300 s steps; pass interval unstated; the "cap still applies" claim false by default; suspension not persisted; a fresh store starts suspended | CONFIRMED | steps up to 300 s confirmed at once, larger ones trusted after 24 h of agreement, suspension only for a classified fault; pass every 10 minutes; the cap on by default (owner); suspension checkpointed; fresh-store rule (§2, §5) |
| `HubClock` lags by 60× the downtime after a suspend or a post-boot NTP sync | CONFIRMED | forward disagreements stepped at once (author's decision); retention protected by confirmation instead (§2) |
| 0010 can't land before 0011; series interning in a catalog sized for thousands; `not_before` has no place; the monthly pass undefined; `/api/storage` has two owners; replay ignores tombstones | CONFIRMED | 0010 and 0011 land together, all four in one release (author's decision); the series table moves into the engine (WAL `Series` records, checkpoints); `Policies.pending`; the weekly purge defined; `/api/storage` owned here; replay drops tombstoned generations (header, §2, §5, §6, §9) |
| series GC frees a cap slot only ~400 days after a series goes quiet; Docker overlay mounts churn series | CONFIRMED | caps count active series (a point within raw retention); container mounts dropped by the snapshot → points rule (owner) (§3, §5) |
| graceful shutdown hangs on an endless SSE stream; push sockets untracked; 10 s `docker stop` | CONFIRMED | shutdown `watch` ends SSE and push; 5 s drain timeout, then `close` regardless; `stop_grace_period: 60s`; a test with an open SSE client and push connection (§6) |
| `ENOSPC` short writes leave a torn record mid-WAL; checkpoint/block `ENOSPC` unspecified; no operator escape | CONFIRMED | `ftruncate` back to the record start, else roll the segment; torn tails accepted per segment; `ENOSPC` on checkpoint, seal and build specified; `HUB_RECOVER_TRUNCATE_WAL` (§6) |
| volatile status: dead push systems `unknown` forever; "delete offline" and "Seen:" lost after a restart | CONFIRMED | `live_status` checkpointed through the volatile provider; `unknown` then `offline` after 120 s (§9, §10) |
| size budgets ignore the timestamp jitter hub-time stamping creates | PLAUSIBLE | adopted: generators model timestamps; a `10+3` timestamp step; budgets re-derived, weighted ceiling 1.8 B/point, measured values recorded in step 1 (§4) |
| inventory: `/history` loses `since`; limits refused rather than clamped; variable count; `system-hub-data/` and keys not ignored; the legacy-id open question | CONFIRMED | `since` kept; limits clamped, `0` empty; seven variables listed; ignore rules added; the open question listed (§8, §10, Impact) |
| the testing plan can't fail on the ordering attacks | CONFIRMED | `CrashFs` fault-injecting layer at every operation, plus the listed rows (Testing plan) |
| checkpoint cost: shard-lock holds of ~0.6 s; ~170–290 GB/day of writes | PLAUSIBLE | adopted: incremental per-shard checkpoints every 15 minutes, state copied under the lock and encoded outside it; write volume stated (~100 GB/day at the target) (§6, §7) |
| data-directory checks too strict for Kubernetes and too loose on ancestor ownership | PLAUSIBLE | adopted: ancestors owned by root or the hub (`StrictModes`); setgid group-writable directory with the hub's gid accepted; supported volume setups documented (§8) |
| minor: `OutOfDomain` missing from `Rejected`; "domain (in units)" ambiguous; `append_round` map guard | CONFIRMED | `OutOfDomain` in the enum and checked in the store; domains in natural units; the map guard dropped before the admission lock (§3, §9, §10) |

The first-pass row on the import said "one resumable operation"; 0013 restarts from an empty
staging store instead, and that is the design (see 0013's Review).

Came closest and survived: the mixed-version fleet (no wire change), the hub workspace layout
(given `--workspace` in the gate), and `i128` sums.

`rfc-adversary`, second pass on this RFC. Eleven first-pass resolutions held. Seven didn't hold
as written (recovery around span close and the head files it relies on; the durability of the
suspension and of pending steps; the caps; the Kubernetes directory rule; volatile status for
"delete offline"; the 14-day erasure promise; the rollup budget). Every finding was acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| a forward step not yet in a checkpoint vanishes at restart, and retention then runs at the faulty future time; `ForwardStep { at: Instant }` can't be persisted | CONFIRMED | the clock file makes a step over 300 s durable **before** any point is stamped with it; agreement persisted as elapsed seconds; at open, an unexplained replayed jump is treated as pending (§2) |
| §2 and §6 disagree on whether a checkpoint holds the full series table; the id counter derived from what's seen | CONFIRMED | every checkpoint holds the shard's full table and the id counter; ids never reused (§2, §6) |
| the head index is neither checkpointed nor rebuilt; its size misstated | CONFIRMED | a head index file per head file (10-byte entries), `fsync`ed and length-recorded with the head files, read back at open; memory and open time restated at a day boundary (§5, §6, §7) |
| recovery deletes a closed span's head files, then replay re-seals checkpointed state into a new one: duplicates | CONFIRMED | recovery marks every span with a block as closed **before** replay, discards its checkpointed state, and makes its `SpanClosed` a no-op; "every point exactly once" checked by every crash test (§6, Testing) |
| a panic is caught by the blocking pool, and the hub continues with a shard whose WAL and head disagree | CONFIRMED | `panic = "abort"` in the workspace profiles, and a poisoned store lock is fatal (author's decision; §1) |
| a clock fault has no safe way out: 60× convergence, a resume guard that never checks the clock, `HUB_RETENTION_RESUME=1` left set | CONFIRMED | a resume names the `FaultId` and discards the fault and pending steps, and retention then acts on `min(hub, system)`; the held clock runs at half speed (2× the gap); every escape hatch is one-shot, echoing its fault (§2, §5, §8) |
| counting only active series leaves the table unbounded, and a reactivation exceeds the active cap | CONFIRMED | an interned cap at twice `HUB_MAX_SERIES`; a reactivation counts as new against the active caps; memory restated at the interned cap (§2, §7, §8) |
| the default cap counts only the store, read once, and degraded mode can't free anything on a shared volume | CONFIRMED | the volume size re-read at every pass; degraded mode deletes raw then minute blocks until a `statvfs` free-space floor is met, whatever the cap says (§5, §6) |
| "delete offline" and "Seen:" rely on a `last_seen` that isn't "last contact"; shutdown blanks it | CONFIRMED | a typed `LiveStatus` with `last_contact` and `offline_since`, set only by contact and transitions; shutdown touches nothing; `last_seen` rendered from `last_contact` (§10) |
| the mount rule drops the Docker data disk and every Kubernetes PV; three `/run` entries dead | CONFIRMED | only mounts strictly under a runtime's storage (and kubelet `volume-subpaths`) are dropped; the prefix and PV mounts kept; `/run` entries removed; rootless Podman, snap and k0s added (author's decision, §3) |
| "within 14 days" is false for the open hour span | CONFIRMED | block builds skip tombstoned generations; the weekly purge rewrites every block holding one, even while retention is suspended; the bound stated as 30 days (§5) |
| the directory rules refuse the Kubernetes `fsGroup` setup; ownership read from `/proc/self` | PLAUSIBLE | adopted: ids from `rustix::process`; supplementary groups accepted; a root-owned setgid mount point with one of the hub's groups accepted as `HUB_DATA_DIR` and as its immediate parent (§8) |
| falling back to the previous checkpoint replays from WAL already deleted | PLAUSIBLE | adopted: no fallback; a bad checkpoint or a missing (numbered) WAL segment refuses to open, naming it (§6) |
| `ENOSPC` leaves a torn record mid-segment; the truncation hatch meets unknown ids | PLAUSIBLE | adopted: a segment that saw `ENOSPC` is sealed and never written again; the truncation hatch skips and counts points of unknown series (§6) |
| "rollups ≤ 5 bytes per bucket" is impossible with the stated encoding; footprint understated | CONFIRMED | per-kind rollup ceilings from the encoding (weighted ≤ 6.5 B); footprint restated per tier (≈ 135 GB); both replaced by measured values in step 1 (§4, §5) |
| physical write volume at small scale far above "1/100" | PLAUSIBLE | adopted: only written shards are `fsync`ed; the rate stated; `HUB_FSYNC_INTERVAL` added, with SD-card advice (§6, §7, §8) |
| a pending shortening may exist only in memory until the next checkpoint | PLAUSIBLE | adopted: written with the override in one catalog transaction (0012), and re-armed with the full delay at every open (§5) |
| `LOCK` has no stated directory; 0013 stages before `store/` exists | PLAUSIBLE | adopted: `main` locks `HUB_DATA_DIR/LOCK` before reading any state; one directory layout in §6 (§6, §8, §9) |
| the compose volume hides `/app/static` | CONFIRMED | fixed as a prerequisite change of its own, before this release (owner's decision; header) |
| `NotAfterLast` also drops snapshots from one honest connection | PLAUSIBLE | stated, with the counter split by same-connection and cross-path (§2, §9) |

Came closest and survived: the mixed-version fleet, and the multi-shard append against the
per-shard checkpoint cut (`p_s` and the head lengths are recorded under the same shard lock as
the WAL write and the seal).
