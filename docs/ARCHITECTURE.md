# Architecture

This document describes the system as it currently exists. It is a living document: whenever
a change alters components, data flow, protocols, schema, or trust boundaries, update the
relevant section in place rather than appending a changelog entry. See `CLAUDE.md` for the
policy on when this file must be updated, and `rfcs/` for the design rationale behind past and
proposed changes.

## Components

Two independent Rust binaries live in this repository. They are not a Cargo workspace — each
has its own `Cargo.toml` and `Cargo.lock` and is built/tested separately.

### `system-agent` (repo root, `src/`)

Runs on every monitored Linux host. Responsibilities:

- Collects local system metrics on a background loop (`collectors/`): CPU, memory, disk,
  network, processes (`system.rs`, via `sysinfo`), installed packages (`packages.rs`, shells
  out to `dpkg`/`rpm`/`pacman`/`apk`), systemd units (`services.rs`), Docker containers
  (`containers.rs`), and listening TCP ports (`ports.rs`).
- Classifies its **execution environment** once at startup, before the first reading (RFC
  0014 §3): `collectors/environment.rs::gather_evidence` reads the container markers
  (`/.dockerenv`, `/run/.containerenv`, `/run/systemd/container`, the `container` and
  `KUBERNETES_SERVICE_HOST` variables) and the virtualisation evidence (the `hypervisor` CPU
  flag, DMI vendor and product, `/sys/hypervisor/type`, a Microsoft kernel release) off the
  runtime, and the pure `environment::classify` turns it into bare metal, a virtual machine,
  a container or undetermined. Only an explicit marker makes a container. A file that can't
  be read is no evidence. The result is logged once and stamped by the sampler on every
  snapshot, and `/api/system` serves it as `environment` through a wire DTO
  (`models.rs::EnvironmentInfo`).
- In a container, locates the **monitored cgroup** at startup (RFC 0014 §4):
  `gather_cgroup_evidence` reads `/proc/self/cgroup`, the cgroup2 line of
  `/proc/self/mountinfo`, `cgroup.type` at the mount point and `/proc/1/cgroup`, and the
  pure `environment::cgroup::cgroup_access` picks the root of a private cgroup namespace
  (it has `cgroup.type`) or else the agent's own cgroup, and places PID 1 inside or outside
  it. No readable v2 hierarchy (v1 only, no cgroup2 mount, the agent outside the mounted
  subtree) is `cgroup: "unreadable"`, and the container reports the kernel's values.
- Measures a container against its **resource capacity** (RFC 0014 §4, §6). Each tick
  `collectors/environment.rs::read_cgroup` reads `cpu.stat`, `memory.current`,
  `memory.stat` and `memory.swap.current` from the monitored cgroup, and `cpu.max`,
  `memory.max` and `memory.swap.max` from it and each visible ancestor (at most 32 levels);
  an absent limit file is unbounded, a malformed or unreadable one fails its reading group.
  The pure `environment::sourcing::choose_readings` then decides each **reading group**
  (CPU, memory, swap) from the cgroup or the kernel by the group's lineage: a group read from
  the cgroup once is never read from the kernel again, and on a failure its last values are
  *carried* for up to 30 s, then *unavailable*. The lineage and carry live in the collector,
  so a rebuilt sampler inherits them, and each transition is logged once. CPU is usage over
  capacity between two readings; memory is the working set (`memory.current` −
  `inactive_file`) over the tightest limit. Uptime is the container's PID 1's and process
  memory % is over the memory capacity, unless PID 1 is outside the monitored cgroup (a
  shared PID namespace). Load average and `logical_cores` stay host-wide.
- Measures **steal time** on every environment (RFC 0014 §5). Each tick
  `collectors/environment.rs::read_steal` reads `/proc/stat`'s aggregate `cpu` line into
  `StealCounters`, and the pure `environment::usage::steal_share` turns two readings into
  `cpu.steal_percent`: Δsteal over Δtotal. It is the host kernel's figure, so in a container
  inside a VM it is the VM's, and on bare metal it is a real 0. With no previous counters,
  no steal column, time standing still or a counter going backwards it is `null`, never a
  guess. It isn't a reading group: nothing is carried, and no alert rule reads it.
- Reads the system in exactly one place (RFC 0014 §6). `collectors/sampler.rs::Sampler` owns
  one long-lived `sysinfo::System`, so CPU usage is measured over the time since the previous
  reading, and the previous cgroup CPU counters and kernel steal counters. Its first reading only primes it and is
  never published. `main` primes it, waits
  1 s and reads the startup snapshot before the listener is bound.
  `collectors/mod.rs::background_collector` then reads every 2 s, the first tick a full
  period after startup, in `spawn_blocking`. It publishes each snapshot (`snapshot.rs::
  PublishedSnapshot`: the snapshot, `collected_at`, the monotonic `read_at` and a
  `SnapshotSeq`) on a `tokio::sync::watch` channel, then records it in history and evaluates
  alerts on it at its `collected_at`. Every `/api/system*` route, the SSE and WS streams and
  the push client read that channel, so request rate doesn't drive collection cost. A panic
  in a tick rebuilds the sampler, at most once a tick. The rebuilt sampler's first publish is
  a full period later, and its seq continues the old one's. If the collector task
  itself ends, `main` logs it once and the push client stops.
- Stops serving a **stale snapshot**, one read more than 30 s ago on the monotonic clock
  (a hung read, or a sampler that keeps panicking; the collector logs a hung read once and
  never abandons it). The `/api/system*` routes answer `503 {"error": "stale snapshot"}`, so
  a polling hub marks the system offline. The SSE and WS streams send one `stale` event and
  then nothing until a fresh snapshot. The push client closes its connection, and makes no
  new handshake until a snapshot is fresh again. Alerts, history and application routes keep
  answering with what they last held.
- Keeps a ring-buffer history of key metrics in memory (`state.rs`, 3600 points).
- Evaluates alert rules against live metrics (`alerts.rs`) — threshold + duration + cooldown
  model, default rules for CPU/memory/disk warning and critical. Each uninterrupted breach is
  one alert incident with a stable incident id (agent run + sequence), so the hub can store
  one record per incident.
- Exposes a REST API, SSE streams, and a WebSocket endpoint (`routes/`) for local/direct
  consumption, plus serves a static single-file dashboard (`static/index.html`). Its header
  shows the execution environment from `/api/system`'s closed enums (adding "cgroup
  unreadable" for a container that reports the kernel's figures). Its core count is always
  the host's, so in a container it reads "Host cores": with a readable cgroup the CPU %
  beside it is over the container's capacity instead.
- Optionally authenticates inbound API requests via a shared bearer/API-key/query-param token
  (`auth.rs`, `SYSTEM_AGENT_TOKEN`). Auth is opt-in: if the env var is unset, the API is open.
- Optionally pushes periodic snapshots to a `system-hub` instance over WebSocket, MessagePack-
  encoded (`push/`), if `PUSH_TO`/`PUSH_TOKEN`/`PUSH_INTERVAL` are configured. Its **push
  system id** is resolved once, in the synchronous `start`, before the runtime exists
  (`push/identity.rs::resolve_push_id`, RFC 0016 §6): `SYSTEM_AGENT_ID_FILE` when set and
  present, else the machine id, the dbus machine id, the host name
  (`/proc/sys/kernel/hostname`, no binary run), or a random UUID, each under the hub's
  `SystemId` rule. A missing id file is written with the resolved id through a temporary file
  and a no-clobber `hard_link`, so it survives container re-creations. A refused id file exits
  78, an I/O failure exits 1. The push task gets the id by value and reuses it on every
  reconnect. Each push tick
  sends the published snapshot, its frame `timestamp` being the snapshot's `collected_at`,
  unless the hub already has that snapshot's seq (kept across reconnects). The hub is never
  sent the same snapshot twice. Since a tick may send nothing, the client also reads the
  connection, and ends its session as soon as the hub hangs up. When
  applications are configured, the same connection also carries each published scrape round
  as an application frame (`push/application_frame.rs`): once right after the handshake if a
  round exists and the scrape loop still runs, then on every publish. The reconnect loop keeps
  the round receiver across connections; once the scrape loop has ended it gives the channel
  up for the agent's lifetime, logging that once, and never re-sends the ended loop's last
  round.
- Optionally mails sealed reports to a hub instead (`mail/`, RFC 0017), when `MAIL_TO` is set
  (with `PUSH_TO` too, startup is refused with 78). `mail/config.rs` parses the `MAIL_*`
  variables before the runtime. `mail/client.rs` looks every 2 s: it offers the published
  snapshot to the pure `MailBatch` (`mail/batch.rs`: one sample per `MAIL_SAMPLE_INTERVAL`, at
  most 60, and every alert incident active since the last report, at most 64), and at each
  `MAIL_INTERVAL`, or at once when an incident becomes active (`IncidentPace`: at most one a
  minute), closes it into a `MailReport` (`mail/report.rs`, named MessagePack), seals it with
  XChaCha20-Poly1305 under `MAIL_KEY` with a random nonce (`mail/seal.rs`), armours it in a
  `text/plain` body, and queues it in the pure `Outbox` (`mail/outbox.rs`: at most 288, the
  oldest dropped first). Sending uses lettre's Tokio SMTP transport: STARTTLS required by
  default, certificates always verified, plain SMTP only to a loopback relay; a 4xx or an
  unreachable relay is retried with a backoff from 30 s up to the interval, a 5xx drops the
  report. The outbox is in memory: a restart loses what it held.
- Reads which Spring Boot applications to monitor from `SPRING_BOOT_APPS` and its companion
  variables (`applications/config.rs`, RFC 0009), and where to serve from
  `SYSTEM_AGENT_LISTEN` (`listen.rs::ListenAddress`, RFC 0015: a literal IP and port, default
  `0.0.0.0:9090`). `main` is synchronous: it parses this configuration before it builds the
  Tokio runtime, so a malformed value refuses startup (exit code 78, `EX_CONFIG`, used for
  nothing else) before any task, bind or connection exists. That the listen parse happens
  before the runtime can't be seen from outside the process, so it is a review rule; the
  tests pin that it precedes the runtime's first log line. The agent logs the address it
  bound (`local_addr`, so port 0 shows the OS's choice) and suggests its dashboard at
  `listen::reachable_at`: `localhost` for an unspecified IP, else the bound one. Every other startup failure is a typed `StartupError` that exits 1.
- Scrapes those applications' Actuators (`applications/`): a scrape loop runs a scrape round
  over every application concurrently, publishes it on a `watch` channel that `AppState`
  holds the receiver of, then sleeps a full scrape interval, so two rounds are always at least
  one interval apart. Requests reach only the configured host: one shared HTTP client with
  redirects and environment proxies off, a 2 s connect and 5 s whole-request timeout, and
  bodies read chunk by chunk up to 64 KiB. `GET /api/applications` serves the latest round,
  and nothing once the loop has ended. Health changes are logged when an application becomes
  unreachable and when it recovers, naming the application and the failure, never a URL.

### `system-hub` (`system-hub/`)

Aggregates data from many `system-agent` instances. Responsibilities:

- Maintains a registry of known systems (`db/mod.rs`, SQLite table `systems`) — name, URL,
  token, poll interval, last-known status/OS info. A system's info (hostname, OS, kernel, CPU
  model and cores) is filled once, while its hostname or OS is missing
  (`registry.rs::needs_system_info`, the rule both ingestion modes apply;
  `db/mod.rs::update_system_info`, which never writes the memory columns). Its **memory
  capacity** follows the agent (RFC 0014 §8): every push frame and every poll that reports
  both its bytes (at most 2^63 − 1) and a display (1 to 64 bytes, with no control character)
  refreshes the stored pair when it differs (`registry.rs::memory_capacity_refresh`, over
  `MemoryCapacity::stored` and `::reported`), written by `db/mod.rs::update_memory_capacity`,
  two fixed columns in one statement and the columns' only writer. Both modes run it on the
  blocking pool. A report without the whole pair, or that breaks either bound, never clears
  it. So a containerised agent's system shows the container's limit, and a VM's memory
  hotplug shows too. The dashboard labels the figure "Memory", true on every environment.
- Push registration never replaces a row (RFC 0007 §4): `db/mod.rs::insert_system_if_absent`
  checks the id with a query that reads only whether the row's `url` is the one presented,
  compared by SQLite so any stored type maps (`Registration::Inserted` / `Known { same_url
  }`), and inserts with `ON CONFLICT(id) DO NOTHING`, so for a known id nothing but the check
  runs. When the check or the insert fails, or the registration panics, the handshake answers
  `registry unavailable`. A new push id is registered `unknown`: it reads online only once a
  snapshot is stored.
- Tracks which push connection is **current** for each push system (`presence.rs::
  PushPresence`, RFC 0016 §2), under the presence lock (`AppState::presence`; lock order: the
  presence lock, then the database mutex, then the live state). The handshake's unit
  registers and, only for a push system's id, accepts the connection, which then holds a
  `ConnectionLease`; a polled system's id gets none, so its connection claims nothing and its
  end writes nothing. A snapshot frame claims currency just before its store; between open
  connections only a snapshot moves it. Only the current connection's end marks the system
  offline and evicts its live metrics (`end_connection`).
- Before it builds `AppState`, `main` turns every `online` push row `unknown`
  (`Database::reset_status`), since the previous process wrote it. A **disconnection sweep**
  (`push/sweep.rs`) runs every 30 s on the blocking pool: over a narrow read of every row's id,
  url and status (`Database::system_sources`, each row mapped on its own), it marks offline
  each push system with no live current connection, or reading online while its current
  connection never claimed, once the hub has run 120 s (`PushPresence::sweep`). A row whose
  id breaks `SystemId` gets `invalid system id`.
- Three ingestion modes, all able to run simultaneously per fleet:
  - **HTTP poll** (`collector/`): every 30 s the hub reads the registry (on the blocking
    pool; there is no systems cache, and a tick whose read fails polls the systems of the last
    read that succeeded) and calls each enabled polled system's (`registry::polled_systems`:
    never a push system) `/api/system`, then `/api/alerts` and
    `/api/applications` (it ignores `poll_interval_secs`). No poll follows a redirect, so the
    per-system token never leaves the registered host; a URL that redirects shows as offline,
    naming the 3xx. The `/api/system` answer is read up to 4 MiB
    (`collector/capped_body.rs::read_capped`, shared with the applications poll): a larger one,
    a transport error, another status or a body that isn't the answer's JSON marks the system
    offline (`PollFailure`). Its snapshot store, registry fill and offline marking run on the
    blocking pool. Used when the hub can reach the agent (agent behind NAT/firewall from the
    hub's perspective is *not* this mode). A polled scrape round is admitted and stored like
    a pushed one, under a pace kept per system; a 404 (an older agent) or a `null` round
    forgets the shown round but keeps the recent rounds; any other failure (another status, a
    body over 256 KiB, bad JSON, a refused round) is logged and changes neither the shown
    round nor the system's status.
  - **WebSocket push** (`push/`): agents connect out to the hub's `/api/push` endpoint,
    authenticate with a shared token (`HUB_PUSH_TOKEN`), then stream MessagePack-encoded
    snapshots every `PUSH_INTERVAL` seconds, and application frames when the agent watches
    Spring Boot applications. Agents auto-register on first successful push
    handshake — no prior entry in `systems` is required. Every binary message spends the
    connection's decode budget before it is decoded (RFC 0007 §3).
  - **Mail** (`mail_intake/`, RFC 0017): when `HUB_MAIL_DIR` and `HUB_MAIL_KEY` are both set
    (one alone, a key that isn't 32 bytes of base64, or a directory without `new/` and `cur/`
    refuses startup), every 10 s, on the blocking pool, the hub takes at most 256 messages from
    the Maildir's `new/`, oldest name first (`scan.rs`), and deletes each once handled,
    accepted or refused; a file over 1 MiB is deleted unread. For each (`ingest.rs`): the first
    armour block in the decoded text parts (`mail-parser` undoes a relay's transfer encoding;
    no header is trusted), the sealed report opened under the id's key (`seal.rs`), the report
    parsed within its bounds (`report.rs`), its freshness (`receipt.rs`: at most 5 minutes
    ahead, at most 7 days old), then one transaction (`db/mail.rs::store_mail_report`). The
    newest report of a system also keeps its newest sample as live metrics, fills the registry
    (`registry_fill.rs`, shared with push) and stores its scrape round. An overdue sweep
    (`overdue.rs`) runs every 60 s. `system-hub mail-key <system id>` (`command.rs`) prints a
    system's mail key and touches no database.
- Stores every snapshot, pushed or polled, through one **snapshot rule** (`snapshot.rs`,
  RFC 0007 §1): at most 1024 disks, valid mount points, finite values; what it leaves out is
  counted and logged, at `warn` at most hourly per system (`left_out_log`). `snapshot_intake.rs`
  is the one step both modes share: the rule, then `Database::store_snapshot`, then the
  system's live metrics.
- Persists time-series metrics (`metrics` table) and alert history, one record per alert
  incident (`alerts` table) to SQLite. A snapshot's points, the prune of each of their series,
  and the system's status and last seen are written in one transaction, with cached
  statements, under the database mutex (`db/history.rs::store_snapshot`, RFC 0007 §2). The
  prune deletes at most 16 of a series' rows past its retention per stored point
  (`metric_retention` table, default 24h), so a backlog drains over several snapshots.
  Application metrics (`app:*`) are stored at hub time and pruned by a task every 10 minutes
  (`retention.rs`), past their `metric_retention` row or 24h.
- Admits application rounds (`applications.rs`, RFC 0009 §8): an exact re-send of one of a
  system's last 8 accepted rounds (same round id and same content digest) is a duplicate, and
  each source (one push connection, or the poller for one system) is paced by its own token bucket (2 rounds, then one per
  8 s). The check and the store are one step under the database mutex
  (`Database::store_round`).
- Holds each system's latest accepted scrape round in memory (`live_applications` in
  `state.rs`) with the ids it accepted recently; a push disconnect keeps both, and deleting
  the system forgets them. `GET /api/systems/:id/applications` serves it with its age and
  freshness, computed on the hub. The dashboard's system detail shows it as an
  Applications section, re-read on each SSE summary and dimmed when stale, and draws an
  application's heap and request-rate history from its `app:*` series when its row is
  opened.
- Holds each system's **live metrics** in memory (`state.rs::live_metrics`, one
  `Arc<LiveMetrics>` per system): the latest snapshot as the rule kept it, moved in by the
  transaction that stored it, and shared rather than copied by every reader. An entry is
  evicted when the system's current push connection ends (`push/ingest.rs::end_connection`)
  and when
  the system is deleted (`AppState::evict_live_metrics`), so live state never outgrows the
  registry. Deleting a connected push system also ends its connection: its next frame finds
  no row (`SnapshotStored::SystemGone`), and the agent reconnects and registers the id again,
  with no history.
- Exposes a REST API and an SSE summary stream (`routes/`), and serves a static fleet
  dashboard (`static/index.html`) that updates every 5s via SSE. One publisher
  (`routes/sse.rs::start_publisher`) builds the summary every 5 s on the blocking pool,
  serialises it once, and replaces it in a `watch` channel (`AppState::summary`) that every
  subscriber reads, so a tick costs one serialisation whatever the number of subscribers. A
  new subscriber gets the current summary at once. The hub refuses to start when the first
  summary can't be built (`StartupError::FirstSummary`); a later tick that fails keeps
  serving the previous one. The dashboard is served from
  `HUB_STATIC_DIR` when it is set (it must then be a directory the hub can search, or the
  hub refuses to start), and from `static` under the working directory otherwise. The container image sets it to
  `/usr/share/system-hub/static`, outside the `/app` data volume, because a named volume is
  filled from the image only when it is created and would otherwise pin the dashboard to the
  volume's first image. `main.rs::app` assembles the served router, so a test can reach it.

### `hub-store` (`system-hub/hub-store/`)

The hub's store crate (RFC 0010), a member of the hub's Cargo workspace (`system-hub/` is a
workspace of two members sharing one `Cargo.lock`; the agent stays an independent crate). It is
being built in RFC 0010's implementation steps and **is not yet used by the hub**, which still
persists to SQLite (see Storage). Synchronous, with no async dependency and
`#![forbid(unsafe_code)]`. Today it holds the pure core of the series format:

- `value.rs`: `ValueKind` (percent, load, count, monotonic, bytes, rate, millis), each with
  its value scale, its domain range and its `Encoding` (delta, or delta of deltas for
  monotonic counters); `Scaled`, a value in its kind's stored unit (`round(v / scale)`),
  unconstructible outside the domain; `OutOfDomain`.
- `name.rs`: `MetricName` (1 to 261 bytes of UTF-8, no control character), `SystemKey` (1 to
  255 bytes), `Generation`; `InvalidName`.
- `tier.rs`: `Tier` (raw, minute, hour), `RollupTier`, `SpanStart`: spans of 1 h (raw,
  minute) and 1 day (hour), and the grace after which a span closes (`SpanStart::is_closed`:
  hub time past its end plus one bucket and one sweep interval).
- `codec/`: the chunk codec, versioned by a leading format byte and total on any input. A raw
  chunk (`RawChunk`, format `0x01`) holds at most 240 points or 1 KiB: a header (count, first
  timestamp, first value) then, per point, the timestamp's delta of deltas (`0` | `10`+3 |
  `110`+7 | `1110`+12 | `1111`+32 bits) and the value's zigzagged step (`0` | `10`+6 | `110`+13
  | `1110`+20 | `11110`+32 | `11111`+64 bits). A rollup chunk (`RollupChunk`, format `0x02`)
  holds one span of buckets (60 minute or 24 hour buckets): per bucket its index's delta of
  deltas (one bit when consecutive), its average's step, `avg − min`, `max − avg` and its
  count. Both reopen from their bytes, which is how a tail will be persisted.
- `rollup.rs`: `Accumulator`, a series' open bucket, summed in `i128`; its average rounds
  half away from zero.
- `series.rs`: `SeriesId` (interned, never reused), `SeriesKey` (system key, generation,
  metric name; its bytes start with the length-prefixed system key, so one system's keys never
  run into another's) and `SeriesRecord` (kind and last span per tier).
- `state.rs`: `SeriesState`, one series in the head: its open raw chunk, its open minute and
  hour buckets and chunks, sealing a chunk when it is full or a point lands in a new span, and
  the sweep that closes quiet series' buckets more than one bucket length past their end;
  `seal_closed` seals the open chunks of closed spans, and `holds_open` says whether the
  series still holds a span open (unsealed data in it, or an open bucket that closes into it).
  `state/tail.rs` persists it as the series' **tail**, checked whole when read back (kind,
  spans on their grid, chunks decodable, values in the domain, buckets consistent, and the
  fields against each other: every point and bucket in its chunk's span and none after the
  series' last time).
- `block.rs` and `block/`: **block files**, where each closed span of a tier lives once handed
  off out of redb (RFC 0010 §5, §6). `block/file.rs` is their I/O: `blocks/<tier>/`, created
  mode 0700; a file written to its `.tmp`, synced, then hard-linked to its name (which fails
  rather than replace an existing file) and the directory synced; reads by offset of the
  ranges the format names. The rest is pure. `block/format.rs`: the file format (`encode_block`,
  `BlockSummary`): a header (`HUBBLK`, format, tier, span, chunk count, index offset), the
  span's chunks ordered by (series id, seq), an index of 22-byte entries in index blocks of
  1,024, a summary of one entry per index block (first series, offset, CRC-32C), and a trailer
  (summary offset, CRC-32C of header and summary, magic). Each chunk's CRC covers its tier, span,
  series id and seq as well as its bytes, so an entry pointing at another series' chunk fails;
  every index block is checked against its CRC, its order (strictly by series and seq), its
  first series against the summary's and its offsets against the data region, and the header
  and summary against each other and the file's length, so a forged file whose CRCs were
  recomputed is still refused as malformed. `block/name.rs`: `BlockName` and its one canonical
  file name, `<span start>[.r<rewrite>].blk` (a `.tmp` suffix while being written; anything
  else in the directory is not the store's). `block/record.rs`: `BlockRecord`, the `blocks`
  row. `block/reconcile.rs`: the open's decision over the files on disk and the rows in redb
  (§6 step 2a), a pure plan: unlink what `pending_unlinks` names; delete a row-less file only
  as a provable redundant copy, (a) a handoff that crashed before its commit (no row, the span
  closed by the persisted clock, no tail open in it, its chunks in `chunks`) or (b) a rewrite
  that crashed before its swap (the row's lower rewrite number with its own file present);
  refuse to open on any other row-less file; open a row whose file is absent as unreadable.
- `clock.rs`: `HubClock` (hub time: `max(system clock, last issued)`, never backwards) and
  `retention_now`, the guarded retention clock (at most twice real time, never past either
  clock, unchanged by the first pass after an open).
- `retention.rs` and `retention/`: retention's pure core (RFC 0010 §5, RFC 0012 §2), not yet
  driven by a retention pass. `retention/policy.rs`: `TierPeriod` (a period inside its tier's
  bounds), the global `RetentionPolicy`, per-system `Override`s whose tiers follow the global
  policy or a fixed period, each with at most one `PendingShortening` (a change at least as
  long as what is enforced applies at once and drops it; a shorter one replaces it), and
  `Policies` (the period each system's tier enforces, and the longest any system enforces).
  `retention/liveness.rs`: `chunk_liveness`, dead when unmapped, tombstoned, or when the whole
  span is older than the retention clock minus the system's period. `retention/files.rs`:
  `plan_files`, the fate of each block file at a pass (retire when every chunk is dead;
  rewrite one file, the earliest span end first, when erased chunks reach a quarter of it or
  when only longer overrides keep it past the global period; leave files in flight alone and
  never rewrite a damaged one), from the generations each file holds.
- `capacity.rs`: disk space's pure core: the storage cap (`StorageLimit`: bytes, a share of
  the volume, 80% by default, or none) and the files it retires (`cap_retirements`: oldest raw
  first, then oldest minute, never hour, never a file in flight), the open-time floor (the
  larger of 1 GiB and 2% of the volume, plus twice a rewind's bytes), and when `hub.redb` is
  worth compacting (over 1 GiB and over a quarter of the file reclaimable).
- `tables.rs`: the store's redb tables (`meta`, `series`, `series_key`, `points_log`, `tails`,
  `chunks`) and the points log's entry format.
- `store.rs` and `store/`: the `Store` (generic over the hub's commit-hook change type).
  - **One writer thread** (`store/writer.rs`) owns every redb write transaction. Callers send
    requests over a bounded channel and wait for the answer. An `append` is stamped at hub time,
    checked and applied to the head (`store/ingest.rs`), and answered at once; a catalog
    transaction (`transact`) runs its closure inside the open transaction (`store/catalog.rs`,
    `CatalogTxn`: catalog tables as bytes, `notify`, staged points) and is answered after the
    commit holding it (`Commit::Durable` forces that commit; a write-free one per its `Answer`;
    a refusal only before its first write, otherwise the store fails stop).
  - **The group commit**: every commit interval (1 s by default), unconditionally, one redb
    transaction with 2-phase commit and quick-repair, made durable before it returns, holds
    the interval's points as one `points_log` entry, every chunk sealed in it, the tails and
    records of the series that sealed or are new, the slice of the **rotating tail flush** due
    by elapsed time (every series' tail once per rotation, 15 minutes by default), the catalog
    writes, the deletion of log entries older than the last completed rotation's start, and
    the clock. After it, the commit hook gets the interval's changes, in commit order.
  - **The span handoff** (`store/handoff.rs`): each sweep (every 60 s by default,
    `StoreOptions::sweep_interval`) closes quiet buckets and seals every open chunk of a closed
    span; the spans with chunks in `chunks` that are closed (past their end plus one bucket and
    one sweep) become due and, after the commit holding those seals, go to the **block writer
    thread**. It reads the span in a read transaction, writes its file and sends the outcome
    back; the writer inserts the `blocks` row and deletes the span's chunks in the next group
    commit, so a span is in exactly one of `chunks` or a file. A failed write leaves the span in
    redb, served from there and retried at the next sweep; more than three spans of one tier
    waiting on failures fail the store stop. `store/blocks.rs` keeps, per committed row, the
    file's summary (or that it is unreadable) and the handoff counts behind `Store::stats`.
  - **The head** (`store/head.rs`): active series in 16 shards behind leaf locks; only the
    writer changes it. **Queries** (`store/query.rs`) read committed chunks in a redb read
    transaction (a span with a `blocks` row from its file: the series' index blocks and
    chunks, each checked against its CRC, a failure answering `Corrupt` for that span only)
    and copy the series' uncommitted chunks and unsealed points from its shard, checking every
    value against the series' kind; an unsealed point at or before the last committed one was
    sealed meanwhile and is read once. A commit hook must not call back into the store.
  - **Recovery** (`store/open.rs`): the block directories created and every `.tmp` deleted,
    redb's own repair, then the head from the tails, then the block files reconciled with
    the rows (`block/reconcile.rs`'s plan over the tails and the persisted clock; a row whose
    file is missing or whose header or summary fails opens unreadable), then the points log
    replayed in order, each point applied only to a series whose last point is earlier, then a
    sweep. The open itself hands nothing off. A panic on the writer thread, an I/O error on
    commit or a refusal after a write fails the store stop: every later call answers `Failed`.

Measured encoded sizes (`hub-store/tests/size_budget.rs`, seeded generators over a day of
points; the test asserts these plus 10%):

| Kind (generator) | Bytes per raw point | Bytes per rollup bucket |
|---|---|---|
| percent, noisy (CPU) | 2.04 | 7.23 |
| percent, slow (memory, disk) | 1.10 | 4.51 |
| load | 1.00 | 4.25 |
| bytes (heap sawtooth) | 2.43 | 8.40 |
| rate | 3.00 | 9.64 |
| millis | 2.14 | 7.18 |
| count | 0.38 | 2.19 |
| monotonic (uptime) | 0.30 | 5.30 |
| **scale-target mix** (8 host series at 2 s, 5 × 11 application series at 15 s) | **1.25** | **4.99** |

## Data flow

![Data flow: in the agent, one background collector reads the system and its cgroup off the runtime and publishes the snapshot that history, alerts, the routes and the push client read; the hub's poller reads the agent's /api/system over HTTP, the agent's push client sends MessagePack frames to the hub's push receiver, and both write the hub's live cache and SQLite store](images/data-flow.svg)

| Mode | Direction | Protocol | Use case |
|---|---|---|---|
| HTTP Poll | Hub → Agent | REST + JSON | Agent reachable from hub; hub controls cadence |
| Push | Agent → Hub | WebSocket + MessagePack | Agent behind NAT/firewall from hub's side; lower latency, smaller payload (~50-70% smaller than equivalent JSON) |
| Mail | Agent → SMTP relay → Maildir → Hub | SMTP + sealed MessagePack | Agent can reach neither the hub nor the internet, only its network's relay; minutes of latency |

A push connection carries two kinds of binary frame: snapshot frames, and application frames
(one scrape round each). Every binary message first spends a token of the connection's decode
budget; a message past the budget is dropped undecoded and counted. The hub then tries a
snapshot, then an application frame, and drops whatever decodes as neither, counting it on the
connection (the first logged at `warn`, the count at the close). An old hub drops application
frames the same way.

## Domain model

This section is the context map and glossary that `CLAUDE.md`'s Domain-Driven Design rules
refer to. It describes where each module sits today, including where the code doesn't yet
match those rules (listed under Open architectural questions below).

### Bounded contexts

| Context | Crate | Domain core | Adapters (I/O) | Owns |
|---|---|---|---|---|
| **Host Telemetry** | agent | `environment/` (the pure core of RFC 0014: `mod.rs`: `ExecutionEnvironment`, `Hypervisor`, `ContainerRuntime`, `LoadScope`, `classify`; `evidence.rs`: `EnvironmentEvidence`, `ContainerMarker`, `CpuArchitecture` and the evidence parsers; `cgroup.rs`: `CgroupPath`, `CgroupEvidence`, `CgroupAccess`, `MonitoredCgroup`, `ResourceLimit`, `CpuCount`, `Bytes`, the cgroup file parsers, `cgroup_access` and the capacity rules; `usage.rs`: `Percent`, `LoadAverage`, `CpuCounters`, `cpu_usage`, `StealCounters`, `steal_share`; `sourcing.rs`: `choose_readings`, `SourcingHistory`, the reading groups, `Origin`, `ProcessView`, `uptime`, `process_memory_base`), `models.rs`, `snapshot.rs` (`CollectedSnapshot`, `PublishedSnapshot`, `SnapshotSeq`, `Priming` / `PRIMING`, `SnapshotFreshness` / `STALENESS_BOUND`, `StreamState` / `StreamEmit`; no tokio types), `state.rs` (`MetricsHistory` ring buffer; `AppState` is wiring, and `AppState::new` mints the agent run) | `collectors/*` (sysinfo; `dpkg`/`rpm`/`pacman`/`apk`, `systemctl`, `docker`, `ss` shell-outs); `collectors/environment.rs` (`gather_evidence` and `gather_cgroup_evidence` at startup, `read_cgroup` and `read_steal` each tick, all over a root path); `collectors/sampler.rs` (`Sampler`, `Gather`, `SysinfoSource`); `collectors/mod.rs` (`first_snapshot`, `background_collector`, `monotonic_now`, the snapshot `watch` channel: `SnapshotSender` / `SnapshotReceiver`) | the snapshot of the agent's execution environment and its recent history |
| **Alerting** | agent | `alerts.rs` (`AlertRule`, `AlertMetric`, `AlertOperator`, `AlertSeverity`, `AgentRun`, `IncidentId`, `Readings` / `Reading`, the per-rule `Breach` state, `AlertManager::evaluate` and `replace_rules`) | `routes/api.rs` alert endpoints, `routes/sse.rs` and `routes/ws.rs` alert streams | deciding when a metric breaches a rule, for how long, and cooldown; the identity of each alert incident |
| **Agent Access** | agent | — | `auth.rs`, `routes/*` | who may read the agent's API |
| **Telemetry Publishing** | agent | `push/identity.rs` (`AgentId`, `AgentIdRule`, `IdSource`, `Resolution`, `AgentIdError`, `IdFilePath`, `resolve_push_id`: the push system id, over injected paths) | `main.rs::push_target` (reads `PUSH_TO` and `SYSTEM_AGENT_ID_FILE` once, before the runtime); `push/` (WS client, agent-side `PushPayload`; `PushFeed` and `SnapshotCursor`, which sends each published snapshot once, by seq; `current_round` / `next_round` / `give_up_rounds` for the round channel); `push/application_frame.rs` (the application frame); `applications/wire.rs` (`ApplicationReportDto`, one report's wire shape, shared with the applications poll response) | sending snapshots and scrape rounds to a hub |
| **Application Telemetry** | agent | `applications/config.rs` (`ApplicationsConfig::parse` over a lookup function, `ApplicationName`, `ActuatorBaseUrl`, `ActuatorCredentials` / `BasicCredentials`, `ScrapeInterval`, `ApplicationsConfigError`); `applications/report.rs` (`ApplicationGauge`, `MeterValue`, `Meters`, `RawScrape`, `ApplicationReport`, `ApplicationHealth`, `ScrapeFailure`, `ApplicationVersion`, `rate_per_second`, `ScrapeHistory::advance`); `applications/round.rs` (`RoundId`, `RoundSequence`, `ScrapeRound`, `NamedReport`, `health_change`) | `main.rs::start` (passes `std::env::var`, logs and exits on a refusal); `applications/actuator.rs` (Actuator answers → meter values, health, version); `applications/scraper.rs` (the HTTP client: `Scraper`, `Timeouts`, the body cap); `applications/scrape_loop.rs` (`scrape_loop`, the round `watch` channel); `routes/applications.rs` (`GET /api/applications` and its JSON) | which Spring Boot applications the operator asked the agent to watch (RFC 0009) |
| **Fleet Registry** | hub | `models.rs` (`SystemInfo`, `SystemStatus`, `SystemId` and the default-name rule); `registry.rs` (`MemoryCapacity`, `memory_capacity_refresh`, `MAX_DISPLAY_BYTES`, `UptimeDisplay`, `LastSeen`, `StatusUpdate` / `after_snapshot`, `PollInterval`, `needs_system_info`, `SystemSource`, `PUSH_URL`, `polled_systems`); `presence.rs` (`PushPresence`, `ConnectionNumber`, `ConnectionLease`, `LeaseHandle`, `Ending`, `Sweep`, `OfflineReason`, `RECONNECT_GRACE`) | `db/mod.rs` `systems` table (`insert_system_if_absent` / `Registration`, `update_system_status` for the offline markings), `db/sources.rs` (`system_sources` / `SourceRow`, `reset_status`), `routes/api.rs` system CRUD | which systems exist, their config and last-known status |
| **Ingestion** | hub | — | `collector/` (HTTP poll: `PolledAnswer` / `PolledInfo`, the parsed answer and the system info the registry fill reads; `PollFailure`; `MAX_SYSTEM_BODY`; `collector/capped_body.rs`: `read_capped`, `CappedBodyError`), `push/` (WS receiver: handshake parsing via `authenticate` / `HandshakeRejection`, the handshake outcome `Handshake` / `Refusal` (with `RegistryUnavailable` / `RegistryFailure`) / `Answer`, the handshake and idle deadlines, the oversize linger; `push/ingest.rs`: hub-side `PushPayload`, `SnapshotFrame` / `PushedInfo`, `SnapshotRefusal`, `register_and_accept`, `register_if_new`, `ingest_frame`, `update_registry`, `end_connection`; `push/connection.rs`: `ConnectionState` (a connection's decode budget, pace and counts), `Tally`, `Occurrence`, `warn_first`, `DecodeBudget`, `DECODE_BURST`); `snapshot_intake.rs` stores one snapshot for either mode (`store_snapshot`: the rule, the store, the live metrics entry); `round_intake.rs` admits and stores one round for any source (`store_round`, `Arrival`); `application_wire.rs` holds `ApplicationFrameDto` / `ApplicationsResponseDto` / `ApplicationReportDto`, their `TryFrom` into `ScrapeRound` / `PolledRound`, and `FrameRefusal`; `collector/application_poll.rs` holds the applications poll (`poll_applications`, `ApplicationsAnswer`, `Unusable`, `log_refused_round`); `round_intake.rs` has `store_round` (push, the connection's pace by value) and `store_polled_round` (poll, the system's pace read and written under the database mutex); `push/config.rs` holds `PushAuth` / `PushToken`, `PushSocketLimits`, `PushConfig` (with `decode_refill`) and the production timings, `DECODE_REFILL` among them) | turning agent output into hub metrics, alerts and status |
| **Fleet History** | hub | `models.rs` (`AlertRecord`, `HubSummary`); `snapshot.rs` (`ReportedSnapshot`, `ReportedDisk`, `Snapshot`, `Scalar`, `MountPoint`, `SnapshotTime`, `LeftOut`, `snapshot_rule`, `MAX_DISKS`, `MAX_MOUNT_POINT_BYTES`, `LeftOutLog`, `left_out_log`, `snapshot_retention`, `LiveMetrics`); `applications.rs` (`ApplicationName`, `ApplicationHealth`, `ApplicationVersion`, `ApplicationGauge`, `Gauges`, `ApplicationReport`, `RoundId`, `ScrapeInterval`, `ScrapeRound`, `ScrapeRoundError`, `HeldRound`, `RoundDigest` / `RoundDigester`, `RecentRounds`, `SourcePace`, `admit`, `freshness`, `application_points`) | `db/history.rs` `metrics` / `metric_retention` tables (`store_snapshot` / `SnapshotStored`, `store_round`, the pruning), `db/mod.rs` `alerts` table, `state.rs` (`live_metrics`, `evict_live_metrics`, `live_applications`, `summary`), `retention.rs` (the `app:*` pruning task), `routes/*` (`routes/applications.rs`: `GET /api/systems/:id/applications`; `routes/sse.rs`: the publisher, `Summary`, `SummaryFailure`, and the wire DTOs `SummaryDto` / `LiveMetricsDto`) | stored time series, alert history, retention, each system's live metrics, and each system's shown scrape round |
| **Fleet Storage** | `hub-store` | `value.rs` (`ValueKind`, `Encoding`, `Scaled`, `OutOfDomain`), `name.rs` (`MetricName`, `SystemKey`, `Generation`, `InvalidName`), `tier.rs` (`Tier`, `RollupTier`, `SpanStart`), `block/` (`encode_block`, `BlockSummary`, `BlockName`, `BlockRecord`, `reconcile`), `codec/` (`RawChunk`, `RollupChunk`, `Bucket`, `decode_raw`, `decode_rollup`), `rollup.rs` (`Accumulator`, `AccumulatorRow`), `series.rs` (`SeriesId`, `SeriesKey`, `SeriesRecord`), `state.rs` (`SeriesState`, `Sealed`, the tail format), `clock.rs` (`HubClock`, `retention_now`), `retention/` (`RetentionPolicy`, `TierPeriod`, `Override`, `TierMismatch`, `PendingShortening`, `RetentionChange`, `RetentionOutcome`, `Policies`, `Owner`, `Tombstones`, `Liveness`, `Death`, `chunk_liveness`, `Holder`, `FileCondition`, `FileFate`, `plan_files`), `capacity.rs` (`Share`, `StorageLimit`, `CapCandidate`, `CapPlan`, `CapOutcome`, `cap_retirements`, `floor`, `check_floor`, `compaction_due`) | `store.rs` and `store/` (redb: the writer thread, the group commit, the head, queries, catalog transactions, recovery; the span handoff and the block writer thread); `block/file.rs` (block-file I/O); `tables.rs` |  how a series is stored: its value kind, its tiers and spans, the chunk codec and rollups (RFC 0010; being built, not yet used by the hub) |

Two pure modules belong to no context and have no domain term of their own:
`token_bucket.rs` (`TokenBucket`, `Refill`, `Empty`), the bucket arithmetic both `SourcePace`
and `DecodeBudget` wrap, and `hourly_warning.rs` (`HourlyWarning`, `hourly_warning`), the
at-most-hourly `warn` that `left_out_log` and the applications poll's `log_refused_round` share.

**Published contracts between contexts** (both sides must change together, and a
mixed-version fleet must keep working):

- *Push handshake*: Telemetry Publishing → Ingestion. JSON text messages: the agent sends
  `{"type":"auth",…}` (hub-side `AuthMessage`), and the hub answers `auth_ok`/`auth_error`
  (agent-side `HubMessage`). The hub answers `registry unavailable` when it can't check or
  register the id (a database error, or a registration that panicked); every shipped agent
  retries any `auth_error` after 5 s. The connection carries two deadlines: the auth message
  within 10 s of the upgrade, and after that some message at least every 90 s. Pings count,
  and the hub answers them itself; every shipped agent pings every 30 s.
- *Push frame*: Telemetry Publishing → Ingestion. A binary MessagePack `PushPayload`,
  declared independently in `src/push/mod.rs` and `system-hub/src/push/ingest.rs`. The agent
  encodes it with `rmp_serde::to_vec`, which is positional: structs become arrays with no
  field names, so field *order* is the contract. The hub drops frames that fail to decode,
  and counts and logs them (the first at `warn`, the count when the connection ends). A frame, like any push message, is at most 512 KiB, and spends the connection's
  decode budget before it is decoded: 3 messages, then one per second, which no shipped
  agent exceeds. A decoded snapshot frame whose `timestamp` is above 2^63 − 1 is refused
  whole; otherwise the snapshot rule decides what is kept. An `uptime_display` or a
  `memory_total_display` over 64 bytes or holding a control character, and a
  `memory_total_bytes` above 2^63 − 1, count as not reported, and the stored value is kept.
- *Application frame*: Telemetry Publishing → Ingestion, on the same connection. A positional
  5-element array `[kind, run, seq, interval_secs, applications]`, each application
  `[name, health, version | nil, {gauge: f64}]`, with `kind` = `"applications.v1"`; an
  incompatible later shape gets a new kind, which a v1 hub drops. Declared independently in
  `src/push/application_frame.rs` (with `src/applications/wire.rs`) and
  `system-hub/src/application_wire.rs`, and tied by the golden bytes
  `testdata/application-frame-v1.msgpack`, which both crates' tests read. A frame and a
  snapshot never decode as each other (5 elements against 21).
- *Mail report*: Telemetry Publishing → Ingestion, through untrusted relays (RFC 0017). The
  sealed bytes are `"SAMR" | 1 | id_len | system id | nonce (24) | ciphertext`, the header
  before the nonce being the AEAD's associated data, under the system's mail key
  (HKDF-SHA256 of the hub's master key, salt `system-agent mail key v1`, info the system id).
  The plaintext is a `mail-report.v1` MessagePack **map** (`to_vec_named`), so a later agent may
  add keys a v1 hub ignores; the reason, an alert's metric and severity are strings (an
  unknown reason reads `Other`). Declared independently in `src/mail/report.rs` and
  `system-hub/src/mail_intake/report.rs`, tied by `testdata/mail-report-v1.msgpack` and
  `testdata/mail-report-v1.sealed`. It travels base64 between armour lines in a `text/plain`
  body.
- *Poll responses*: the agent's `/api/system`, `/api/alerts` and `/api/applications` JSON →
  Ingestion (`collector/`), none of them through a redirect. `/api/system` is read up to
  4 MiB, and its snapshot and memory capacity go through the same rules as a push frame's.
  `/api/alerts` is read field by
  field from untyped `serde_json::Value`. `/api/applications` is `{round: {run, seq} | null,
  interval_secs, scraped_at, applications: [{name, health, version, gauges}]}`; the hub
  ignores `scraped_at`, and the golden body `testdata/applications-v1.json` ties the two
  declarations (`src/applications/wire.rs`, `system-hub/src/application_wire.rs`).
  Each active alert's `id` is its incident id, the same on every tick of the incident, and
  `fired_at` is the tick the incident became active. The hub treats the id as an opaque
  string.

### Glossary (ubiquitous language)

| Term | Meaning | In code |
|---|---|---|
| **agent** | the `system-agent` process running on one monitored host | `system-agent` crate |
| **hub** | the `system-hub` process aggregating many agents | `system-hub` crate |
| **system** | a monitored host *as the hub knows it*: registry entry, config, status | `SystemInfo`, `systems` table |
| **memory capacity** (hub) | a system's memory total as its agent last reported it, bytes and display always together: the agent's resource capacity for memory, so a container's limit in a container. A display that is empty, over 64 bytes or holding a control character is no report, and neither are bytes above 2^63 − 1, which SQLite can't hold: the stored capacity is then kept | `MemoryCapacity`, `MAX_DISPLAY_BYTES` |
| **system id** | the identifier an agent presents in the push handshake; the hub uses it as the system's primary key. It is one URL path segment: non-empty, at most 255 bytes, and not `.` or `..`. The hub refuses any other id at the push handshake; rows stored before the rule may still hold one (see Open architectural questions) | `SystemId` (hub) |
| **default system name** | the name the hub gives a newly pushed system until its first snapshot supplies a hostname: the longest prefix of the system id that is at most 8 bytes and ends on a character boundary | `SystemId::default_name`, `SystemId::is_default_name` |
| **system status** | the hub's view of whether a system is reachable: online / offline / unknown. A push system is marked offline when its current connection ends (a connection ends at the latest 90 s after its last message, or 30 s after an oversize one), or by the disconnection sweep; it reads unknown when registered and after every hub start, until a snapshot is stored. A polled system is marked offline by each poll that fails. A system is marked online by each stored snapshot, in the snapshot's transaction | `SystemStatus`, `StatusUpdate` (one write of a system's status, its last seen and its error) |
| **last seen** | what the hub shows in a system's `last_seen` column, which several writers share until RFC 0010's contact time: the uptime its last stored push frame reported (at most 64 bytes, with no control character, else the previous value is kept), blank once its push connection ends, or the time of its last poll, whether that poll succeeded or marked it offline. So a polled system's last seen moves on while it is down | `LastSeen`, `UptimeDisplay` (a stored snapshot's); `update_system_status` (the offline markings') |
| **snapshot** | one point-in-time reading of the agent's execution environment: its CPU, memory, swap, disks, network, processes, etc. | `SystemSnapshot` (agent); `ReportedSnapshot` and `Snapshot` (hub, before and after the snapshot rule), `SnapshotTime` (its time: the frame's `timestamp` on push, the hub's clock on poll); `PushPayload` (wire) |
| **collected snapshot** | a snapshot the sampler read at least 1 s after its priming reading, not yet published, with its `collected_at` (unix seconds on the agent's clock) and `read_at` (the monotonic clock) | `CollectedSnapshot` |
| **priming reading** | a sampler's first reading, which only gives the next one an interval to measure CPU over. It is never published, since sysinfo's first CPU figure is a since-boot average. A reading becomes a collected snapshot only once priming is done | `Sampler::prime`, `Priming` |
| **published snapshot** | the latest collected snapshot, published by the background collector with its snapshot seq. It is the only snapshot any route, stream or push frame reads | `PublishedSnapshot`, `SnapshotReceiver` |
| **snapshot seq** | the collector's count of published snapshots, from 0 at startup. It only grows, across sampler rebuilds, and never reads the wall clock. It stays inside the agent | `SnapshotSeq` |
| **stale snapshot** | a published snapshot read more than 30 s ago on the monotonic clock. The agent never serves one. Not *application freshness*, which is the hub's rule for scrape rounds | `SnapshotFreshness`, `STALENESS_BOUND` |
| **raw readings** | what one collection reads from sysinfo, the OS and, in a container, the monitored cgroup's files, before the snapshot's rules (sourcing, averages, percentages, the top processes) are applied. The OS description is the one field already resolved, since its `lsb_release` fallback runs only when needed. Not *metric readings* | `RawReadings`, `CgroupReadings` |
| **execution environment** | what the agent runs in, classified once at startup: *bare metal*, a *virtual machine* (with its hypervisor when known), a *container* (with its runtime when known) or *undetermined*. Only an explicit container marker makes a container; a cgroup limit or an overlay root never does | `ExecutionEnvironment`, `classify` |
| **container marker** | an explicit statement that the agent runs in a container: `/.dockerenv`, `/run/.containerenv`, `KUBERNETES_SERVICE_HOST`, or a `container` value (the agent's variable or `/run/systemd/container`) other than `wsl`. The only thing that makes a container | `ContainerMarker` and the marker fields of `EnvironmentEvidence` |
| **environment evidence** | the observations classification weighs: container markers, the hypervisor CPU flag, DMI, Xen and a WSL kernel, each parsed at the edge. An unreadable source is no evidence | `EnvironmentEvidence` |
| **monitored cgroup** | in a container, the cgroup holding the whole workload: the root of the agent's cgroup namespace when that namespace is the container's own (the mount point shows a non-root cgroup, which has `cgroup.type`), else the agent's own cgroup | `MonitoredCgroup`, `cgroup_access` |
| **resource limit** | a cgroup's bound on one resource: *bounded* by an amount, or *unbounded* (`max`, or no limit file because the controller isn't enabled there) | `ResourceLimit` |
| **resource capacity** | how much CPU (possibly fractional) and memory the monitored environment may use: the kernel's amount outside a container; in one, the least of the host's amount and every resource limit on the monitored cgroup and its visible ancestors | `cpu_capacity`, `memory_capacity`, `CpuCount`, `Bytes` |
| **reading group** | readings sourced and carried together so their invariants hold: CPU (usage, capacity), memory (total, used, free, available), swap | `ReadingGroup`, `CpuGroup`, `MemoryGroup`, `SwapGroup` |
| **reading source** | where a reading group came from on a tick: the *cgroup*, the *kernel* (the host-wide view), or *unavailable* (carried past 30 s). A carried group's source is still the cgroup. A group's *lineage* is kernel until its first cgroup reading and cgroup ever after | `Origin`, `ReadingSource`, `SourcingHistory` |
| **steal time** | the share of CPU time the hypervisor withheld from a virtual machine between two readings, from the host kernel's `/proc/stat`: a real 0 on bare metal, unmeasured when the kernel has no steal column | `StealCounters`, `steal_share` |
| **workload** | the processes inside the container that the agent monitors. When PID 1 of the agent's PID namespace is outside the monitored cgroup, the namespace is shared and the process list isn't only the workload | `ProcessView` |
| **load scope** | whose load average the kernel reports to the agent: the host's in a container, which shares the host's run queue, else the execution environment's own | `LoadScope` |
| **metric point** | one timestamped value of one metric | `MetricPoint` (both crates); a snapshot's are `Snapshot::metric_points` |
| **snapshot rule** | how the hub turns a snapshot into what it keeps, the same for push and poll: `cpu`, `memory`, `swap`, `load1` and `load5`, and one point per disk. A value that isn't reported or isn't finite, a disk with an invalid mount point, and the disks past the first 1024 valid ones are each left out on their own, and counted. What was left out is logged at `warn` at most once an hour per system | `snapshot_rule`, `Snapshot`, `Scalar`, `LeftOut`, `left_out_log`, `MAX_DISKS` |
| **mount point** | where a disk is mounted, as the hub keeps it: 1 to 256 bytes, with no control character. A disk's series is `disk:<mount point>` | `MountPoint`, `MAX_MOUNT_POINT_BYTES` |
| **live metrics** | a system's latest snapshot as the snapshot rule kept it, held in memory for the dashboard. Written with the snapshot's store, and removed when the system is deleted or its current push connection ends | `LiveMetrics`, `AppState::live_metrics` |
| **history** | the agent's in-memory ring buffer of recent metric points (3600 per series) | `MetricsHistory` |
| **alert rule** | a metric, an operator, a threshold, a duration and a cooldown | `AlertRule` |
| **agent run** | one lifetime of the agent process, identified by a random UUID minted at startup | `AgentRun` |
| **tick** | one evaluation of every enabled alert rule against one snapshot, every 2 s | `AlertManager::evaluate` |
| **metric readings** | the values of one snapshot that alert rules read on a tick (CPU, memory, swap, disks, load, core count). CPU, memory and swap are each a *reading*: *measured* on the tick, *carried* (the tick couldn't measure it and the last measured value stands: the rule skips the tick, keeping its breach and incident, notifying nothing), or *unavailable* (the rule clears, ending its incident or pending breach, and stays idle until a measured tick). A reading follows its group's reading source: a group read on the tick (from the cgroup or the kernel) is measured, a carried group is carried, an unavailable one is unavailable, and a measured percentage that isn't a number is unavailable too. Percentages are over the resource capacity | `Readings`, `Reading`, `ReadingOrigins` |
| **percent** (agent) | a share of a resource, held to 0–100 inclusive. NaN isn't one. The hub keeps a snapshot's percentages as the snapshot rule passes them: any finite value (see Open architectural questions) | `Percent` |
| **load average** | runnable tasks averaged over 1, 5 or 15 minutes. Not a percent and not bounded above | `environment::usage::LoadAverage` (domain), `models::LoadAverage` (the snapshot's DTO) |
| **alert incident** | one uninterrupted breach of one alert rule. It becomes active on the first tick the breach has lasted the rule's duration, and ends on the first tick the rule no longer breaches, when the rule set is replaced, or when the agent restarts | `Incident`, held by `Breach::Active` |
| **incident id** | identifies one alert incident: `<agent run>-<sequence>`, where the sequence counts the run's incidents from 1 and never rewinds. Every active alert of the incident carries it | `IncidentId` |
| **active alert** | the report, on one tick, of an alert incident that is active | `ActiveAlert` |
| **notification** | an active alert's announcement (the agent's `🚨 ALERT` log line), made when the incident is active and the rule's cooldown since its last notification has elapsed. The cooldown spans incidents | `Report::Notify`, the return value of `evaluate` |
| **severity** | how serious an alert is (info / warning / critical). The hub stores it as a free `String`, defaulting to `"warning"` | `AlertSeverity` (agent), `AlertRecord.severity` (hub) |
| **alert record** | the hub's stored copy of one alert incident, keyed by `<system id>_<incident id>` and inserted with `INSERT OR IGNORE`, so it keeps the values first seen | `AlertRecord`, `alerts` table |
| **retention** | how long the hub keeps metric points per system per metric: a `metric_retention` row, else 24 h. A negative (hand-set) row counts as none, and so does a snapshot metric's row that isn't an integer | `metric_retention` table, `snapshot_retention`, `APPLICATION_RETENTION_SECS` |
| **poll** | the hub fetching a system's snapshot, alerts and scrape round over HTTP, every 30 s, following no redirect | `collector/` |
| **push** | an agent streaming snapshots and scrape rounds to the hub over WebSocket + MessagePack | `push/` (both crates) |
| **push system** | a system registered by a push handshake (`url` = `push://`), never polled. **Polled system**: one that is neither a push nor a mail system | `SystemSource`, `PUSH_URL`, `polled_systems` |
| **mail system** | a system registered by its first accepted mail report (`url` = `mail://`): never polled, and a push handshake for its id is refused (`transport mismatch`). Marked offline when **overdue**: more than 3 mail intervals and 15 minutes since its newest report | `SystemSource::Mail`, `MAIL_URL`, `mail_status`, `MailPresence` |
| **mail report** | what one mail message carries: its report id (agent run + a sequence from 1), its creation time, the mail interval, its reason (scheduled, incident), its samples (1 to 60, a push snapshot frame's values without processes, the system info on the newest only), every alert incident active since the previous report, and the latest scrape round | `MailReport` (both crates), `MailedSnapshot`, `MailedAlert`, `ReportId`, `ReportReason` |
| **mail interval** / **sample interval** | the time between scheduled reports (60 s to a day, default 300 s), and between the samples a report keeps (10 s up to the mail interval, at most 60 a report, default 60 s) | `MailInterval`, `SampleInterval` |
| **incident report** | a report mailed at once because an alert incident became active, at most one a minute | `ReportReason::Incident`, `IncidentPace` |
| **sealed report** / **armour** | a report encrypted and authenticated under its system's mail key, with its header as associated data; its base64 between `-----BEGIN/END SYSTEM-AGENT REPORT-----` lines | `seal`, `open`, `armour`, `dearmour` |
| **mail key** / **mail master key** | the hub's 32-byte secret (`HUB_MAIL_KEY`), and one system's key derived from it and its system id, which the agent holds (`MAIL_KEY`) | `MailMasterKey`, `MailKey` |
| **mail receipt** / **receipt window** | the record of one accepted report, keyed by system and report id, which refuses its duplicates and replays; kept 7 days (each system's newest current receipt always), and retired, not deleted, when the system is deleted | `MailReceipt`, `mail_receipts`, `RECEIPT_WINDOW_SECS` |
| **backfill report** | a report older than its system's newest accepted one: it adds history and alert records, never status, last seen, live metrics or the shown round | `Recency::Backfill` |
| **push system id** (agent) | the id the agent presents in the handshake, resolved once per process, before the runtime | `AgentId`, `resolve_push_id`, `IdSource` |
| **connection number** | the in-memory number identifying one accepted push connection; never persisted | `ConnectionNumber`, `ConnectionLease` |
| **current connection** | of a push system's open connections, the one whose snapshot frame claimed last, or, before any, the first one accepted while none was current. Only its end marks the system offline | `PushPresence`, `Ending` |
| **disconnected push system** | one with no live current connection (its task is gone, or none was accepted). The disconnection sweep marks it offline once the hub has run 120 s, and also an online one whose current connection never claimed | `Sweep`, `OfflineReason`, `RECONNECT_GRACE` |
| **push token** | the shared secret (`HUB_PUSH_TOKEN`) an agent must present in the push handshake when one is configured. Unset or empty leaves push open; a value that isn't UTF-8 makes the hub refuse to start | `PushToken`, `PushAuth` (hub) |
| **push handshake** | the JSON text exchange that authenticates a push connection. A *rejection* is an auth message the hub refuses by its content (shape, token or system id); a *refusal* is any `auth_error` answer: a rejection, a handshake timeout, or `registry unavailable` (registering a new push id failed or panicked) | `AuthMessage`, `HandshakeRejection`, `Refusal`, `Handshake` (hub), `HubMessage` (agent) |
| **push frame** | one binary, positional MessagePack message on the push connection: a snapshot frame or an application frame | `PushPayload` (both crates) |
| **decode budget** | a token bucket per push connection that every binary message spends before it is decoded, whatever its kind: 3 messages, then one per second, on the monotonic clock. A message past it is dropped undecoded and counted. A round's admission is still its source pace | `DecodeBudget`, `DECODE_BURST`, `DECODE_REFILL` |
| **application frame** | a push frame carrying one scrape round | `ApplicationFrame` (agent), `ApplicationFrameDto` (hub) |
| **application** | a Spring Boot service the agent's operator names in `SPRING_BOOT_APPS`, watched through its Actuator. Not a package, unit or container: those are the host inventory. `ApplicationTarget` is an application *as configured* (name, actuator base URL, credentials), as opposed to what a scrape of it reports | `ApplicationTarget` |
| **application name** | how the operator names an application: 1 to 64 bytes of `[A-Za-z0-9_.-]`, not `.` or `..`. Upper-cased with `-` and `.` as `_`, it is the application's credential key, and two names with the same key can't coexist | `ApplicationName` |
| **actuator base URL** | where an application's Actuator endpoints live: an http(s) URL with no userinfo, query or fragment, whose path ends in `/` | `ActuatorBaseUrl` |
| **actuator credentials** | none, or an HTTP Basic username and password from `SPRING_BOOT_APP_<KEY>_USERNAME` / `_PASSWORD`. A username can't hold `:`, and neither can hold a control character. The password never leaves the type except as the Basic header | `ActuatorCredentials`, `BasicCredentials` |
| **scrape interval** | the time between one scrape round's end and the next one's start, 10 s to 3600 s (default 15 s) | `ScrapeInterval` |
| **applications configuration** | off (no `SPRING_BOOT_APPS`), or up to 16 applications and one scrape interval | `ApplicationsConfig`, `Applications` |
| **scrape** | one read of one application's health, info and Micrometer meters through its Actuator. It reaches the application, or ends in a **scrape failure** (connect, timeout, unauthorized, an HTTP status, or a bad body), which stays in the agent's log | `RawScrape`, `ScrapeFailure` |
| **meter value** | one Micrometer meter as a scrape read it: published with a value, not published (Actuator's 404), or unavailable (any other failure). Not a **metric reading**, which is a host value alert rules read | `MeterValue` |
| **application gauge** | one of the ten curated values the agent derives from an application's meters: a meter's value, or a rate between two scrapes. Missing means "couldn't be derived", never zero | `ApplicationGauge` |
| **application health** | what Actuator's `/actuator/health` reported (up, down, out of service, unknown), or that the application was unreachable | `ApplicationHealth` |
| **application report** | the result of one scrape of one application: unreachable, or its health, version (≤ 64 chars) and gauges | `ApplicationReport`, `ApplicationVersion` |
| **scrape history** | an application's baseline for rates: the counters of its previous reachable scrape, when they were read, and how many requests the agent itself made then. Cleared by an unreachable scrape | `ScrapeHistory` |
| **scrape round** | every application report from one pass over the configured applications, published as one unit. The hub's copy holds at most 16 reports under distinct names | `ScrapeRound` (both crates) |
| **held round** | the scrape round the hub shows for a system, and when it arrived on the hub's clock | `HeldRound`, `SystemApplications::shown` |
| **application freshness** | whether a held round is still current: stale once more than two scrape intervals and 30 s have passed since it arrived | `Freshness`, `freshness` |
| **round digest** | a keyed hash of a scrape round's content as the hub converted it (interval, and each application's name, health, version and gauges). One key per hub process, so no sender can compute one | `RoundDigest`, `RoundDigester` |
| **recent rounds** | the last 8 (round id, round digest) pairs the hub accepted for a system; a round matching one of them exactly is a duplicate. Kept across disconnects | `RecentRounds` |
| **source pace** | a token bucket per source of rounds (one push connection, or the poller for one system): 2 rounds, then one per 8 s, on the monotonic clock. The poller's is read and spent inside the admission step, so overlapping polls share it | `SourcePace` (over `TokenBucket`), `ConnectionState::pace`, `SystemApplications::poll_pace` |
| **admission** | the hub's decision on a round: accept, duplicate, or too soon | `Admission`, `admit`, `RoundStored` |
| **round id** | identifies a scrape round: the **agent run** and a sequence counting the run's rounds from 1, never rewinding. The hub parses the run as a UUID and compares ids only for equality | `RoundId` (both crates), `RoundSequence` |
| **health change** | an application becoming unreachable, or recovering; a change among reported healths (up to down) is not one. The agent logs each: `warn` naming the scrape failure, `info` on recovery | `HealthChange`, `health_change` |
| **application restart** | any counter of an application going down, uptime included, between two scrapes. It resets every counter: the scrape that sees it becomes the new baseline and reports no rates | `ScrapeHistory::advance` |
| **value kind** | how a metric's values are stored (RFC 0010 §3): percent, load, count, monotonic, bytes, rate or millis, each with a value scale, a domain range and an encoding. A series' kind is fixed at its first point | `ValueKind` (`hub-store`) |
| **value scale** | the natural value one stored unit stands for (0.01 % for percent, 1 KiB for bytes); a value is stored as `round(v / scale)`, exact down to the scale | `ValueKind::scale`, `Scaled` |
| **domain range** | the values a kind accepts, from 0 to its top (1,000 % for percent, 2⁶⁰ bytes for bytes); a value outside it, or not finite, is refused | `ValueKind::max_natural`, `OutOfDomain` |
| **tier** | one resolution of a series' history: raw points, 1-minute rollups or 1-hour rollups | `Tier`, `RollupTier` |
| **span** | the stretch of time a tier's chunks never cross: 1 h for raw and minute, 1 day for hour | `SpanStart`, `Tier::span_of` |
| **chunk** | one series' encoded points (raw: at most 240 or 1 KiB) or buckets (a span's worth) in one tier, versioned by its format byte | `RawChunk`, `RollupChunk` |
| **system key** | a system id's bytes as the store keys a series by them (1 to 255 bytes); not a new name for a system, only the system id in Fleet Storage's own type, which knows no other hub term | `SystemKey` (`hub-store`) |
| **generation** | one registration of a system (RFC 0011): a system deleted and registered again gets a new generation, so its old series never mix with the new ones | `Generation` (`hub-store`) |
| **hub time** | the one clock every metric point will be stamped with in the store, in whole seconds, which never runs backwards (RFC 0010 §2); spans and chunk timestamps are in hub time | `SpanStart`, `RawPoint::ts` |
| **series** | one metric of one system's generation, as the store keeps it: `(system key, generation, metric name)`, interned once as a series id in the **series table** | `SeriesKey`, `SeriesId`, `SeriesRecord` |
| **tail** | a series' open state (open chunks and buckets, last time) as persisted, rewritten when the series seals a chunk and by the rotating tail flush | `SeriesState::to_tail` |
| **head** | the store's in-memory state of every active series: its open chunks and buckets, and its chunks sealed since the last commit; changed only by the writer thread, read by queries under a shard lock | `Head`, `HeadEntry` |
| **seal** | closing a series' open chunk (full, or a point or bucket in a new span, or its span closing) into a **sealed chunk** of its span, at the span's next seq, committed with the next group commit | `Sealed`, `SeriesState::seal_span` |
| **closed span** | a span whose tier's grace has passed in hub time (its end plus one bucket and one sweep interval): no point or bucket can land in it any more, so its open chunks are sealed and it can be handed off | `SpanStart::is_closed`, `SeriesState::seal_closed` |
| **block file** | the immutable file holding one closed span of one tier once handed off out of redb, with its index, summary and CRC-32C checks; named by its span and **rewrite number** (0 for the handoff's file, then 1, 2, … for each rewrite) | `encode_block`, `BlockSummary`, `BlockName`, `Rewrite` |
| **span handoff** | moving a closed span out of `chunks` into its block file: the file written durably by the **block writer** thread, then its `blocks` row inserted and the span's chunks deleted in one commit | `store::handoff`, `Handoffs` |
| **handoff backlog** | the spans of a tier waiting on failed handoffs; more than three fail the store stop, naming the tier and the last failure's cause | `Handoffs::choose`, `Backlog`, `FailCause::HandoffBacklog`, `HandoffFailure` |
| **unreadable block file** | a row whose file is missing, or whose header or summary fails its check: the open keeps going, and every query of that span answers `Corrupt` | `StoreError::Corrupt` |
| **reconciliation** | the open's pass over the block files against the `blocks` rows: finishing pending unlinks, deleting only provably redundant row-less files, refusing the open on any other, and marking rows whose file is missing as unreadable | `block::reconcile` |
| **rotating tail flush** | the commit's writing of a slice of the series' tails, sized by elapsed time, so every tail is rewritten once per rotation (15 minutes by default) however many commits run | `Rotation` |
| **commit interval** | the time between group commits (1 s by default, `HUB_COMMIT_INTERVAL`): the durability window, the most a crash can lose | `StoreOptions::commit_interval` |
| **points log** | one entry per group commit holding its points, so the points not yet in a tail survive a crash; replayed at open, truncated once every tail was rewritten | `points_log`, `LoggedPoint` |
| **group commit** | the one redb transaction the writer thread commits per commit interval (1 s by default), holding everything since the previous one: the durability window | `Writer::commit` |
| **catalog** | the hub's own tables in the store's file (registry, alert records, receipts), written only through catalog transactions on the writer thread | `CatalogTable`, `CatalogTxn`, `CatalogRead` |
| **commit class** | when a catalog transaction is committed: durable (at once) or batched (with the next group commit) | `Commit` |
| **commit hook** | what the store calls after each commit, in commit order, with that commit's catalog changes | `Store::on_commit` |
| **rollup** | one closed bucket of a series in a rollup tier: the average, minimum, maximum and count of its points | `Bucket`, `Accumulator` |
| **retention policy** (store) | how long the store keeps each tier, one bounded **tier period** per tier (raw 1 h to 30 d, minute 1 d to 400 d, hour 7 d to 10 y); the global one (default 24 h, 14 d, 30 d) applies to every system without an override. Distinct from the SQLite hub's per-metric **retention** above | `RetentionPolicy`, `TierPeriod`, `Policies` |
| **retention override** | a system's own setting per tier, following the global policy or fixed to a period, never a copy of the global one | `Override`, `TierOverride`, `TierSetting`; a requested change (`PUT`, or `DELETE` back to global) is a `RetentionChange`, and what it did a `RetentionOutcome` |
| **pending shortening** | a change that shortens what a tier enforces, held for a delay before it applies; at most one per tier, replaced by every later change to that tier, while a change at least as long applies at once (RFC 0012 §2) | `PendingShortening`, `TierOverride::change` |
| **tombstone** | the record that a system's generation was deleted: its data is unreadable from that commit on | `Tombstones`, `Owner` |
| **live / dead chunk** | a chunk is dead when its series maps to no system, its generation is tombstoned, or its whole span is older than the retention clock minus its system's tier period; queries never read a dead chunk and retention removes it | `chunk_liveness`, `Liveness`, `Death` |
| **retirement / rewrite** | a block file whose every chunk is dead is retired (deleted whole); one still holding live chunks is rewritten without its dead ones when erased chunks reach a quarter of it, or when its span is past the global period and only longer overrides keep it; at most one rewrite per pass, the earliest span end first | `plan_files`, `FileFate`, `RewriteCause` |
| **holder** | one generation's chunks in one block file and their bytes (a `block_generations` entry), with the system it belongs to, or unmapped when the generation maps to no system | `Holder` |
| **span in flight / damaged file** | a file whose handoff or rewrite is running is in flight: retention and the cap leave it alone until it commits or fails. A file opened unreadable, or one whose rewrite met a chunk failing its CRC, is damaged: retired like any other, never rewritten | `FileCondition` |
| **compaction** | shrinking `hub.redb` to the pages it uses, at start and only when asked (`HUB_STORE_COMPACT=1`) and worth it: over 1 GiB and over a quarter of the file reclaimable | `compaction_due` |
| **storage cap** | the bytes the store may take on disk (`hub.redb` plus block files): bytes, a share of the volume (80% by default) or none; over it, a pass retires the oldest raw files, then the oldest minute files, never hour files | `StorageLimit`, `StorageCap`, `cap_retirements` |
| **floor** | the free space the open requires: the larger of 1 GiB and 2% of the volume (plus twice what a clock rewind brings back); below it the hub refuses to start | `capacity::floor`, `BelowFloor` |

## Trust boundaries & auth

- **Client → Agent**: optional bearer/API-key/query-token auth (`SYSTEM_AGENT_TOKEN`). Health
  check, static files, and the dashboard HTML are always unauthenticated. Auth middleware
  (`auth::require_auth`) compares the presented token to the configured one in constant time via
  `subtle::ConstantTimeEq` (`auth::tokens_match`), closing the timing side-channel a `==`
  comparison would have (see `rfcs/0001-constant-time-token-comparison.md`). Token *length* is
  still observable via timing (the comparison short-circuits on a length mismatch before the
  constant-time byte loop) — only token *content* is protected, which matches standard practice
  for this kind of fixed-secret comparison.
- **Agent → Hub (push)**: agent presents `system_id` + `token` in a JSON handshake frame before
  any data frame is accepted. `main` parses `HUB_LISTEN` (`listen.rs::ListenAddress`, RFC 0015,
  default `0.0.0.0:9091`; an invalid or non-UTF-8 value refuses startup, naming the variable,
  never the value) and `HUB_PUSH_TOKEN` once, before it opens the database
  (`PushAuth::from_env`). It logs the address it bound and suggests its dashboard and push URLs
  at `listen::reachable_at`, as the agent does. Unset or empty leaves push open, and startup logs a
  warning. A value that isn't UTF-8 makes the hub refuse to start: it logs the variable's
  name, never its value, exits non-zero, and has created no file. `push::authenticate` parses the handshake into a
  `SystemId` or a typed `HandshakeRejection`, checking the shape first, then the token
  (in constant time via `subtle::ConstantTimeEq`, as on the agent), then the id: it must be
  one URL path segment, so not empty, not longer than 255 bytes, and not `.` or `..`
  (`SystemIdError`, all answered `invalid system_id`). So an unauthenticated client can't
  probe id validation. Each rejection is logged
  at `warn` with its variant, never the token; an id rejection names the broken rule
  (`InvalidSystemId(DotSegment)`), never the id. Neither `PushToken` nor the `AuthMessage` DTO
  implements `Debug`. The id itself is self-asserted: any token holder can push as any
  system id (see Open architectural questions). See
  `rfcs/0003-hub-push-handshake-hardening.md`.

  The connection is bounded (`rfcs/0006-push-connection-limits.md`):
  - The first message must arrive within 10 s of the upgrade, or the hub answers
    `handshake timeout`. After `auth_ok`, some message must arrive within 90 s of the last
    one, or the connection ends and the system is marked offline. Each deadline wraps one
    receive, never the whole connection.
  - Messages and frames are at most 512 KiB (`PushSocketLimits`, whose `PRODUCTION` value is
    checked at compile time). An oversize message before `auth_ok` drops the connection at
    once. After `auth_ok` the hub logs it, holds the socket unread for 30 s, and then drops
    it, because shipped agents reconnect with no backoff after a drop.
  - The hub never awaits an unbounded send: tungstenite answers pings on its own, the write
    buffer is capped at 64 KiB (a peer that never reads gets its pongs parked and replaced,
    never an ever-growing buffer), and handshake answers are sent under a 5 s timeout.
  - Every exit after registration, a failed `auth_ok` included, runs `end_connection`, which
    marks offline and evicts live metrics only when the ending connection is the system's
    current one (RFC 0016 §2). Log lines name
    a system id in `Debug` form, so a self-asserted id can't forge log lines.

  What a connection's messages cost is bounded too (`rfcs/0007-push-ingestion-cost.md`):
  - Every binary message spends the connection's decode budget before it is decoded: 3
    messages, then one per second. A message past it is dropped undecoded, counted, and
    reported in one `info` line when the connection ends; the connection stays open. It is
    still read off the socket, up to 512 KiB.
  - A decoded snapshot is bounded by the snapshot rule: at most 1024 disks, each mount point
    1 to 256 bytes with no control character, and only finite values. The displays the
    registry keeps (uptime, memory capacity) are at most 64 bytes with no control character.
    A refused snapshot frame, and one that fails to store, is logged at `warn` the first time
    on a connection and at `debug` after that, and counted.
  - A snapshot is stored in one transaction, on the blocking pool. When the system's row is
    gone, nothing is written and the connection ends.
- **Agent → Hub (mail)** (RFC 0017): every relay on the way is untrusted and may read,
  delay, reorder, duplicate, drop or rewrite messages, so the report is sealed end to end and
  the hub trusts no header. Each system has its own key, so a host's key speaks only for its
  own id (unlike the shared push token). `open` parses the id before deriving a key, and every
  refusal is counted by reason and logged at most hourly, never with content, an
  unauthenticated id or a key. Mail reports never write into a pushed or polled system's row,
  and push never into a mail system's. The relay link is STARTTLS by default with verified
  certificates; plain SMTP only to a loopback relay, never with credentials. `MAIL_KEY`,
  `MAIL_RELAY_PASSWORD` and `HUB_MAIL_KEY` are never logged; their types implement no `Debug`.
  The Subject line (the system id and report id) crosses relays in clear.
- **Hub → Agent (poll)**: hub sends the per-system token stored in `db/mod.rs` (as
  configured via `POST/PUT /api/systems`) as the agent's expected auth token, in
  `X-API-Key`. No poll follows a redirect: reqwest strips only standard credential headers
  across hosts, so a followed redirect would carry the token anywhere. The poll client still
  honours environment proxies. The `/api/system` answer is read up to 4 MiB, as it arrives,
  and a larger one marks the system offline; its snapshot goes through the same snapshot
  rule as a pushed one.
- **Client → Hub**: no auth on the hub's own REST/SSE API in the current implementation — the
  hub is assumed to sit behind a trusted network boundary or reverse proxy (see "Deployment
  patterns" in `README.md`). Adding hub-side client auth would be an architectural change
  requiring an RFC.
- **CORS**: both services currently allow any origin/method/header (`CorsLayer::new()...Any`).
  This is a known misconfiguration (see `CLAUDE.md` security section), not an intentional
  trust decision — don't treat it as load-bearing design.
- **SSRF surface**: the hub polls arbitrary URLs supplied via `POST /api/systems`; there is no
  URL validation today. Anyone who can call that endpoint can make the hub issue HTTP requests
  to arbitrary hosts reachable from the hub.
- **Hub API → hub dashboard**: every string the dashboard renders is untrusted. Names, URLs,
  OS fields, disk mount points, alert messages and severities come from agent JSON or push
  frames, and system ids are self-asserted in the push handshake, so any agent, anyone who
  can register or re-point a system, and any push-token holder controls them. The dashboard
  therefore builds its DOM with `createElement` + `textContent` and never assembles HTML from
  data. Ids reach click handlers through closures and URLs through `encodeURIComponent`,
  never through inline `onclick` markup. `encodeURIComponent` keeps an id in one path segment
  only because `SystemId` refuses `.` and `..`, which the URL parser would resolve away. It
  also refuses ids over 255 bytes, a margin far below the request-line limits (65,534 bytes
  in hyper, 8 KB by default in nginx) that an encoded id, up to three times its length, has
  to fit. Rows stored before that rule may still break it. Values used as CSS classes are checked against a
  fixed allowlist: an unknown system status renders as `unknown`, and an unknown severity
  gets no severity class. Numbers are type-checked before formatting or use in styles: a
  card metric or disk percentage that isn't a number renders as `—`, and a core count that
  isn't one is left out. Applications (RFC 0009 §10) follow the same rule: names, versions
  and gauges render as text, a health outside the five known values renders as `unknown`, a
  gauge or chart point that isn't a number renders as `—` or is left out of the chart, and
  an application's name reaches the metric query only through `encodeURIComponent`. The hub
  serves no Content-Security-Policy yet, so this rendering rule is the only XSS control. `system-hub/dashboard-tests/xss.mjs` checks it (see Testing
  architecture).

## Storage

SQLite database `system-hub/system-hub.db`, auto-created on first run, migrations applied at
startup (`db/mod.rs`; metric history in `db/history.rs`).

| Table | Contents |
|---|---|
| `systems` | Registered agents: id, name, URL, token, status, OS info, poll interval |
| `metrics` | Time-series rows: system_id, metric name (`cpu`, `memory`, `swap`, `load1`, `load5`, `disk:{mount}`, and per application `app:{name}:{gauge}` and `app:{name}:up`), value, timestamp |
| `alerts` | Alert history, one record per alert incident, with acknowledge support |
| `mail_receipts` | One row per accepted mail report: system id, run, seq (the primary key), created_at, interval_secs, received_at, and `retired` (set when the system is deleted). No foreign key: deleting a system retires its receipts, which keep refusing replays of its reports |
| `metric_retention` | Per-system, per-metric retention window (default 24h). A snapshot metric is pruned in each transaction that stores a point of it, at most 16 rows per point, oldest first; `app:*` metrics by `retention.rs` every 10 minutes, in batches of at most 5,000 rows, each under its own hold of the mutex |

Snapshot points carry the agent's clock on push (the frame's `timestamp`) and the hub's clock
on poll; `app:*` points carry the hub's clock at arrival. A snapshot's points, their series'
prunes and the system's status and last seen are written in one transaction
(`Database::store_snapshot`), which first checks that the system's row exists and writes
nothing when it doesn't. A scrape round's points are inserted in one transaction.

The `system-agent` has no persistent storage — its metric history is an in-memory ring buffer
(`state.rs`, 3600 points) that resets on restart.

## Testing architecture

Both crates have test coverage colocated with the code under test — `#[cfg(test)] mod tests`
blocks at the bottom of each source file, per standard Rust convention. Each crate also has a
`tests/` directory, used only for tests that must run the real binary (described below).

- **Pure logic** (alert threshold/duration/cooldown evaluation, `ss`/`dpkg`/`rpm`/`pacman`/`apk`
  output parsing, byte/uptime formatting, ISO-8601 formatting, dynamic-SQL clause building) is
  tested directly as unit tests. Where parsing logic originally lived inline in a
  command-shelling `collect()` function or a handler closure, it was extracted into a standalone
  function first specifically to make it unit-testable without invoking real system commands.
- **Push connection tests that assert something didn't happen** first wait for an event that
  comes after it would have: the hub dropping the socket (EOF, read by
  `messages_until_closed`), which follows `end_connection`, or a pong
  (`wait_until_hub_caught_up`), which follows every earlier frame's ingestion. The hub's
  `tests/fail_closed.rs` also checks, on the real binary, that a push row left `online` reads
  `unknown` once the hub serves again (the startup reset); the sweep's start isn't tested
  there, since its 120 s grace can't be injected into the binary. The agent's identity tests
  run `resolve_push_id` over temp directories, never `/etc` or `/proc`.
- **HTTP route handlers** (`routes/api.rs` in both crates) are tested by building the crate's
  `Router` and driving requests through it with `tower::ServiceExt::oneshot` — no real socket is
  bound.
- **SSE and WebSocket endpoints** can't be fully exercised through `oneshot` (a WebSocket upgrade
  needs a real hyper connection's `OnUpgrade` extension, which `oneshot` doesn't provide — such
  requests get a `426 Upgrade Required` instead of `101`). Those get real-server integration
  tests instead: `axum::serve` bound to an ephemeral `127.0.0.1:0` port inside the test, driven
  from a real client (`tokio_tungstenite::connect_async` for WS, a direct request for SSE
  headers).
- **`system-hub`'s push receiver** (`push/`): the handshake is parsed by the pure
  `authenticate` function, which a table-driven unit test covers without a server. The
  real-server tests inject a `PushConfig` (token, deadlines, limits, linger) through
  `router_with_config` instead of setting `HUB_PUSH_TOKEN`, so they run in parallel with no
  `unsafe` env mutation and never wait for a production deadline. `PushConfig` also carries
  the decode budget's refill period, so a test that sends several frames can take a zero
  refill (a bucket that never runs out), while the budget's own tests send faster than a
  short refill allows. After closing the socket they wait for the offline marking, which is
  ordered after every frame's ingestion and before the eviction of the live metrics. The
  deadline and linger tests use rows whose time windows don't overlap, so no fixed duration
  passes them; the pong test shrinks both sockets' kernel buffers so that the write-buffer
  cap is what the returned pongs show. The snapshot rule, the token bucket and the display
  rules are pure and tested by tables, without a server.
- **Mail goldens** (`testdata/`): `mail-report-v1.msgpack`, written by
  `generate_mail_report_v1.py` (hand-rolled, independent of rmp-serde), which the agent's
  encoding must equal and the hub must parse; and `mail-report-v1.sealed`, the agent's sealing
  of it for `web-01` under the key derived from a master key of 32 bytes of 1 (an HKDF vector
  computed with Python's `hmac`), which the hub must open. The agent's SMTP client is tested
  against a scripted relay on an ephemeral port; the hub's Maildir reader over temp
  directories.
- **Contract golden files** (`testdata/` at the repo root): `application-frame-v1.msgpack`,
  written by `generate_application_frame_v1.py`, a MessagePack encoder independent of
  rmp-serde. The agent's test asserts its encoding of a fixed round equals the bytes; the
  hub's test decodes them into its own DTO and compares the whole value. The agent's push
  client is tested against a fake hub (`tokio_tungstenite::accept_async` on an ephemeral
  port).
- **`tests/`** (agent) runs the real `system-agent` binary with a cleared environment. A
  malformed applications configuration must exit 78, name the variable, print no credential,
  log nothing past the parse, and make no push connection. Valid and empty configurations,
  with and without `PUSH_TO`, must get past the parse. The plain-text credentials warning
  must come before startup and name the application but not its URL. An invalid or non-UTF-8
  `SYSTEM_AGENT_LISTEN` is refused the same way, before the runtime's first line; a valid one
  (`127.0.0.1:0`, a fixed port on `127.0.0.2`, `0.0.0.0:0`, `[::]:0`) is bound exactly, logged,
  suggested and answered on. The configuration
  parse itself is a table-driven unit test over a lookup table, so no test mutates the
  environment.
- **`system-hub/tests/`** runs the real binary (`CARGO_BIN_EXE_system-hub`) in a temp dir to
  check startup configuration no router test can reach: a non-UTF-8 `HUB_PUSH_TOKEN` or an
  invalid `HUB_LISTEN` refuses startup before any file is created, an unopenable database is a
  logged exit, not a panic, and `HUB_LISTEN` is bound, logged and suggested as configured.
  Every hub it starts to serve listens on a port the OS chose (`HUB_LISTEN=…:0`, read from the
  startup line) or a probed free one, so none needs 9091 free. The `[::]:0` case needs IPv6,
  which CI's runners have. It also checks that the running hub starts its SSE publisher: a
  system registered after startup reaches a subscriber within one tick.
- Route tests drive each module's `router`. In the hub, `main.rs::app` assembles the
  production router (the module routers, `ServeDir` and CORS), and its tests check that the
  API, applications, SSE and push routes, CORS and the configured dashboard directory all survive the
  assembly. The agent's `main.rs` still assembles its router without such a test.
- **`system-hub`'s SQLite layer** (`db/`) and its agent-polling logic (`collector/`) are
  tested against real (but temporary) SQLite files via the `tempfile` crate — never against the
  real `system-hub.db`. `store_snapshot` is tested for its one transaction (a failing
  statement leaves no point and no status behind), its capped prune and its row check;
  `insert_system_if_absent` for writing nothing for a known id (a recording `BEFORE INSERT`
  trigger, and a `query_only` connection); `register_if_new` panics on a poisoned database,
  so the handshake's `registry unavailable` for a panic has a real-server test. A test that needs
  history in place plants it with the test-only `Database::plant_point`.
  `collector/`'s `poll_system` (including the applications poll and the no-redirect rule) is tested against a small mock
  `system-agent`-shaped `axum::serve` instance covering the success, JSON-parse-error,
  HTTP-error, connection-refused, over-4-MiB and failed-store branches.
- **The SSE publisher** (`routes/sse.rs`) is tested on its watch channel: a new subscriber's
  first summary arrives at once and is the published one, not a fresh build; a summary
  published with no subscriber is kept; three subscribers over two ticks hold two
  allocations, so each tick is serialised once and shared. The summary event's bytes for a
  fixed state are pinned, so the shared serialisation can't change what the dashboard
  reads.
- **The hub dashboard** (`static/index.html`) has no JS unit-test harness. Its rendering rule
  is checked by an XSS smoke test, `system-hub/dashboard-tests/xss.mjs`: plain Node with no
  npm dependencies, driving headless Chromium (`CHROME_BIN`, else the first found on `PATH`,
  else in Playwright's Linux browser folders: every headless shell before any full Chromium,
  and within each `$PLAYWRIGHT_BROWSERS_PATH` before `~/.cache/ms-playwright`, newest build
  first; a folder that can't be read is skipped). It stubs `fetch` with a
  route table and `EventSource` with a hostile hub. Every free-text field of an unknown, an
  offline and an online system, and of every alert record, breaks out of text, quoted
  attributes and raw-text elements (statuses and severities are hostile only where they test
  the allowlists' fallback); ids also break out of inline JS; system urls are hostile
  both as markup and as `javascript:` URLs; the opened system has no hostname yet; and
  numbers arrive as strings or out of range. Each system's applications are hostile the same
  way: names break out of markup and of the metric query string, healths test the
  allowlist, entries that aren't objects are mixed in, and gauges and chart series arrive as
  strings; a stubbed canvas records each chart's axis labels, so its scale can be read. On a
  desktop-sized, hover-capable page it opens a system, opens one of its applications, takes
  a detail refresh, opens another application, takes a live refresh with it open and a
  detail refresh whose round has dropped it, takes a live refresh, and acknowledges an alert
  record. With an application open again, it switches to a system whose round has an
  application of the same name, then declines and accepts each delete. It then opens an
  offline system and takes a changed summary, in which that
  system has no live metrics and changes status, and a system and an alert record arrive.
  Last, it fires pointer, mouse (with each button and with modifiers), focus, key and form
  events at every element (shadow roots included), plus window and document events, and lets
  two minutes of virtual time pass for timers. It checks that no
  script ran, no element outside the dashboard's own tags was added, no handler attribute was
  added or changed against the page's own markup, no `javascript:` URL appeared or was
  opened, and the class allowlists held. Opening a system, opening an application, a detail
  or live refresh, acknowledging an alert record and each delete must make exactly their
  expected requests, and every request must go to a
  known route, whose keys carry ids percent-encoded. It exits 0 on pass, 1 on a failed check or a
  Chromium failure, and 2 when no Chromium is found. `cargo test` does not run it; `CLAUDE.md`
  makes a passing run part of the gate for every change to the hub dashboard or to the test,
  and CI runs it on every pull request, every push to `main` and weekly.
  The agent dashboard has no equivalent.
- `tower` (`features = ["util"]`, for `ServiceExt::oneshot`), `tempfile`, and `futures-util` (hub
  only, for WS test streams) are the test-only additions beyond what production code already
  depended on.
- No coverage-measurement tool (`cargo llvm-cov`/`tarpaulin`) is installed in this environment;
  `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test` and, for the hub
  dashboard, `xss.mjs` are what currently gate a change per `CLAUDE.md`, together with the
  test-contract guard (`.claude/hooks/tdd_guard.py`, RFC 0018): Claude Code hooks that
  refuse an edit to a graded test without a logged release and refuse a commit while a test
  differs from `HEAD` unreleased, a new test is ungraded, or production changed unaudited.
  The guard's own tests (`python3 .claude/hooks/test_tdd_guard.py`) run in CI.
- CI (`.github/workflows/ci.yml`, GitHub Actions) runs that gate on every pull request,
  every push to `main`, and weekly, so a new stable toolchain's lints surface in a run of
  their own. It has one job per crate (`cargo fmt --check`, then clippy, `cargo test` and
  `cargo build --release` with `--locked`, in the crate's own directory, since the two crates
  are not a workspace) and one job for `xss.mjs`, pointed at the runner's preinstalled Chrome
  through `CHROME_BIN`. A newer push cancels a pull request's run, but never a run on `main`.
  The jobs have timeouts (30 and 10 minutes). The workflow has read-only `permissions`, needs
  no secrets, runs fork pull requests under `pull_request` (never `pull_request_target`), and
  pins every action to a commit SHA. The pins are updated by hand; the owner chose not to run
  Dependabot.

## Open architectural questions / known gaps

Tracked here so they aren't rediscovered from scratch; promote any of these to an RFC
(`rfcs/`) before acting on them.

- Mail (RFC 0017): the agent's outbox lives in memory, so a restart loses the reports it held.
  There is no per-system revocation of a mail key: changing `HUB_MAIL_KEY` re-keys every mailed
  system. Mail registrations are unbounded, as push's are (RFC 0008). A report that fails to
  store is deleted with its samples, not retried. A backfill report's scrape round is dropped,
  since `store_round` stamps `app:*` points at hub time. A disabled mail system's reports still
  mark it online, and the overdue sweep skips it, so its status means only "last known".

- In a container, disks list bind mounts whose sizes are the host filesystem's.
- The alert readings are rebuilt from the snapshot DTO's percentages (`collectors/mod.rs::
  alert_readings`), not from the domain's reading groups: a CPU group with no usage reaches
  the rules as a NaN in the DTO that parses as unavailable. Building `Readings` from
  `SourcedReadings` in the sampler would keep the wire shape out of the alert decision.
- Podman (rootful and rootless) is covered only by fixture trees: the manual check ran under
  Docker alone, so where crun's systemd driver writes the limits is unverified on a real host.
- A container with an init under `--cgroupns=host` is measured by the agent's own service
  cgroup, not the container's: not detected (RFC 0014 §4).
- Limits the cgroup namespace hides aren't seen: Kubernetes pod-level limits with no
  container-level limit read as the host's amount.
- The hub doesn't know a system's execution environment until the push frame is versioned,
  so it can't mark the switch of a containerised system's `cpu` and `memory` history from
  host to workload values at the agent upgrade, nor tell a carried value from a measured
  one: past the carry bound the agent's API says `unavailable`, while the frame keeps the
  carried value.

- After a hub restart, a push system whose agent is gone reads `unknown` for 120 s to 150 s,
  then offline. `PUT` can still write the `push://` sentinel and make a polled row a push
  system (RFC 0011). A live current connection whose snapshots never store keeps the status
  its last stored snapshot left (`unknown` after a restart). A connection's task stuck in a
  unit of blocking work keeps its presence entry past its socket's death.
- The poller ignores `poll_interval_secs` and polls every enabled polled system every 30 s, so with
  the default 15 s scrape interval it stores about every other round: push is the path for
  full resolution.
- The hub's poll client honours environment proxies (`HTTP_PROXY` and friends), so polls,
  and their per-system tokens, go through one when it is set.
- A refused polled round creates a system's `live_applications` entry to remember when it
  last warned; a poll racing `DELETE /api/systems/:id` can leave such an entry, holding only
  that time, for a deleted system.
- The hub keeps a snapshot's `cpu`, `memory`, `swap` and disk usages as any finite `f32`: the
  snapshot rule refuses what isn't finite but not what is out of 0–100, so a pushed −5 or
  1e30 reaches history and the SSE summary. A hub-side percent with an out-of-range reason
  among the rule's left-out counts would be a behaviour change of its own.
- A snapshot series that stops receiving points (a disk unmounted, a system gone quiet) is
  never pruned: snapshot pruning runs only when that metric is inserted. Upgrading to RFC
  0007 orphans more series: disks past a host's first 1024, the poller's old `disk:` series
  (a disk without a mount point), and mount points over 256 bytes or holding a control
  character. No cleanup ships: disks past the first 1024 can't be told apart in SQL from
  disks that are merely unmounted.
- The client-chosen push `timestamp` drives pruning: a frame from the far future prunes up to
  16 of the oldest points of each series it carries, so a sender that keeps sending such
  frames empties those series' history, and the far-future points it leaves are never
  pruned by honest frames. Only a push token holder (or anyone, with push open) can send
  one.
- Anyone who can push as a system id (the push token, or anyone when it's unset) can push
  scrape rounds as that system, interleaved with the honest ones; the dashboard shows
  whichever arrived last. Admission rules out locking the honest rounds out, not forging.
- A client that opens many push connections gets a fresh round pace on each: the number of
  sources, and so the rate of stored rounds per system, is bounded only by the rate of push
  handshakes, which nothing bounds.
- `app:*` points are at hub time and snapshot points at agent time, so the two kinds of chart
  are offset by the agent's clock skew.

- No TLS in either binary — deployments are expected to terminate TLS at a reverse proxy
  (documented in `README.md`). Confirm this is still the intended posture before changing it.
- No rate limiting or request body size limits on either API.
- No hub-side client authentication (see Trust boundaries above).
- CORS is unconditionally permissive in both crates.
- No URL validation on hub-side system registration (SSRF surface).
- Domain and wire shapes are the same structs: `models.rs` in both crates derives serde on
  the types the rest of the code treats as the domain model, so there is no anti-corruption
  layer between the API/push/SQLite shapes and domain logic.
- Agents older than RFC 0004 still report `ongoing_rule_<index>` ids, which collide across
  incidents, so the hub drops their later incidents until they are upgraded. Hub databases
  may still hold stale `<system>_ongoing_rule_N` rows.
- Neither binary sends a Content-Security-Policy (or any security headers) with its
  dashboard. The hub dashboard's only XSS control is its rendering rule (see Trust
  boundaries). A `script-src 'self'` policy would first need the inline `<script>` moved to
  a file and the static `onclick` attributes replaced by listeners.
- A hub may still store systems whose ids break the `SystemId` rule (`.`, `..`, or over 255
  bytes), registered before the rule existed. None of them can push again. The disconnection
  sweep marks them offline with `invalid system id`. To find them without printing
  hostile bytes, run
  `SELECT hex(substr(id, 1, 16)), length(CAST(id AS BLOB)) FROM systems WHERE id IN ('.', '..') OR length(CAST(id AS BLOB)) > 255`.
  How to delete one depends on the id:
  - `.` and `..` can't be opened or deleted from the dashboard. Delete them with
    `curl --path-as-is -X DELETE .../api/systems/%2E` (or `%2E%2E`): the router decodes the
    segment exactly once.
  - An id over 255 bytes whose encoded URL still fits a request line opens and deletes from
    the dashboard as usual.
  - An id whose encoded URL doesn't fit (from about 2.7 KB behind nginx's default, or 21 KB
    against hyper directly) can't be addressed by any URL. Delete it in SQLite from `metrics`,
    `alerts`, `metric_retention` and `systems` (see RFC 0005, Rollout).

  So `DELETE /api/systems/:id` takes the stored id as it is, and must not parse it into
  `SystemId` while such rows can exist. Anyone who parses DB rows into `SystemId` must skip
  such a row and log that a row was skipped, never its id, rather than fail `list_systems`
  (whose callers fall back to an empty list).
- When a per-system fetch fails (the system was deleted between two refreshes, or its URL
  can't be served), the dashboard returns early and keeps showing the previously opened
  system's details and charts.
- Alert-record ids are unbounded: `collector/` appends the agent's alert id to the system
  id unchecked, so a long enough agent alert id makes its acknowledge URL hit
  `414 URI Too Long`.
- The bundled SQLite enforces foreign keys (`SQLITE_DEFAULT_FOREIGN_KEYS=1` in
  `libsqlite3-sys`), so `Database::insert_system`'s `INSERT OR REPLACE` would cascade-delete a
  system's metrics, alerts and retention rows if it ever replaced an existing row. Its one
  caller is `POST /api/systems`, which inserts a new UUID. Push registration doesn't use it:
  `insert_system_if_absent` checks with `system_exists`, which reads no column, and inserts
  with `ON CONFLICT(id) DO NOTHING`, so no failed lookup can make it replace a row.
- A row an older hub stored with a negative `poll_interval_secs` (its `PUT` accepted a value
  above 2^63 − 1, which `PollInterval` now refuses with 422) maps back through no read.
  `get_system` then fails for that row, so its `PUT` answers 500 and only `DELETE` removes it,
  and `list_systems` fails for every row: the REST list and the SSE summary fall back to no
  systems, and the poller keeps its last good read. Its push agent's snapshots are still
  stored, and its registry fill is skipped.
- `POST /api/systems` still takes `poll_interval_secs` without `PollInterval`: a value above
  2^63 − 1 answers 500, and one below 5 is clamped to 5, while a `PUT` of 0 is stored.
- The hub's `alerts` table has no retention. With one record per alert incident, a flapping
  rule adds a record for each incident a poll sees.
- An agent alert without an `id` gets a random id on the hub (`collector/`), so it becomes a
  new record on every poll.
- An alert rule with no disk reading (a named mount point missing from the snapshot, or no
  disks reported at all) reads `0.0`. A mount that briefly disappears ends a `Gt` incident,
  which returns as a new record; for `Lt`/`Lte` it opens a phantom incident.
- Replacing the alert rule set ends every incident, including those of rules the new set leaves
  unchanged: they come back one duration later under new ids. Per-rule state is keyed by
  position because rules have no ids.
- Most blocking work is not offloaded. The inventory collectors' `std::process::Command`
  shell-outs (`dpkg-query`, `rpm`, `pacman`, `apk`, `systemctl`, `docker`, `ss`) run on the
  async runtime, per request. The system reading (sysinfo, `/etc/os-release`,
  `lsb_release`) run in `spawn_blocking`; the push system id is resolved before the runtime
  exists. The
  hub's synchronous `rusqlite` calls hold a `std::sync::Mutex`. The push receiver
  (registration, frame ingestion, offline marking), the poller (its registry read, snapshot
  store, registry fill, offline marking and applications store) and the SSE publisher run
  them in `spawn_blocking`; the poller's alert records and every REST handler, `DELETE`
  included, still run them on the async runtime.
- Retention's fallback (no row, or a negative or non-integer one, means 24 h) is the pure
  `snapshot_retention`; the DELETE of older rows stays in SQL, inside `store_snapshot`'s
  transaction.
- The push system id is self-asserted (API1). The hub trusts whatever `system_id` the
  handshake presents, and `GET /api/systems` lists every id without auth. So anyone holding
  the single shared push token, or anyone at all when it is unset, can push as any
  registered system, a polled one included: inject metrics or trigger the rename to
  hostname, and, for a push system, alternate its status by sending snapshots. A connection
  that sends no snapshot, or presents a polled system's id, can no longer mark it offline
  (RFC 0016). Fixing this needs per-system push credentials.
- Push auto-registration is unbounded (API4). Every handshake with an unseen system id
  inserts a permanent, enabled `systems` row, which the sweep reads every 30 s (RFC 0008). What one frame costs to ingest is bounded: at most 512 KiB, one decode per
  budget token, at most 1029 points in one transaction (RFC 0007).
- Every poll, of any system, builds its own reqwest client on a runtime worker, loading the
  CA roots each time. With OpenSSL 3.0.13 in a 4-core container that cost about 0.36 s of
  CPU per poll (RFC 0007's measurement), so such a hub keeps up with about 300 polls per 30 s
  tick. One client per system would remove it.
- The system info strings (`hostname`, `os`, `kernel`, `cpu_model`, and the name taken from
  the hostname) are unbounded, and reach every SSE summary: push writes them once per
  registration, poll on every poll.
- The single SQLite mutex bounds a fleet's aggregate load: every snapshot store, round store,
  REST handler and summary build takes it in turn. RFC 0007's Appendix measured a store's
  hold at about 2 ms for a 5-disk snapshot and 54 ms for a 1024-disk one, on a disk where a
  commit costs about 1 ms: by itself the mutex admits about 1,000 such five-disk systems, or
  37 with 1024 disks, pushing every 2 s.
- SSE subscribers are unbounded. Each tick is serialised once and shared, but each
  subscriber's event still copies the summary's bytes.
- A deleted push system comes back while its agent runs: nothing binds the id to a
  credential, so the agent's reconnect registers it again, with no history.
- A message the decode budget drops is still read off the socket, up to 512 KiB: the budget
  bounds decoding and storing, not reading.
- Connections that never finish their HTTP request are unbounded (API4): `axum::serve`
  gives hyper no timer, so hyper's 30 s header-read timeout is off for every route. The fix
  is a hyper-util server with `TokioTimer`, which touches every route.
- The number of push connections is unbounded (API4): a client may open many and keep each
  alive with a ping every 89 s.
- The agent never reads after the push handshake, so the hub's automatic pongs collect in
  its receive buffer. That's harmless now that the hub's write buffer is capped.
- The Fleet Registry fill on each push frame is still written in the Ingestion adapter
  (`push/ingest.rs::update_registry`): it reads the row, then applies the domain's rules
  (`needs_system_info`, the default-name rule, the memory capacity refresh) in separate
  statements, outside the snapshot's transaction. Marking the system online is the pure
  `StatusUpdate::after_snapshot`, written by `store_snapshot`.
- Rejected push handshakes are logged without the peer's address. The hub isn't served with
  connect info, and behind a reverse proxy it would need a forwarded-header policy.
- The agent doesn't treat a missing handshake answer as a failure. Its handshake check has
  no branch for it, so it enters its push loop, which returns `Ok(())` as soon as the
  socket closes, and `main` reconnects without backoff. After an oversize message the hub's
  30 s linger spaces those reconnects; after other drops nothing does.
- Ingestion reads agent alerts from untyped `serde_json::Value` inside `collector/` and
  discards `insert_alert` errors (`let _ =`). Parsing, domain mapping and storage are
  interleaved in one poll function.
