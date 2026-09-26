# RFC 0013: One-Shot Import from SQLite into the Hub Store

- Status: Rejected
- **Rejected on 2026-09-26 (owner's decision): no migration from SQLite.** The hub store is
  rebuilt on redb (RFC 0010), and the new hub starts with an empty store. The old
  `system-hub.db` is left untouched for the old hub, and nothing imports it. Operators register
  polled systems again with the admin token (RFC 0012); push agents register themselves on their
  first handshake (RFC 0008). The design below is kept as it was reviewed, for the record; its
  references to RFC 0010 point at the custom engine that RFC 0010 replaced.
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-26
- Affects: `system-hub`
- Depends on: RFC 0010 (the engine, its import writer, the data-directory layout and lock,
  `LiveStatus`), RFC 0011 (the catalog, sealed tokens, sources, alert limits), RFC 0008 (the
  push registry limit counts imported push systems)
- Part of the release that replaces SQLite: 0008 and 0010–0013, all shipped together (0010's
  header), after the static-files prerequisite. The import runs once, before ingestion;
  history collected *during* the import isn't recorded, and the README gives the expected
  duration.

## Motivation

Existing hubs hold their registry, alert records and metric points in `system-hub.db`. An
upgrade that dropped them would lose every registered polled system (its URL and token), every
acknowledgement, and the recent history. The import must:
- never loop, and never leave the hub unable to start without a way out;
- never mix its data with live data, or attach old data to the wrong series;
- not let legacy data break the new store's rules: agent timestamps (possibly in the future),
  ids over 255 bytes, unbounded fields, values outside a kind's domain, rows of the wrong type;
- scale to the large databases hubs accumulate, since series that stop are never pruned today.

## Proposed design

### 1. When it runs

At startup, after configuration is parsed and **after `main` has taken 0010's exclusive lock
on `HUB_DATA_DIR/LOCK`**, and before it reads any state. So two hubs on one volume (a rolling
update, a second `compose up`) can never both import, or delete each other's staging. The
directory layout (`store/`, `store.import/`, `import.failed`, `import.attempts`) is 0010 §6's.

| State at startup | Outcome |
|---|---|
| `store/` exists | no import. If it records `hub/import = NotApplicable` or `Skipped`, or `Done`, that is final. |
| `store/` absent, no source file | create `store/` with `hub/import = NotApplicable`, and start |
| `store/` absent, source present, feature compiled in | **import** (§2) |
| `store/` absent, source present, feature not compiled in (the release after) | **refuse to start**: "`system-hub.db` holds data from before RFC 0010; upgrade through release N first, or set `HUB_IMPORT=skip` to start empty". This check needs no `rusqlite`, and stays in every later release. |
| `store.import/` present, source absent | **refuse to start**: "an import was interrupted and its source is gone: restore it, or set `HUB_IMPORT=skip`" |
| `import.failed` present | **refuse to start**, printing the recorded reason and the attempt id, until the operator sets `HUB_IMPORT=retry:<attempt id>` or `HUB_IMPORT=skip` |

The source is `system-hub.db`, or `HUB_IMPORT_FROM` if set. `HUB_IMPORT` is:
- unset;
- `retry:<attempt id>`: delete `import.failed`, `import.attempts` and any `store.import/`, then
  import. **One-shot**: it acts only when the id equals the one `import.failed` records (and the
  refusal prints); any other value is logged at `error` and ignored, so a value left in a
  manifest can't turn a deterministic failure into a loop;
- `skip`: delete them, create `store/` with `hub/import = Skipped`, and start empty. It acts only
  while there is something to skip (`import.failed`, or a source with no `store/`), and is
  ignored otherwise;
- `rollback-journal` (below).
An unknown value refuses startup.

**Source checks, only when an import is about to run:**
- the journal files are named from the source path: `<source>-journal` and `<source>-wal`. If
  either exists and isn't empty, the old hub didn't stop cleanly (it is PID 1 in the image and
  ignores SIGTERM, so `docker stop` kills it), and a read-only handle can't roll the journal
  back. The hub refuses to start with the instruction: "set `HUB_IMPORT=rollback-journal` once,
  to let SQLite roll the journal back; then back up the file". The image has no `sqlite3`, so
  the hub does it itself. This refusal writes no state, so it repeats until the operator acts,
  and never loops the import;
- `HUB_IMPORT=rollback-journal` opens the source **read-write once**, only so that SQLite rolls
  the hot journal back (a `PRAGMA quick_check`), closes it, logs that it did, and then continues
  as an ordinary import on a read-only handle. It acts only when a hot journal exists, and is
  ignored otherwise;
- the import itself opens the source read-only (`SQLITE_OPEN_READ_ONLY`) and never modifies
  it.

### 2. One staged operation, never mixed with live data

The import builds a **separate store**, `HUB_DATA_DIR/store.import/`, through 0010's
`ImportWriter` (0010 §9), and publishes it only when it is complete:

0. **Preflight** (writes no state): estimate the staged store's size (legacy points × 1.8 B,
   plus rollups, plus a WAL segment per shard) and compare it with the free space `statvfs`
   reports for `HUB_DATA_DIR`. If it doesn't fit with 10% to spare, refuse to start: "not
   enough space to import: free N GB, or move the source with `HUB_IMPORT_FROM`". The storage
   cap (0010 §5) is **off** inside the staged store, so it can't delete imported blocks.
1. **Count the attempt**: increment `import.attempts` (a small file: the attempt id, a random
   64-bit hex, and the count; written, `fsync`ed and renamed) **before** anything else. If the
   count shows three attempts that never finished (each process that ended without reaching
   step 4 or step 6, by an out-of-memory kill, a panic or a power loss, left its count behind),
   write `import.failed` with "3 attempts didn't finish" instead of trying again.
2. Delete any leftover `store.import/` (an earlier interrupted attempt) and create it.
3. Import systems, then alert records, then metric points (§3, §4).
4. Write the catalog record `hub/import = Done { source, counts }` in the staged store, and
   call `ImportWriter::finish(last_issued)` (§3), which writes its final checkpoint and clock
   file.
5. Rename `store.import/` to `store/`, and `fsync` `HUB_DATA_DIR`. That rename is the single
   commit point. Then delete `import.attempts`.
6. Only then open `store/` normally and start ingestion.

- An interruption anywhere before step 5 leaves `store/` absent, so the next start begins again
  from step 0, with one more attempt counted. Restart means deleting one directory, so
  nothing half-written is ever read, and no old block can be read under another series' id.
- The import never touches a store that has served live data: it runs only while `store/`
  doesn't exist.
- **Failure is terminal, not a loop.** A source I/O error (`SQLITE_IOERR`, `SQLITE_CORRUPT`,
  `SQLITE_FULL`), a store in degraded mode (0010 §6, `ENOSPC`), the clock check (§3), or a
  rejection share over the limit below:
  1. removes `store.import/` **first**, so an `ENOSPC` failure frees the space it needs to
     record itself;
  2. writes `HUB_DATA_DIR/import.failed` with the reason and the attempt id (never data);
  3. logs at `error` and exits non-zero.
  The next start refuses as §1 says. If even step 2 can't be written, the attempt counter
  (step 1 above) still stops the loop after three attempts.
- **Rejections are counted per reason.** `SeriesCapReached`, `KindMismatch` and `Degraded` from
  the store, and every skip rule in §4, are counted. If the store rejected, **or the clock rule
  dropped as "later than the import's hub time"** (§3), more than 1% of the points the other
  skip rules let through, the import fails (above), rather than recording `Done` over a history
  it didn't keep.
- **`SourceIdentity`** (`len`, `mtime`, SHA-256 of the first 1 MiB) is stored in `Done`. At each
  later start, while the source still exists, a different identity logs at `warn`: "system-hub.db
  changed since it was imported; nothing is re-imported", and, while the feature is compiled
  in, the number of systems in the source that the registry lacks. That is what a rollback and
  re-upgrade looks like, and the README says what it costs (below).

### 3. How points are imported

The import is driven **span by span**, oldest first, so it never sorts the whole table and
never needs SQLite temp space:
1. List the series: `SELECT DISTINCT system_id, metric FROM metrics`, and parse each through §4.
2. For each series, read its `MIN(timestamp)` and `MAX(timestamp)` through the index (two index
   lookups), and clip that range to `[import hub time − the longest tier retention, import hub
   time]`. A series has work only in the spans inside its clipped range, so one point stamped
   `0` by a bad agent clock costs nothing (the old hub never pruned it), rather than 236,000
   empty spans.
3. For each 2 h raw span that some series' range covers, oldest first: for each series whose
   range covers it, read that span's points with an index range scan
   (`WHERE system_id = ? AND metric = ? AND timestamp >= ? AND timestamp < ? ORDER BY
   timestamp`, which uses `idx_metrics_system_time`), and give them to
   `ImportWriter::append_at`. `ORDER BY` makes the order independent of the query planner.
4. After each span, call `ImportWriter::close_through(span_end)`. **The import's cursor, not
   hub time, closes spans and buckets**, and the staged store runs no sweep, no retention and no
   storage cap, so rollups are built exactly from the imported points.

- **Each tier only takes what it would keep.** A point older than the policy's raw retention
  goes only into the minute and hour tiers; older than the minute retention, only into the hour
  tier; older than the hour retention, nowhere (counted). Legacy series that stopped long ago
  therefore become rollup history, not raw blocks that would expire at once, and they don't
  count against the series caps, which count only active series (0010 §5).
- **The import's hub time** is sampled from the system clock when the import starts; the
  final one is sampled again at `finish`.
- **The clock reference** is the later of:
  - the newest **poll-path** point: poll points are stamped with the old hub's own clock
    (`collector.rs`: `timestamp: now_secs`);
  - the source file's **mtime**, which is the old hub host's clock at its last write, and which
    every database has, including a push-only fleet's, whose points all carry agent clocks.

  If the reference is more than 300 s ahead of the import's hub time, **the new hub's clock is
  behind**: the import fails (§2) with "the hub clock appears to be behind by N s; fix it and set
  `HUB_IMPORT=retry:<attempt id>`", rather than drop the history as "future". A Raspberry Pi that
  boots before NTP, or a WSL2 host after sleep, is caught this way.
- **Every point later than the import's hub time is dropped**, poll-path or push-path, and
  counted; the 300 s rule above is only the failure trigger. The drops count toward the 1%
  failure threshold (§2). So no imported point can set a series' last timestamp ahead of live
  points.
- `ImportWriter::finish(last_issued)` is passed the hub time **sampled at `finish`**, which is
  after every imported point. So live hub time starts after every imported point, and 0010's
  clock sees no step, even after an import that took hours: a store whose legacy data ends
  weeks ago opens with a fresh clock and nothing pending.
- **Progress** is logged at `info` every 5% of spans, with points so far and an estimate of the
  time left.

**Serving while importing.** The hub binds its port **before** the import, so orchestrators see
it alive:
- `GET /api/health` answers 200 with `{"status":"importing","progress":…}`, so a liveness probe
  doesn't kill it mid-import. **A readiness probe must check `status == "ok"`**, not only the
  200, or a rolling update would retire the old hub at the start of the import; the README says
  so, with examples;
- `GET /api/stream/summary` answers **200** `text/event-stream` with one `importing` event
  carrying the progress, then closes. The dashboard's `EventSource` treats a closed stream as a
  network error and reconnects, whereas a non-200 answer would stop it for good and freeze an
  open dashboard on stale data;
- every other route answers 503 with the same progress object and `Retry-After: 30`;
- push handshakes are answered `auth_error` / `hub is importing, retry later`. Shipped agents
  treat that as an error and retry every 5 s, so they reconnect as soon as it ends;
- no poll runs until ingestion starts.
The README's Upgrading section gives the expected duration (about one minute per 5 million
legacy points on the reference container, measured and recorded by the performance test), and
says to allow for it in health-check timeouts.

### 4. What is imported, and what is skipped

Every legacy row is parsed through the **same constructors the live path uses**, reading columns
as SQLite `Value`s so a wrong type or a `NULL` is a counted skip, never an abort:

| Legacy data | Rule |
|---|---|
| system id not a valid `SystemId` (empty, `.`, `..`, over 255 bytes) | skipped with its alerts and points; counted; logged as a count and the hex of its first 16 bytes, never the bytes themselves |
| system `url = "push://"` | `Source::Push` |
| any other `url` | `Source::Poll` through `SystemUrl` (0011 §2); a URL it refuses (including an IPv6 zone id) skips the system, counted |
| a polled system whose id isn't UUID-shaped | imported, and **counted and logged at `warn`** in `Debug` form: hub-minted ids are always UUIDs, so this is likely a push-registered system that was given a URL, whose agent RFC 0012 will now refuse. The README's Upgrading section says to delete that entry so the agent re-registers as a push system |
| a polled system's `last_seen` (an RFC 3339 time from the old hub's clock) | imported into the first checkpoint's volatile blob as 0010's `LiveStatus.last_contact`, when it parses and isn't later than the import's hub time. So "delete offline" counts offline time from the real last contact, not from the first open |
| a push system's `last_seen` (the agent's uptime display, not a time) | not imported: `last_contact` none, so offline time starts at the store's first open (0010 §10) |
| `poll_interval_secs` outside `PollInterval`'s 5 ..= 86,400 (including `0`, and `-1` from a stored `u64::MAX`) | clamped into range, counted |
| `name` over 255 bytes | cut on a character boundary, counted |
| info fields over 256 bytes | cut on a character boundary, counted |
| `token = ''` | no token (every push system has one today) |
| any other token | sealed (0011 §4) with the new generation; a token over 1 KiB, or equal to the admin token, is dropped and counted, and the system's id is logged in `Debug` form so the operator can re-enter it |
| alert fields | cut to 0011 §5's edge limits, counted |
| alert id over 256 bytes | skipped, counted |
| alert records | imported with `last_seen_active` = **the import's hub time, minus the record's rank in seconds** (rank 0 for the newest `stored_at`), so their relative order survives for 0011's eviction and ties are broken; `stored_at` is the incident's *first* sighting, so using it would evict acknowledged, still-firing incidents at the first retention pass |
| more than 1,000 alert records for one system | keep acknowledged records first, newest `stored_at` first, then unacknowledged ones, up to 1,000; the rest counted. An acknowledged incident that is still firing is never the one dropped, so it can't come back unacknowledged |
| more than 50,000 alert records in total | the same order across systems, counted |
| metric name breaking 0010's rule (empty, over 261 bytes, a control character) | skipped, counted |
| metric name with no kind in 0010's metric-name enum | skipped, counted |
| a container mount (0010 §3) | skipped, counted |
| any point later than the import's hub time | dropped, counted toward the failure threshold (§3) |
| a point not after its series' previous one (duplicates, disorder) | dropped, counted |
| a value outside its kind's domain, or not finite | dropped, counted |
| `metric_retention` rows | not imported (retention is per tier and per system now). Counted, and if any exist, logged at `warn` with the equivalent `PUT /api/systems/:id/retention` (0012) |

Catalog records are written with `Catalog::transact` in chunks of at most 1,000 operations or
4 MiB, so the import costs one `fsync` per chunk, not one per record.

### 5. After the import

- The counts are logged at `info`, and every skip or clamp category is logged at `warn` if
  non-zero.
- 0011 §6's plaintext warning is logged at every start while the source file exists.
- After `skip`, 0011 §6's warning uses its `Skipped` wording: the file is now the only copy of
  the old registry and tokens.
- The README gains an **Upgrading** section:
  - stop the old hub; if the new hub reports a hot journal, set `HUB_IMPORT=rollback-journal`
    once; **then** back up `system-hub.db` (copying the file alone while a hot journal exists
    saves a torn copy);
  - upgrade; expect the import to take about a minute per 5 million points, with the hub
    answering 503 meanwhile;
  - check the counts in the log, including polled systems with non-UUID ids (§4);
  - use `status == "ok"` from `/api/health` for readiness;
  - a rollback means running the old hub on the old file and losing everything since the
    upgrade: data **and registry changes** (systems registered, deleted or re-tokened on the
    new hub). A re-upgrade after that doesn't import again;
  - scrub or delete `system-hub.db` only once you are sure you won't roll back;
  - the `sqlite-import` feature is compiled in for exactly one release (N) and removed in
    N + 1; a hub that skips release N must use `HUB_IMPORT=skip` or upgrade through N.

## Domain impact

- **Fleet Registry**, **Fleet History**: a one-time anti-corruption mapping from the legacy
  schema to 0011's records and 0010's series, through the live constructors.
- **Fleet Storage** (0010): the staged import store, `ImportWriter`, the cursor-driven span
  close, the cap switched off in staging, and the directory layout and lock; the `hub/import`
  key in the catalog's `Meta` namespace, hub-owned (0011 §1).
- **Ingestion**: the push handshake answers `hub is importing, retry later` during the import,
  and the poller doesn't start until it ends.
- **Hub Access** (0012): `DELETE` and every `:id` route parse ids into `SystemId`, since the
  import skips invalid legacy ids.
- **Glossary**: added import (one-shot, staged), import state (`NotApplicable`, `Skipped`,
  `Done`), import failure, import attempt, import counts, clock reference.
- **Published contracts**: during the import only, the hub answers 503, the SSE stream sends
  one `importing` event and closes, `/api/health` answers `{"status":"importing",…}`, and the
  push handshake answers `auth_error` / `hub is importing, retry later`.
- **Mixed-version fleet**: agents reconnect after the import. Hub downgrade is unsupported
  after the upgrade.

## Alternatives considered

- **Import into the live store, and "empty the store" on restart** (the second draft). An
  interrupted wipe could leave blocks read under reassigned series ids, and a late import could
  wipe live data. A staged directory with one rename can't do either.
- **Resumable import from a progress cursor.** Faster after a crash, but it needs idempotent
  writes across three kinds of data. Restarting a staged import is simpler.
- **Read the points in global timestamp order.** An unindexed sort of the whole table in SQLite
  temp space (often the container's writable layer). Span-by-span index scans need none.
- **Keep future-dated points, clamped to the import's hub time.** It invents a time. Dropping
  push-path future points is honest, and a clock that is behind the old hub's own points is
  treated as the fault it is.
- **An import subcommand run by the operator.** An extra step that upgrades would forget. The
  automatic import is logged, and the escape hatches are explicit.
- **Delete `system-hub.db` after import.** Destructive, and rules out a rollback.

## Security implications

**OWASP Top 10 (2021)**

- **A01 Broken Access Control:** N/A (it runs at startup, before routes do anything but 503).
- **A02 Cryptographic Failures:** tokens are sealed on import. The plaintext source file is
  warned about at every start while it exists, and the README says when to scrub it.
- **A03 Injection:** the import reads with fixed SQL, binding values, and nothing from the rows
  reaches SQL.
- **A04 Insecure Design:**
  - a staged store with one commit point;
  - terminal failure with explicit `retry` and `skip`;
  - legacy rows parsed through the live constructors, so legacy data can't break live
    invariants;
  - a clock reference that refuses to drop history on a wrong clock.
- **A05 Security Misconfiguration:** read-only source; a hot journal refused with an
  instruction; a release-skip refusal that needs no SQLite.
- **A06 Vulnerable Components:** `rusqlite` behind a feature, removed a release later.
- **A07 Identification & Authentication Failures:** N/A.
- **A08 Software & Data Integrity Failures:** one recorded, staged operation, with counts for
  everything dropped, clamped or cut, and a failure instead of a `Done` over missing data.
- **A09 Logging & Monitoring Failures:** progress, counts, warnings; hostile ids only as hex
  prefixes; token values never logged.
- **A10 SSRF:** N/A.

**OWASP API Security Top 10 (2023)**

- **API1–API3:** N/A.
- **API4:** per-series ranges clipped to the tiers' retention, so hostile timestamps can't
  stretch the import; span-by-span index scans with bounded memory; catalog writes in bounded
  batches; a free-space preflight; every legacy field bounded by 0011's limits.
- **API5–API8:** N/A.
- **API9:** `HUB_IMPORT_FROM`, `HUB_IMPORT` (with `retry:<id>`, `skip` and
  `rollback-journal`), the import-time 503, the SSE `importing` event, the `/api/health` body,
  and a `hub is importing, retry later` row in the README's `auth_error` table.
- **API10:** legacy rows are treated as untrusted input and parsed into domain types.

## Testing plan

A fixture `system-hub.db`, built by the test with today's schema. Each rule is a table row: the
input, and whether it was imported, skipped, clamped or cut, and counted.

- **States** (§1), as a table: `store/` present; absent with no source (`NotApplicable`);
  absent with a source (imports); the post-removal build with a source (refuses; `skip` starts
  empty); `store.import/` with no source (refuses); `import.failed` (refuses; `retry` and `skip`
  work); an unknown `HUB_IMPORT` value.
- **Source checks**: a non-empty `-journal` and a non-empty `-wal` (refused, no state written,
  the same on a second start); an empty `-journal` (accepted); with `HUB_IMPORT_FROM=/x/old.db`,
  the check looks at `/x/old.db-journal`; a leftover journal on a start that won't import (a
  `store/` exists) is ignored; `HUB_IMPORT=rollback-journal` with a hot journal rolls it back
  and imports, without one is ignored.
- **One-shot escapes**: `retry:` with the recorded attempt id imports again; with another id,
  logged and ignored; left set after a successful import, ignored; `skip` with nothing to skip,
  ignored.
- **Attempts**: three processes killed mid-import (child-process kills) leave
  `import.failed` "3 attempts didn't finish" on the fourth start, with no fourth import; a
  successful import deletes `import.attempts`.
- **Preflight**: an injected free space below the estimate refuses with no state written; the
  staged store never deletes a block for the cap.
- **Two hubs**: a second process on the same `HUB_DATA_DIR` during an import is refused at the
  lock and never touches `store.import/`.
- **Staging**, with 0010's `CrashFs` and child-process kills:
  - killed after systems, during points, between the final checkpoint and the rename, and
    after the rename: at restart, either `store/` doesn't exist and the import runs again
    completely, or `store/` holds the whole import, never a mix;
  - a leftover `store.import/` is removed and rebuilt;
  - the rename is the only commit: no `store/` ever holds `hub/import` other than a final state.
- **Failure**: a source that returns `SQLITE_CORRUPT` mid-read; `ENOSPC` in the staged store
  (degraded; `store.import/` removed before `import.failed` is written); the series cap reached
  for more than 1% of points; more than 1% of points later than the import's hub time; each
  writes `import.failed` with its attempt id, exits non-zero, and isn't retried until
  `HUB_IMPORT=retry:<id>`.
- **Rows** (§4), each a table row: ids empty, `.`, `..`, 255 and 256 bytes; `push://` and a
  polled URL; `javascript:` and a URL without a host; `poll_interval_secs` `0`, `-1`, `4`, `5`
  and `86,401`; a 256-byte name; an info field of 257 bytes; `token = ''`; a token of 1,025
  bytes; a token equal to the admin token; a `NULL` and a wrong-typed column; alert fields over
  the limits; an alert id of 257 bytes; 1,001 alert records on one system, acknowledged and
  not; metric names of 261 bytes (kept) and 262 bytes (skipped); a name outside the enum; a
  container mount; a `cpu` of `9e16`; duplicates and disorder; `metric_retention` rows counted
  and warned about.
- **Clock reference**: a poll-path point 300 s ahead of the import's hub time (imports, the
  point itself dropped) and 301 s ahead (fails with the instruction); **a push-only fixture**
  whose file mtime is a day ahead of the hub clock (fails), and one whose mtime is behind it
  (imports); a push-path point in the future (dropped, counted).
- **Span ranges**: a series with one point at `ts = 0` and the rest recent: the number of
  span queries stays proportional to the recent range (a query counter), and the old point is
  counted as older than every tier's retention.
- **Tiers**: a point older than raw retention lands only in minute and hour; older than minute
  retention only in hour; older than hour retention nowhere; a stale legacy series doesn't count
  as active.
- **Rollups**: after a multi-hour import, every minute and hour bucket equals a recomputation
  from the imported points (no split buckets).
- **Alerts**: an acknowledged incident first stored more than 90 days ago, still active,
  survives the first retention pass and the first poll, and stays acknowledged.
- **`last_issued`**: after an import whose data ends two weeks ago, the store opens with nothing
  pending, and a live point lands after every imported one; after an import that took longer
  than 300 s (an injected clock), the store opens with nothing pending, since `finish` used the
  final sample.
- **Offline time**: an imported polled system whose legacy `last_seen` is two hours old is
  offline for two hours by `offline_since` at the first open; a push system's starts at the
  open.
- **Alert order**: imported records keep their `stored_at` order in `last_seen_active`, with no
  ties.
- **Non-UUID polled ids**: counted and warned about, named in `Debug` form.
- **Serving**: during the import `/api/health` is 200 with `status: importing`, the SSE route is
  200 with one `importing` event and then closes, another route is 503 with `Retry-After`, and a
  push handshake gets `auth_error`; after it, all work, and an `EventSource` opened during the
  import is receiving summaries.
- **Batching**: the import's catalog writes are at most one `fsync` per 1,000 operations.
- **After**: the plaintext warning on this start and the next; a changed `SourceIdentity`
  warned about with the missing-system count; the source file's bytes and mtime unchanged.
- **Performance**: an import of 50 million points, timed and recorded, for the README's
  estimate.

## Impact on `docs/ARCHITECTURE.md`

- **Storage**: a short "upgrading from SQLite" note (the staged import, its states and escape
  hatches), removed a release after the feature.
- **Components**: startup order (configuration, `LOCK`, source checks, preflight, bind,
  import, open, ingest).
- **Domain model**: the glossary terms above, and the push-handshake contract's import-time
  answer.
- **Open questions**:
  - closes: the legacy-id cleanup SQL (legacy ids are skipped on import), and "`DELETE` takes
    the stored id as it is" (0012 now parses ids);
  - replaces: the `sqlite3 system-hub.db` advice in the README (`README.md`, Database section)
    with `GET /api/storage` and the import counts.

## Rollout / migration notes

- One-way. Stop the old hub, roll back a hot journal if the new hub reports one
  (`HUB_IMPORT=rollback-journal`), and back up `system-hub.db`.
- The static-files prerequisite ships first; without it, Compose users would keep the old
  dashboard, whose admin actions no longer work (RFC 0012).
- The `sqlite-import` feature is compiled in for release N (the release that ships 0010–0013)
  and removed in N + 1, together with `rusqlite`. The release-skip refusal (§1) stays in every
  release after N.
- History collected by agents during the import isn't recorded: the hub serves 503 and agents
  retry. The README gives the expected duration.

## Review

This RFC holds this finding from the first `rfc-adversary` pass on the undivided draft (the
full table is in RFC 0010's Review):

| Finding | Verdict | Resolution |
|---|---|---|
| import: future timestamps pin series; not resumable; over-long ids and keys; non-ASCII mounts; plaintext `system-hub.db` | CONFIRMED | future points dropped; one restartable operation; skip-and-count rules; 0010's metric-name rule; plaintext warning (§2–§5) |

`rfc-adversary`, first pass on this RFC after the split. Every finding was acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| a deterministic failure (the hot journal a `docker stop` leaves; row decode errors; temp space; `ENOSPC`) becomes a wipe-and-restart loop with no way out | CONFIRMED | hot journal refused with an instruction and no state written; row errors are counted skips; terminal `import.failed` with `HUB_IMPORT=retry` and `skip` (§1, §2, §4) |
| "empty the store" undefined; a crash mid-wipe attaches old blocks to reassigned series | CONFIRMED | a staged `store.import/` published by one rename; restart deletes one directory (§2) |
| the import can run into, and later wipe, a store holding live data | CONFIRMED | the import runs only while `store/` doesn't exist; `NotApplicable` recorded on a fresh start; an interrupted import without its source refuses (§1, §2) |
| a degraded or full store still gets `Done`, losing history for good | CONFIRMED | degraded mode fails the import; store rejections over 1% fail it; `SeriesCapReached` and `KindMismatch` counted (§2) |
| alert records imported with `last_seen_active = stored_at` resurface acknowledged incidents; the cap drops long-running ones | CONFIRMED | `last_seen_active` = import hub time; the cap keeps acknowledged records first (§4) |
| "at most 24 h of rows" is false; the global sort is unindexed; startup blocked with the port closed; liveness probes kill it | CONFIRMED / PLAUSIBLE | span-by-span index range scans; each tier takes only what it keeps; bind first, 503 with progress, health 200, push `auth_error`; duration documented; the header no longer claims no gap (§3) |
| dropping "future" points against a hub clock that is behind drops everything; `last_issued` unspecified | CONFIRMED / PLAUSIBLE | poll-path points as the clock reference, failing the import on a clock behind by > 300 s; `last_issued` persisted as stated (§3) |
| legacy rows the new rules refuse (interval out of range, oversize values, `token = ''`, empty id, names outside the enum) and rows that fail to decode | CONFIRMED | every row through the live constructors; `Value`-typed reads; each case a counted skip, clamp or cut (§4) |
| the sweep and span close driven by hub time split imported buckets | PLAUSIBLE | the import's cursor drives `close_through`; the staged store runs no sweep or retention; a recomputation test (§3) |
| skipping release N silently starts with an empty registry | CONFIRMED | a refusal without `rusqlite` in every later release; the §1/Rollout wording reconciled (§1, Rollout) |
| rollback and re-upgrade silently discard registry changes; `SourceIdentity` never read | CONFIRMED | a changed identity warned about, with the missing-system count; the README states the registry cost and when to scrub (§2, §5) |
| the plaintext warning fires once | CONFIRMED | at every start while the file exists (§5; 0011 §6) |
| catalog writes per record | PLAUSIBLE | `batch` in chunks of ≤ 1,000 operations or 4 MiB (§4) |
| `metric_retention` dropped silently | PLAUSIBLE | counted, and warned about with the equivalent `PUT` (§4) |
| domain seams: `Meta` ownership, no wipe API, "resumable" wording, legacy `DELETE` ids | CONFIRMED | `hub/` keys in `Meta` are hub-owned (0011 §1); no wipe needed (staging); 0010's Review notes the wording; 0012 parses ids (Domain impact) |
| missing test rows for all of the above | CONFIRMED | rows added (Testing plan) |

Came closest and survived: the mixed-version fleet (no contract change beyond the import
window), A03 (fixed SQL on a read-only handle), and the future-point rule for live
`NotAfterLast` (now with `last_issued` persisted).

`rfc-adversary`, second pass on this RFC. Most first-pass resolutions held (the staged rename,
never importing into a live store, degraded failing the import, import-time alert times, the
cursor-driven spans, the release-skip refusal, the identity warning, batched catalog writes,
the `metric_retention` warning, the index claim). The failure-loop and clock resolutions held
only in part. Every finding was acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| a push-only fleet has no clock reference, so a hub clock that is behind drops history and records `Done` | CONFIRMED | the reference is the later of the newest poll-path point and the source file's mtime; drops as "future" count toward the 1% failure threshold; a push-only test (§2, §3) |
| "failure is terminal" covers only failures the process records: `ENOSPC`, OOM or panic, `retry` left set | CONFIRMED | `store.import/` removed before `import.failed` is written; an attempt counter stops after three unfinished attempts; `retry:<attempt id>` is one-shot (§1, §2) |
| the span loop's range is set by timestamps push clients control | CONFIRMED | per-series `MIN`/`MAX` clipped to the longest tier retention; only covered spans queried; `ORDER BY timestamp`; a `ts = 0` test (§3) |
| the hot-journal instruction can't run in the shipped image; the backup copies a torn file; the journal name is literal; the check runs on every start | CONFIRMED | `HUB_IMPORT=rollback-journal` rolls the journal back from the hub itself; the README says roll back, then back up; names from the source path; checked only when importing (§1, §5) |
| no lock covers the state decision and the delete of `store.import/`; the layout disagrees with 0010 | CONFIRMED / PLAUSIBLE | `main` takes 0010's `LOCK` before reading any state; one layout in 0010 §6 (§1) |
| a 503 on the SSE route kills open dashboards; health 200 passes readiness | CONFIRMED / PLAUSIBLE | the SSE route answers 200 with an `importing` event and closes; the README tells readiness probes to check `status == "ok"` (§3, §5) |
| the upgrade never delivers the new dashboard to Compose users | CONFIRMED | the static-files prerequisite ships first (owner's decision; 0010's header, Rollout) |
| `last_issued` isn't the import's hub time when poll points are kept ahead, or when the import is long | CONFIRMED / PLAUSIBLE | every point after the import's hub time is dropped; `finish` gets the time sampled at `finish` (§3) |
| imported systems have no `last_seen`, which "delete offline" depends on | PLAUSIBLE | adopted: polled systems' legacy `last_seen` becomes `last_contact` in the first checkpoint; push systems start at the open (§4) |
| disk space: source, staging and the cap share a volume | PLAUSIBLE | adopted: a free-space preflight that writes no state; the cap off in staging (§2) |
| inventory and domain impact incomplete | CONFIRMED | the `auth_error` row, the `/api/health` body, the README's `sqlite3` advice, the ARCHITECTURE glossary and contract, and Ingestion in Domain impact (§3, API9, Domain impact, Impact) |
| imported alert records all share one `last_seen_active` | CONFIRMED (low) | offset by rank, keeping `stored_at` order (§4) |
| after `skip`, the plaintext warning says to scrub the only copy | CONFIRMED (low) | the `Skipped` wording (0011 §6; §5) |
| legacy push systems given a URL become polled and are refused (from 0012's pass) | PLAUSIBLE | imported polled systems with non-UUID ids are counted and warned about (§4) |

Came closest and survived: orphaned legacy rows (the bundled SQLite enforces foreign keys, so the
old hub could never store one), and a crash between the rename and the directory `fsync` (at
worst `store.import/` survives and is rebuilt).
