# RFC 0009: Spring Boot Application Telemetry

- Status: Accepted
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-26
- Affects: both
- Depends on: RFC 0006 (implemented: the 512 KiB message limit bounds the new frame)
- Related:
  - RFC 0007 (Draft). This RFC builds the guarded-store API 0007 describes
    (`store_…(…, decide, on_stored)` on `Database`, with the live state touched only inside
    the guard, lock order database then live state, and a missing row reported as
    `SystemGone`) for scrape rounds, in its own commit 3. 0007 can then extend it to
    snapshots. 0007's `MIN_FRAME_SPACING` applies to snapshot frames only; application frames
    have their own admission rule (§8).
  - RFC 0008 (Draft) bounds the number of push systems, and so the fleet-wide total of what
    §8 bounds per source.
  - RFC 0010 (Draft) replaces SQLite with a dedicated hub store. This RFC ships first, on
    SQLite. Its `app:*` series, pruning task and `store_round` are then taken over by 0010.

## Motivation

The agent reports on the host: CPU, memory, disks, processes, packages, units, containers and
ports. Most services on these hosts are Spring Boot applications, and to the agent a JVM is
one opaque process. Its heap, threads, GC pauses, HTTP traffic, error rate, latency and
connection pool are invisible, and so is whether it considers itself healthy. Spring Boot
already publishes all of this through Actuator and Micrometer. The owner wants the agent to
read it, send it to the hub over both ingestion paths, and show it on the system's page of
the hub dashboard.

If we don't, operators need a second monitoring stack for the thing they actually run, and
"the host is fine" and "the service is down" stay two unrelated screens.

The owner chose, for this RFC:

- **Source:** `/actuator/health`, `/actuator/info` and `/actuator/metrics/{name}`: plain
  JSON, one request per meter. It needs no extra dependency in the application.
  `/actuator/prometheus` was rejected (see Alternatives).
- **Supported versions:** Spring Boot 2.2 to 4.x. The `/actuator/metrics/{name}` document
  (`name`, `baseUnit`, `measurements[{statistic, value}]`, `availableTags`) has the same
  shape in all of them; Boot 3 and 4 serve it as `application/vnd.spring-boot.actuator.v3+json`,
  but every version answers `Accept: application/json` with plain JSON. Boot 4 moved metrics
  into their own starter (`spring-boot-starter-micrometer-metrics`), so a Boot 4 application
  needs that starter or it has no `metrics` endpoint to read. 2.2 is the floor because the
  `outcome` tag on `http.server.requests`, which the error rate filters on, arrived in 2.2.
- **Metrics:** a fixed, curated set (§3), not user-configurable meter names.
- **Actuator auth:** none, or HTTP Basic per application.
- **Ingestion:** push *and* poll.
- **A malformed configuration refuses startup** (§2), rather than starting without
  applications.
- **Hub poll requests stop following redirects**, the two existing ones included (§8).

## Proposed design

### 1. Terms

- An **application** is a Spring Boot service the agent's operator names in its
  configuration, reachable at an **actuator base URL** (e.g. `http://127.0.0.1:8081/actuator`,
  or `…/manage` when `management.endpoints.web.base-path` is changed). On the hub, an
  application is identified by its system id and its application name.
- A **scrape** reads one application's health, info and Micrometer meters once.
- An **application gauge** is one of the ten curated values the agent derives from an
  application's Micrometer meters: a meter's value, or a rate between two scrapes. ("Gauge",
  not "reading": **metric readings** already names the host values alert rules read.)
- An **application report** is the result of one scrape of one application: its name, its
  application health, its version if `/actuator/info` gives one, and its gauges.
- A **scrape round** is every application report from one pass over the configured
  applications. A **round id** identifies it: the **agent run** (the existing glossary term:
  the UUID minted at startup, `AgentRun`) and a sequence that counts the run's rounds from 1
  and never rewinds.
- An **application frame** is a scrape round on the push connection (§7).
- **Application freshness** is the hub's view of whether a system's latest scrape round is
  recent: fresh or stale (§8).

### 2. Agent configuration

| Variable | Meaning | Default |
|---|---|---|
| `SPRING_BOOT_APPS` | comma-separated `name=actuator-base-url` pairs | unset: the feature is off, no task runs |
| `SPRING_BOOT_APP_<KEY>_USERNAME` / `_PASSWORD` | HTTP Basic credentials for one application | unset: no `Authorization` header |
| `SPRING_BOOT_SCRAPE_INTERVAL` | seconds between scrape rounds, 10 to 3600 | 15 |

`<KEY>` is the application name upper-cased with `-` and `.` replaced by `_`
(`order-service` → `SPRING_BOOT_APP_ORDER_SERVICE_PASSWORD`).

The domain types:

```rust
/// One URL path segment and one metric-name segment: 1 to 64 bytes of [A-Za-z0-9_.-],
/// and not `.` or `..`. So it needs no escaping in a URL, an env key or `app:<name>:<gauge>`.
pub struct ApplicationName(String);

/// An absolute http(s) URL with a host, and no userinfo, query or fragment. Its path is
/// normalised to end in `/`, so `health`, `info` and `metrics/<meter>` join *under* it:
/// `http://h/actuator` and `http://h/actuator/` both scrape `http://h/actuator/health`.
pub struct ActuatorBaseUrl(url::Url);

/// Private fields, one constructor (`parse`) that enforces RFC 7617 §2. No `Debug`, no
/// `Display`, no `Serialize`: the only way out is the Basic header.
pub struct BasicCredentials { username: String, password: String }
pub enum ActuatorCredentials { None, Basic(BasicCredentials) }

pub struct ApplicationTarget { name: ApplicationName, base_url: ActuatorBaseUrl, credentials: ActuatorCredentials }

pub struct ScrapeInterval(Duration);   // 10 s ..= 3600 s

pub struct Applications { targets: Vec<ApplicationTarget>, interval: ScrapeInterval }
pub enum ApplicationsConfig { Off, On(Applications) }

impl ApplicationsConfig {
    /// Pure: env access comes in through `lookup`, so every row of the table below is a plain
    /// unit test, with no (edition 2024 `unsafe`) env mutation.
    pub fn parse(lookup: impl Fn(&str) -> Result<String, VarError>) -> Result<Self, ApplicationsConfigError>;
}
```

| Input | Outcome |
|---|---|
| `SPRING_BOOT_APPS` unset, empty or blank | `Off` (the other variables are then not read) |
| a pair without `=`, an empty name or URL | `MalformedPair` |
| a name breaking the `ApplicationName` rule | `InvalidName` |
| a URL that isn't http(s), has no host, or carries userinfo, query or fragment | `InvalidUrl` |
| two names, or two names' `<KEY>`s, that are equal | `DuplicateName` |
| more than 16 applications | `TooMany` |
| only one of `_USERNAME` / `_PASSWORD` set (an empty one counts as unset) | `IncompleteCredentials` |
| a username with `:`, or either value with a control character (HTTP Basic can't carry them, RFC 7617 §2) | `InvalidCredential` |
| any of these variables not UTF-8 | `NotUnicode` |
| interval not a number, or outside 10..=3600 | `InvalidInterval` |

**Startup fails closed.** `main` becomes a synchronous `fn main() -> ExitCode`. It parses
`ApplicationsConfig` **before it builds the Tokio runtime**, so by construction no task, bind
or connection can exist before the parse. Only then does it build the runtime and run
`async fn run(…) -> Result<(), StartupError>` (commit 2 passes it the configuration). A
refused configuration exits with **78** (`EX_CONFIG`), and nothing else uses that code:
every other `StartupError` exits 1. On an error it logs the variable's *name* and the variant, never a value, and `main`
returns a failure code. The same change turns the existing `TcpListener::bind` and
`axum::serve` `.unwrap()`s into `StartupError` variants (tech debt named in `CLAUDE.md`).

The trade-off, as decided by the owner: a typo in `SPRING_BOOT_APPS` also stops host
telemetry and push until it's fixed, and the hub shows the host offline. The alternative,
starting without applications, hides the typo behind an empty Applications section.

A URL with userinfo is refused, not stripped: credentials belong in the password variable,
where they can't leak through a logged URL. Basic credentials over `http://` to a host that
isn't a loopback address log a startup warning naming the application (A02). A Docker bridge
address is a common same-host target and isn't loopback, so refusing would break a legitimate
setup.

### 3. The curated gauges

Each gauge is optional. A missing gauge means the application doesn't publish the meter (for
example no HikariCP pool, or no HTTP request served yet, since Micrometer registers
`http.server.requests` on the first request). It never means zero.

```rust
pub enum ApplicationGauge {
    HeapUsedBytes, HeapMaxBytes, CpuPercent, LiveThreads, GcPausePercent,
    HttpRequestsPerSecond, HttpServerErrorsPerSecond, HttpMeanLatencyMs,
    DbConnectionsActive, UptimeSeconds,
}
```

| Gauge (wire name) | Actuator request | Rule |
|---|---|---|
| `heap_used_bytes` | `jvm.memory.used?tag=area:heap` | `VALUE` |
| `heap_max_bytes` | `jvm.memory.max?tag=area:heap` | `VALUE`; negative (a pool with no max makes the sum −1 or less) → missing |
| `cpu_percent` | `process.cpu.usage` | `VALUE` × 100; negative → missing |
| `live_threads` | `jvm.threads.live` | `VALUE` |
| `gc_pause_percent` | `jvm.gc.pause` | rate of `TOTAL_TIME` seconds per elapsed second, × 100 |
| `http_requests_per_second` | `http.server.requests` | rate of `COUNT` |
| `http_server_errors_per_second` | `http.server.requests?tag=outcome:SERVER_ERROR` | rate of `COUNT`. If `http.server.requests` exists but this query is 404, no 5xx has happened yet, so the count is **0** (the baseline starts at 0 and the first burst shows) |
| `http_mean_latency_ms` | `http.server.requests` | Δ`TOTAL_TIME` / Δ`COUNT` × 1000; no requests in the interval → missing |
| `db_connections_active` | `hikaricp.connections.active` | `VALUE` |
| `uptime_seconds` | `process.uptime` | `VALUE` |

Micrometer timers report `TOTAL_TIME` in seconds through `/actuator/metrics` in every
supported version. A value that isn't finite is missing.

**The agent's own traffic.** Actuator requests are recorded in `http.server.requests` like any
other. A round makes about 12 requests per application, which adds roughly 0.8 req/s at 15 s.
So the request-count delta used for `http_requests_per_second` is reduced by the number of
requests the agent completed against that application in the *previous* round, floored at 0.
Those requests have certainly finished by the next read, and the in-flight error is at most one
round's worth. The mean latency can't be corrected this way and includes Actuator's own
(fast) requests; the README says so. The error count needs no correction, since Actuator
doesn't answer the agent with 5xx unless it is itself failing.

**Rates need a cumulative registry.** An application whose only Micrometer registry is
step-based (Datadog, Elastic, New Relic, OTLP with delta temporality) reports each step's
`COUNT` and `TOTAL_TIME` through `/actuator/metrics`, not running totals. The rate rules then
see constant "resets" and produce nothing useful. The Simple registry (Boot's default when no
other is present) and the Prometheus registry are cumulative. The README states the
requirement, and the manual compatibility check covers one step-registry application to record
what the dashboard shows.

**Rates** are computed on the agent, so the hub stores only gauges and no consumer has to know
about counter resets:

```rust
pub struct CounterSample { value: f64, at: Instant }
/// None on the first sample, on a counter reset (`curr < prev`), or when no time has passed.
pub fn rate_per_second(prev: Option<&CounterSample>, curr: &CounterSample) -> Option<f64>;
```

`ScrapeHistory` keeps the previous samples per application and is folded by the pure
`ScrapeHistory::advance(self, raw: RawScrape, now: Instant) -> (ScrapeHistory, ApplicationReport)`.
An application restart is a reset for **every** counter of that application. It is detected
either by `uptime_seconds` going down, which catches a restart whose new count already
passed the old one, or by any counter going down. A reset or a missing meter starts a new
baseline and produces no rate that round.

**Application health** comes from `/actuator/health`'s top-level `status`:

```rust
pub enum ApplicationHealth { Up, Down, OutOfService, Unknown, Unreachable(ScrapeFailure) }
pub enum ScrapeFailure { Connect, Timeout, Unauthorized, HttpStatus(u16), BadBody }
```

`UP`, `DOWN`, `OUT_OF_SERVICE` and `UNKNOWN` map to their variants; a custom status maps to
`Unknown`. Actuator answers a `DOWN` or `OUT_OF_SERVICE` application with **503 and a normal
body**, so a 503 whose body parses is a health reading, not a failure. If the health request
fails, the report is `Unreachable` and the scrape of that application stops there: no info,
no meters. A failing meter request makes only that gauge missing: 404 is expected, and
anything else is logged at `debug`.

The version is `build.version` from `/actuator/info`. It is present only when the
application builds with `spring-boot-maven-plugin`'s `build-info` goal (or Gradle's
`bootBuildInfo`). It is free text, cut to its first 64 `char`s (never a byte index, which
could split a character and panic).

### 4. The actuator adapter (`src/applications/actuator.rs`)

One shared `reqwest::Client` (a new agent dependency; the hub already uses 0.12) with:

- connect timeout 2 s, request timeout 5 s;
- redirects **off** (`Policy::none()`) and proxies **off** (`ClientBuilder::no_proxy()`).
  reqwest otherwise honours `HTTP_PROXY`/`HTTPS_PROXY` from the environment, with no loopback
  bypass. That would send every scrape, `Authorization` header included, to the proxy, and
  resolve `127.0.0.1` on the proxy. With both off, a request and its credentials reach only
  the configured host;
- `Accept: application/json`, which every supported Boot version answers with plain JSON;
- each body read through `chunk()` into a buffer capped at 64 KiB. A longer body is
  `BadBody` and is never parsed.

The requests of one application run concurrently, and so do the applications of a round
(`futures_util::future::join_all`). All of it is async I/O; nothing blocks the runtime.

Actuator responses are parsed into DTOs (`ActuatorMetricDto { measurements:
Vec<MeasurementDto> }`, `ActuatorHealthDto { status }`, `ActuatorInfoDto { build: Option<…> }`)
and converted to domain values in one place. Unknown fields are ignored.

Logging: a scrape failure is logged at `warn` when an application's health changes to
`Unreachable`, and at `info` when it recovers, never on every round. Lines name the
application and the `ScrapeFailure` variant, never a URL or a header.

### 5. Agent state and the scrape loop

The scrape loop owns a `tokio::sync::watch::Sender<Option<Arc<ScrapeRound>>>`, and
`AppState` holds the matching `Receiver`. `None` means no round has finished yet. The loop
reuses the `AgentRun` that `AppState::new` already mints. When `ApplicationsConfig` is `On`,
`run` spawns `applications::scrape_loop`. The loop runs a round, folds it through
`ScrapeHistory::advance`, numbers it with the next sequence, publishes it, and then **sleeps
the full interval**. So two publishes are always at least one interval (≥ 10 s) apart,
however long a round took. A fixed-rate tick would let a slow round followed by a fast one
publish closer together than that.

The **outer** push loop in `run` (the one that reconnects) owns an
`Option<watch::Receiver<…>>`: `None` when the config is `Off`, so an agent without
applications has no application arm at all. It lends the receiver to each
`run_push_client` call. If the scrape loop ends (a panic in the task), its sender drops.
tokio's `changed()` then fails **on every call**, not once, while `borrow()` still returns the
last round. So:
- the select arm is guarded, and the first `Err` makes the outer loop set its `Option` to
  `None` for good, logging at `error` once for the agent's lifetime, not once per
  connection;
- the post-handshake send happens only when the channel is open (`has_changed()` is `Ok`),
  so a dead loop's last round is never re-sent after a reconnect;
- the push loop goes on sending snapshots and never spins or reconnects because of it. `/api/applications` treats a closed channel as `round: null`. A dead loop
therefore never re-serves its last round as current.

### 6. Agent API: `GET /api/applications`

This is the poll contract, and it's useful locally too. It sits behind the existing
`SYSTEM_AGENT_TOKEN` middleware like every other `/api` route (it isn't on `auth.rs`'s exempt
list).

```json
{
  "round": { "run": "7f0c…", "seq": 42 },
  "interval_secs": 15,
  "scraped_at": 1790000000,
  "applications": [
    {
      "name": "orders",
      "health": "up",
      "version": "2.4.1",
      "gauges": { "heap_used_bytes": 3.1e8, "http_requests_per_second": 12.5 }
    },
    { "name": "billing", "health": "unreachable", "version": null, "gauges": {} }
  ]
}
```

- `health` is one of `up`, `down`, `out_of_service`, `unknown`, `unreachable`. The
  `ScrapeFailure` stays on the agent, in its log.
- `gauges` is a map from gauge wire name to number, and a missing gauge is left out. A map
  (not a positional struct) lets a later agent add a gauge without breaking a hub. On the
  agent it is a `BTreeMap` keyed by wire name, so its encoding is deterministic.
- `round` is `null` (and `applications` `[]`) until the first round finishes, and always when
  the feature is off.
- `scraped_at` is the agent's clock, for local readers. The hub ignores it (§8).

"Application" gets one meaning. The labels that use it for the host inventory are renamed
"Host inventory": the route comment (`src/routes/api.rs:51`), the section header
(`src/routes/api.rs:252`) and the agent dashboard's "Applications & Services" section
(`static/index.html`), a text-only change.

### 7. Push: the application frame

**Why a new frame, not a new field.** `PushPayload` is positional. rmp-serde 1.3 refuses an
array with more elements than the struct has fields (`Error::LengthMismatch`, `decode.rs:569`),
so appending a field would make every old hub drop *every snapshot* from a new agent. An old
hub fails to decode a separate binary message as `PushPayload` and drops it silently, as it
drops any frame that fails to decode, while snapshots go on arriving.

The **application frame** is a binary MessagePack message on the existing, authenticated push
connection, encoded with `rmp_serde::to_vec` (positional):

```text
ApplicationFrame     = [ kind: "applications.v1", run: str, seq: u64, interval_secs: u64,
                         applications: [ApplicationReportDto] ]
ApplicationReportDto = [ name: str, health: str, version: str | nil, gauges: map<str, f64> ]
```

- `kind` is a literal. A later incompatible shape gets a new kind (`applications.v2`), which a
  v1 hub drops like any frame it can't decode.
- A 5-element array can't decode as the 21-field `PushPayload`: its second element is a
  string where `PushPayload` wants a string too, but its third is a `u64` where `PushPayload`
  wants a string, and any length difference is `LengthMismatch`. A `PushPayload` can't decode
  as an `ApplicationFrame` either, for the same two reasons. So the hub tries `PushPayload`
  first, as today, then `ApplicationFrame`, and drops whatever decodes as neither.
- The agent sends a frame when a scrape round is published (a third `tokio::select!` arm on
  the watch receiver in `run_push_client`), and once right after the push handshake if a
  round exists. A round re-sent after a reconnect carries the same round id, and the hub's
  admission rule drops it as a duplicate (§8: the hub's memory of recent round ids survives
  disconnects).
- 16 applications × 10 gauges is about 4 KiB, far below the 512 KiB message limit.

The push handshake and `PushPayload` do not change.

### 8. Hub

**Domain** (`system-hub/src/applications.rs`, Fleet History). Pure values and rules, no DTOs,
no I/O:

```rust
pub struct ApplicationName(String);        // same rule as the agent's, declared independently
pub enum ApplicationHealth { Up, Down, OutOfService, Unknown, Unreachable }
pub struct ApplicationVersion(String);     // ≤ 64 chars, cut on a char boundary
pub enum ApplicationGauge { … }            // the ten wire names; an unknown name doesn't parse
pub struct ApplicationReport { name, health, version: Option<ApplicationVersion>, gauges: BTreeMap<ApplicationGauge, f64> }
pub struct RoundId { run: Uuid, seq: u64 }   // `run` parsed as a UUID at the edge: fixed size
pub struct ScrapeRound { id: RoundId, interval: ScrapeInterval, applications: Vec<ApplicationReport> }

/// What the hub shows for a system: its latest accepted round and when it arrived (hub clock).
pub struct HeldRound { round: ScrapeRound, received_at_ms: u64 }

/// A digest of a round's content *as converted* (names, health, version, gauge values).
pub struct RoundDigest(u64);

/// Computes digests with one SipHash key for the whole hub process. It is built once at
/// startup from one `std::hash::RandomState` and kept in `AppState`, and both conversions
/// (push and poll) use it. `RandomState::new()` is *not* one key per process: each instance
/// differs (std caches keys per thread and increments one per call), so a digester built at
/// a call site would give one round two digests. A fixed-key `DefaultHasher` is refused for
/// the opposite reason: a sender could then compute digests.
pub struct RoundDigester(RandomState);

/// The rounds the hub accepted most recently for a system, at most RECENT_ROUNDS (8), as
/// (id, digest). Kept apart from `HeldRound`: it survives disconnects, and is dropped only
/// with the system.
pub struct RecentRounds(VecDeque<(RoundId, RoundDigest)>);

/// A token bucket per source (one push connection, or the poller for one system): capacity 2,
/// one token every MIN_ROUND_SPACING (8 s), on the **monotonic** clock (`Instant`), so a
/// wall-clock step can neither panic the arithmetic nor starve an honest source. Kept by that
/// source, never shared between sources.
pub struct SourcePace { tokens: u8, last_refill: Instant }
impl SourcePace {
    /// Refill for the time elapsed, then spend one token. The whole rule lives here.
    pub fn take(self, now: Instant) -> Result<SourcePace, TooSoon>;
}

pub enum Admission { Accept { pace: SourcePace }, Duplicate, TooSoon }
/// Duplicate: `recent` holds the same id *and* the same digest (an exact re-send, never
/// "seq not above"). TooSoon: `pace.take(now)` refused. Otherwise Accept, carrying the spent
/// bucket, which `on_stored` only stores.
pub fn admit(recent: &RecentRounds, pace: SourcePace, incoming: &RoundId, digest: RoundDigest, now: Instant) -> Admission;

pub enum Freshness { Fresh, Stale }
/// Stale once `now` is more than 2 × interval + 30 s (the poll tick) past `received_at`.
pub fn freshness(held: &HeldRound, now_ms: u64) -> Freshness;

/// The metric points one round adds to the system's history.
pub fn application_points(round: &ScrapeRound) -> Vec<(String, f32)>;
```

- **No sender-asserted ordering.** Nothing compares clocks or sequence numbers:
  - duplicate detection is an exact match of the round id *and* digest against the last 8
    accepted;
  - pacing uses the hub's monotonic clock; freshness and stored points use hub receive
    time.
  So neither a far-future `scraped_at`, nor a `seq` of `u64::MAX` under an honest agent's
  run, can make an honest round look old.
- **Pre-claiming an id locks nothing out.** Round ids are predictable: the run is public
  through `/api/applications` and the hub's alert ids, and `seq` counts up. A sender who
  pushes fake content under the honest round's next id gets accepted, but the honest round
  that follows under the same id has a different digest, so it is accepted too. Only an exact
  re-send (same id, same content) is a duplicate.
- Wall time (`received_at`) is used only for what is shown, stored and aged: the history
  timestamp and freshness. Pacing uses `Instant`.
- **What a push-token holder can still do** (standing API1, stated plainly): push rounds as any
  system id, interleaved with the honest ones. The dashboard shows whichever round arrived
  last, and the history holds both. Fixing that needs per-system push credentials, which
  RFC 0003 already records as the fix for the same risk on snapshots.
- **Pacing is per source, as a token bucket.** Each push connection keeps its own
  `SourcePace`, and the poller keeps one per system. So a hostile sender can't starve the
  honest source, only spend its own tokens. The bucket's second token absorbs the one early
  round an honest source produces: after a reconnect, the re-sent round lands at an arbitrary
  point in the interval (it may be new to the hub if it was lost on the dead connection, or
  if the hub restarted), and the next publish can follow it within seconds. Otherwise an
  honest agent publishes at least 10 s apart (§5). The long-run rate is still one round per
  8 s per source.
  What bounds a hostile sender's volume per system is then the rate of push handshakes, which
  is unbounded today (the standing "number of push connections is unbounded" open question,
  which applies equally to snapshot frames). A04 states this.
- Points are named `app:<application name>:<gauge wire name>`, next to `disk:<mount>` in the
  `metrics` table, plus `app:<name>:up`: 1 when health is `Up`, else 0, so availability has a
  history. Values are stored as `f32`, as every metric is today; a heap in bytes keeps about
  seven significant digits, which is enough for a chart.

**Wire DTOs and their conversion** live at the Ingestion edge, in
`system-hub/src/push/application_wire.rs`: `ApplicationFrameDto`, `ScrapeRoundDto` (the poll
JSON), `ApplicationReportDto`. The poller reuses them. One `TryFrom` for each round DTO
builds a `ScrapeRound`:

- more than 16 applications, an invalid or repeated name, a `run` that isn't a UUID, or an
  interval outside 10..=3600 → the whole round is refused (`ScrapeRoundError`). Conversion runs
  before pacing, so a connection could send refused rounds at wire speed. The first refusal
  per connection (or per system, for the poller, per hour) is logged at `warn` with the variant
  and the system id in `Debug` form; later ones only at `debug`, and counted;
- an unknown `health` string → `Unknown`, and an unknown gauge name or a non-finite value is
  skipped. There's no cap on the raw map beyond the 512 KiB message (push) or 256 KiB body
  (poll), since only the ten known names survive parsing. So a newer agent's additions always
  degrade instead of failing;
- a version is cut to 64 chars.

**Storing a round is one operation on `Database`** (the guarded-store API 0007 describes,
built here in commit 3):

```rust
pub enum RoundStored { Stored, Duplicate, TooSoon, SystemGone }

impl Database {
    /// Takes the connection mutex once and holds it throughout. Never calls another
    /// `Database` method (the mutex isn't reentrant).
    pub fn store_round(
        &self,
        system_id: &SystemId,
        points: &[(String, f32)],
        received_at: u64,
        decide: impl FnOnce() -> Admission,
        on_stored: impl FnOnce(),
    ) -> Result<RoundStored, rusqlite::Error>;
}
```

Holding the guard, it:
1. checks the system's row with its own `SELECT` on the held connection, and returns
   `SystemGone` if it's missing;
2. calls `decide`. The Ingestion adapter's closure takes the live lock only long enough to
   read `RecentRounds` and its source's `SourcePace`, calls the pure `admit`, and releases it;
3. on `Accept`, inserts the points in **one transaction** (at most 16 × 11 = 176 rows, with no
   per-point retention query), commits, and calls `on_stored`. `on_stored` takes the live lock
   again only to push the id onto `RecentRounds`, update the pace and replace the `HeldRound`.

Lock order is always database, then live state, and the live lock is never held across the
INSERTs. The closures run while the database mutex is held, so a panic in one would poison
it for every `db.rs` method. They are therefore limited to a map read, a pure call and a map
write, with no fallible or indexing code. They acquire the live lock with
`unwrap_or_else(PoisonError::into_inner)`, so a panic elsewhere while the live lock was held
can't cascade into the database mutex. The async `GET /api/systems/:id/applications` handler therefore waits only for a
map read or replace, never for a transaction. Today no code nests the two (`sse.rs:40-52`
takes them one after the other), and none may take the database mutex while holding the live
lock. Because the compare and the store happen under one database guard, two overlapping polls,
or a poll and a push, can't both store one round. On push, `SystemGone` ends the connection,
as in 0007.

**Live state.** `AppState` gains `live_applications: RwLock<HashMap<String,
SystemApplications>>`, where `SystemApplications { shown: Option<HeldRound>, recent:
RecentRounds, poll_pace: SourcePace }`. Each push connection keeps its own `SourcePace` in its
task. What is evicted, and when:
- a push disconnect evicts **nothing**. Freshness already covers a system that went quiet,
  and keeping `recent` means a reconnect's re-sent round is still a duplicate;
- a poll that gets 404 or a `null` round clears `shown`, because the agent was downgraded or
  its applications were unconfigured. It keeps `recent`;
- `DELETE /api/systems/:id` removes the whole entry, **after** the database row is deleted.
  In the reverse order, an in-flight `store_round` could recreate an entry that nothing would
  evict; in this order it finds the row gone and returns `SystemGone`.

A poll that fails otherwise leaves `shown`, and it goes stale on its own.

**Retention for `app:*` series.** This is an interim mechanism until RFC 0010's store
replaces it. Today, pruning happens only inside `insert_metric`, for the metric being
inserted, so a series that stops receiving points (a renamed application, a pool that went
away) would keep its rows forever. Application points therefore use no per-insert pruning.
Instead a hub task runs every 10 minutes on the blocking pool, iterating systems. For each
system it deletes, in batches of at most 5,000 rows:

```sql
DELETE FROM metrics WHERE rowid IN (
  SELECT rowid FROM metrics
  WHERE system_id = ?1 AND metric >= 'app:' AND metric < 'app;' AND timestamp < ?2
    AND metric NOT IN (SELECT metric FROM metric_retention WHERE system_id = ?1)
  LIMIT 5000)
```

It then applies the same statement, one per `metric_retention` row of an `app:*` metric,
with that row's cutoff. The hub clock sets every cutoff. (No code writes `metric_retention`
rows today. The branch exists so a hand-set row isn't cut back to 24 h, and a test covers it
with a row inserted directly.) `DELETE … LIMIT` needs a compile option the bundled SQLite
doesn't have, hence the `rowid IN` sub-select. The index `(system_id, metric, timestamp)`
seeks to the system and scans its `app:*` range, testing `timestamp` on each entry. So one
batch holds the mutex for a scan of at most one system's `app:*` entries: about 190k at the
honest default. Batching bounds each deletion, not that scan. The implementation measures one
pass at 190k and 1.9M entries on this project's container, and records the numbers in the
change summary. Snapshot metrics keep their per-insert pruning, and their stale series are a
pre-existing gap, recorded under Open questions.

Push snapshot points carry the agent's clock, while `app:*` points carry the hub's. On a host
whose clock is skewed, the two kinds of chart are offset by the skew. That is recorded rather
than fixed, because RFC 0010 stores everything at hub time.

**Push ingestion** (`system-hub/src/push/`): `ingest` tries `PushPayload`, then
`ApplicationFrameDto`, and stores a round through `on_blocking_pool`, as snapshots are, so the
order of a connection's frames is kept.

**Poll ingestion** (`collector.rs`). The poller polls every enabled system on a fixed 30 s
tick (it ignores `poll_interval_secs`), so with the default 15 s scrape interval it ingests
about every other round. That's accepted: push is the path for full resolution. After a
successful `/api/system` poll, the poller GETs `/api/applications` with the same client and
token.

| Answer | Outcome |
|---|---|
| 404 | an agent older than this RFC: evict any entry; no error |
| 200, `round: null` | evict any entry |
| 200 with a round that converts | `store_round` in `spawn_blocking` |
| anything else (other status, body over 256 KiB, bad JSON, refused round) | logged at `debug`; the system's status is left alone, because it describes the agent, not its applications |

**Poll redirects off (owner's decision).** The poll client today follows up to 10 redirects,
and reqwest strips only `Authorization`, `Cookie`, `Cookie2`, `Proxy-Authorization` and
`WWW-Authenticate` across hosts (`redirect.rs:239-251`), so the per-system token in
`X-API-Key` follows a redirect anywhere. The poll client is built with
`redirect(Policy::none())`, for `/api/system`, `/api/alerts` and `/api/applications`. A
registered URL that answers with a redirect then stops working. That shows as the system
going offline with `last_error` naming the 3xx, and is fixed by registering the final URL.

### 9. Hub API: `GET /api/systems/:id/applications`

Reads the live state only, with no database call. Ages and freshness are computed on the hub,
so the dashboard needs no clock:

```json
{
  "system_id": "…",
  "received_at": 1790000000,
  "age_secs": 7,
  "freshness": "fresh",
  "applications": [ { "name": "orders", "health": "up", "version": "2.4.1", "gauges": { "heap_used_bytes": 3.1e8 } } ]
}
```

An unknown system, or one without a held round, answers 200 with `received_at`, `age_secs`
and `freshness` all `null`, and `applications: []`, the way `/metrics` answers an unknown
system with no points. History: `GET /api/systems/:id/metrics?metric=app:orders:heap_used_bytes`.

### 10. Hub dashboard

The system detail panel gains an **Applications** section, hidden when `applications` is
empty. It's fetched when a system opens and on each detail refresh. Its header shows
"updated N s ago", and when `freshness` is `stale` the whole section is dimmed and labelled
stale.

Each application is a row with:
- its name, and its health as a badge whose class comes from an allowlist (`up`, `down`,
  `out_of_service`, `unknown`, `unreachable`, anything else → `unknown`);
- its version, and the gauges formatted with the existing `finiteNumber` / `fixed1` helpers:
  heap as MiB used / max, CPU %, threads, req/s, 5xx/s, mean latency ms, active DB
  connections, uptime. A missing or non-numeric gauge renders `—`.

Clicking a row draws two charts for it with the existing `drawChart`, on two new canvases
(`appHeapChart`, `appRequestsChart`): heap used in MiB, and HTTP requests per second.
`drawChart` scales to a fixed `maxVal`, which fits percentages but not these, so each chart's
`maxVal` is the largest finite point × 1.1, and at least 1. Heap points are divided by 2²⁰
before drawing. The series are fetched with `systemPath(id, "/metrics?metric=" +
encodeURIComponent("app:" + name + ":heap_used_bytes"))`. All DOM is built with
`createElement` + `textContent`, per the dashboard rendering rule.

The agent dashboard gets only the label rename in §6.

### 11. Out of scope

- Alert rules on application health or gauges, which need their own RFC: agent-side
  `AlertMetric` variants keyed by application.
- Auto-discovery of applications (for example from listening ports or Docker labels).
- User-configurable meters, `/actuator/prometheus`, Bearer/OAuth2-protected Actuator.
- Showing applications on the fleet summary cards or in the SSE summary, and on the agent
  dashboard.
- Honouring `poll_interval_secs` in the poller.

## Domain impact

- **New agent context: Application Telemetry.**
  - Domain core: `src/applications/config.rs` (`ApplicationName`, `ActuatorBaseUrl`,
    `ApplicationTarget`, `BasicCredentials`, `ApplicationsConfig::parse`), then the later domain
    modules under `src/applications/`: `ApplicationGauge`,
    `ApplicationHealth`, `CounterSample`, `rate_per_second`, `ScrapeHistory`,
    `ApplicationReport`, `ScrapeRound`. It reads no env and no clock.
  - Adapters: `src/applications/actuator.rs` (HTTP to Actuator), `applications::scrape_loop`,
    `routes/api.rs` `GET /api/applications`, and the application frame in `push.rs`.
  - It owns what the operator asked the agent to watch, and what it saw.
- **Telemetry Publishing** gains the application frame.
- **Ingestion** gains `push/application_wire.rs` (the DTOs and conversions), the frame
  decoding, `store_round`, the poll of `/api/applications`, and redirect-free polling.
- **Fleet History** gains `applications.rs` (`ApplicationReport`, `ScrapeRound`, `admit`,
  `freshness`, `application_points`), the `app:*` metric names, `live_applications`, and the
  `app:*` pruning task.
- **Glossary**:
  - added: application, application name, actuator base URL, scrape, application gauge,
    application health, application report, scrape round, round id, application frame,
    application freshness;
  - reused: **agent run**, now also the first half of a round id;
  - changed: **poll** (now fetches snapshots and scrape rounds), **push** (streams snapshots
    and scrape rounds), and **push frame** (a snapshot frame or an application frame).
  - **snapshot** stays host-only: a scrape round is not a snapshot.
- **Published contracts**:
  - new: *Application frame* (Telemetry Publishing → Ingestion).
  - new: *Applications poll response* (`/api/applications` → Ingestion), added to the *Poll
    responses* contract.
  - changed: the hub no longer follows redirects on any poll.
  - unchanged: push handshake, `PushPayload`, the `/api/system` and `/api/alerts` bodies.

| Fleet | Push | Poll |
|---|---|---|
| new agent, old hub | the old hub fails to decode application frames as `PushPayload` and drops them silently, with no log line; snapshots are unaffected | the old hub never asks for `/api/applications` |
| old agent, new hub | no application frames arrive; the system shows no applications | `/api/applications` is 404 → no applications, the system stays online |
| agent without `SPRING_BOOT_APPS`, new hub | no application frames | `round: null` → nothing held |
| a later agent adds a gauge | the v1 hub skips the unknown name | same |
| a later agent changes the frame shape | it must use a new `kind`; a v1 hub drops it silently | a later agent must keep this JSON shape or add a new endpoint |

## Alternatives considered

- **Append a field to `PushPayload`.** Rejected: rmp-serde's `LengthMismatch` makes an old hub
  drop every snapshot from a new agent (see §7).
- **Switch push to named encoding (`to_vec_named`).** It would make the frame
  self-describing, but it's an RFC-level change to the existing contract on its own, and a
  second frame doesn't need it.
- **`/actuator/prometheus`.** One request for everything, but every application needs
  `micrometer-registry-prometheus`, and the agent needs a text-format parser. The exposition
  format also shifted under Boot 3.3+ (Prometheus client 1.x, OpenMetrics negotiation). The
  metrics endpoint's JSON has been stable from Boot 2.2 to 4.x.
- **Store cumulative counters on the hub and derive rates there.** Every reader would then
  have to handle counter resets.
- **Deduplicate on `scraped_at`.** It was this RFC's first draft. It trusts a clock the
  sender asserts under an id it also asserts, and a single far-future value froze a system's
  applications and pruned their history (see Review).
- **A new `applications` table.** Health and version change per scrape, and are live state,
  with health history covered by `app:<name>:up`. A table would mean a migration on databases
  already on disk, for no query we need. An RFC for alerting on applications can revisit
  this.
- **The hub scraping Actuator directly.** It would widen the hub's SSRF surface to every
  application URL, and fail for applications behind NAT, which is why push exists.
- **Refuse Basic over plain `http://` to non-loopback hosts.** Rejected in favour of a warning,
  because a Docker bridge address is a common, legitimate same-host target.
- **Start without applications on a bad config.** Rejected by the owner (§2).
- **Keep redirects for the existing polls, off only for the new one.** Rejected by the owner
  (§8).
- **Do nothing.** Operators keep a second stack for application health.

## Security implications

**OWASP Top 10 (2021)**

- **A01 Broken Access Control:** `GET /api/applications` is under the agent's existing auth
  middleware (not on the exempt list, `src/auth.rs:43-47`), and a test pins that. The hub
  endpoint is unauthenticated like the rest of the hub API (standing risk, unchanged). A
  push-token holder, or anyone when `HUB_PUSH_TOKEN` is unset, can push rounds as any
  system id, interleaved with the honest ones, and the dashboard shows whichever arrived last
  (standing API1, §8). What this RFC does rule out is locking the honest rounds out or
  pruning them. Admission compares no sender-asserted clock or sequence, a pre-claimed id
  doesn't suppress honest content (the digest differs), and pacing is per source.
- **A02 Cryptographic Failures:**
  - Basic credentials come only from env vars. The password type has no
    `Debug`/`Display`/`Serialize`, and URLs with userinfo are refused.
  - Credentials over plain HTTP to a non-loopback host log a warning (see Alternatives).
  - The Actuator client ignores redirects and environment proxies, so credentials reach only
    the configured host.
  - Hub polls no longer carry `X-API-Key` across redirects.
  - The hub's poll client still honours environment proxies. That's standing, and for the hub
    presumably intended; recorded under Open questions.
- **A03 Injection / XSS:** application names are `[A-Za-z0-9_.-]` on both sides, but the
  dashboard treats every field as hostile anyway: the version is free text, health is
  allowlisted, `freshness` is allowlisted, and numbers are type-checked. `xss.mjs` is
  extended. Metric names reach SQL only as bound parameters, and the prune's `'app:'` bounds
  are constants.
- **A04 Insecure Design:** bounded at every step:
  - on the agent: ≤ 16 applications, 10 gauges, 64 KiB per Actuator body;
  - on the hub: 256 KiB per poll body, 512 KiB per push message, ≤ 176 points per round in
    one transaction, and per source a long-run rate of one round per 8 s with a burst of 2.
    Each new connection starts with a fresh burst of 2.
  - Rows per system per day: at the honest default (15 s, say 3 applications) about 5,760 ×
    33 ≈ 190k. One hostile push connection at the 8 s floor with 16 applications adds 10,800
    × 176 ≈ 1.9M, pruned to 24 h by the task above.
  - **Not bounded by this RFC:** the number of sources. A client that opens many push
    connections, or reconnects after each frame, multiplies that figure. It can do the same
    with snapshot frames today. That is the standing open question "the number of push
    connections is unbounded", and this RFC doesn't claim to close it.
  - Live memory per system is bounded: 8 `(RoundId, RoundDigest)` pairs of fixed size, plus
    one held round of at most 16 applications.
  - The fleet-wide total scales with the number of systems. Push auto-registration stays
    unbounded until RFC 0008 ships. This RFC doesn't depend on it, and doesn't claim that
    bound.
- **A05 Security Misconfiguration:** an invalid agent configuration fails closed at startup.
  CORS is unchanged.
- **A06 Vulnerable Components:** `reqwest` 0.12 is added to the agent, with `native-tls` like
  `tokio-tungstenite`. Run `cargo audit` on both lockfiles.
- **A07 Identification & Authentication Failures:** no change to either token. The agent never
  forwards `SYSTEM_AGENT_TOKEN` or `PUSH_TOKEN` to an application.
- **A08 Software & Data Integrity Failures:** frames are decoded strictly, by kind. An unknown
  kind or shape is dropped. Rounds are deduplicated by id, not by a sender's clock.
- **A09 Logging & Monitoring Failures:** scrape failures are logged on state change only, with
  typed variants. Refused hub rounds are logged with the variant and the system id in `Debug`
  form. No line carries a credential, a URL or a header. A startup refusal names the
  variable, never its value.
- **A10 SSRF:** the agent fetches only operator-configured URLs, which no API can change. It
  follows no redirects and uses no proxy, and appends only fixed paths. The hub's new request
  is a fixed path on a URL it already polls, and no poll follows redirects any more, so the
  existing hub SSRF surface (registration of arbitrary URLs) is narrowed, not widened.

**OWASP API Security Top 10 (2023)**

- **API1:** see A01.
- **API2:** unchanged.
- **API3:** response DTOs are explicit. A scrape round carries no URL, credentials or
  `ScrapeFailure` detail, so an application's address and its error messages don't leave the
  agent.
- **API4:** see A04.
- **API5:** no new privileged function.
- **API6:** N/A.
- **API7:** see A10.
- **API8:** see A05.
- **API9:** both new endpoints and all new env vars go into the README tables, and the push
  protocol section documents the second frame.
- **API10:** the agent consuming Actuator: capped bodies, typed DTOs, non-finite values
  dropped. The hub consuming agent frames and JSON: one `TryFrom` per shape with explicit
  limits, and unknown health or gauges degrade instead of failing.

## Testing plan

All test-first, per `CLAUDE.md`. Table-driven where there is more than one case.

Agent, unit:
- `ApplicationsConfig::parse` with a lookup table, one row per outcome in §2's table.
  - Boundaries: 16 and 17 applications; interval 9, 10, 3600, 3601; a 64- and a 65-byte name;
    `.` and `..`; names whose `<KEY>`s collide.
  - Base URLs with and without a trailing slash, and `/manage`, each checked by the URL
    `health` joins to.
  - No error's `Display` contains a password.
- `rate_per_second`: first sample, a steady increase, a counter reset, zero elapsed time.
- The scrape loop, with a stub scraper: a slow round followed by a fast one still publishes
  at least one interval apart, and a loop that ends leaves `/api/applications` at
  `round: null`.
- The push client with a closed application channel, and with none (`Off`): the loop keeps
  sending snapshots, neither spins nor reconnects (loop iterations counted over one tick), and
  logs the closure once across several reconnects. After a reconnect with a closed channel, no
  application frame is sent, although the channel still holds a round.
- `ScrapeHistory::advance`, one row per gauge rule in §3's table:
  - negative max heap, NaN CPU, a missing meter, mean latency with no requests;
  - the SERVER_ERROR 404 read as 0 when requests exist, and missing when they don't;
  - an uptime drop resetting every counter even when the new count exceeds the old one;
  - the agent's own requests from the previous round subtracted from the request delta, and
    floored at 0.
- `ApplicationHealth` from status and body: 200 UP, 503 DOWN with a body, 503 without a
  parseable body, a custom status, 401.
- Version cut: a multibyte character straddling the 64th position (on both crates).

Agent, adapter: a mock Actuator served with `axum::serve` on `127.0.0.1:0`, with fixtures
from the Spring Boot 2.7, 3.x and 4.1 reference docs.
- A full healthy scrape, a 404 meter, a 401 health, a body over 64 KiB, and a timeout.
- A redirect that is not followed: the redirect target counts hits and must see none.
- Basic credentials sent as the header, never in the URL.
- **Proxy bypass.** The test re-runs its own test binary as a child process, filtered to one
  `#[ignore]`d child test, with `HTTP_PROXY` and `http_proxy` pointing at a counting
  listener. Setting env on a child is safe code. The child scrapes the mock Actuator and must
  succeed; the parent asserts the proxy saw no connection.

Agent, router and binary:
- `GET /api/applications`: the shape before the first round and after one, and 401 without
  the token when `SYSTEM_AGENT_TOKEN` is set.
- A new agent `tests/` directory runs the real binary (`CARGO_BIN_EXE_system-agent`) with a
  malformed `SPRING_BOOT_APPS` and `PUSH_TO` pointing at a counting listener. The test checks
  that the binary:
  - exits non-zero within a timeout;
  - logs a line naming the variable, and no password value set alongside;
  - logs no "listening" line;
  - makes no connection to the counting listener.
  Since the parse happens before the runtime exists (§2), no task can race the exit, so these
  checks are deterministic. A busy port 9090 can't pass the test either.

Contracts, across the two independent crates:
- A **golden application frame**, `testdata/application-frame-v1.msgpack` at the repo root.
  The agent test asserts that its encoding of a fixed round equals the file byte for byte
  (the gauges are a `BTreeMap`, so the encoding is deterministic). The hub test decodes the
  file into its `ApplicationFrameDto` and asserts the **whole decoded value** against the
  same fixed round, written out in the test. The fixed round gives same-typed neighbours
  distinct values (`seq` ≠ `interval_secs`, and a non-null version that isn't a health word),
  so a swapped declaration fails. Both crates read the same bytes, so the two declarations are
  tied without a shared crate.
- The hub test also asserts that the golden frame does **not** decode as `PushPayload`, and
  that a snapshot frame does not decode as `ApplicationFrameDto`.
- A **golden poll body**, `testdata/applications-v1.json`, used the same way.

Hub:
- The round conversion:
  - one row per `ScrapeRoundError` variant;
  - one row per degrading rule: unknown health, unknown gauge, non-finite value, long and
    multibyte version, 16 and 17 applications, and a raw map with many unknown keys still
    accepted;
  - a `run` that isn't a UUID, and one of 512 KiB, both refused;
  - two conversions of the same round, on different threads, give the same digest (one
    shared `RoundDigester`);
  - refused rounds on one connection logged at `warn` once, then at `debug`.
- `admit`:
  - an empty `RecentRounds`; an exact repeat; the same run with a higher seq and with a lower
    one (both accepted, since only exact matches are duplicates); a new run;
  - `u64::MAX` under the honest run, followed by the honest next round (accepted);
  - an id pre-claimed with different content, then the honest round under that id (accepted),
    then an exact re-send of it (duplicate);
  - the 9th id pushing the 1st out of `RecentRounds`;
  - `SourcePace::take`: two rounds back to back accepted, a third refused, and refilling
    exactly at, just below and past 8 s (on `Instant`s built from a base, so the test controls
    time); two sources pacing independently;
  - a re-sent round accepted right after a handshake, then the next round 3 s later also
    accepted.
- `freshness`: exactly at, and just past, 2 × interval + 30 s.
- `application_points`: names, and the `up` point for each health.
- `Database::store_round`:
  - `SystemGone` writes nothing and calls neither closure;
  - a failing insert leaves nothing stored and doesn't call `on_stored`;
  - points carry the hub's time, not the agent's;
  - two concurrent stores of one round store it once;
  - a reader of `live_applications` isn't blocked for the length of a transaction (a barrier
    inside the insert, with a reader that must finish while it's held).
- Pruning: an `app:*` series that stopped receiving points is pruned past 24 h, and past its
  `metric_retention` row. A snapshot series is untouched.
- Push, on a real server:
  - an application frame is stored and held, and a snapshot frame on the same connection
    still is too;
  - after a disconnect the entry is still shown and goes stale, and a reconnect that re-sends
    the same round stores nothing (two connections, not one);
  - connection B's disconnect doesn't touch what connection A pushed;
  - a third round within 8 s on one connection is dropped, and a second connection's round in
    that window isn't.
- Poll, on the existing mock agent:
  - 200, 404 (evicts, system stays online), `round: null` (evicts), 500, oversize body, bad
    JSON;
  - a far-future `scraped_at` in the body changes nothing;
  - the same round served on two consecutive ticks is stored once;
  - a redirect on `/api/system` is not followed: the target counts hits and must see none,
    and the system goes offline.
- `GET /api/systems/:id/applications`: unknown system, no round, fresh, and stale.

Dashboard: `xss.mjs` extends its hostile hub.
- Every system gains applications whose name, version, health, freshness and gauge values
  (strings, `NaN`, huge numbers) break out of text, attributes, raw-text elements and inline
  JS. One health and one freshness are outside the allowlist.
- The test opens an application row, and checks that the only new requests are
  `/api/systems/<id>/applications` and `/metrics?metric=`, with the id and metric
  percent-encoded.
- `red-test-adversary` attacks the extension in mutation mode.

Manual compatibility check (not in CI): run the agent against Spring Boot 2.7, 3.5 and 4.x
sample applications on this machine (Java, Maven and Docker are available). Include an idle
application, to see the request-rate floor after the own-traffic correction, and one with
only a step-based registry. Record the gauges each shows in the change summary.

## Impact on `docs/ARCHITECTURE.md` and `README.md`

`docs/ARCHITECTURE.md`:
- **Components**: agent responsibilities (application scraping, `/api/applications`); hub
  responsibilities (application ingestion, `app:*` pruning).
- **Data flow**: the diagram and mode table gain the Agent → Spring Boot application hop and
  the application frame.
- **Domain model**:
  - the new Application Telemetry context; Telemetry Publishing, Ingestion and Fleet History
    rows extended;
  - the *Push frame* contract becomes two frames, and *Poll responses* gains
    `/api/applications` and "no redirects";
  - glossary: the terms added, and **agent run**, **poll**, **push** and **push frame**
    changed, per Domain impact.
- **Trust boundaries**:
  - a new *Agent → Spring Boot application* boundary: operator-configured URLs, Basic
    credentials, no redirects, no proxy, capped bodies;
  - *Hub → Agent (poll)*: no redirects;
  - *Hub API → hub dashboard*: application fields.
- **Storage**: the `app:<name>:<gauge>` and `app:<name>:up` names, and a corrected
  description of pruning: per insert for snapshot metrics, a periodic hub-clock task for
  `app:*`. The current "enforced by periodic pruning" is inaccurate.
- **Testing architecture**: the agent's `tests/` directory, the mock Actuator, the child
  process proxy test, the golden contract files, and the `xss.mjs` extension.
- **Open questions**:
  - snapshot series that stop receiving points are never pruned;
  - the hub poll client honours environment proxies;
  - the poller ignores `poll_interval_secs`.

`README.md`:
- the agent and hub endpoint tables;
- the agent environment variables;
- the **Push protocol** section (the application frame);
- the **Database** table's `metrics` row, and the pruning sentence under it;
- a Spring Boot section with the Actuator exposure an application needs:
  `management.endpoints.web.exposure.include=health,info,metrics`, and for Boot 4 the
  `spring-boot-starter-micrometer-metrics` starter. It also covers the need for a cumulative
  registry for rates, and the note that mean latency includes Actuator's own requests.

## Rollout / migration notes

- No schema migration, and no change to existing frames or response bodies.
- **Behaviour change on upgrade:** a hub whose registered agent URLs answer with a redirect
  stops following it (§8). Check `last_error` after upgrading, and re-register with the final
  URL.
- Any deployment order works (see the mixed-version table). Applications appear on the
  dashboard once both the agent and the hub run this version.
- Implementation lands in five commits, each passing the full gate. Each commit updates
  `docs/ARCHITECTURE.md` (context map and glossary) and the README tables for what it adds,
  as `CLAUDE.md` requires. Commit 5 covers only what's left (the Spring Boot section and the
  final pass):
  1. agent `main` → `ExitCode` / `run()`, and the pure `ApplicationsConfig::parse` with the
     fail-closed binary test;
  2. agent Application Telemetry: domain, actuator adapter, scrape loop, `/api/applications`;
  3. application frame (agent and hub), and the hub domain, `store_round`, live state, pruning
     and `GET /api/systems/:id/applications`;
  4. hub poll of `/api/applications`, and redirect-free polling;
  5. hub dashboard and `xss.mjs`, the agent dashboard label, and the README and ARCHITECTURE
     updates.

## Review

`rfc-adversary`, first pass, on the first draft. Every finding was acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| `scraped_at` high-water mark trusts the sender's clock | CONFIRMED | round ids plus hub-clock spacing, points stored at hub time (§8) |
| live state never goes stale | CONFIRMED | eviction on disconnect, 404 and `null` round; hub-computed freshness shown on the dashboard |
| collides with RFC 0007's pacing and live-state shape | CONFIRMED | adopts 0007's store shape; pacing split by frame kind (header, §8) |
| `app:*` series that stop are never pruned | CONFIRMED | periodic hub-clock prune of `app:*` (§8) |
| stored volume and lock cost unbounded | CONFIRMED | 8 s hub-side spacing, one transaction, volumes stated in A04 |
| Basic credentials go to `HTTP_PROXY` | CONFIRMED | `no_proxy()`, and a child-process test |
| fail-closed startup has no exit path or test | CONFIRMED | `main → ExitCode`, pure `parse(lookup)`, binary test, trade-off recorded |
| 5xx gauge missing when zero; restart missed | PLAUSIBLE | adopted: a 404 reads as 0 when requests exist, and an uptime drop resets |
| poll premise wrong; overlapping polls double-store | CONFIRMED / PLAUSIBLE | text corrected (30 s tick); compare-and-store under one guard |
| hub poll follows redirects with `X-API-Key` | CONFIRMED | owner chose redirects off for every poll |
| 32-entry cap contradicts graceful degradation | CONFIRMED | cap removed; only known names survive parsing |
| testing traps (map order, URL join, char cut, charts) | PLAUSIBLE | adopted: `BTreeMap`, trailing-slash normalisation, char cut, computed `maxVal` |
| glossary conflicts and domain placement | CONFIRMED | "gauge", `ScrapeRound`, pure `admit`/`freshness`, DTOs at the Ingestion edge, all three labels renamed |
| documentation inventory incomplete | CONFIRMED | README and ARCHITECTURE lists completed |

Came closest and survived: frame confusion in either direction, and auth coverage of
`GET /api/applications`.

`rfc-adversary`, second pass, on the amended design:

| Finding | Verdict | Resolution |
|---|---|---|
| eviction on disconnect erases the admission memory: re-stored rounds, and the bound defeated by reconnects | CONFIRMED | no eviction on disconnect; `RecentRounds` kept apart from what is shown, surviving disconnects; a dead scrape loop stops sending (§5, §8) |
| `seq ≤ held` is a sender-asserted high-water mark; the run is public; per-system pacing lets a hostile sender starve the honest one | CONFIRMED | exact-match dedupe over the last 8 ids; pacing per source; A01 states the interleaving that remains |
| `store_round` has no `Database` API, and built from existing methods it deadlocks | CONFIRMED | `Database::store_round(…, decide, on_stored)` holding one guard, with its own `SELECT`; the live lock is never held across INSERTs; built in commit 3 |
| golden-frame test can't catch positional swaps | CONFIRMED | whole-value assertion with distinct same-typed values |
| 8 s spacing drops honest rounds after a slow round | CONFIRMED | the loop sleeps a full interval after each publish; slow-then-fast test |
| Actuator's own requests inflate the rates; step registries break rate rules | PLAUSIBLE | adopted: subtract the previous round's own requests; document the latency skew and the cumulative-registry requirement; both cases in the manual check |
| prune cost and the dead retention branch | PLAUSIBLE / CONFIRMED | exclusion of series with a retention row, `rowid IN … LIMIT` batches, a directly-inserted retention row in the test, the scan cost to be measured; interim until RFC 0010 |
| the binary test can't tell a refused config from a busy port | PLAUSIBLE | adopted: a counting `PUSH_TO` listener, and no "listening" line |

Came closest and survived: poll redirects off (the 3xx shows as offline with `last_error`, as
stated), frame confusion, and the child-process proxy test's feasibility.

`rfc-adversary`, third pass:

| Finding | Verdict | Resolution |
|---|---|---|
| a closed watch channel makes the push loop spin or reconnect forever (dead loop, and probably `Off`) | CONFIRMED | `Option<Receiver>` (`None` when `Off`), a guarded arm cleared on the first `Err`, and a test counting loop iterations |
| round ids are predictable, so pre-claiming the honest next id locks honest rounds out | CONFIRMED | `RecentRounds` keeps (id, keyed digest); only an exact re-send is a duplicate; A01 and §8 restated |
| `RoundId.run` is an unbounded string: ~4 MiB of live memory per system | CONFIRMED | `run` parsed as a UUID at the edge; A04 states that systems stay unbounded until RFC 0008 |
| a reconnect's re-sent round makes the next honest round `TooSoon` | CONFIRMED | per-source token bucket (capacity 2, one token per 8 s); test for re-send then +3 s |
| the binary test could pass with the wrong startup order | PLAUSIBLE | adopted: a synchronous `main` parses before building the runtime |
| one `warn` per refused round, before pacing | PLAUSIBLE | adopted: first per connection at `warn`, then `debug` and a counter |

Residuals recorded from the same pass: `DELETE` evicts after the row is gone, and the closures
under the database guard are kept panic-free.

`rfc-adversary`, fourth pass:

| Finding | Verdict | Resolution |
|---|---|---|
| `RandomState::new()` keys differ per instance, so poll re-fetches of one round get distinct digests and are stored twice | CONFIRMED | one `RoundDigester` built at startup and kept in `AppState`, used by both paths; test rows for cross-thread digests and a round polled twice |
| a real-server test row still states strict spacing, contradicting the bucket; A04 wording | CONFIRMED | test row and A04 rewritten for the bucket (burst 2, fresh burst per connection) |
| a dead loop's last round is re-sent after each handshake, and the error logged per reconnect | CONFIRMED | the outer loop owns the `Option<Receiver>` and drops it on the first `Err`; the post-handshake send requires an open channel; test row |
| the bucket rule has no pure home, and wall-clock arithmetic can panic under the guard | PLAUSIBLE | adopted: `SourcePace::take(self, Instant)`, with `Admission::Accept` carrying the new pace |
| taking the live lock can panic on poison, under the database guard | PLAUSIBLE | adopted: `unwrap_or_else(PoisonError::into_inner)` |

These amendments implement the fixes the pass proposed, and change no rule of the design:
admission, pacing and eviction are what the third pass reviewed. So by `CLAUDE.md`'s rule on
re-runs, there's no fifth pass. Came closest and survived: non-deterministic conversion
reopening double stores (an honest agent never reaches one system id through both push and
poll), and a reconnect-per-frame log flood (no amplification beyond what connections already
log). Came closest and survived: a deadlock between
the database guard and the live lock (no nesting path exists), and cross-source double
stores (every path goes through one guard, and 8 slots cover a poll's lag).
