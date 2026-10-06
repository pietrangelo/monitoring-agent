# RFC 0010: Hub Store on redb — Tiered Time Series and the Hub's Catalog in One Embedded Database

- Status: Draft
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-26 (rewritten on redb the same day, after three `rfc-adversary` passes on a
  custom engine; amended 2026-10-03 for RFC 0017's mail systems; see Review)
- Affects: `system-hub`, and a new crate, `hub-store`, inside it
- Depends on: RFC 0009 (Accepted, ships first on SQLite; this RFC takes over its `app:*`
  series, its interim prune and its guarded store)
- **Carries RFC 0017 (Implemented): mail systems.** A mail report holds up to 60 snapshots at
  agent-clock times and may arrive late. **This store keeps at most one point per series per system per scan: the
  newest snapshot's, stamped at hub time when the hub accepts it** (owner's decision,
  2026-10-03, §2 *Mail points*), so mail fits the hub-time, append-only series every other
  source uses. A report's points commit with its receipt (§6, §9), and mail `LiveStatus`
  changes are applied in commit order (§10). The mail receipts themselves are 0011's catalog
  tables. RFC 0012 §2's requirements on this RFC (mail last contact on hub time, `since` after a
  restart, `TierSetting`) are met in §5 and §10.
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
   (RFC 0007 measured about 9.7 ms per point on a table filled to 24 h), under the mutex that async handlers also take on
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

**Block files came back, smaller** (owner's decision, 2026-10-06). A measured spike (§1) showed
that closed spans cost ≈ 1.7× their payload in redb and that the space retention frees there
never returns to the volume. Closed spans now leave redb as immutable files, written once and
deleted whole; everything a commit changes stays in redb, where the own engine had failed, and
one redb transaction decides on which side of the handoff a span lives (§5, §6).

**Decisions carried by this RFC:**

| Question | Decision | By |
|---|---|---|
| Where it runs | embedded in the hub process, as a library crate behind a narrow API; no network protocol | owner |
| Engine | **redb** holds the catalog and the **hot** part of the series (points log, tails, the chunks of open spans); every **closed span** is an **immutable block file** of our own format, written once and deleted whole (§5, §6). The hub's codec, tiers, rollups, retention and catalog sit on top | owner (block files: 2026-10-06, after a measured spike, §1) |
| Migration | **none**: a new hub starts with an empty store; the old `system-hub.db` is left untouched for the old binary (RFC 0013 Rejected) | owner |
| What it stores (with 0011) | everything: time series, the series table, the registry, alert records, retention overrides; SQLite removed | owner |
| History | tiered downsampling: raw points, 1-minute rollups, 1-hour rollups | owner |
| Sequencing | RFC 0009 ships first on SQLite | owner |
| Precision | a declared resolution per metric kind (values stored as scaled integers) | owner |
| Default tiers | raw 24 h, 1-minute 14 days, 1-hour **30 days** (so by default no metric history is kept longer than 30 days; a longer hour tier is set through `HUB_RETENTION` or a system's override). Alert records follow their own `events=` period, 90 days by default (0011 §5), and freed pages of `hub.redb` keep old bytes until reused or compacted (Impact) | owner (30 days: 2026-10-06) |
| Erasure of a deleted system | unreadable at once; removed from `hub.redb` within one retention pass; in block files written before the delete, removed when the file is rewritten or retired, at the latest at the tier's longest effective retention (30 days with the defaults) | owner (2026-10-06) |
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
| space for 1.2M values inserted **in key order**, span-first | **1.19×** the payload at 160 B and 700 B values; 1.30× at 3.2 KB (one value per page) |
| the same, key order **series-first** | 1.8–2.0× (random inserts leave half-full leaves) |
| open, clean | ≈ 2 ms (2.8 GB file) |
| open after `SIGKILL` mid-write, every commit 2PC | 3–7 ms, and the first write after it ≈ 1–4 ms, in six runs, with or without quick-repair |
| open after `SIGKILL`, the first crash after a history of 1-phase commits | 3.6 s (a full repair walk of 2.8 GB) |
| quick-repair's cost at this commit size | within noise (513 vs 518–532 commits in 5 s) |
| deleting 2.1M entries in one transaction | 6.0 s (≈ 3 µs per entry) |
| `compact()` after that delete | 19.5 s, 6.8 GB → 1.5 GB |
| file length while open vs after a clean close | a file grows in regions (8.6 GB while open for 5 GB allocated) and is trimmed at close (5.2 GB) |

**The storage spike (2026-10-06).** A throwaway crate ran one synthetic workload (10,000 series,
a point every ≈ 2.3 s, 8 simulated hours, 134M points, 1 h spans, 5 spans kept, 2-phase commit,
one codec) stored two ways: every sealed chunk in redb's `chunks` table, or the same with each
closed span moved to an immutable file (temporary file, `fsync`, rename, directory `fsync`, then
the span's chunks deleted from redb in one transaction). Container disk, raw tier only, a
byte-aligned codec (the same in both); write volume from `/proc/self/io` `write_bytes`:

| Measurement | All in redb | Closed spans in block files |
|---|---|---|
| space on disk / live chunk payload, closed spans | **≈ 1.6–1.7×** | **1.03×** |
| total on disk, commits every 1 s / 10 s | 390 MB / 405 MB | 327 MB / 332 MB |
| space returned when retention is cut to the open span | **0** | the whole closed part (327 → 165 MB) |
| bytes written to disk per point, commits every 1 s / 10 s | 59 B / 36 B | 59 B / 38 B |

- In real use chunks **seal in series order scattered over the span** (each series fills its
  chunk at its own pace), so the B-tree's leaves fill like the series-first case above, not like
  the in-key-order measurement. The previous revision's 1.19× footprint was therefore optimistic.
- Freed redb pages never return to the volume while the hub runs; a deleted file does at once.
- Physical writes are dominated by the commits of the hot part (points log, tail rotation,
  2-phase commit), identical in both layouts: block files don't reduce write wear, and the
  commit interval remains the lever for SD cards and eMMC.

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
  every chunk whose span starts after the system clock (the misdated future) and the `blocks` rows
  of such spans with their `block_generations` entries (their files queued in `pending_unlinks`
  and unlinked after the commit, §5), **brings back into `chunks` the handed-off span that holds
  the system clock**, in each tier (it copies the file's chunks into `chunks` under their own
  seq, deletes the row and its `block_generations` entries and queues the file in
  `pending_unlinks`: at most one file per tier, ≈ 0.94 GB read and ≈ 1.6 GB written into redb at
  the scale target, which the floor check counts when the rewind will act, §8), so the rules
  below act on it as on a span never handed off and its next handoff writes a new file, never
  over the old one (§6); a span whose file is unreadable (§6 step 2a) can't be brought back and
  is dropped instead (its row and `block_generations` entries removed and its file queued in
  `pending_unlinks`, as for a brought-back span, logged at `warn` and counted), and a chunk of a
  brought-back file that fails its CRC is skipped, logged at `warn` and counted, so the rewind
  always commits and never loops at open. It discards open
  chunks that start after it, resets `last_issued` to the system clock and every series' last
  timestamp to `min(last, system clock)`, applies `min(x, system clock)` to every other persisted hub
  time (0011's receipts' and `MailNewest`'s `received_at`, `last_seen_active`, the flushed
  `LiveStatus` times, alert records' `stored_at` and their `alerts_listed` keys, tombstones'
  times, each series record's last span per tier, and
  `meta/retention_clock`, and 0011's `mail_prune_floor`, set to `min(floor, rewound retention
  clock − receipt window)`), **rebuilds each alert record's `alerts_seen` entry** as its rewound
  `last_seen_active` plus its class bound (the old key deleted, the new one inserted, in the same
  transaction, so 0011 §5's key invariant holds and no orphan key can name a refreshed incident
  as a victim), in each tier's **current span** rewrites every series' chunks and
  tail without the points and buckets after the system clock (at most three spans per active
  series; each series' last chunk in a brought-back span is reopened into its tail, so a rollup
  tier keeps one chunk per series per span), **reopens into the tail a closed bucket that straddles the system clock**, rebuilding
  its accumulators from the surviving raw points of its interval (raw retention is at least an
  hour bucket; a closed bucket holds no sum to subtract from), so the next point can't open a
  second accumulator with the same start and the rollup invariant still holds, **flushes every
  tail** (one full rotation, seconds at open) **and every chunk sealed since open**, and
  **deletes every `points_log` entry** (every logged point is then durable in a tail or a chunk,
  so no future-stamped point can be replayed and no pre-rewind point is lost; a `SIGKILL` right
  after keeps the same point count). No point of the straddling bucket is ever after the system
  clock (hub time jumps straight from the fault's start to its value), so the rebuild only
  recovers a sum, never subtracts, so no misdated bucket
  stays behind an older one; it logs the counts at `warn`. **The rewind runs after recovery's
  replay** (§6 step 6), on a head that already holds every logged point.  A value that doesn't match is logged at `error` and ignored.

**Mail points** (RFC 0017; owner's decision, 2026-10-03). A mail report carries up to 60
samples taken at agent-clock times over one mail interval, and may sit in a relay for hours.
Three earlier designs kept every sample at its reported time, and each `rfc-adversary` pass
found samples lost or reordered at report boundaries, bucket closures and clock faults (Review).
So a mail report is ingested like a push snapshot:

- **Only one report per system per scan stores points**: the system's newest `Newest` report of
  the scan, chosen by one pure function, `receipt::newest_of_scan` (by `created_at`; ties by `seq`
  within a run, by arrival order across runs), which also picks the report for the live metrics, the info fill,
  the round and `mail_newest` (0011 §7). **Currency gates the points, the round and the live
  metrics, and nothing else**: they are stored or shown only if the report is *current*,
  `created_at` no older than one mail interval plus 15 minutes on the system clock; the status
  (`Online`, `last_contact`), `mail_newest`, the info fill and the alert records are written for
  every accepted `Newest` choice, current or not. A backlog drained after an outage therefore
  stores no point and shows no stale round until its current reports arrive, while the system
  is online and never flaps. A report that isn't current is **counted** (`mail_not_current` in
  `/api/storage`, and per system) and **logged at `warn` at most hourly per system**, because an
  agent whose clock is slow by more than that bound would otherwise be online with no history
  and no sign of why; the README says so. It stores only its **newest snapshot's**, at **hub now**
  (`append`'s ordinary stamping), with the same snapshot rule as a push frame. Its other samples
  are not stored; its alert records all are (0011 §5), and so is its round (§10). A mail system's
  history therefore has at most one point per series per report it receives: one every 5 minutes at 0017's
  default interval. *Behaviour change: RFC 0017 stores every sample at its own `collected_at`.*
- **A `Backfill` report stores no point** (decided by 0017's `Recency` read before any write):
  its newest snapshot is older than what is already shown, and stamping it at hub now would
  present old data as current. Its alert records are stored.
- The agent's clock therefore never stamps a point: no rebase, no late-point rule, no closing
  rule of its own (it decides only whether a report is current, above). A mail series is stamped at hub time like any other series, under the same caps,
  sweep, recovery and retention.
- 0017's agent still samples every `MAIL_SAMPLE_INTERVAL` and mails the samples; the README says
  that this hub stores the newest only, so `MAIL_SAMPLE_INTERVAL` affects alerts, not history.

**The guarded retention clock.** Retention never acts on hub time directly:

```rust
/// Persisted in `meta/retention_clock` by each retention pass; created at
/// `min(hub_now, system_now)` in the open's first transaction when absent (a new file).
pub fn retention_now(previous: u64, hub_now: u64, system_now: u64, since: Elapsed) -> u64 {
    // At most twice as fast as the monotonic clock says time passed, and never past either
    // clock. The first pass after open has no `Instant` to measure from and advances nothing.
    let allowed = match since { Elapsed::SinceOpen => 0, Elapsed::Measured(d) => 2 * d.as_secs() };
    hub_now.min(system_now).min(previous + allowed)
}
```

- `since` is the `Instant` time since the previous pass. At open there is no `Instant`
  continuity, so the first pass after open passes `SinceOpen` and **advances nothing**; the
  second, ten minutes later, is `Measured`. A hub off for two weeks still catches up at 2× from
  its second pass. (Two earlier formulas granted a pass of slack: on every pass, which allowed 3×
  at the pass interval, then once per open, which a restart loop under a forward fault turned
  into 60× real time.)
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
  use this to displace a polled system, and RFC 0017 already kept push away from mail systems;
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

In `hub.redb`, redb's page checksums cover chunks (§1); in a block file, every chunk carries a
CRC-32C of its own (§5).

- The `10 + 3` timestamp step exists because hub-time stamping makes gaps jitter by a second or
  two. The `11110 + 32` value step keeps a large delta (a 512 MiB heap swing) at 37 bits.
- **Why these chunk sizes.** While hot, a chunk is one redb entry (about 30 bytes of cost, and a
  value over about a third of a page takes a whole page, §1); chunks of up to 1 KiB keep each
  one a small entry. In a block file each chunk costs one 22-byte index entry, about 0.1 B per
  raw point, 0.4 B per minute bucket and 0.9 B per hour bucket (§5's footprint).

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

These are *encoded* sizes. On disk each is multiplied by its layout's overhead (§1's spike:
1.03× in a block file, ≈ 1.7× in redb's hot part), which the footprint (§5) includes. The first implementation step
measures every number from the generators and records it here and in `docs/ARCHITECTURE.md`;
the test then asserts the measured value plus 10%.

### 5. Tables, tiers and retention

**Two places on disk**, both in `HUB_DATA_DIR`:
- **`hub.redb`**, one redb file: the catalog and the **hot** part of the series, everything a
  commit changes;
- **`blocks/<tier>/<span start>[.r<n>].blk`**: one **immutable block file** per closed span per
  tier, written once (§6 *Span handoff*), read with `pread`, and deleted whole by retention.
  Its name is derived from its row's typed key and rewrite number `n`, never stored as a
  string.

| Table | Key → value | Written by |
|---|---|---|
| `meta` | `&str` → bytes: `format`, `id_counter`, `clock` (`last_issued`), `retention_clock`, `purge/cursor`; hub-owned keys under `hub/` (0008's counter, 0011's key id and generation counter) | every group commit (clock), passes |
| `series` | `SeriesId` → record (version, system key, generation, metric name, kind, last span per tier) | first point of a series; GC |
| `series_key` | len-prefixed system key ‖ generation ‖ metric name → `SeriesId` | first point; GC |
| `points_log` | batch sequence `u64` → the points of one group commit (series id, timestamp, scaled value) | every group commit; truncated as tails are flushed |
| `tails` | `(tier u8, SeriesId)` → the series' open chunk and accumulators, its last timestamp, and the seq its next seal takes in the open chunk's span (one past the highest in `chunks`) | the rotating tail flush (§6) |
| `chunks` | `(tier u8, span start u64, SeriesId, seq u16)` → a sealed chunk **of a span not yet handed off** | when a chunk seals; emptied for a span by its handoff (§6) |
| `blocks` | `(tier u8, span start u64)` → `BlockRecord`: format, rewrite number `u32`, length, chunk count, tombstoned bytes | the handoff, a rewrite, retention, a tombstone, the rewind, `HUB_STORE_FORGET_BLOCK` |
| `block_generations` | `(Generation, tier u8, span start u64)` → the generation's chunk bytes in that file | written with its `blocks` row; **deleted in every transaction that removes or swaps that row** (retirement, rewrite, rewind, forget), so every key has a row |
| `pending_unlinks` | `(tier u8, span start u64, rewrite u32)` → `()`: files no row names any more, not yet unlinked | a retirement, a rewrite's swap or its refusal, the rewind, a forget; emptied after each unlink |
| 0011's catalog tables | systems, alert records and their indexes, tombstones | catalog transactions |
| `retention` | len-prefixed system key → override and pending shortening (store-owned: the store enforces it) | 0012's routes, through catalog transactions |

**Why closed spans leave redb** (§1's spike): kept in redb, closed spans cost ≈ 1.6–1.7× their
payload, because chunks seal scattered over the span and leave the B-tree's leaves half full,
and the space retention frees never returns to the volume while the hub runs. In block files
they cost 1.03×, and retention frees space at once by deleting a file. **The hot part stays in
redb**, because that is where crash safety is hard: the first three drafts' own engine failed
there (Review), and redb's atomic commits are what made the redb rewrite converge. A block file
is written once and never changed, so its own protocol is short (§6).

| Tier | Content | Span | Default retention |
|---|---|---|---|
| `Raw` | every accepted point | 1 h | 24 h |
| `Minute` | 1-minute rollups | 1 h | 14 days |
| `Hour` | 1-hour rollups | 1 day | 30 days |

A chunk never crosses a span: a point in a new span seals the open chunk first. A minute chunk
holds at most 60 buckets and an hour chunk 24 (§4), so a rollup tier has one chunk per series per
span. The spans are shorter than the previous revision's (1 day and 30 days for the rollups) so
the hot part stays small: while handoffs succeed, at most two spans per tier are in redb at
once, the open one and the one before it until its handoff; a failing handoff's backlog is
bounded at three spans per tier (§6).

**Block file format** (versioned by its format byte; `crc32c`, a small pure-Rust crate, is the
one dependency it adds):

```text
header : magic "HUBBLK" · format u8 · tier u8 · span start u64 · chunk count u32 · index offset u64
data   : the span's live chunks, ordered by (SeriesId, seq)
index  : per chunk (SeriesId u32, seq u16, offset u64, length u32, CRC-32C u32), sorted,
         in index blocks of 1,024 entries
summary: per index block (first SeriesId u32, offset u64, CRC-32C u32 of the index block)
trailer: summary offset u64 · CRC-32C of header and summary · magic
```

- Written by one sequential write, then never modified; read with `FileExt::read_exact_at`
  (no memory mapping, so no `unsafe`).
- **A chunk's CRC-32C covers (tier, span start, `SeriesId`, seq) and its bytes**, so an index
  entry with a flipped series id fails the check instead of serving another series' chunk.
  **Every index block and every chunk read is checked against its CRC.** A mismatch fails that
  span of that query with `StoreError::Corrupt`, counted in `/api/storage` and logged at `error`
  once per file per hour; it never fails the store. redb's checksums cover the hot part, CRC-32C covers block files
  against bit rot; neither protects against someone who can write `HUB_DATA_DIR` (A08).
- **In memory, each file keeps its summary** (the sparse index: 16 B per 1,024 chunks, about
  1–2k entries per file), read and checked at open with the header; the full index is never
  read at open. A query reads one index block (≤ 22 KiB), checks it, and reads the chunks it
  names.

**Footprint at the scale target with the defaults** (codec ceilings × 1.03 for block files, the
spike's ≈ 1.7× for the hot part; step 1 replaces them with measured values):

| Tier | Per day | Kept | On disk |
|---|---|---|---|
| Raw (6.6 B points × 1.8 B × 1.03, plus ≈ 0.1 B of index) | ≈ 12.9 GB | 24 h + 1 h | ≈ 13 GB |
| Minute (630k series × 1,440 × (6.5 B × 1.03 + 0.4 B)) | ≈ 6.4 GB | 14 d + 1 h | ≈ 90 GB |
| Hour (630k × 24 × (6.5 B × 1.03 + 0.9 B)) | ≈ 0.12 GB | 30 d + 1 d | ≈ 4 GB |
| `hub.redb`: two hot spans per tier at ≈ 1.7×, tails, points log, series table, catalog | | | ≈ 6 GB |
| **Total** | **≈ 19.5 GB of new data a day** | | **≈ 113 GB**, against ≈ 180 GB with every closed span in redb at the measured 1.7×, and ≈ 330 GB for *one day* of raw rows in SQLite |

At 100 systems (and their applications) the same policy needs about 1.1 GB. The minute tier
still dominates, and an operator short of disk shortens `minute=` first.

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
/// One setting per tier; never a `RetentionPolicy` copy, so "follow the global policy" survives
/// a change of `HUB_RETENTION`.
pub struct Override { tiers: PerTier<TierOverride> }
pub struct TierOverride { setting: TierSetting, pending: Option<PendingShortening> }
pub enum TierSetting { Global, Fixed(TierPeriod) }
/// A retention period inside its tier's bounds (the table below); unconstructible otherwise.
pub struct TierPeriod { tier: Tier, period: Duration }
pub struct PendingShortening { next: TierSetting, delay: Duration }
```

- **Per tier, at most one pending shortening** (RFC 0012 §2): a `Vec` could hold two for one
  tier, and a `Duration` couldn't say "follow the global policy", which is what `DELETE
  .../retention` and an omitted tier ask for. A `Global` setting, enforced or pending, resolves
  to the global policy **when the pass reads it**, so a restart with a new `HUB_RETENTION`
  is followed. An override whose every tier is `Global` with nothing pending is removed.
  Which change makes a tier pending is 0012's rule; the store keeps the result.
- **A pending shortening** (RFC 0012) is written in the same catalog transaction as its
  override. The store arms its delay on the monotonic clock, and **re-arms it with the full
  delay at every open**, so a restart can only lengthen the window. When the delay ends, the
  writer thread applies `next` in a transaction of its own.
- **The retention pass** runs every **10 minutes** of `Instant` time, on the writer thread; its
  redb deletions run as bounded transactions of at most 50,000 entries each (about 0.15 s at the
  measured 3 µs per entry), so ingestion commits keep flowing between them. It:
  1. computes `retention_now` (§2) and persists it;
  2. re-reads the volume size for the storage cap;
  3. deletes the **dead chunks still in `chunks`** (a span not yet handed off, or held back by
     a failing handoff, §6), in the bounded transactions;
  4. **retires the block files whose every chunk is dead** (below): one transaction deletes the
     file's `blocks` row and its `block_generations` entries and inserts it into
     `pending_unlinks`; after that commit the block writer thread evicts and closes the file's
     cached handle, unlinks the file and deletes the `pending_unlinks` entry. A crash in
     between leaves the entry, and the open finishes the unlink (§6);
  5. asks the block writer thread for **at most one rewrite** (below); the rewrite runs there,
     and only its row swap goes through the writer;
  6. purges deleted generations from the hot part: their chunks in `chunks`, their tails and
     their log entries, recording progress in `meta/purge/cursor` so a restart resumes it;
  7. runs series GC: a series with no chunk in `chunks`, no tail and, in each tier, no `blocks`
     row for a span at or before the series' last span in that tier is removed from `series`
     and `series_key`, so a file never holds a `SeriesId` the table can't map;
  8. calls `on_retention_pass` (§9) at most 10 times, stopping at `Done`.
- **Live and dead chunks.** A chunk, in `chunks` or in a block file, is **dead** when its series'
  generation is tombstoned (0011 §3), when its `SeriesId` maps to no series (defence in depth),
  or when its span ended before `retention_now` minus its system's effective retention for that
  tier (the global policy, or the system's override). One pure function of the domain core,
  `chunk_liveness(tier, span, series → (system, generation), &Policies, &Tombstones,
  retention_now) -> Liveness`, decides it. **Queries never return a dead chunk**: a query reads
  `Policies`, the tombstones and `meta/retention_clock` in the same redb read transaction as the
  rows it reads (all three are persisted, so the snapshot is consistent and no lock is added,
  §9). What a delete removed is unreadable at once. What a policy expired is unreadable while
  that policy holds; lengthening a policy makes expired chunks still on disk readable again
  (retention is a storage rule; erasure is a deletion's promise, below).
- **Tombstoned bytes.** The `blocks` row keeps the bytes of tombstoned generations' chunks only,
  summed from `block_generations`, which records each generation's bytes per file: the
  transaction that commits a tombstone adds the generation's entries (a prefix scan of at most
  one entry per file, no file read) to each file's tombstoned bytes, and the transaction that
  inserts or swaps a row computes its tombstoned bytes from the tombstones **at that commit**,
  not from the block writer's snapshot. Expiry by retention is never stored; it follows from
  span and policy when needed.
- **The handoff and every rewrite write only the chunks live in the block writer's snapshot**,
  so a file never receives a chunk of a generation tombstoned before it; one tombstoned between
  that snapshot and the row's commit is counted as tombstoned bytes by that commit, so the 25%
  rule still sees it.
- **A file is retired when every chunk in it is dead**: with no overrides, when its span passes
  its tier's global retention, the whole span at once.
- **A span in flight is left alone.** While the block writer hands off or rewrites S (from its
  read transaction to the commit or the failure of its row), the writer keeps an in-memory set
  of such spans, and pass steps 3, 4 and 6 and the storage cap skip S: they don't delete its
  chunks from `chunks`, retire its row or purge its generation's chunks there, and the cap moves
  to the next oldest file. S's turn comes at the next pass. So a crash in the middle always
  leaves what §6 step 2a's rules expect: a handoff's chunks still in `chunks`, a rewrite's row
  still at its number with its own file present. A crash empties the set, and reconciliation
  runs at open before any pass.
- **Rewrite.** A file that still holds live chunks is rewritten without its dead ones (§6: a new
  file under the next rewrite number, a row swap that queues the old file in `pending_unlinks`)
  when **(a)** its tombstoned bytes reach 25% of its length, or **(b)** its span is past the
  tier's global retention and only longer overrides keep it, so one system with `raw=30d` keeps a
  small file of its own chunks, not every system's raw data for 30 days. At most one rewrite per
  pass, the most overdue file first; a rewrite reads one file and writes less, on the block
  writer thread. **A damaged file is never selected**: a file opened unreadable, or one whose
  rewrite met a chunk failing its CRC (that rewrite is abandoned, its temporary file deleted, the
  file marked damaged in memory and counted; after a restart one failed attempt marks it again),
  is skipped, so it can't starve the other files' rewrites; retention and the cap still retire
  it.
- **Erasure of deleted systems.** A deleted system's data is unreadable at once (0011). Its chunks
  in redb, its tails and its log entries are removed within one retention pass, as before, and
  no file begun after the delete holds its chunks (one already being written counts them as
  tombstoned bytes from its commit). In the files written before it, its chunks
  are dead and their bytes are removed **when the file is rewritten or retired**: once
  tombstoned bytes reach 25% of a file, and at the latest when the file's span passes its tier's
  longest effective retention (by default a day for raw, 14 days for minute, 30 days for hour,
  so 30 days at most unless a policy or an override is longer). *This weakens the previous
  revision's "removed from the store within one pass"* (owner's decision, 2026-10-06); 0011 §3 and the README say so, and `/api/storage`
  shows the tombstoned bytes still on disk. 0011's tombstone of the generation is kept until
  `block_generations` names it in no file.
- **Compaction** applies to `hub.redb` only, now the hot part: a few GB at the scale target, so
  the pause is seconds. The hub compacts **at start, before serving**, only when asked
  (`HUB_STORE_COMPACT=1`, not one-shot: it compacts only when reclaimable space is over 1 GiB and
  25% of the file). `/api/storage` shows the reclaimable bytes, and the pass logs a `warn` hint
  once a day when they pass that threshold.

**Storage cap.** `HUB_STORAGE_LIMIT` bounds the bytes the store takes on disk: `hub.redb`'s
length plus every block file's length. Block files are what grows, and retiring one frees its
bytes once it is unlinked, right after the commit (its cached handle is closed first; a query
still reading it holds the bytes until that query ends), so the rule acts on the real total and
stops as soon as it is met: while the total exceeds the cap, each pass retires the oldest raw
block file, then the oldest minute block file, never an hour file, the hot part or a tail,
logging each at `warn`. Between passes the total can exceed the cap by at most ten minutes of
ingest. `hub.redb`'s length is bounded by the hot part (two spans per tier, plus a handoff
backlog of at most three, §6); it never drops while the hub runs, and the cap counts it as it
is. Unset,
the cap defaults to 80% of the size of the volume holding `HUB_DATA_DIR` (`statvfs`), re-read
at every pass; `none` disables it. If the cap can't be met from raw and minute files, that's
logged at `error` once per hour, and ingestion goes on. (This replaces the previous revision's
rule on allocated bytes and its region slack, which no longer exist.)

### 6. The writer thread, the group commit and durability

**One writer thread** owns every redb write transaction (redb allows one writer anyway). Callers
send it requests over a bounded channel and wait for its answer:
- `append` batches: stamped, checked against the series' state, applied to the in-memory head,
  and queued for the next group commit. The `AppendReport` (accepted, rejected with reasons)
  is answered **when the batch is applied**, not when it is durable;
- catalog transactions (0011): run in the writer's transaction, answered **after the commit
  that holds them** and after the commit hook ran (read-your-writes). A catalog transaction may
  **stage points** (`CatalogTxn::append`, §9): they are checked as an `append` is, applied to
  the head only if `f` returns `Ok`, and committed in the same redb transaction as its catalog
  writes. **A transaction that writes nothing** forces no commit. Its `f` returns, with its
  value, when it may be answered: `Answer::AfterCommit` (after the commit holding the
  transaction it read, since it may rest on earlier transactions' uncommitted writes) or
  `Answer::AtOnce` (a refusal that a retry corrects, RFC 0008 §3). A transaction that writes is
  always answered after its commit. The mail intake uses it, so a report's receipt and its points commit together or not
  at all (RFC 0017 §6), as one SQLite transaction does today. Staged points refused for any
  reason don't abort the transaction: the report is accepted, its
  refusals counted, and the message deleted, as RFC 0017 deletes every handled message.
  **An `f` may abort only before its first write.** Every refusal a transaction can make
  (a wrong source, a duplicate or stale receipt, a backfill report's points, a push registry
  over its limit) is decided by reads first; a
  write (catalog or staged) is never undone. `CatalogTxn` enforces it: after a write, `Abort`
  is a programming error that fails the store stop, as a writer panic does, and a redb error
  after a write is `StoreError`, also fail-stop. So redb needs no savepoint inside the shared
  group-commit transaction, and nothing staged or interned survives an abort, because nothing
  was;
- retention, purge, GC, sweeps and the tail flush: the writer's own jobs.

A full channel makes callers wait on the blocking pool, which slows a push connection instead of
growing memory.

**The group commit.** Every `HUB_COMMIT_INTERVAL` (default **1 s**), **unconditionally** (a
commit holding only `meta/clock` still runs, so a pending `notify` is always delivered within
one interval, however quiet the hub), or at once when a catalog transaction marked `Durable` is
waiting, the writer commits **one redb write transaction** with
`Durability::Immediate`, 2-phase commit and quick-repair. It holds, atomically:
- the interval's points, as one `points_log` entry;
- every chunk sealed in the interval (inserted into `chunks`, and removed from its tail);
- the next slice of the **rotating tail flush**, sized by **elapsed `Instant` time**, not by
  commit count: the tails due since the previous commit, so every series' tail is written once
  every 15 minutes however many commits run (a burst of durable catalog commits doesn't multiply
  tail writes; at one commit a second this is 1/900 of the active series per commit);
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
2a. **Reconcile the block files** with the `blocks` table (every `.tmp` file was already deleted
   before the floor check, §8):
   - every file in `pending_unlinks` is unlinked if still present, and its entry deleted;
   - a `.blk` file with no row is deleted only when that is provably safe, so that only a
     redundant copy is ever deleted: **(a)** its span has no row, S is closed by the persisted
     `meta/clock` (hub time past S's end plus its grace, §6), no tail of the tier is still open
     in S, and S's chunks are in `chunks` (a handoff that crashed before its commit: step 2 had
     sealed all of S, so `chunks` holds what the file holds); or **(b)** a row for its span holds
     a lower rewrite number `n` **and** that row's own file `S.r<n>.blk` is present (a rewrite
     that crashed before its swap always leaves it). Any other row-less file means `hub.redb` is
     older than `blocks/` (one restored without the other): the hub **refuses to start**, naming
     the file, since deleting it would lose history the store no longer knows. The operator
     restores both together (Impact, backups) or moves the file aside;
   - a row whose file is missing, or whose header or summary fails its CRC, **opens with that
     file unreadable**: every query of its span answers `StoreError::Corrupt`, counted in
     `/api/storage` and logged at `error` once per file per hour, and retention and the cap
     retire it as any other file. One flipped bit never stops the hub. The one-shot
     **`HUB_STORE_FORGET_BLOCK=<tier>/<span start>`** deletes its row and its
     `block_generations` entries and queues its file in `pending_unlinks`, in one transaction at
     open, logged at `warn`, to clear the error before then.
   Then each file's summary is loaded.
3. Load `tails` into the head: each active series' open chunks, accumulators and last
   timestamp.
4. Replay `points_log` in sequence order, applying each point only to a series whose last
   timestamp is earlier than the point's. Points already in a tail or a sealed chunk are
   therefore skipped, and none is applied twice.
5. Run a sweep at hub now (closing buckets that are past, which gives the same buckets the live
   run would have closed).
6. Apply `HUB_CLOCK_REWIND` if asked (§2), on the replayed head, then the retention pass's
   first run.

Within redb there is no ordering to reconstruct: every state change a commit made is in one
atomic redb transaction. Between redb and the block files there is one ordering, the handoff's
(below), and step 2a reconciles it.

**Span handoff.** A closed span moves from `chunks` to a block file once, by this protocol:
1. A span S of tier T **closes** once hub time is past S's end plus the tier's grace: one bucket
   length (raw 60 s, minute 1 min, hour 1 h) plus one sweep interval, so the sweep has closed
   every bucket in S. Hub time never runs backwards while the hub runs (§2), so no point can
   still land in S. The one exception, `HUB_CLOCK_REWIND`, runs at open before any handoff and
   first brings the handed-off span holding the system clock back into `chunks` (§2).
2. At its next commit the writer **seals every tail chunk still open in S** (quiet series), so all
   of S is in `chunks`, and S's chunks never change again.
3. A **block writer thread** (not the writer: a raw file is ≈ 0.55 GB at the scale target, and
   writing it would stall commits) reads S's chunks in a redb read transaction taken after that
   commit (MVCC), writes S's **live** chunks (§5) to `blocks/T/S.blk.tmp` in one sequential write
   with mode 0600, `fsync`s it, renames it to `S.blk` **without replacing**
   (`renameat2(RENAME_NOREPLACE)` through `rustix`; where the filesystem lacks it, `link` then
   `unlink` of the temporary, which also fails on an existing name), and `fsync`s the directory.
   An existing name is a block-write failure (below), never overwritten.
4. It then submits one catalog transaction: insert the `blocks` row (its tombstoned bytes from
   the tombstones at this commit, §5) and S's `block_generations` entries and delete S's chunks
   from `chunks`, in one commit. From that commit on, queries read S from the file.

**The invariant:** a span's chunks are in exactly one of `chunks` or the file a `blocks` row
names, and the file is durable before the row exists. A crash at any step leaves either the
chunks in redb and at most an orphan file (deleted at open), or the row and its durable file.
A span whose handoff didn't complete is handed off again after open. A **rewrite** (§5) runs on
the block writer thread with the same steps, under the next rewrite number (`S.r<n>.blk`), and a
row swap that also replaces S's `block_generations` entries and queues the old file in
`pending_unlinks`. **The swap is conditional**: it applies only while S's row still holds the
rewrite number the rewrite read; otherwise (a retirement or the cap removed the row meanwhile)
it writes nothing but the new file's `pending_unlinks` entry, so a rewrite never undoes a
retirement. **A failure writing a block file** (`ENOSPC`, `EIO` on the file, an existing name)
is not a fail-stop at first: redb is untouched, so the block writer deletes the temporary file
and, if the rename had succeeded (a failed directory `fsync`), the final name too, since no row
names it yet, logs at `error` (at most hourly), counts it in `/api/storage`, and retries at the next
pass; S stays in redb meanwhile, served from there, expired by step 3 of the pass like any
chunk, and counted by the storage cap. **The backlog is bounded**: when more than three spans of
any tier are past their close and not handed off, the store **fails stop** with an error naming
the block-write failure, since `hub.redb` would otherwise grow without bound and never shrink
while the hub runs (§5); the next open retries. `/api/storage` shows the backlog per tier.

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
- **`ENOSPC` or `EDQUOT` on commit fails the store stop**, like any I/O error: redb latches a
  failed write or `sync_data` and refuses every later operation on that `Database`
  (`StorageError::PreviousIo`, `cached_file.rs`), so the process can't keep serving from it.
  The interval's points are lost, as a crash would lose them; nothing is answered as committed.
- **There is no degraded mode** (owner's decision, 2026-10-03). The store never deletes history
  to make room beyond what the storage cap already deletes. At open, if free space (`statvfs`'s
  `f_bavail`) is below the **floor** (the larger of 1 GiB and 2% of the volume), the hub
  **refuses to start**, logging the data volume, the free space and the floor, and exits with a
  distinct status; Compose's restart then retries until the operator frees space or grows the
  volume. While running, the storage cap (§5) is the only automatic deletion, and a commit that
  hits `ENOSPC` or `EDQUOT` fails stop as above.
- **Limits stated, not engineered around** (README): a user or group quota (`EDQUOT`) is
  invisible to `statvfs`, so a hub under quota fails stop at its first commit and restarts until
  the quota is raised; RFC 0017's Maildir, which anyone can mail, should sit on a volume other
  than `HUB_DATA_DIR`, or a mail flood can fill the data volume; a message whose content makes
  the store fail stop is retried on every restart (the intake logs the names of the scan in
  flight at `error` when its transaction answers `StoreError::Failed`, so the operator can move
  it aside).

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
| block files: summaries (≈ 390 files with the defaults, ≈ 1–2k entries of 16 B each) and an LRU of 256 open handles | — | ≈ 10 MB | ≈ 15 MB |
| **Total** | | **≈ 0.76 GB** | **≈ 1 GB** |

Quiet interned series and every sealed chunk live on disk, not in memory: in redb until their
span's handoff, in a block file after it. Open file handles are an LRU of 256, keyed by file
name (tier, span, rewrite number); retiring a file evicts and closes its handle before the
unlink. A query holds its file's handle while it reads, so a file retired meanwhile stays
readable to it (POSIX keeps an open file's data until the last close); a query over 400 days
opens its files one at a time. **Descriptors:** the scale target's ≈ 10,000 push connections
need a raised limit anyway, so the README and `docker-compose.yml` set `nofile` to 65,536, and
the open logs at `warn` when `RLIMIT_NOFILE` is below 4,096 plus `HUB_MAX_PUSH_SYSTEMS`.
`EMFILE` on a query's open is `StoreError::Io` for that query; on the block writer, a
block-write failure (§6). Mail series follow the same rule as any other. A mail scan
holds at most 256 decoded reports at once (each sealed report ≤ 512 KiB, RFC 0017): ≤ 128 MiB
transient, typically a few MiB. At 100 systems the
total is about 270 MB, almost all of it the page cache, which the README says can be lowered.
The performance test measures resident memory at the scale target, and it must land within 20%
of this table, or the table is corrected.

**Open time** at the scale target: loading the tails (≈ 630k × 0.6 KB ≈ 0.4 GB: the open chunks
and accumulators of the table above), replaying at most 15 minutes of `points_log` (≈ 70M
points), and reading each block file's header and summary (≈ 390 files with the defaults, ≤ 30 KB each, ≈ 15 MB;
never the full indexes, which are checked one index block at a time as queries read them), all
measured by the performance test. Target:
under 30 s at the scale target, under 1 s at 100 systems.

**Write volume** at the scale target, per day, before redb's copy-on-write amplification:

| Writer | Volume |
|---|---|
| `points_log` (~12 B per point) | ~80 GB |
| rotating tail flush (630k × ~0.6 KB every 15 minutes) | ~36 GB |
| sealed chunks (raw, minute, hour), into redb | ~20 GB |
| block files: each closed span written once, plus rewrites | ~20 GB |
| deletions (page rewrites of retention and purge) | ~5 GB |
| **Total** | **≈ 145 GB/day logical**, against ≈ 19.5 GB/day of new data kept (§5) |

Copy-on-write rewrites branch pages on each commit; the performance test measures physical
writes and records them. §1's spike measured 36 to 59 B written per point, dominated by these
commits and the same with or without block files (+6% with them at 10 s commits): block files
save space, not write wear. At small fleets the per-commit cost dominates: two `fsync`s a second
(2-phase commit), a few pages each, about 1–2 GB/day on a journalling filesystem. On SD cards
and eMMC the README advises `HUB_COMMIT_INTERVAL=10s`, accepting a 10 s durability window.

### 8. Configuration and the data directory

Parsed once in `main`, before anything opens. An invalid value refuses startup, naming the
variable, never the value:

| Variable | Meaning | Default |
|---|---|---|
| `HUB_DATA_DIR` | the hub's data directory | `./system-hub-data` (inside the image's `/app` volume) |
| `HUB_RETENTION` | global policy, e.g. `raw=24h,minute=14d,hour=90d`; omitted tiers keep their default (`events=` is 0011's) | the defaults above |
| `HUB_STORAGE_LIMIT` | cap on the bytes the store takes on disk (`hub.redb` plus block files): bytes with `k`/`M`/`G`/`T` suffixes (powers of 1024), a percentage of the volume (`80%`), or `none` | `80%` |
| `HUB_MAX_SERIES` | active series in total | 1,000,000 |
| `HUB_MAX_SERIES_PER_SYSTEM` | active series per system; interned series per system are ten times this | 1,500 |
| `HUB_COMMIT_INTERVAL` | the group commit: the durability window; `100ms` to `60s` | `1s` |
| `HUB_STORE_CACHE` | redb's page cache, bytes with suffixes; at least 16 MiB | `256M` |
| `HUB_STORE_COMPACT` | `1`: compact at start if over 1 GiB and 25% reclaimable (§5) | unset |
| `HUB_CLOCK_REWIND` | the `last_issued` value `/api/storage` shows: rewinds a far-future clock at start (§2); one-shot | unset |
| `HUB_STORE_FORGET_BLOCK` | `<tier>/<span start>` of a block file that is missing or corrupt: its row is deleted at open and that span's history is lost (§6); one-shot | unset |

The admin token is 0012's, the secret key 0011's, `HUB_MAX_PUSH_SYSTEMS` 0008's, `HUB_LISTEN`
0015's, and `HUB_STATIC_DIR` the prerequisite's.

**Opening, in order**, in `main`, after configuration and before any other state:
1. the data-directory checks below (they create nothing when a check fails);
2. **the directory lock**: an exclusive, non-blocking `flock` on `HUB_DATA_DIR/hub.lock`
   (created 0600, `rustix::fs::flock`, held for the process's life) before any other state; a
   second hub on the same directory gets `EWOULDBLOCK`, refuses to start naming the file, and
   touches nothing. redb's own file lock is still taken when the file opens. Under the lock,
   every `*.tmp` under `blocks/` is deleted (no row ever names one) **before redb opens**, since
   a crash in the middle of a raw handoff or rewrite (a fail-stop on `ENOSPC` among them) can
   leave ≈ 0.55 GB that would otherwise keep redb's repair, or the floor check, short of space;
3. when `hub.redb` exists, **open it**; a redb open that fails for lack of space exits as the
   floor check does. When `hub.redb` is absent and `blocks/` holds any `.blk` file, the hub
   refuses to start, naming the directory (an empty store must not adopt or delete history it
   doesn't know). Then **compaction**, when `HUB_STORE_COMPACT=1` asks for it (§5), on that
   open database: **before** the floor check, since a data volume full of reclaimable
   `hub.redb` is the case it exists for;
4. the **reconciliation** of the block files (§6 step 2a), also before the floor check, since
   the files a crash left (`pending_unlinks`, a handoff or rewrite before its commit) can hold
   as much space as a `.tmp`;
5. the **floor check** (§6): `statvfs` on `HUB_DATA_DIR`; below the floor the hub exits before
   creating anything. When `HUB_CLOCK_REWIND` matches, the floor is raised by twice the
   length of the files the rewind will bring back (§2). The storage cap's retirements run at the first retention pass, after this
   check: with the default cap the store is at most 80% of the volume, so a volume below the
   floor is full of something else, which the store must not delete to make room (the owner's
   no-degraded-mode decision, §6);
6. when `hub.redb` is absent, create it with mode 0600 (`OpenOptionsExt::mode`) and open it
   through `Builder::create_file`, so redb's file never takes the umask's mode. **A new file
   is created only when the system clock is past the binary's build date** (a compile-time
   constant); otherwise the hub exits naming the clock, so a host that boots before NTP (no
   real-time clock) can't create a retention clock decades in the past (§2). An existing file
   keeps its persisted clocks. `blocks/` and its tier directories are created with mode 0700.
   Everything 0011 writes beside the store (the generated key) happens after `hub.lock` is
   held, so two hubs can't both generate a key.
   (The steps of §6's recovery that follow the reconciliation run after this list.)

**Data-directory safety** (A05), with no `unsafe` (`rustix`'s safe `process` and `fs` APIs for
`geteuid`, `getegid`, `getgroups` and `statvfs`), run first, so no file is created in a
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
- `blocks/` and its tier directories pass the same checks as `HUB_DATA_DIR` (not a symlink,
  owned by the hub's euid, not group- or world-writable unless in the `fsGroup` shape), so
  neither a link to another volume (where the cap's `statvfs` would measure the wrong one) nor a
  writable directory is followed; block files are opened with `O_NOFOLLOW`;
- `.gitignore` and `system-hub/.dockerignore` gain `system-hub-data/` and `*.key`.

### 9. Store API

```rust
impl Store {
    /// Checks the directory, opens `hub.redb` (§8), re-derives memory (§6), starts the writer.
    pub fn open(data_dir: &Path, options: StoreOptions) -> Result<Store, OpenError>;

    pub fn append(&self, system: &SystemKey, generation: Generation, points: &[(MetricName, ValueKind, i64)]) -> AppendReport;
    /// Reads closed spans from block files, spans not yet handed off from `chunks`, and the
    /// unsealed points from the head; skips dead chunks (§5).
    pub fn query(&self, series: &SeriesKey, q: Query) -> Result<Series, StoreError>;
    pub fn metrics_of(&self, system: &SystemKey, generation: Generation) -> Vec<MetricName>;

    /// Runs `f` inside the writer's transaction. A transaction that wrote is answered after the
    /// commit that holds it and the commit hook (`Durable` forces that commit at once); a
    /// write-free one as its `Answer` says (§6). `Abort` is answered at once.
    pub fn transact<T: Send + 'static, A: Send + 'static>(&self, class: Commit, f: impl FnOnce(&mut CatalogTxn) -> Result<(T, Answer), Abort<A>> + Send + 'static) -> Result<T, TransactError<A>>;
    /// A redb read transaction over the catalog tables (MVCC: never waits for the writer).
    pub fn read_catalog<T>(&self, f: impl FnOnce(&CatalogRead) -> T) -> Result<T, StoreError>;
    /// Called on the writer thread after each commit, in commit order, with that commit's
    /// catalog changes (0011 maintains its in-memory Registry with it).
    pub fn on_commit(&self, hook: Box<dyn Fn(&[CatalogChange]) + Send + Sync>);
    /// Called by each retention pass on the writer thread as its step 7 (§5), after its own
    /// steps, to run the hub's own pruning (0011's receipt prune) in bounded transactions.
    /// The store calls it at most 10 times per pass and stops at `Done`; a hook answers
    /// `PassProgress::More` only after a transaction that deleted something.
    pub fn on_retention_pass(&self, hook: Box<dyn Fn(&mut RetentionPassTxn) -> PassProgress + Send + Sync>);

    pub fn stats(&self) -> StoreStats;
    pub fn close(&self) -> Result<(), StoreError>;
}
pub enum Commit { Durable, Batched }
/// When a write-free transaction is answered: after the commit holding the transaction it read
/// (at once if that transaction held no uncommitted catalog write by an earlier `f`; appended
/// or staged points don't count, RFC 0008 §3), or at once.
pub enum Answer { AfterCommit, AtOnce }
/// An `f`'s own refusal to go on, before its first write (§6): a value the caller defines, so a
/// transaction that never aborts uses `Infallible`.
pub struct Abort<A>(pub A);

/// Inside `transact`: 0011's tables, plus the store-owned operations.
impl CatalogTxn {
    pub fn tombstone(&mut self, system: &SystemKey, generation: Generation);   // SystemGone from here on
    pub fn set_retention(&mut self, system: &SystemKey, change: RetentionChange) -> Result<RetentionOutcome, RetentionError>;
    /// The hub time of this transaction: the writer advances `HubClock` once before `f` runs
    /// and persists it with `meta/clock` in the same commit, so no stored hub time
    /// (`received_at`, `last_seen_active`) is later than the committed `last_issued`.
    pub fn now(&self) -> u64;
    /// A change for the commit hook that writes nothing to redb (a mail overdue mark), held in
    /// memory and delivered with the commit's `CatalogChange`s; dropped if `f` aborts.
    pub fn notify(&mut self, change: CatalogChange);
    /// Stages points in this transaction (§6): applied to the head only if `f` returns `Ok`,
    /// committed with the catalog writes. Checked against the head as staged so far, so a
    /// report's snapshots append in order. `SystemGone` if the generation is tombstoned.
    pub fn append(&mut self, system: &SystemKey, generation: Generation, points: &[(MetricName, ValueKind, i64)]) -> AppendReport;
    // … 0011's typed-byte tables: get, range, insert, remove
}

pub struct Query { range: TimeRange, tier: Tier, order: Order, limit: Limit }
pub enum Order { Earliest, Latest }          // which end `limit` keeps; results always ascending
pub struct Limit(u16);                       // 0 ..= 10,000; 0 returns nothing, as today

pub struct AppendReport { at: u64, accepted: u16, rejected: Vec<(MetricName, Rejected)> }
pub enum Rejected { NotAfterLast, SeriesCapReached, KindMismatch, OutOfDomain, InvalidName, SystemGone }
pub enum StoreError { Closed, Failed, Io(IoKind), Corrupt(BlockRef) }   // Corrupt: a block file missing, or a chunk or index block failed its CRC
/// What `transact` answers when it doesn't return `T`: `f`'s abort (answered at once, nothing
/// written), or the store's error.
pub enum TransactError<A> { Aborted(Abort<A>), Store(StoreError) }
```

- **`SystemGone`**: a `tombstone` committed in a transaction takes effect on the writer thread
  **at that commit**; a later `append` for that generation is refused there. The writer is the
  only thread that touches the head, so there is no window between the catalog and the store.
- **Queries** read committed chunks in a redb read transaction, and copy the series' unsealed
  points from the head under that series' shard lock (16 shards, taken only by the writer and
  by queries, never nested), then merge. A query never waits for a commit. The same read
  transaction gives it the `blocks` rows, `Policies`, the tombstones and the retention clock
  that filter dead chunks (§5), so no hub lock is added. A span in a block file is read through
  the handle LRU after its row: a rewrite or a retirement can unlink the file in between, and
  the open then fails with `ENOENT`; the query retries once in a fresh read transaction (which
  sees the swapped row, or none), and a second `ENOENT` for a row it still sees is a missing
  file (`Corrupt`).
- **Lock order, in full** (with 0008 §3, 0011 and §10). The hub's locks, in their one order:
  **presence → Registry → `live_status` | `live_metrics` → admission → `live_applications` →
  head shard**. `live_status` and `live_metrics` are never nested in each other. Rules:
  - **No hub lock is held across a store call, with one exception**: RFC 0009's admission lock
    is held across `append` (lock the admission, `decide`, `append`, *record*); nothing else is
    taken under it, and the writer never takes it, so a caller waiting
    on the writer under it can't close a cycle.
  - **The Registry guard is never held while taking presence or while re-taking the Registry.**
    A path that enumerates systems (the push sweep, the overdue sweep, 0012's offline-candidate
    list, the SSE summary) copies `(id, generation, registered, name, enabled)` and drops the
    guard before taking anything else.
  - **The commit hook** takes the Registry's write lock, updates it, **releases it**, and only
    then takes `live_status`, `live_metrics` or `live_applications` one at a time (mail status,
    evictions after a delete, comparing generations). It never takes presence or an admission
    lock, and never holds two hub locks at once.
  - **Every path that holds two**, with its order: a claim or an end (presence → Registry read,
    the generation check; then, after dropping the Registry, `live_status` or `live_metrics`
    with a generation compare); a fill, `keep_live_metrics` (Registry read → `live_metrics`);
    `append_round`'s admission lookup and its *show* step (Registry read → `live_applications`,
    the guard held across the insert; the admission lock is released before *show*, RFC 0008
    §3). No other path nests.
  - A head shard lock is a leaf, taken only by the writer and by queries.
  No test can show a lock order; review must.

**`/api/storage`** is owned by this RFC. It answers `StoreStats` as JSON: bytes on disk per
tier and in total (block files and `hub.redb`), block files per tier and their tombstoned bytes, the handoff backlog per tier, unreadable files,
`hub.redb`'s length and reclaimable bytes, block-write failures and corrupt chunks, the cap and its source, the volume size
and free space, the open-time floor; active and interned series and the caps; points per
second, the commit interval, the last commit's duration; hub time, system time, `last_issued`,
the retention clock and how far it trails hub time, any clock hold; counters per `Rejected`
reason (`NotAfterLast` split by cause) and cap deletions. RFC 0012 adds its
refusal counters, and RFC 0008 its registry counters. It exposes no key, token or path beyond
tier names, and is an open read like the rest of the hub API.

### 10. Hub integration

`storage/` replaces `db.rs`, with 0011 in the same step.

- **`append_snapshot(system, generation, snapshot)`** replaces `store_metrics`. It maps the
  snapshot to points through the one snapshot rule (RFC 0007 §1 where implemented, else today's
  `metric_points`), including §3's container-mount and name rules. An all-`SystemGone` report
  ends the push connection.
- **`store_mail_scan(reports)`** replaces RFC 0017's `Database::store_mail_report`: **one
  `Batched` transaction per scan** (up to 256 opened reports, in the scan's oldest-first order),
  on the writer thread. For each report, `f` runs 0017 §6's steps (0011 §7: every refusal decided
  by reads before that report's first write, so one report's duplicate or transport mismatch
  skips that report alone). **Points are staged at the end of `f`, once per system**, for the
  system's newest accepted `Newest` report of the scan chosen by `newest_of_scan` (§2), after every
  report's refusals are known, with `CatalogTxn::append` (§2 *Mail points*), **and `mail_newest`
  is written then too, once per system, for that same choice**; the other `Newest` reports of
  that system write their alert records only. The outcomes are returned when the commit holding
  them has landed (within one commit interval), and only then are the scan's messages deleted;
  a fail-stop before that leaves them in the mailbox for the next scan, where any that did
  commit are duplicates. One commit per scan instead of one per message keeps a busy mailbox
  from multiplying commits. The round of a system's newest `Newest` report of the scan goes
  through `append_round` after the commit, as 0017 does.
- **The scan's messages are named in the log on a store failure**: when the scan's transaction
  answers `TransactError::Store(e)` for any `e` but `Closed` (a commit that hit `ENOSPC` answers
  `Io`), the intake logs their Maildir names at `error`, and `main` waits for the intake's
  in-flight call before it exits, so a message that fails the store on every restart can be found and moved aside (§6).
- **Freshness uses the system clock**, as 0017's `receipt::fresh` does today, never hub time: a
  hub time held in the future (§2) would otherwise make every report look older than the
  receipt window and refuse them all.
- **`append_round(system, generation, round, decide, on_stored)`** replaces RFC 0009's
  `Database::store_round`. It clones the system's `Arc<Mutex<ApplicationAdmission>>` out of
  `live_applications`, **drops the map guard**, locks the admission, calls `decide` (the pure
  `admit`), then `append`, then *record* and *show* (RFC 0008 §3). A round **with points** of
  which **none** was accepted makes the round `NotStored(reason)`: *record* isn't called, so the
  round isn't recorded in `RecentRounds`. A partly accepted round is `Stored` with its rejections
  counted, and so is an **empty round** (an agent whose last application was removed sends one),
  which is recorded and replaces `shown`, as today. Two queued rounds in one second after a
  stall (the pace allows two) leave the second all-`NotAfterLast` and so `NotStored`: `shown`
  stays one round behind until the agent's next round, one scrape interval (15 s by default, up
  to an hour), as §2 accepts for queued snapshots.
- **Volatile system status stays in memory**, typed, and is written into `meta/hub/live_status`
  by the rotating flush (a slice sized by elapsed time, like tails), so a restart recovers it:

  ```rust
  pub struct LiveStatus {
      generation: Generation,          // a write for another generation is ignored (0011)
      liveness: Liveness,
      last_contact: Option<u64>,       // hub time of the last successful frame or poll
      last_error: Option<String>,      // ≤ 256 bytes
  }
  pub enum Liveness { Online, Offline { since: u64 }, Unknown }
  ```

  - `Liveness` is one enum, so "online with an offline time" can't be represented.
  - **Entries are created only by the registration hook** and removed by the delete's hook; a
    status write for an id with no entry, or for another generation, is dropped, so a late
    frame or poll can't recreate a deleted system's status (RFC 0008 §3).
  - Which push connection is current is **not** flushed: it means nothing across processes.
    It stays in RFC 0016's in-memory `PushPresence`, with each entry's liveness (its lease).
  - **Set only by contact and by transitions.** A successful frame or poll sets `Online` and
    `last_contact`. A failed poll, the end of the current push connection (RFC 0016 §2), and
    RFC 0016's disconnection sweep (whatever RFC 0016 §4's sweep table marks, counting the
    120 s from the store's open, or from a system's registration if later, RFC 0008 §3; a new push id starts `Unknown`, RFC 0016 §2) set `Offline { since: now }` unless already offline. `last_contact`
    is a display value, not a liveness rule: a connection that pushes every 300 s is online. Shutdown
    touches nothing.
  - **Mail systems** (RFC 0017 §7). **Mail `LiveStatus` changes are applied by the commit hook**,
    on the writer thread, in commit order, never by a caller after the commit: a scan's commit
    reports each `Newest` report's system, and the hook sets `Online` and `last_contact` to the
    **hub time at which the report was accepted** (`received_at`), never its agent-stamped
    `created_at`. A `Backfill` report touches neither.
  - **The overdue sweep** (every 60 s on hub time, only while the mail intake is on and the store
    skipping disabled systems) reads each mail system's `MailNewest` (0011 §7) and
    decides with `mail_status`, which measures from **`received_at`** (owner's decision,
    2026-10-03, a change to 0017 §7): *overdue* means "no report has arrived for 3 intervals plus
    15 minutes". **One write-free transaction per sweep** carries every candidate it read (overdue
    systems not already `Offline`, and on-time `Unknown` ones) with the receipt each read; `f`
    re-reads each `MailNewest` and, for those still naming that receipt, `notify`s an overdue
    mark or an on-time promotion. The hook applies them in commit order with the scans: `Offline`
    with `since = min(now, received_at + 3 × interval + 15 min)` and `last_error = "mail
    overdue"` (as 0017 does; the next `Online` clears it), **and evicts the system's live
    metrics** (as 0017 does), comparing the entry's generation, after releasing the Registry
    lock (§9); a promotion sets `Online` without touching `last_contact`. A report committed in
    between wins, as 0017's single-statement update does today. The transaction is
    `AfterCommit`, answered at once when no uncommitted catalog write lies beneath it.
  - **After open, the sweep waits for the mailbox's backlog**:
    it runs once a scan, whose listing succeeded, finds no file in `new/` delivered before that
    moment that it could read (Maildir names start with their delivery time; an empty `new/`
    counts; an unparseable name counts as delivered before; a file the scan can't read is
    counted in `/api/storage` as stuck and doesn't hold the gate; a failed listing keeps it
    closed). Whatever is left, the gate opens 3 × the longest mail interval plus 15 minutes
    after that moment. So a backlog draining oldest first can't mark reporting systems offline, and a
    silent mailbox doesn't keep the sweep off. A relay's own deferred queue can still deliver
    late after that; the systems it holds back flap once, and the retention clock's lag after
    an outage keeps them from becoming offline candidates at once (RFC 0012 §2).
  - **A hub time held in the future** makes `now − received_at` negative, so no mail system is
    overdue for the length of the hold; the hourly hold `error` and `/api/storage` say so, until
    `HUB_CLOCK_REWIND` (§2).
  - The API's `last_seen` is rendered from `last_contact` (RFC 3339), empty when there is none.
    *Behaviour change: today a push system's `last_seen` holds its agent's uptime display, and a
    failed poll updates it; a mail system's holds its newest report's `created_at`
    (`LastSeen::ReportedAt`, RFC 0017 §7), which becomes the time the hub accepted it.*
  - At open every system has `Unknown` (RFC 0016 §4's startup reset) and a `last_contact` that
    is the latest of the flushed value (up to one flush rotation old) and the newest timestamp of
    its series (durable to the commit interval through `points_log` and tails); for a mail
    system, its `MailNewest.received_at` alone (its application rounds are stamped after it). A
    polled system not heard from within **120 s** becomes `Offline { since: last_contact }`, or
    the open time if it has none. A push system goes offline only through its current
    connection's end or RFC 0016's disconnection sweep; a mail system only through the overdue
    sweep. **A transition from `Unknown` to `Offline` uses `since` = when the system went silent
    by its own rule**, not when the sweep ran: `last_contact` for a polled or push system, and
    `received_at + 3 × interval + 15 min` (the moment it became overdue) for a mail system, never
    later than `now`, and `now` when there is no contact. A transition from `Online` uses
    `since: now` for a polled or push system and the overdue moment for a mail system, which is
    `now` for a live sweep. So a restart neither resets nor
    stretches an offline age (RFC 0012 §2). One pure function, `offline_since(liveness,
    silent_since, now)`, decides it for every source. With the mail intake off, no sweep runs and
    mail systems stay `Unknown`.
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
  and the retention clock, retention enforcement and overrides, the hot part in redb and the
  block files with their handoff and rewrites, the storage cap, the open-time floor, the
  data-directory checks. It knows no hub term but "system key" and "generation", and
  stores 0011's catalog values as opaque bytes.
- **Fleet History**: `storage/` replaces `db.rs`; `RetentionPolicy` parsed from `HUB_RETENTION`;
  the metric-name → kind function and the container-mount rule in the snapshot → points
  function; retention decisions as pure functions (closing "Retention policy is decided inside
  `Database::insert_metric`").
- **Fleet Registry**: volatile status becomes the typed `LiveStatus`; the rest is 0011's and
  0008's.
- **Ingestion**: `append_snapshot`, `append_round`, and `store_mail_scan` (RFC 0017's intake on
  the store, one transaction per scan, one point per series per system per scan, at hub time).
- **Glossary**:
  - added: series, series table, active series, interned series, reactivation, generation, value
    kind, value scale, domain range, tier (raw, minute, hour), rollup, span, chunk, tail, points
    log, group commit, commit interval, writer thread, hub time, clock hold, clock rewind,
    retention clock, retention policy, override, tier setting (global or fixed), pending
    shortening, storage cap, the open-time floor, compaction, series cap, container
    mount, last contact (hub time, for every source), hot part, block file, block summary, span
    handoff, handoff backlog, live and dead chunk, tombstoned bytes, rewrite, retirement,
    unreadable block file;
  - changed: **retention** (per tier, globally and per system; no longer per metric), **metric
    point** (stamped in hub time), **system status** (the typed `LiveStatus`), **last seen**
    (the last successful contact; for a mail system, when the hub accepted its newest report),
    **mail system** (at most one point per series per system per scan, the newest current report's newest sample, at hub time; overdue
    when no report has *arrived* for 3 intervals plus 15 minutes; its offline age counts from
    that moment),
    **snapshot** (container mounts are not metric points), **backfill report** (RFC 0017: its
    alert records are stored, no point).
- **Published contracts**: push frames, poll responses and the handshake are untouched (RFC
  0008 adds handshake answers). Metric responses gain optional `min`/`max`; `/metrics` accepts
  `resolution` and `until`; limits are clamped; `last_seen` changes meaning; container-mount
  disks disappear.
- **Mixed-version fleet**: agents are unaffected. Mail agents keep sealing the same report
  (`mail-report.v1`); the hub keeps only each report's newest sample (one report per system per
  scan) and nothing of a backfill report. A hub downgrade after upgrading finds the old
  SQLite file as it was left, without anything collected since (Rollout).

## Alternatives considered

- **Our own engine** (the first three drafts): a per-shard WAL, incremental checkpoints, head and
  head-index files, immutable block files, a recovery order, a clock file with pending steps,
  faults, suspension and resume. Three `rfc-adversary` passes each found data loss, duplication
  or a history wipe in it (Review). Rejected by the owner for redb. §5's block files keep only
  the part of it that never changes after a commit, with redb's transaction ordering the
  handoff.
- **Other embedded engines.** `sled` (its own docs describe it as beta, and it keeps large
  in-memory structures), `fjall` (an LSM tree: good write amplification, but compaction runs
  in background threads we'd have to bound, and deletes are tombstones until compacted),
  `rocksdb` (C++, `unsafe` FFI, a large dependency). redb is pure Rust with no dependencies, a
  stable file format, and the smallest surface to review.
- **One redb entry per point.** ≈ 30 B of entry cost per point, twenty times the codec's size.
  Chunks of up to 1 KiB spread it.
- **Every closed span in redb** (the previous revision). One engine and one protocol, but §1's
  spike measured ≈ 1.6–1.7× space for closed spans, with chunks sealing scattered over the span,
  and space freed by retention never returning to the volume while the hub runs. On the small
  disks this store is meant for, that cost decided it (owner, 2026-10-06).
- **Block files for the hot part too** (a log and tails of our own, as the first three drafts
  had). It would cut write wear (the spike's 36–59 B per point are the hot part's commits), but
  it is where those drafts lost or duplicated data; redb keeps it.
- **A block file per system, or per shard of systems.** Erasing a deleted system would rewrite
  only its own files, but at 10,000 systems × ≈ 390 spans that is millions of files, and small
  fleets would get thousands of tiny ones. Rewriting a shared file at 25% tombstoned bytes bounds the
  waste instead.
- **Series-first keys** (`(tier, series, span)`). Measured 1.8–2× space for random inserts.
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
  - the storage cap on by default, on the real bytes on disk; a hub that won't start below the
    free-space floor;
  - the span handoff: a block file durable before its row, reconciled at open, so no crash loses
    or duplicates a span;
  - **deleted systems' data in block files is unreadable at once but erased only when its file is
    rewritten (25% dead) or expires** (up to the tier's longest retention), a weaker promise
    than the previous revision's one pass; stated in the README and 0011 §3;
  - domain and name refusal at the edge and in the store, `i128` sums;
  - atomic transactions with a stated content, and fail-stop on a writer panic or I/O error;
  - a per-system interned-series cap, so one system can't starve the rest.
- **A05 Security Misconfiguration:** directory, ancestor, ownership, group and symlink checks
  before any file is created, on `HUB_DATA_DIR`, `blocks/` and its tier directories; block files
  opened with `O_NOFOLLOW` and named from typed keys; `hub.redb` and block files created 0600 in
  0700 directories; redb's
  file lock behind `hub.lock`; fail-closed configuration; one-shot `HUB_CLOCK_REWIND` and `HUB_STORE_FORGET_BLOCK`;
  no new store created before the binary's build date; the data directory and key ignored by git and
  Docker.
- **A06 Vulnerable Components:** `redb` (pure Rust, no dependencies), `crc32c` (pure Rust) and
  `rustix` (`fs`, `process`); `proptest` as a dev-dependency. `rusqlite` and its bundled C library are removed.
  Run `cargo audit` on the hub workspace's lockfile.
- **A07:** N/A here (0012).
- **A08 Software & Data Integrity Failures:** redb's checksummed pages and atomic commits, with
  **2-phase commit** because agent-controlled data reaches the file (§1's cited attack on
  1-phase commits); block files carry a CRC-32C per chunk (over its key and bytes), per index
  block and over header and summary, so bit rot fails one span of one query, never the store: a
  damaged or missing file opens unreadable; a block file is never renamed over an existing
  one; our codec's and the block format's version bytes
  refuse an unknown version; decoders are total. Neither checksum resists someone who can write
  `HUB_DATA_DIR`, which the directory checks guard.
- **A09 Logging & Monitoring Failures:** clock holds, rewinds, retention-clock lag, cap and
  series-cap refusals, the cap's retirements, block-write failures and the handoff backlog,
  corrupt and missing block files, `HUB_STORE_FORGET_BLOCK`, fail-stops and compaction hints are
  logged and
  counted in `/api/storage`. No log line carries a token or a value.
- **A10 SSRF:** N/A. The store makes no network requests.

**OWASP API Security Top 10 (2023)**

- **API1:** `NotAfterLast` would let a token holder displace a polled system; RFC 0012 reserves
  push ids. Push connections sharing one self-asserted id remain the standing API1 risk. A mail
  key holder writes only its own system's series (one key per id, RFC 0017 §3), at hub time.
- **API2:** N/A (0012).
- **API3:** `StoreStats` exposes counts, sizes and clock state only.
- **API4:** active and interned series caps (per system and total); `limit` clamped to 10,000;
  `auto` resolution; the bounded writer channel; the storage cap and floor. A mail flood on a
  shared volume is a stated limit (§6).
- **API5–API7:** N/A here.
- **API8:** see A05.
- **API9:** `/metrics` gains `resolution` and `until`; `/history` keeps `since`; the clamp and
  the new `last_seen` are documented; `GET /api/storage` is new; §8's ten variables go into the
  README.
- **API10:** values checked against their kind's domain at the edge. No agent-chosen time stamps a
  point: push and mail points alike are stamped at hub time; a mailed `created_at` only decides
  whether a report is current, and a slow clock is counted and warned, never silent (§2).

## Testing plan

Test-first, per `CLAUDE.md`. `hub-store` is synchronous Rust over a temporary redb file, so
nearly all of it is unit-testable. The crash tests target **our** invariants on top of redb
(redb's own atomicity is its test suite's job).

- **Codecs** (property tests with `proptest`): round-trips for 1 and 240 points, 1 and 60
  buckets; every timestamp step and value escape at its boundaries; `i64::MIN`/`MAX` steps;
  delta-of-delta for `Monotonic`; arbitrary bytes never panic nor allocate past a bound; an
  unknown format byte refused.
- **Size budget**: seeded per-kind generators with timestamp models, each asserted against its
  ceiling, and the weighted totals; plus the on-disk factors for a day of the mix: the hot
  part's through redb's `stats()`, block files' from their lengths.
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
  time (advances at most 20 minutes); the system clock back after that (stops at the system
  clock while hub time holds); the first pass after open (unchanged), the second with
  `Measured(600 s)` (+1,200 s), so a two-week downtime catches up at twice real time from the
  second pass; a first open with no `meta/retention_clock` (created at `min(hub_now,
  system_now)` in the open's first transaction, never 0); never past hub time or the system
  clock.
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
- **Series table and caps**: the counter and the first point in one transaction; the per-system
  and global active caps, exactly at and one past; reactivation counted as new; the per-system
  interned cap, exactly at and one past, with other systems unaffected; GC removing a series
  once its last chunk expired.
- **Block files and the span handoff**:
  - a child process killed at every step of a handoff (after the temporary write, after the
    rename, after the directory `fsync`, after the commit): at reopen, each span's chunks are in
    exactly one of `chunks` or a file, queries return every point once, and orphans and
    temporary files are gone; the same for a rewrite, and for a retirement between its commit
    and the unlink (the `pending_unlinks` entry finishes it); after every kill, every
    `block_generations` key has its `blocks` row;
  - mixed backups refuse startup naming the file and delete nothing: `hub.redb` older than a
    rewrite (its row at `r1`, `S.r1.blk` gone, `S.r3.blk` present), and `hub.redb` taken while
    S was still open (part of S in `chunks`, `S.blk` whole); a row-less file no rule explains
    refuses the same way; `hub.redb` absent with files in `blocks/` refuses;
  - a second hub on the same directory refuses at `hub.lock` and deletes no `.tmp`; a raw `.tmp`
    left by a fail-stop on `ENOSPC` is deleted before redb opens;
  - a kill inside each in-flight window (a handoff after its rename while pass step 3 would
    expire S, a rewrite after its rename while the cap would retire S): the pass and the cap
    skipped S, and the reopen deletes the orphan by rule (a) or (b) and starts;
  - an orphan `.tmp` of 0.55 GB that holds free space below the floor is deleted before the
    floor check, and the hub starts; a rename that succeeded before a failed directory `fsync`
    leaves no row-less `S.blk`;
  - `HUB_STORE_FORGET_BLOCK` on a corrupt file, then two restarts: the hub starts both times, the
    file is gone and its `block_generations` entries with it;
  - a delete through a seam between the block writer's snapshot and its commit (handoff and
    rewrite): the row's tombstoned bytes include the deleted generation's; a rewrite whose row
    was retired meanwhile writes no row and its file is unlinked;
  - one damaged file and one file at 30% tombstoned bytes: the second is rewritten;
  - the handoff never replaces a file: an existing `S.blk` makes it a block-write failure;
  - a quiet series' open tail in a closing span is sealed and handed off;
  - a row whose file is missing, or whose header or summary fails its CRC, opens with that span
    unreadable (`Corrupt` per query, counted; other spans answer); `HUB_STORE_FORGET_BLOCK`
    naming it deletes the row;
  - a chunk whose bytes are flipped, an index block with a flipped byte, and an index entry whose
    `SeriesId` is flipped to another existing series each fail that span of the query with
    `Corrupt`, counted, and never serve another series' chunk, while other spans and other
    series answer;
  - the format: header, index and trailer round-trip; an unknown format byte refuses the file;
    the sparse index finds the first and the last series and one absent;
  - a query reading a file while retention unlinks it completes; a query whose row is read
    before a rewrite's swap or a retirement and whose open follows the unlink retries once and
    answers from the new file, or nothing; 300 files under an LRU of 256 handles answer;
  - a block-write failure (an injected `ENOSPC` on the file) leaves the span in redb, served, and
    retried at the next pass; the store keeps running; with the backlog held at three spans
    (`raw=1h`, a 1-hour span past retention while still waiting), its chunks in `chunks` expire
    on time (pass step 3); separately, a fourth span in the backlog fails the store stop;
  - with `raw=1h`, a raw span already expired at its close is deleted from `chunks`, not handed
    off;
  - the open reads only headers and summaries: with synthetic files for 400 days of hour spans (an override past the 30-day default),
    14 days of minute and 25 h of raw, open time stays within §7's target.
- **Retention on files**: `chunk_liveness` as a table (live, expired at the boundary and one
  second past, tombstoned, an unmapped `SeriesId`, an override longer and shorter); a span past
  every system's retention is retired whole; a deleted system's chunks are unreadable at once;
  a file whose tombstoned bytes reach 25% is rewritten without them, and one at 24% is not; a
  handoff and a rewrite after a delete write none of its chunks; a system with `raw=30d` past
  the global 24 h keeps a rewritten file of its own chunks; one rewrite per pass, on the block
  writer thread; series GC keeps a series while a file at or before its last span exists; the
  tombstone is kept until `block_generations` names it in no file; lengthening a policy makes
  expired chunks still on disk readable again, and a delete's never.
- **Storage cap on files**: over the cap, the oldest raw then minute files are retired, and
  free space by `statvfs` rises once the cached handle is closed (the LRU holding it); deletes
  stop as soon as the total is under; hour files are never retired by the cap.
- **First open**: a system clock before the build date refuses to create `hub.redb`; an
  existing file opens whatever the clock.
- **Retention**: global expiry exactly at each boundary; a longer and a shorter override; a
  pending shortening before and after its delay, re-armed with the full delay after a restart;
  `TierSetting::Global` enforced and pending, following a changed `HUB_RETENTION` after a
  restart; an override of all-`Global` tiers with nothing pending removed; the persisted form
  round-trips one optional entry per tier; `TierPeriod` out of a tier's bounds unconstructible
  (a table of each bound and one unit past);
  the pass's transactions bounded to 50,000 deletions; the purge removing a deleted system's
  chunks, tails and log entries, resuming from its cursor after a restart; tier minimums and
  maximums refused.
- **Mail points**: a current `Newest` report of 60 snapshots stores one point per series, the
  newest snapshot's, at hub now, whatever its samples' `collected_at`; a report whose
  `created_at` is one interval + 15 min + 1 s old stores no point, no round and no live metrics,
  but is `Online`, writes `mail_newest` and is counted and warned once an hour; a relay always
  30 minutes late gives no flapping; a `Backfill` report
  stores no point and all its alert records; a mail series behaves as any series under the
  sweep, caps and retention; **three `Newest` reports of one system in one scan store one point
  per series, with the newest report's values** (and a tie on `created_at` broken by `seq`), and
  the same report feeds live metrics, info, round and `mail_newest`; **a backlog spanning
  several scans stores no point until a current report arrives**; with an unheld clock, normal
  scans never count `NotAfterLast` for a mail point (a clock hold may).
- **Transactions and aborts**: an `f` that refuses by reads (a duplicate receipt, a transport
  mismatch) writes and stages nothing; **an `f` that stages points (including a new series) and
  then returns `Abort` fails the store stop** (a child-process test: at reopen there is no series
  row, no `id_counter` advance, no log entry and no head point); a scan transaction holding three
  reports where the second is a duplicate commits the first and third; the scan's messages are
  deleted only after the commit, and a child killed before it leaves them in the mailbox.
- **Overdue marks**: an overdue mark evicts the live metrics; a report committed after the
  sweep's read keeps the system online and its live metrics; one sweep over 2,000 candidates is
  one transaction; a stuck file in `new/` doesn't hold the gate, a failed listing does, and the
  time bound opens it; an overdue mark sets `last_error = "mail overdue"` and the next report
  clears it.
- **Disk full**: a fault-injecting backend failing a commit with `ENOSPC` fails the store stop,
  with the scan's message names logged at `error`; an open below the floor exits with its
  distinct status, naming the volume, and creates no file (the data directory may be created,
  mode 0700); at the floor it opens; `HUB_STORE_COMPACT=1` with a data volume below the floor
  compacts first and then opens, and a compaction open that fails with an I/O error is reported
  with its own I/O error first, then the volume and free space, under the floor's exit status;
  a retention pass right after open advances the retention clock by nothing, and
  a restart loop of 100 opens under a +1 year fault moves it by nothing.
- **Mail status**: the overdue sweep on `received_at` (on time, overdue, exactly at the bound);
  an agent clock 2 days slow stays `Online`; **a report committed between the sweep's read and
  its mark keeps the system `Online`** (a two-thread test through the writer gate); after a
  restart an on-time mail system goes from `Unknown` to `Online` at the first sweep, and a
  disabled one stays `Unknown`; the sweep waits for the backlog delivered before open and runs
  with an empty `new/`; a mail system overdue 5 minutes before a restart shows an offline age of
  about 5 minutes after it; a mail system with application rounds, restarted then silent, is
  marked overdue; `last_contact` at open is the latest of the flushed value and the newest
  series timestamp (a child killed 1 minute after a frame whose flushed status was an hour old:
  `since` is 1 minute before the kill), and `received_at` for mail; freshness on the system
  clock while hub time is held a year ahead (reports accepted, none marked overdue);
  `HUB_CLOCK_REWIND` brings every persisted hub time, the mail prune floor and every tail
  bucket back to the system clock (a series' last timestamp only down, never up), the
  straddling bucket's counts equal a recomputation from raw, a series whose last points were
  only in the log and whose tail sat in the previous span keeps them, a +2-day fault leaves no bucket after the system clock in
  any tier's current span, the bucket straddling the clock is reopened, and **a `SIGKILL` right
  after the rewind, before any tail rotation, reopens with no point after the system clock**
  (the log was emptied); a fault and rewind inside one hour give one hour bucket; **a +2-day
  fault with the spans holding the system clock already handed off, then the rewind**: those
  spans are back in `chunks` and their files retired, the next handoff writes a new file and
  replaces none, the counts equal a recomputation from raw, and every pre-fault point of the
  brought-back span survives a further seal in it; a rewind whose span's file is unreadable
  drops that span, counted, and the hub opens, then restarts twice and opens both times with
  the file gone; a brought-back file with one corrupt chunk skips that chunk, counted, and the
  rewind commits.
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
  refused by `hub.lock` (and redb's lock behind it).
- **Hub adapter and routes**: the metric tests of `db.rs` ported; resolution boundaries (`auto`
  without `since`; exactly at `limit × 2 s` and one second past; exactly at `limit × 60 s`;
  `since` at a tier's retention edge and one second older); `limit` 0, 10,000 and 10,001;
  `/history` with and without `since`; `append_round`'s `NotStored` and `Stored`; `LiveStatus`
  transitions, a write for another generation ignored, the 120 s rule after a restart, shutdown
  leaving statuses untouched; `offline_since` as a table (`Unknown` with and without a last
  contact, `Online`, already `Offline`) for the poll rule, the push sweep and the overdue sweep;
  a mail system's `last_contact` is the hub time of its intake for an agent clock 2 days slow,
  and a `Backfill` report leaves `LiveStatus` untouched; graceful shutdown with an open SSE client and push connection
  (a child-process test).
- **Performance** (an `#[ignore]`d test at the scale target, run and recorded in the change
  summary): ingest ≥ 100k points/s through the writer; a 24 h raw query of one series < 5 ms;
  the longest commit; open time; resident memory within 20% of §7; physical writes per day
  extrapolated from an hour; footprint per tier after a simulated day; open time with 400 days
  of synthetic block files.

## Impact on `docs/ARCHITECTURE.md`

- **Components**: the hub workspace and `hub-store` on redb; `storage/` replacing `db.rs` (with
  0011); `main`'s startup order (configuration, directory checks, store open, key, serve).
- **Data flow**: the `db.rs (SQLite)` box becomes `storage/ → hub-store (writer thread) → redb`;
  the mail intake's arrow goes through `store_mail_scan`, at most one point per series per system per scan.
- **Open questions** (mail): "a report delivered late adds its history" becomes "the hub keeps
  each report's newest sample only; a backfill report adds alert records only"; overdue is
  measured on hub time (`received_at`); a hub time held ahead hides overdue mail systems until a
  rewind.
- `README.md`'s mail section: the same sentences (its "A report delivered late adds its
  history"); that
  `MAIL_SAMPLE_INTERVAL` no longer shapes the hub's history; that the Maildir belongs on a
  volume other than `HUB_DATA_DIR`, with the quota and fail-stop-message limits of §6.
  ARCHITECTURE § Trust boundaries (mail) and § Open questions say the same.
- **Domain model**: the Fleet Storage context; Fleet History and Fleet Registry rows; the
  glossary per Domain impact.
- **Trust boundaries**: the data directory, `blocks/` and its tier directories (checks, modes,
  `O_NOFOLLOW`, `hub.lock` and redb's lock); block files trusted as far as redb's pages are (CRCs against bit
  rot, not against a writer of `HUB_DATA_DIR`).
- **Storage**: rewritten: `hub.redb` and its tables, the block files and their format, the span
  handoff and its reconciliation, live and dead chunks and rewrites, tiers and spans, the group
  commit and durability window, the points log and rotating tail flush, retention and the
  retention clock, the cap, the floor and compaction, memory, footprint and write volume.
- **Testing architecture**: property tests, the size budget, child-process crash tests of our
  invariants, the performance test.
- **Open questions**:
  - closes: retention inside `insert_metric`, non-pruned stale series, blocking SQLite calls on
    runtime workers, the per-frame cache refresh;
  - adds: points aren't tamper-evident; no online backup API (a copy takes `hub.redb` and
    `blocks/` together with the hub stopped: the open deletes only a provably redundant row-less
    file and refuses any other, and a row whose file is gone opens unreadable);
    a deleted system's chunks stay in block files until their rewrite or retirement (§5, the
    owner's decision); freed pages keep old bytes until reused or compacted; a
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
  - push agents re-register themselves on their next handshake (within seconds); mail agents
    with their next report (within one mail interval), since the receipts start empty too. Drain
    the mailbox before the upgrade (RFC 0017's order), so the new hub doesn't take up reports the
    old one already stored; a replay of a report from the last 7 days is accepted once by the
    new hub, and on the empty store such a replay is `Newest`: its old snapshot would show as
    current and the system online until the agent's next report. Draining first avoids it;
  - polled systems must be registered again, with the admin token (RFC 0012), with their URL and
    token;
  - the old `system-hub.db` is ignored. It can be kept for the old binary (a rollback runs the
    old hub on it, without anything collected since the upgrade) or deleted; while it exists
    the hub warns at every start that it holds agent tokens in plaintext (0011 §6);
  - a hub downgrade after upgrading is a rollback to the old file, as above.
- Push snapshot timestamps switch from agent time to hub time, and so do mail reports' points,
  of which only the newest sample is kept (§2, owner's decision).
- **Behaviour changes:** container-runtime and per-pod mounts disappear from disk lists and
  history; a mail report stores one point per series (its newest sample, at hub time) and a
  backfill report none (its alert records are kept), nor a report older than one interval plus
  15 minutes, which also shows no round or live metrics but is online and counted; a disk below the floor at open stops the hub from starting, and an
  `ENOSPC` on commit fails it stop; a mail
  system's `last_seen` is when the hub accepted its newest report, and it is overdue when no
  report has *arrived* for 3 intervals plus 15 minutes; the store caps itself at 80% of its volume; `last_seen` is the last successful
  contact; the hub restarts itself (Compose) after a store fail-stop.
- 0010 and 0011 are one implementation step; 0008 and 0012 follow; all four ship in one release.
- Implementation, each step through the full gate:
  1. codecs, value kinds and the size-budget test (recording measured budgets);
  2. the redb tables, the writer thread, the group commit, the points log and rotating tails,
     recovery, and the crash tests;
  2b. block files: the format, the span handoff, reconciliation at open, queries across files
     and redb, and their crash tests;
  3. the clock and retention clock, retention, overrides and pending shortenings, the purge,
     GC, caps, the storage cap, the open-time floor, compaction, directory safety;
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
| recovery duplicated sealed chunks; span close left block and head both claiming a span; `fsync` order (1, 2, 3) | CONFIRMED | **moot**: no head files, blocks or checkpoints; a seal inserts the chunk and updates the tail in one redb transaction; replay skips points not after a series' last timestamp (§6) *Reopened by the block-file amendment: block files and a span handoff exist again; its order and reconciliation are §6.* |
| `HubClock = max(system, last)` wiped history on a forward step and stopped ingestion on a backward one (1) | CONFIRMED | **resolved differently**: hub time keeps that rule, but retention acts only on the guarded retention clock, which can't outrun real time by more than 2× nor pass the system clock (§2) |
| catalog `fsync` per frame (1) | CONFIRMED | **resolved**: volatile status in memory, flushed on rotation; the registry written on configuration changes; `Batched` catalog work rides the group commit (§6, §10) |
| deletion, `SystemGone` and lingering data (1) | CONFIRMED | **resolved**: `tombstone` in the delete transaction, effective at that commit on the writer thread; the purge removes the data within one pass (§5, §9) *Reopened by the block-file amendment: data in block files is erased at rewrite or retirement (§5, owner's decision pending).* |
| head memory, index cache and open time (1, 2, 3) | CONFIRMED | **resolved**: memory holds active series' open chunks only; closed chunks and quiet series live in redb; budgets restated (§7) *Reopened by the block-file amendment: closed chunks live in block files; open reads summaries only (§5, §7).* |
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
| the head index was neither checkpointed nor rebuilt (3) | CONFIRMED | **moot**: no head index; chunks are redb entries *Reopened by the block-file amendment: closed chunks are in block files with an index checked per block (§5).* |
| recovery re-sealed a closed span's state (3) | CONFIRMED | **moot**: no span-close protocol; a span boundary seals chunks within the writer's transaction *Reopened by the block-file amendment: the span handoff is §6's protocol; the rewind brings a handed-off span back (§2).* |
| a panic caught by the blocking pool left a torn shard (3) | CONFIRMED | **resolved**: store state changes only on the writer thread; its panic fails the store and the hub exits; redb repairs at the next start (§6) |
| a clock fault had no way out that kept history; a persistent resume switch (3) | CONFIRMED | **resolved**: no fault state; `HUB_CLOCK_REWIND` is one-shot and echoes `last_issued` (§2, §8) |
| counting only active series left the table unbounded (3) | CONFIRMED | **resolved**: a per-system interned cap (§2) |
| the default cap counted only the store and couldn't free a shared volume (3) | CONFIRMED | **resolved**: the floor deletes whatever the cap says; the volume size re-read each pass (§5, §6) |
| `last_seen` wasn't "last contact"; shutdown blanked it (3) | CONFIRMED | **resolved**: typed `LiveStatus`; shutdown touches nothing (§10) |
| the mount rule dropped the Docker disk and every PV (3) | CONFIRMED | **resolved**: strictly-under rule; per-pod mounts dropped (author's revision), `globalmount` kept (§3) |
| erasure "within 14 days" false for the hour span (3) | CONFIRMED | **resolved**: the purge removes a deleted system's data within one retention pass; freed-page bytes stated (§5) *Reopened by the block-file amendment: bytes in files written before a delete wait for rewrite or retirement (§5, owner's decision pending).* |
| rollup ceilings impossible with the encoding; footprint understated (3) | CONFIRMED | **resolved**: ceilings from the encoding, redb's measured overhead and entry cost included; footprint restated at ≈ 200 GB (§4, §5) |
| physical writes at small scale (3) | PLAUSIBLE | **resolved**: one group commit per interval, stated `fsync` rate, `HUB_COMMIT_INTERVAL` advice (§7) |
| a pending shortening only in memory until a checkpoint (3) | PLAUSIBLE | **resolved**: written with its override; re-armed at every open (§5) |
| `LOCK` had no stated directory; staging before `store/` (3) | PLAUSIBLE | **resolved**: redb's lock, taken before any other state; no staging (§8) |
| the compose volume hid `/app/static` (3) | CONFIRMED | **resolved**: the prerequisite shipped (`8a3a153`) |
| `NotAfterLast` dropped snapshots of one honest connection (3) | PLAUSIBLE | **stated**, counted by cause (§2) |
| after a resume, a later forward fault deleted everything (third pass on the split draft) | CONFIRMED | **moot**: no resume; the retention clock bound holds at all times (§2) |
| the checkpoint was the only map from series id to key (same) | CONFIRMED | **moot**: the series table is in redb, in the same file as the chunks *Reopened by the block-file amendment: series GC keeps a series while a file may hold it; an unmapped id is dead (§5).* |
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

**Mail amendment (2026-10-03).** RFC 0017 (Implemented) added mail systems after the passes
above, and RFC 0012's fourth and fifth passes found what this RFC must carry for them:

| Change | Where |
|---|---|
| mailed points keep their reported time (`PointTime::Reported`, rebased per system, §2); `Late` refuses one not after its series' last; every other path stays on hub now | §2, §9 |
| a backfill report's points are dropped, its alert records kept (owner's decision; changes 0017 §6 step 5) | §2, Rollout |
| mailed points older than retention are accepted and removed by the pass; 0017's per-point `Expired` count goes | §2 |
| `CatalogTxn::append` stages points in a catalog transaction, so a report's receipt and points commit together | §6, §9 |
| `store_mail_scan`: one batched transaction per scan, messages deleted after its commit | §10 |
| a mail system's `last_contact` is the hub time of its intake; `last_seen` follows it | §10 (0012 §2's requirement) |
| `Unknown → Offline` uses `since: last_contact` for every source, in one `offline_since` function | §10 (0012 §2's requirement) |
| `TierSetting { Global, Fixed }`, one optional pending entry per tier, `Global` resolved when read | §5 (0012 §2's requirement) |
| `HUB_LISTEN` attributed to RFC 0015 | §8 |

`rfc-adversary`, first pass on the mail amendment. Every finding was acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| the closed-bucket `Late` rule dropped in-order mail points (10 s sampling at every report boundary; an hour after a delay) | CONFIRMED | a `Reported` series' buckets close on a later point or past the receipt window, never on the 60 s sweep; a per-series `TimeSource`; rows for 10 s sampling and a 2 h backlog (§2, §5) |
| per-point clamping squashed a fast agent's newest samples onto one second | CONFIRMED | the report is rebased as a whole; `mail_rebased` counted; a five-snapshot row (§2) |
| the abort row couldn't fail; abort mechanics unstated | CONFIRMED / PLAUSIBLE | an `f` aborts only before its first write, enforced (fail-stop otherwise); a stage-then-abort child-process row (§6) |
| `ENOSPC` left staged points in the head for a later tail flush | CONFIRMED | the head re-derived from redb before degraded mode; messages stay in the mailbox; a row (§6) |
| `HUB_CLOCK_REWIND` moved mail series' last forward | CONFIRMED | `min(last, system clock)`; a row (§2) |
| durable commits per message multiplied the tail flush | CONFIRMED | slices sized by elapsed time; one batched transaction per scan, messages deleted after its commit (§6, §10) |
| mail systems `Unknown` for up to an interval after every restart | CONFIRMED | the overdue sweep sets an on-time `Unknown` system `Online` (§10) |
| the recovered `last_contact` could be a flush rotation stale | CONFIRMED / PLAUSIBLE | the latest of the flushed value, the newest `HubNow` series point and the newest `received_at`; a crash row (§10) |
| README and ARCHITECTURE mail text would contradict the design | CONFIRMED | listed (Impact) |
| overdue on agent time flaps next to a hub-time `last_seen` | PLAUSIBLE | `mail_status` on `received_at` (owner's decision, a change to 0017 §7) (§10) |
| freshness clock unstated | PLAUSIBLE | the system clock, never hub time (§10) |
| the overdue sweep could run before a backlog drains | PLAUSIBLE | held until a scan takes fewer than 256 messages (§10) |
| bare primitives in `PointTime` and `TierSetting` | minor | `ReportedTime`, `TierPeriod` (§2, §5) |

Came closest and survived: replay against closed buckets (a refused point never reaches the
log, so replay reproduces the head).

`rfc-adversary`, second pass on the mail amendment (the first pass's resolutions). Every finding
was acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| closing `Reported` buckets at 7 days was too early for 24 h intervals, and ran on hub time, which a held clock pushes ahead | CONFIRMED | the mail horizon (window + longest interval + slacks) on `min(hub, system clock)`; a closed-bucket point is `Late` (§2) |
| a per-report rebase varied with each delivery delay and lost samples when delivery got faster | CONFIRMED | a per-system clock lead, the maximum over the receipt window, never decreasing within it (§2; 0011 §7) |
| degraded mode stored receipts and dropped their points, so resends became duplicates | CONFIRMED | no mail scan while degraded; the mailbox is the buffer (§6) |
| `last_contact` at open mixed in `HubNow` application rounds, so the overdue compare-and-set never matched again | CONFIRMED | mail systems take `received_at` alone; the compare-and-set is on the newest receipt (§10; 0011 §7) |
| after a restart a mail system's offline age jumped by up to 3 intervals | CONFIRMED | `since` is when the system went silent by its own rule: overdue time for mail (§10) |
| `HUB_CLOCK_REWIND` left other hub times in the future | CONFIRMED | the same `min` applied to `received_at`, `last_seen_active` and `LiveStatus` times (§2) |
| mail accumulators lived outside the active definition and recovery | CONFIRMED | active per time source; recovery loads every open bucket (§2, §6, §7) |
| a mailed point in an open raw chunk outlived its span | CONFIRMED | the pass seals and drops expired open raw chunks (§2) |
| nothing actually dropped a backfill report's points | CONFIRMED | decided by `Recency` before any write: a backfill stages nothing (§2) |
| a per-scan count let anyone keep the overdue sweep off | PLAUSIBLE | gated on the first message delivered after open (§10) |
| a deterministic fail-stop message could loop the hub | PLAUSIBLE | in-flight names in `meta`, one per transaction after a restart, quarantine on a second failure (§10) |
| re-deriving the whole head on `ENOSPC` stalls for the open time | PLAUSIBLE | only the aborted interval's series (§6) |
| wording, 0012 §2, glossary, scan buffer | CONFIRMED (minor) | corrected; 0012 §2 noted; the glossary; the scan buffer in §7 |

Came closest and survived: one batched transaction per scan with refusals decided by reads, so a
report's duplicate never aborts after another report's writes.

`rfc-adversary`, third pass on the mail amendment, and the **simplification** it led to (owner's
decision, 2026-10-03). The pass found that redb latches every operation after an I/O error, so
the running degraded mode could never run, and five more defects in the reported-time design
(an overdue gate that never opened on a silent mailbox, a compare-and-set that couldn't be
atomic, a clock lead that one `(secs, seen_at)` pair can't hold, a degraded store whose sweep
still marked systems offline, a horizon a +8-day fault could close). Each of three passes had
found new ways to lose samples in that design, so the owner chose to store a mail report like a
push snapshot:

| Finding | Verdict | Now |
|---|---|---|
| redb refuses every operation after a failed write, so degraded mode while running is unreachable | CONFIRMED | `ENOSPC` fails the store stop; degraded mode is an open-time state; the floor is kept while running by deleting before the disk fills (§6) |
| the overdue gate never opens with no new delivery | CONFIRMED | opens at the first scan that finds no backlog delivered before open (or before leaving degraded mode); an empty `new/` counts (§10) |
| the overdue compare-and-set can't be atomic across `read_catalog` and `live_status` | CONFIRMED | mail `LiveStatus` changes applied by the commit hook in commit order; the mark is a transaction that re-reads `MailNewest` (§10) |
| one `(secs, seen_at)` pair can't hold a sliding maximum; a rising lead reorders | CONFIRMED | **moot**: no reported times, no rebase (§2) |
| a degraded store's sweep marked every mail system offline; the Maildir can eat the floor | CONFIRMED / PLAUSIBLE | the sweep held while degraded and re-gated after it; the Maildir on another volume (§6, §10) |
| a held hub time hides overdue mail systems | PLAUSIBLE | stated, in the hold `error` and `/api/storage` (§10) |
| the rewind missed `seen_at`, receipt `received_at` and future tail buckets | PLAUSIBLE | every persisted hub time and future tail bucket named (§2); `seen_at` moot |
| a +8-day fault closed every open mail bucket | CONFIRMED (minor) | **moot**: mail series close as any `HubNow` series (§2) |
| 0012 §2's wording, test rows, README and ARCHITECTURE | CONFIRMED (minor) | corrected; the README's Maildir, quarantine and degraded sentences listed (Impact) |

Came closest and survived: the in-flight quarantine list, durable only because it is awaited
before the scan's transaction (now stated in §10).

`rfc-adversary`, pass on the simplified design. Every finding was acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| §10 staged points for every `Newest` report of a scan, against 0011 §7: a backlog's oldest report shown as current | CONFIRMED | points staged once per system at the end of `f`, for its newest `Newest` by `(created_at, seq)`; a three-report row (§2, §10) |
| the overdue mark no longer evicted live metrics, and evicting in the hook could invert the lock order | CONFIRMED | the hook evicts after releasing the Registry lock, comparing generations; the total lock order stated (§9, §10) |
| a mail flood on the data volume drives floor deletes and holds degraded mode | CONFIRMED | `st_dev` check with an explicit opt-in; unauthenticated messages deleted while degraded (§6, §8) |
| one unreadable file kept the overdue gate shut forever; a listing error opened it | CONFIRMED | stuck files counted, not gating; a failed listing keeps it closed; a time bound (§10) |
| one write-free transaction per system made a sweep take hours | CONFIRMED | one transaction per sweep with every candidate (§10) |
| `EDQUOT` invisible to `f_bavail`: a restart loop that quarantined good mail | CONFIRMED | a scratch `fallocate` at open; disk-caused fail-stops never quarantine (§6, §10) |
| §9's API had no `Answer`, no way to emit a mark, no hub `now` | CONFIRMED | `Result<(T, Answer), Abort>`, `CatalogTxn::notify`, `CatalogTxn::now` (§9) |
| text from the removed design; a contradictory degraded row | CONFIRMED | rewritten; the row removed (§2, Domain impact, Rollout, Testing) |
| Registry and `live_status` unordered | PLAUSIBLE | one total order; the hook never holds two hub locks (§9) |
| the quarantine's state machine unspecified | PLAUSIBLE | `{ name, solo }` entries, each cleared by its own commit; solo attempts wait while degraded (§10) |
| the rewind missed `stored_at`, tombstone times and series' last spans | PLAUSIBLE | named (§2) |
| `since` for `Online → Offline` mail ran late after a drain | PLAUSIBLE (minor) | `min(now, overdue moment)` (§10) |

Came closest and survived: the overdue mark as a write-free transaction, sound once batched,
delivered through `notify` and evicting.

`rfc-adversary`, pass on the latest resolutions, and the **scope cut** that followed (owner's
decision, 2026-10-03). The pass found the safety mechanisms added over the last rounds were the
source of new defects: a degraded mode that could never leave (deleting spans inside `hub.redb`
frees nothing on the volume) and erased history trying; a quarantine with nowhere to record a
crash's cause, which blamed innocent messages; a shared-volume refusal that broke the documented
single-disk mail setup on upgrade. The owner chose to cut them:

| Finding | Verdict | Now |
|---|---|---|
| degraded mode never left and erased raw and minute history | CONFIRMED | **cut**: no degraded mode; below the floor at open the hub refuses to start; the storage cap is the only automatic deletion (§6) |
| quarantine had no place to record a fail-stop's cause and blamed innocent messages | CONFIRMED | **cut**: no quarantine; the scan's message names logged at `error` on a store failure; a stated limit (§6, §10) |
| `HUB_MAIL_SHARED_VOLUME` refused the documented single-disk setup | CONFIRMED | **cut**: no check; the README advises another volume, a stated limit (§6) |
| while degraded, kept messages hid a flood | CONFIRMED | **moot**: no degraded mode |
| a backlog drained over many scans showed day-old reports as current, one scan at a time | CONFIRMED | points only for a report current on the system clock (one interval + 15 min) (§2) |
| the rewind kept misdated buckets in each tier's current span | CONFIRMED / PLAUSIBLE | current-span chunks and tails rewritten without them (§2) |
| the rewind missed `alerts_seen` keys and the retention clock | PLAUSIBLE | named (§2) |
| the scratch `fallocate` on NFS; a leftover file | PLAUSIBLE | **moot**: no `fallocate`; quotas a stated limit (§6) |
| `st_dev` misses shared pools | PLAUSIBLE | **moot**: no check |
| §9's order contradicted itself on the admission lock and left out `live_applications` | CONFIRMED (minor) | one order with the admission exception and every nested path listed (§9) |
| "`NotAfterLast` never counts a mail point" can't hold under a hold | CONFIRMED (minor) | the row narrowed (Testing) |
| "per report" text; two tie-breaks | CONFIRMED (minor) | corrected; one `newest_of_scan` (§2; 0011 §7) |

From RFC 0008's fifth pass: `transact` returns `TransactError { Aborted, Store }` (§9), and a
`LiveStatus` write for an absent entry is dropped (§10). Came closest and survived: the overdue
mark as one write-free transaction delivered through `notify`.

`rfc-adversary`, final pass of 2026-10-03, resolved in one round with 0008's and 0011's:

| Finding | Verdict | Resolution |
|---|---|---|
| the rewind left future-stamped points in `points_log` for replay | CONFIRMED | the rewind empties the log after flushing the rewritten tails, and runs after replay (§2, §6) |
| a slow mail clock stored no history, silently | CONFIRMED | `mail_not_current` counted and warned hourly per system; API10 and the row corrected (§2) |
| the currency gate's scope differed between §2, §10 and 0011 §7 | CONFIRMED | points, round and live metrics gated; status, `mail_newest`, info and alerts never (§2; 0011 §7) |
| the cap bounded allocated bytes, not file length; floor and compaction unordered at open | CONFIRMED | the cap bounds the file length; §8's open order starts with the floor, then compaction, then the floor again |
| who writes `mail_newest`, and when | CONFIRMED (minor) | once per system at the end of `f` for the `newest_of_scan` choice (§10; 0011 §7) |
| §9's `on_stored` path vs 0008; "the only nested pair" | CONFIRMED (minor) | Registry read → `live_applications`, held across the insert (§9) |
| `Answer` wording | CONFIRMED (minor) | catalog writes by an earlier `f` only (§9, §10) |
| scan names logged only on `Failed` | CONFIRMED (minor) | on any store error but `Closed`; `main` waits for the in-flight scan (§10) |
| the floor check vs §8 creating the directory | CONFIRMED (minor) | the floor is §8's step 1, on the nearest existing ancestor |
| a bucket straddling the rewind point stays closed | PLAUSIBLE | reopened by the rewind (§2) |
| `now(&self)` couldn't advance `HubClock` | PLAUSIBLE | the writer stamps one hub time per transaction before `f`, persisted with the commit (§9) |
| the retention clock's formula allowed 3× | CONFIRMED (text) | the slack only on the first pass after open (§2) |
| `Abort` undefined (0008's pass) | CONFIRMED (low) | `Abort<A>`, `TransactError<A>` (§9) |

`rfc-adversary`, verification of that round. Three of its resolutions didn't hold and were
re-made:

| Finding | Verdict | Resolution |
|---|---|---|
| a cap on the file length, which never drops while running, deleted raw and minute history every pass for ever | CONFIRMED | the pass acts on allocated bytes against `cap / region slack`, re-measured; stops as soon as under; a row (§5) |
| one pass of slack at every open, times a restart loop under a forward fault, voided the 2× bound | CONFIRMED | the first pass after open advances nothing (§2) |
| the floor check before compaction made compaction unreachable when needed | CONFIRMED | directory checks, then compaction, then the floor (§8) |
| 0011 §7 still had the caller set the mail status after the commit | CONFIRMED (minor) | the hook sets it (0011 §7) |
| `transact` used `A` undeclared | CONFIRMED (minor) | declared (§9) |
| the reopened straddling bucket had no sum to rebuild from | PLAUSIBLE | rebuilt from the surviving raw points (§2) |
| emptying the log could lose a series' log-only points in a previous span's tail | PLAUSIBLE | every tail flushed in the rewind transaction (§2) |
| a quiet hub might never commit, so a `notify` would never be delivered | PLAUSIBLE (low) | the interval commit is unconditional (§6) |
| a series' last timestamp reset up to the system clock | noted | `min(last, system clock)` (§2) |

`rfc-adversary`, verification of the previous fixes (commit d8eb544). Every finding was acted
on:

| Finding | Verdict | Resolution |
|---|---|---|
| a slack re-measured as `length / allocated` made the trigger `length > cap` again: the file-length rule in disguise | CONFIRMED (high) | a constant `SLACK` (1.7, `HUB_STORAGE_SLACK`), never re-measured; the row rewritten (§5, §6, Testing) |
| `transact` without `'static` can't cross the writer channel without `unsafe` | CONFIRMED (low) | `'static` bounds; callers clone what they capture (§9) |
| the `retention_now` test rows pinned the removed slack | CONFIRMED (text) | rewritten (Testing) |
| "creates nothing" vs the directory created first | CONFIRMED (text) | "creates no file" (Testing) |
| `previous` at a first open undefined: retention from 1970 | PLAUSIBLE | created at `min(hub_now, system_now)` (§2) |
| chunks sealed since open not in the rewind transaction | PLAUSIBLE (low) | included (§2) |
| a compaction open at zero free bytes dies with a redb error | PLAUSIBLE (low) | reported as the floor failure (Testing) |
| two queued rounds in one second leave `shown` one behind | PLAUSIBLE (low) | stated (§10) |
| the rewind's `alerts_seen` keys as plain hub times (from 0011's verification) | CONFIRMED | rebuilt as rewound `last_seen_active` + bound (§2) |

Came closest and survived: expiring the straddling bucket's raw points under a fault-driven
retention clock (the bucket's points all precede the fault, so their raw span outlives the
rewound clock).

**Block files (2026-10-06, owner's decision after a measured spike, §1).** Every closed span
moves from redb to an immutable block file; redb keeps the catalog and the hot part. The
amendment also carries the last verification's open findings:

| Change | Where |
|---|---|
| closed spans in block files: format, CRC per chunk, sparse index, `pread`, LRU of handles | §5, §7 |
| the span handoff (durable file before its row), reconciliation at open, `HUB_STORE_FORGET_BLOCK` | §6, §8 |
| retention by file: dead chunks filtered at read, files unlinked when all dead, rewritten at 25% dead or when only longer overrides keep them | §5 |
| erasure of deleted systems in files delayed to their file's rewrite or expiry (confirmed by the owner, 2026-10-06, with the hour tier's default lowered to 30 days, which bounds the delay at 30 days by default) | §5, Security; 0011 §3 |
| spans: minute 1 h, hour 1 day, so the hot part is at most two spans per tier | §5 |
| the storage cap on the real bytes on disk; `SLACK` and `HUB_STORAGE_SLACK` removed | §5, §8 |
| footprint ≈ 155 GB at the scale target (≈ 250 GB at the measured 1.7× in redb); ≈ 113 GB after the owner lowered the hour tier's default to 30 days (2026-10-06) | §5 |
| from the last verification: no new store before the build date; the hook's cap stated by the store; `newest_of_scan` in §10; "15 s" corrected; compaction errors reported as themselves | §8, §9, §10, Testing |

**Still open**, carried into the next `rfc-adversary` pass: whether the rotating tail flush plus
the log meets the stated open time at the scale target (to be measured), and whether redb's
copy-on-write amplification keeps the physical write volume near §7's logical figure.

`rfc-adversary` on the block-file amendment (commit 6f8f84e). Every finding was acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| the rewind put points into a handed-off span, and the next handoff renamed over its durable file | CONFIRMED | the rewind brings the span holding the system clock back into `chunks` and retires its file; renames never replace (`RENAME_NOREPLACE`, or `link`) (§2, §6) |
| retention no longer expired chunks still in redb | CONFIRMED | liveness defined for every chunk; pass step 3 deletes dead chunks in `chunks`; the handoff writes live chunks only (§5) |
| a damaged index stopped the whole hub | CONFIRMED | the file opens unreadable (`Corrupt` per query); `HUB_STORE_FORGET_BLOCK` only clears it (§6) |
| the open read every full index (≈ 11 GB) | CONFIRMED | a per-file summary with a CRC per index block; the open reads headers and summaries only; 400-day open-time test (§5, §7, Testing) |
| 0008's lock-free forget let an in-flight round resurrect | CONFIRMED | the ordinal is taken at `decide`, under the lock, and `Forgotten` carries one (0008 §3) |
| the floor check ran before reconciliation | CONFIRMED | `.tmp` cleanup, the redb open and reconciliation precede the floor check; the cap's retirements don't (decision below) (§8) |
| a query could race a rewrite or a retirement into `ENOENT` | CONFIRMED | one retry in a fresh read transaction; a second `ENOENT` is a missing file (§9) |
| the LRU kept retired files allocated | CONFIRMED | the LRU is keyed by file name and a retirement closes the handle before the unlink; the cap test measures `statvfs` (§5, §7) |
| a flipped `SeriesId` in an index served another series' chunk | CONFIRMED | the chunk CRC covers (tier, span, `SeriesId`, seq) and its bytes; index blocks have CRCs (§5) |
| 0011's tombstone rule needed generations `BlockRecord` lacked; GC could unmap ids in files | CONFIRMED | `block_generations` table; GC keeps a series while a file may hold it; an unmapped id is dead (§5; 0011 §3) |
| the handoff wrote tombstoned generations | CONFIRMED | handoff and rewrite write live chunks only (§5) |
| a failing handoff let the hot part grow without bound | CONFIRMED | backlog bounded at three spans per tier, then fail-stop; shown in `/api/storage` (§6) |
| stale all-redb text; reopened Review rows; missing test rows; incomplete Impact | CONFIRMED | rewritten in place; rows marked reopened; rows added; Impact extended |
| reconciliation deleted any row-less file on a false premise; backups | PLAUSIBLE / CONFIRMED | deleted only when provably safe, otherwise refuse naming it; retirements go through `pending_unlinks`; backups take both together (§6, Impact) |
| dead bytes stored but stale; lengthening a policy resurrects | PLAUSIBLE | **adopted**: only tombstoned bytes are stored; expiry derived at read; the resurrection stated (§5) |
| rewrites on the writer thread | PLAUSIBLE | **adopted**: on the block writer; only the swap through the writer (§5, §6) |
| queries need `Policies`, tombstones, the clock | PLAUSIBLE | **adopted**: read in the query's own redb transaction; no lock added (§5, §9) |
| descriptor budget | PLAUSIBLE | **adopted**: `nofile` 65,536 in compose and README, a `warn` at open, `EMFILE` mapped (§7) |
| `blocks/` outside the directory checks; stored file names | PLAUSIBLE (low) | **adopted**: same checks, `O_NOFOLLOW`, names from typed keys (§5, §8) |

**Decided, not adopted:** the storage cap's retirements don't run before the floor check. With
the default cap the store is at most 80% of the volume, so a volume below the floor is full of
something else, and the owner ruled out deleting history to make room (§6, §8).

Came closest and survived: crash windows inside the handoff itself (each leaves the chunks in
redb plus an orphan, or the row plus a durable file, and MVCC serves an overlapping query from
`chunks` until the commit).

`rfc-adversary`, verification of those fixes (commit 13a7afe). The handoff and rewrite crash
windows, the rewind's atomicity, the CRCs, the summaries, the `ENOENT` retry, the open order and
the backlog bound held. Every finding was acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| the "provably safe" deletions deleted the only copy in mixed-backup cases | CONFIRMED | a higher-rewrite file deleted only when the row's own file is present; a handoff orphan only when S is closed by the persisted clock, no tail is open in it and its chunks are in `chunks`; test rows for both backups (§6, Impact) |
| `HUB_STORE_FORGET_BLOCK` left a row-less file that refused the next start | CONFIRMED | the forget queues the file in `pending_unlinks` and deletes its `block_generations` entries (§6) |
| the rewind left `block_generations` stale | CONFIRMED | every transaction that removes or swaps a row deletes its entries; an invariant checked after every kill (§2, §5, Testing) |
| a tombstone between the block writer's snapshot and its commit escaped the tombstoned bytes; a swap could undo a retirement | CONFIRMED | tombstoned bytes computed at the row's commit from per-generation bytes; the swap conditional on the rewrite number (§5, §6; 0011 §3) |
| the tombstone index pass was unbounded and not crash-safe | CONFIRMED | no index pass: `block_generations` records each generation's bytes per file, summed in the tombstone's own transaction (§5) |
| a damaged file starved every rewrite | CONFIRMED | damaged files are never selected; a rewrite meeting `Corrupt` is abandoned and marks the file (§5) |
| the rewind had no answer for an unreadable span or for seq; its size understated | CONFIRMED / PLAUSIBLE | the span dropped and counted; seq from the tail, last chunks reopened into tails; ≈ 0.94 GB read, ≈ 1.6 GB written, counted by the floor (§2, §5, §8) |
| 0008: a forget before the first *show* was lost | CONFIRMED | `forgotten` ordinal in the admission entry (0008 §3) |
| a Testing row contradicted the backlog bound | CONFIRMED | split (Testing) |
| `.tmp` cleanup ran before redb's lock | CONFIRMED (low) | redb opened first; cleanup and compaction under its lock (§8); *superseded by the final verification: `hub.lock` first, then `.tmp` cleanup, then redb* |
| leftovers outside the rules: a failed directory `fsync`, an absent `hub.redb` beside files | PLAUSIBLE (low) | **adopted**: the final name unlinked on failure; an absent `hub.redb` with files in `blocks/` refuses to start (§6, §8) |

Came closest and survived: the backlog fail-stop under `ENOSPC` on a shared volume (the restart's
floor check either hands the volume to the operator or leaves room for the raw write).

`rfc-adversary`, final verification (commits 5003309 and 0d21d7e). The mixed-backup refusals,
`HUB_STORE_FORGET_BLOCK`, `block_generations`, tombstoned bytes, the damaged-file skip, the
rewind's floor and the 30-day default across 0010, 0011 and 0012 held. Every finding was acted
on:

| Finding | Verdict | Resolution |
|---|---|---|
| the rewind dropped an unreadable span without queueing its file, so the next start refused | CONFIRMED | the file queued in `pending_unlinks` and its entries deleted; a two-restart test row (§2, Testing) |
| a crash while a retirement, the cap or an expiry raced an in-flight handoff or rewrite left a file no rule explained | CONFIRMED | spans in flight are left alone by the pass and the cap until their commit or failure, so step 2a's rules always hold; kill rows in both windows (§5, Testing) |
| one corrupt chunk in a brought-back file looped the rewind | CONFIRMED | the chunk skipped and counted; the rewind always commits (§2, Testing) |
| "nothing kept longer than 30 days" ignored alert records (90 days) and freed pages | CONFIRMED (text) | the decision row scoped to metric history, with both stated (decisions table) |
| redb's repair ran before a 0.55 GB `.tmp` could be deleted | PLAUSIBLE | **adopted**: `hub.lock` first, `.tmp` cleanup, then redb (§8) |
| 0008: the forget's `fetch_max` and `shown` order | PLAUSIBLE | **adopted**: `fetch_max` first, *show* reads under the `live_applications` lock (0008 §3) |

**For the owner:** alert records keep their own 90-day default (`events=`, 0011 §5); lowering it
to 30 days is a one-line change if wanted.

Came closest and survived: reusing a rewound span's name (`RENAME_NOREPLACE` refuses while the old
file exists, and its `pending_unlinks` entry is gone long before S can close again).

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
