# RFC 0011: Registry and Alert Catalog, with Sealed Agent Tokens

- Status: Draft
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-26 (rewritten on redb the same day; amended 2026-10-03 for RFC 0017's mail
  systems; see Review)
- Affects: `system-hub` (the `storage/` adapter, routes, collector, push registration),
  `system-hub/Dockerfile`, `docker-compose.yml`, `.env.example`
- Depends on: RFC 0010 (the store on redb: one file, the writer thread, `transact`, the commit
  hook, tombstones, the retention clock, `LiveStatus`). **0010 and 0011 are one implementation
  step**, and 0008, 0010, 0011 and 0012 ship in one release (0010's header). RFC 0013 (the
  import) is Rejected: there is no migration.
- **Carries RFC 0017 (Implemented): mail systems.** A third source, `Source::Mail`, its
  receipts as catalog tables, alert records from mail reports, and the mail system's row in the
  per-source API table (§2, §3, §5, §7). RFC 0012's requirements on this RFC (`Mail` in `Source`,
  receipts retired on delete, `enabled` for mail systems, sources fixed by the transport) are met
  there. RFC 0017's alternative of replacing its derived mail keys with this RFC's sealed
  credentials is not taken (§7).
- With this RFC the registry and alert records leave `db.rs`, and `db.rs` and `rusqlite` are
  gone. RFC 0008's push registry limit is enforced inside this RFC's registration transaction.

## Motivation

After RFC 0010, metric points live in the new store, but the registry and alert records are
still SQLite rows:

1. **Plaintext secrets.** Per-system agent tokens are stored in plaintext (`db.rs:44`).
2. **Dynamic SQL.** `update_system_config` and `get_alerts` assemble SQL with `format!`.
3. **Unbounded alert records.** No retention, one record per incident a poll sees, and
   agent-controlled field sizes.
4. **Sentinels for sources.** `url = "push://"` and `url = "mail://"` are the only things that
   mark a push or a mail system, and `PUT /api/systems/:id` can rewrite either.
5. **Two storage engines** would otherwise remain.

**Decisions carried by this RFC:**

| Question | Decision | By |
|---|---|---|
| What the new store holds | everything, in 0010's one redb file; SQLite removed | owner |
| Migration | none: the registry starts empty (0010's Rollout) | owner |
| Encryption at rest | secrets only: per-system agent tokens sealed with AEAD; metrics unencrypted | owner |
| What bounds the registry | RFC 0008's push registry limit, in this release; polled systems need the admin token (0012); mail systems need a key the operator derives per id (RFC 0017 §3) | owner (mail: author) |
| Losing the key | `HUB_SECRET_KEY_RESET=<key id>` drops every token it can't open; one-shot (it must name the stored key id) | author |
| Key publication and rotation | a generated key is published atomically and checked before the first seal; rotation re-seals every token with a new key in one transaction, at start | author |
| Alert-record caps | 1,000 per system and 50,000 in total; messages cut to 512 bytes | author |
| The poll interval | **honoured**: the poller polls each system on its own interval (default 30 s, 5 s to 86,400 s). Today it is stored and ignored (a fixed 30 s tick) | author |

## Proposed design

### 1. The catalog: tables in 0010's redb file

The catalog is a set of redb tables in `hub.redb` (0010 §5), written only through
`Store::transact` and read through `Store::read_catalog` (0010 §9). Keys and values are bytes to
the store; this RFC's adapter, `storage/`, defines their encoding.

| Table | Key → value |
|---|---|
| `systems` | system key → `SystemRecord` |
| `alerts` | record key (§5) → `AlertRecordEntry` |
| `alerts_seen` | `evictable_at` (u64 BE) ‖ record key → (); `evictable_at` = `last_seen_active` + the record's class bound (§5) |
| `alerts_listed` | `acknowledged` (u8) ‖ `stored_at` (u64 BE, inverted for newest first) ‖ record key → () |
| `alerts_api_id` | first 16 bytes of SHA-256 of the API id (§5) → record key |
| `mail_receipts` | system key ‖ run (16-byte UUID) ‖ u64 BE seq → `MailReceipt` (§7) |
| `mail_receipts_by_time` | `created_at` (u64 BE) ‖ receipt key → () |
| `mail_newest` | system key → the newest current receipt's key, `created_at`, `received_at` and interval (§7) |
| `meta`, keys under `hub/` | `hub/generation`, `hub/key_id`, `hub/push_systems` (0008), `hub/mail_prune_floor` (§7) |

The store owns `tombstones`, `retention` and the unprefixed `meta` keys (0010).

- **A system key is length-prefixed**: one byte of length (a `SystemId` is at most 255 bytes),
  then the id. Every key that starts with a system key is therefore prefix-free: the keys of
  system `a` never include those of `a/b`.
- **Every value** is a leading version byte, then postcard. An unknown version byte refuses to
  open, naming the table and the key's first 16 bytes in hex.
- **Transactions.** `transact(class, f)` runs `f` on 0010's writer thread, inside the redb write
  transaction of the next group commit, and returns **after that commit and after the commit
  hook** (read-your-writes). One writer means `f` always reads the current committed state plus
  everything earlier in the same transaction, so there is no lost update and nothing to
  overlay.
  - `Commit::Durable` (registration, `PUT`, delete, acknowledgement, retention) forces the
    commit at once;
  - `Commit::Batched` (a poll's alert records, the system-info fill and rename) rides the next
    group commit, within `HUB_COMMIT_INTERVAL`.
  - A transaction whose writes equal the current values writes nothing.
- **The in-memory Registry** (§2) is maintained by 0010's commit hook, on the writer thread, in
  commit order. Readers take its `RwLock` for reading. **No handler holds a Registry guard
  across a store call**, and every decision that must be atomic with a write (the source of a
  system, its generation) is read inside `f`. So the hook's write lock can't deadlock with a
  reader, and the Registry never diverges from the catalog.
- **Alert records live in redb, not in memory.** `/api/alerts` reads `alerts_listed` in a redb
  read transaction (MVCC: it never waits for the writer).

**Memory**: only the Registry, at most about 6 KiB per system at every field's limit (name 255 B,
URL 2 KiB, unsealed and sealed token ≤ 2.1 KiB, info 5 × 256 B, map overhead), about 0.7 KiB
typical. RFC 0008 caps push systems at 10,000 (its upper bound), polled systems need the admin
token, and mail systems a key the operator derives for each id (RFC 0017 §3: a report opens only
under its own id's key, so one key registers one system). The two operator-bounded sources have
no cap of their own, so the worst case is about 6 KiB × all systems (RFC 0008 §2): ≤ 60 MB worst
and ≈ 7 MB typical at 10,000 systems of any mix, with the count per source and the estimate in
`/api/storage`. redb's
page cache (0010 §7) serves the alert tables.

### 2. Systems

**The domain model and the persisted record are separate types.** The Registry,
`RwLock<BTreeMap<SystemId, System>>`, is listed by name then id (as today's `ORDER BY name`, with
a stable tie-break). `storage/` converts between `System` and `SystemRecord`, and nothing else
sees `SystemRecord`.

```rust
/// Domain value (Fleet Registry).
pub struct System { id: SystemId, generation: Generation, name: SystemName, source: Source, info: Option<SystemInfoFields>, registered: Registered }
/// In memory only (RFC 0008 §3's sweep grace): every system loaded at open is `BeforeOpen`.
pub enum Registered { BeforeOpen, At(Instant) }
pub enum Source {
    Push,
    /// Registered by its first accepted mail report (RFC 0017). `enabled` silences its overdue
    /// marking (0017 §6); it is never polled.
    Mail { enabled: bool },
    Poll { url: SystemUrl, token: Option<PollToken>, interval: PollInterval, enabled: bool },
}
/// The unsealed token, and the sealed bytes it came from, so re-encoding an unchanged record
/// never re-seals it (a fresh nonce would make every write look new).
pub struct PollToken { plain: AgentToken, sealed: SealedToken }

/// Persistence struct, in `storage/` only.
struct SystemRecord { generation: Generation, name: SystemName, source: SourceRecord, info: Option<SystemInfoFields> }
enum SourceRecord { Push, Mail { enabled: bool }, Poll { url: String, token: Option<SealedToken>, interval: u32, enabled: bool } }

/// An http(s) URL, at most 2 KiB, stored as the operator's text.
pub struct SystemUrl(String);
/// How often the poller polls this system: 5 ..= 86,400 s; default 30 s.
pub struct PollInterval(u32);
/// A poll token: at most 1 KiB, and a valid HTTP header value (visible ASCII, space, tab).
pub struct AgentToken(String);
```

- **Only a poll system has a URL, a token and an interval.** A poll system and a mail system have
  `enabled`: for a poll system it means "polled", for a mail system "marked offline when
  overdue" (RFC 0017 §6); a mail system's reports are stored either way.
- **`SystemUrl`** keeps the operator's text (trailing slashes trimmed, as today) and refuses, with
  400 and the reason:
  - any scheme but `http` and `https` (`push://`, `javascript:`, `file:`), and a URL without a
    host;
  - userinfo, a query or a fragment;
  - whitespace or a control character anywhere;
  - an IPv6 zone id (the `url` crate can't parse one; today's poller fails on it too).
  It doesn't add SSRF validation of the host (A10). Non-canonical hosts (`http://2130706433`) and
  dot segments are polled as the `url` crate resolves them; the API shows the text as given.
- **Request URLs keep a path prefix.** The poller parses the text once per poll and appends path
  segments (`path_segments_mut().pop_if_empty().extend(["api", "system"])`), so
  `http://proxy/agents/web01` polls `http://proxy/agents/web01/api/system`, exactly what today's
  string concatenation polls for every URL `SystemUrl` accepts.
- **`AgentToken`** that isn't a valid header value is refused with 400. Today such a token is
  silently not sent.
- **The poll interval is honoured** (author's decision). The poller keeps one timer per enabled
  poll system, at the system's interval, each started at an offset from a hash of its id, so
  polls spread instead of landing on one tick. A new registration without an interval gets 30 s,
  today's effective cadence. The poller visits only enabled `Poll` systems.
- **The API, per source:**

| Field | Push system | Mail system | Poll system |
|---|---|---|---|
| `url` (response) | `push://` | `mail://` | the `SystemUrl` text |
| `poll_interval_secs` (response) | `null` | `null` | the interval |
| `enabled` (response) | `true` | the flag | the flag |
| `token` | never in a response (removed from the DTO) | never in a response | never in a response |
| `status`, `last_seen`, `last_error` | from 0010's `LiveStatus` | from 0010's `LiveStatus` | from 0010's `LiveStatus` |
| `POST` | can't create one (`push://` refused) | can't create one (`mail://` refused) | created; `token: ""` or absent means none; the interval clamped into 5..=86,400, default 30 |
| `PUT name` | allowed | allowed | allowed |
| `PUT enabled` | `true` accepted as a no-op; `false` is 400 `not a polled system` | allowed | allowed |
| `PUT url`, `poll_interval_secs`, `token` | **the values a `GET` shows are accepted as no-ops** (`url: "push://"`, `poll_interval_secs: null`, `token` absent); any other value is 400 `not a polled system` | the same, with `url: "mail://"`; any other value is 400 `not a polled system` | allowed; `token: ""` clears it, absent leaves it; the interval clamped |
| `PUT` changing the source | 400 | 400 | 400 |

  So a script that `GET`s any system and `PUT`s it back keeps working. A system changes source
  only by deletion and re-registration. A `url` is matched against the two sentinels exactly
  before `SystemUrl` parses it: `push://` and `mail://` are refused by `SystemUrl` as non-http
  schemes anyway, and on a `PUT` they mean "this system's own source" only when they equal it. `RegisterSystemPayload` and `UpdateSystemPayload` get a
  hand-written `Debug` that prints `token: <redacted>`.
- **Field limits at the edge:** `name` ≤ 255 bytes and `token` ≤ 1 KiB (400 above). Agent-supplied
  info fields (hostname, OS, kernel, CPU model, memory display) have **control characters
  stripped** and are cut to 256 bytes on a character boundary, so their JSON form is at most
  about 1.5 KiB in total and fits 0012's 8 KiB `PUT` cap.
- **The info rule.** System info is filled while `hostname` or `os` is missing (today's rule),
  from push frames, polls and a mail report's newest sample alike (0017's fill), and written
only if the values differ. A default name is
  replaced by the hostname, **cut to 255 bytes** (the name limit), only if they differ. So a short
  hostname equal to its default name writes nothing per frame.
- `POST /api/systems` never takes an id from the caller: it mints a UUID, as today.

**Generations and registration.** A new registration is one `Durable` transaction that reads
`systems/<key>` and `meta/hub/generation`, and writes the record and the incremented counter.
A push registration also reads and increments `meta/hub/push_systems` and asks RFC 0008's
limit, in the same transaction. **Every registration is source-checked on committed state**:
a present record answers by its source, so a push handshake gets RFC 0008's
`Reserved(ReservedSource)` for a poll or mail record (RFC 0012 §3), and a mail report gets
RFC 0017's `TransportMismatch` for a push or poll record. A push handshake decides a held id by
an MVCC read and a new one inside its transaction (RFC 0008 §3); the mail report decides inside
its scan's transaction (§7). A stale read can only follow a delete and is fenced by the
generation, so neither path can write into the other's system. A mail registration is part of the report's own
transaction (§7) and doesn't touch the push counter. At open, the generation counter is raised to one above every
generation in `systems` and `tombstones`, as defence in depth.

### 3. Deletion, generations and tombstones

- **`delete_system` is one `Durable` transaction** that:
  1. reads `systems/<key>` (404 if absent) and its generation;
  2. deletes every `alerts` record under the system's key prefix, with its `alerts_seen`,
     `alerts_listed` and `alerts_api_id` entries;
  3. removes the system's retention override and pending shortening (`set_retention(…,
     Remove)`, store-owned, 0010 §9), so a re-registered id can't inherit them;
  4. decrements `meta/hub/push_systems` for a push system (RFC 0008);
  5. for a mail system, removes its `mail_newest` entry and **keeps its receipts**, which are
     retired by their generation (§7), so a replay of a report accepted before the delete is
     still a duplicate (RFC 0017 §6);
  6. deletes `systems/<key>`, and calls `tombstone(key, generation)` (0010 §9).
  Everything a poll committed before it is deleted with the rest; a poll committing after it
  finds no system at its generation and is refused. At that commit the store refuses the
  generation's appends (`SystemGone`) on the writer thread, and the hook removes the system from
  the Registry, and **the same hook** removes the deleted generation's `live_status`,
  `live_metrics` and `live_applications` entries, by generation, one lock at a time (0010 §9);
  nothing removes entries by id after the transaction, since a re-registration may already
  have committed a new generation's (RFC 0008 §3).
- **Immediately:** appends of the generation are refused, queries return nothing.
- **In-flight polls and late writers.** Every alert record and every `LiveStatus` carries its
  system's generation; a poll's alert transaction, a late info fill or rename, and a status
  write for another generation are refused or ignored. A deleted system can't come back through
  "delete offline".
- **Re-registration** of the same id gets a new generation, and its history starts empty.
  - **An online push system re-registers by itself**: its connection ends on `SystemGone`, the
    agent reconnects within seconds, and the handshake registers the id again. The README and
    the dashboard's delete confirmation say so: to remove a push system for good, stop its
    agent first.
  - **A mail system re-registers with its agent's next new report** (RFC 0017 §6), within one
    mail interval; stop its agent's mail first to remove it for good.
- **Physical removal** (0010 §5): the retention pass purges a deleted generation's chunks,
  tails and log entries within one pass (10 minutes), resuming from its cursor after a restart.
  The README's "Remove system + all data" becomes "removes the system; its data is unreadable
  at once and removed from the store within about 10 minutes (freed pages may keep old bytes
  until reused, or until the file is compacted)".
- A tombstone is dropped once the purge has finished for its generation **and** it is at least
  one raw-retention period old.
- Who may delete is 0012's decision: the admin token.

### 4. Sealed tokens and the key

`SealedToken = XChaCha20-Poly1305(key, random 192-bit nonce, token, aad)`, using the RustCrypto
`chacha20poly1305` crate, with

```text
aad = u8 len(system id) ‖ system id ‖ u64 BE generation ‖ u16 BE len(url text) ‖ url text
```

- The associated data binds a sealed token to its system, its generation and its URL. A token
  copied onto another record or generation, or kept while someone who can write the file points
  the system's URL at their own host, fails to open instead of being sent there.
- **A `PUT` of a system's URL re-seals its token inside `f`**: it opens the sealed token of the
  record `f` reads (with the old URL in the associated data) and seals it again with the new URL.
  It never reads the Registry, so two `PUT`s in one group commit (a token, then the URL) compose:
  the second sees the first's record. A record whose token, id, generation and URL are unchanged
  keeps its sealed bytes.
- **The key** is 32 bytes.
  - `HUB_SECRET_KEY_FILE` **set**: that file is the key. Missing, the hub refuses to start,
    naming the variable; it never generates a key at a configured path.
  - **Unset**: the hub uses `HUB_DATA_DIR/secret.key`, generating it on first start (after 0010
    has opened the store and holds redb's lock, so two hubs can't both generate one) and logging a
    `warn` that the key sits beside the data it protects. It is published (temporary file with
    mode 0600, `fsync`, rename, directory `fsync`) and then **checked like any key file** before
    the first seal. A filesystem that doesn't keep modes (some drvfs or 9p mounts) fails the
    check at once, with an error saying so.
  - **Checks:** 32 bytes; owned by the hub's euid or by root; not world-readable; group-readable
    only if its group is one of the hub's groups (`getgroups`: Kubernetes secret volumes with
    `fsGroup` and `defaultMode: 0440`). A symlink is followed and its target checked (Kubernetes
    mounts secrets through `..data/`). Anything else refuses startup, naming the file.
  - **Key id.** `meta/hub/key_id` holds the first 8 bytes, in hex, of HMAC-SHA256(key,
    `"system-hub key id"`), written with the first seal.
- **A key that doesn't open the tokens.** At start, if the configured key's id differs from the
  stored one, or a token fails to open, the hub refuses to start with an error naming both ways
  out: "if you are rotating keys, set `HUB_SECRET_KEY_FILE_PREVIOUS` to the old key's file"; "if
  the key is lost, set `HUB_SECRET_KEY_RESET=<stored key id>` to drop the tokens it sealed".
- **Key loss** (author's decision). `HUB_SECRET_KEY_RESET` acts only when its value equals the
  stored key id. Then, in one `Durable` transaction at start: every token that doesn't open is
  dropped, `meta/hub/key_id` becomes the current key's id, each affected system id is logged at
  `warn` in `Debug` form, and the count goes into `/api/storage`. A value left set afterwards no
  longer matches, and is logged at `warn` and ignored. The operator re-enters the tokens with
  `PUT` (admin, 0012). History doesn't depend on the key.
- **Rotation** (author's decision), at start, before serving: with `HUB_SECRET_KEY_FILE_PREVIOUS`
  set to the old key and `HUB_SECRET_KEY_FILE` to the new one, each token is opened with
  whichever key works and re-sealed with the new key, and `meta/hub/key_id` is updated, in one
  transaction. A token that opens with neither refuses startup. The old ciphertexts then sit in
  freed pages until redb reuses them, or until `HUB_STORE_COMPACT` compacts the file (0010 §5);
  the README says so. They are useless without the old key.
- **Unsealed-token cache.** Tokens are opened once, at start and on write, into the Registry's
  `PollToken` (the plaintext `AgentToken` has no `Debug`, no `Serialize`). No AEAD work per poll.
- Tokens are never logged or returned by the API.

### 5. Alert records

```rust
/// Persistence struct: a leading version byte, then postcard.
struct AlertRecordEntry {
    generation: Generation,
    record: AlertRecordFields,   // API id, system name, severity, message, value, fired_at, stored_at, acknowledged
    last_seen_active: u64,       // hub time of the latest poll that still reported the incident
}
```

Alert records come from polls and from mail reports (push frames carry no alerts). A mail
report's alerts (RFC 0017 §2: every incident active since the previous report, at most 64,
typed) go through the same rules, inside the report's own transaction (§7), with the hub time
of the intake as `last_seen_active`. Where a rule below says "poll", it means either.

- **Identity: one function.**
  ```rust
  /// The store key of an incident's record. Used by every path that writes or finds a record.
  pub fn record_key(system: &SystemId, incident: &IncidentId) -> RecordKey;
  // = system key (length-prefixed, §1) ‖ first 16 bytes of SHA-256(incident id)
  ```
  The **incident id** is the agent's alert `id` (RFC 0004's glossary term). The **API id** stays
  `<system id>_<incident id>`, as today, so acknowledgement URLs keep their shape. The API id
  can't be split back reliably (a `SystemId` may contain `_`), so acknowledgement finds the
  record through `alerts_api_id` (16 bytes of SHA-256 of the API id → record key), written with
  the record.
- **Bounded at the ingestion edge** (collector):
  - an agent's `/api/alerts` body over 1 MiB is refused, counted;
  - `message` cut to 512 bytes, `severity` to 32 bytes and `fired_at` to 64 bytes, on character
    boundaries;
  - an incident id over 256 bytes skips that alert, counted;
  - an agent alert **without an id** gets a stable one: `anon-` and 16 hex digits of SHA-256 over
    the length-prefixed system id, **every field of its `rule`** (metric, operator, threshold,
    severity, mount point, duration) and `fired_at`. The message is left out, because agents may
    put live values in it. An id-less alert **without `fired_at`** can't be told from the rule's
    next incident, so it is skipped and counted. (This repo's agent always sends an id since RFC
    0004; this rule is for other agents.)
- **One `Batched` transaction per poll**, which:
  - checks `systems/<key>` holds the poll's generation, and refuses otherwise;
  - refreshes `last_seen_active` of **every** reported incident that already has a record,
    whatever its position in the response;
  - inserts **at most 100 new** records; further new ones are counted, and logged once per
    system per hour;
  - chooses eviction victims inside the transaction (below).
- **Insert keeps the first values**, as today's `INSERT OR IGNORE`. A later report of the same
  incident updates only `last_seen_active`, coalesced: written only when at least 1 h has passed
  since the stored value. A repeated acknowledgement writes nothing.
- **Retention (`events`)** evicts a record once `last_seen_active` is older than the `events`
  period (default 90 days, bounds 1 d to 400 d, `events=` in 0010's `HUB_RETENTION`), measured
  on **0010's retention clock**, not hub time, so a forward clock fault can't evict incidents
  that are still firing. It runs in 0010's retention pass, in transactions of at most 1,000
  evictions. An incident still reported is never evicted, however old.
- **Caps:** at most **1,000 records per system** and **50,000 in total** (author's decision).
  When an insert would exceed a cap, the transaction evicts:
  - per system: the system's record with the oldest `last_seen_active` among those not reported
    by this poll;
  - in total: the **first `alerts_seen` entry**, which is keyed by `evictable_at`, the moment
    the record **can't belong to an incident still firing**: `last_seen_active` plus its
    **class bound**, which is **2 hours or twice its system's poll interval, whichever is
    longer**, and for a mail system the **receipt window** (7 days), since a relay outage or a
    lost outbox can delay a still-firing incident's refresh by hours, and 0017 treats that as
    normal. The entry is re-keyed when `last_seen_active` is refreshed and when a `PUT` changes
    the system's interval, in the same transaction. The first entry is the only candidate, so a
    victim is found or refused in one read, and old mail records can't hide an evictable polled
    record behind them. A still-firing incident is refreshed at least once per coalescing
    window (1 h) of polls, so its `last_seen_active` is never older than the window plus one
    interval; twice either bound leaves a margin.

  If no candidate qualifies, the new record is refused, counted, and logged once per system per
  hour. Because the key is `evictable_at`, a search is one read; a transaction that found no
  victim refuses its further new records without reading again.
- `GET /api/alerts?limit=` defaults to 100, and a larger `limit` is **clamped** to 1,000.
  `count_active_alerts` counts `alerts_listed` entries with `acknowledged = 0`, kept as a
  counter in `meta/hub/unacknowledged`, updated in the same transactions.

### 6. The legacy SQLite file

There is no import (owner's decision). While `system-hub.db` exists in the hub's working
directory, the hub logs at `warn` **at every start** that the file is no longer used, holds every
agent token in plaintext, and should be kept only as long as a rollback to the old hub might be
wanted, then deleted or scrubbed. The check is a file-existence test.

`docker-compose.yml` gains an example of mounting the key from outside the data volume (a Compose
secret with `HUB_SECRET_KEY_FILE` pointing at it), and `.env.example` a commented line. Because
file secrets in Podman and Docker Compose are bind mounts that keep the host file's owner and
mode, **`system-hub/Dockerfile` pins the hub user's uid and gid to 10001**, and the example says
to `chown 10001:10001` and `chmod 0400` the key file on the host. A smoke test of the example is
part of the implementation step.

### 7. Mail systems

RFC 0017's `mail_receipts` SQLite table becomes three catalog tables (§1), and its one SQLite
transaction per report becomes one `Batched` transaction per scan, holding each report's steps
in turn (0010 §10's `store_mail_scan`). The pure decisions stay 0017's (`receipt::fresh` on the
system clock, `receipt::recency`, `mail_status`, which 0010 §10 moves onto `received_at`). The
amendment adds two pure rules of its own, in `mail_intake/receipt.rs` and table-tested:
`newest_of_scan(reports) -> per-system choice` (by `(created_at, seq)` within a run, else arrival
order; 0010 §2) and `stale_before(created_at, prune_floor) -> bool`.

```rust
/// Persistence struct: a leading version byte, then postcard.
struct MailReceipt { generation: Generation, created_at: u64, interval_secs: u32, received_at: u64 }
struct MailNewest { receipt: ReceiptKey, created_at: u64, received_at: u64, interval_secs: u32 }
```

- **Current and retired.** A receipt is *current* while its generation is the system's, and
  *retired* once the system is deleted (its generation tombstoned) or registered again. This
  replaces 0017's `retired` column: the delete transaction rewrites no receipt (§3), and a
  receipt can't be current for the wrong registration.
- **Each report's steps**, inside the scan's `f` on the writer thread. **Every refusal is
  decided by reads, before the report's first write** (0010 §6: an `f` never undoes a write):
  1. read `systems/<key>`: a push or poll record → `TransportMismatch` (counted), skip the
     report; a mail record → its generation; absent → to be registered;
  2. read `mail_receipts/<key ‖ run ‖ seq>`: present, **whatever its generation**, is a
     duplicate (counted), skip the report, so a replay of a report accepted before a delete
     neither re-registers the system nor stores its points (0017 §6). Reads see the writes of
     earlier reports in the same scan, so a duplicate inside one scan is caught too;
  3. read `mail_newest/<key>` (written only for the current generation) for `Recency`;
  4. read `meta/hub/mail_prune_floor`: a report with `stale_before(created_at, floor)` is
     **stale** (counted as 0017's `Stale`), skip the report. The floor is the cutoff of the
     latest receipt prune and **never decreases** (each prune writes `max(previous, cutoff)` in
     the same transaction as its deletions), so a pruned receipt's replay can never pass as new,
     even when the retention clock steps back after a corrected forward fault. The one thing
     that lowers it is `HUB_CLOCK_REWIND` (0010 §2), to `min(floor, rewound retention clock −
     window)`: otherwise a floor raised during a long forward fault would refuse every current
     report as stale until real time passed it. The cost, a replay of a receipt pruned during the
     fault and still inside the window, is taken once; `/api/storage` shows the floor;
  then the writes: register a `Mail { enabled: true }` system if absent (a new generation);
  insert the receipt; the alert records (§5). **Then, at the end of `f`, once per system**, for
  the system's choice by `newest_of_scan` among its accepted `Newest` reports (known only once
  every report's refusals are): `mail_newest/<key>` is written for that choice alone, and, **if
  the choice is current** (0010 §2), its newest snapshot's points are staged at hub now
  (`CatalogTxn::append`), so two reports of one system in a scan never stamp the same second.
  Step 3's `Recency` read sees only the previous scan's `mail_newest`, which is what it must
  compare against. A skipped report writes and stages nothing. The status (`Online`,
  `last_contact`) is set **by the commit hook** for the same choice (0010 §10), never by the
  caller. **After the commit**, the caller runs the info fill once per system for the choice,
  and the live metrics and the round (`append_round`) only if the choice is current; so an older
  report later in the scan never replaces what is shown, and a backlog's choice never shows a
  stale round. Every message of the scan is deleted after the
  commit holding the scan, accepted or refused, as 0017 deletes every handled message; a
  fail-stop before it leaves them for the next scan.
- **Pruning** runs from 0010's retention pass through its `Store::on_retention_pass` hook
  (0010 §9), after the pass has committed its retention clock, which loops the hook while it
  answers `More` under the pass's usual bound (so a prune never exceeds one transaction's
  50,000 entries, and at 10,000 mail systems on the default interval, 20,000 new rows per pass,
  it keeps up with margin): receipts whose `created_at` is older than the
  receipt window (7 days) on the **retention clock** are removed, with their
  `mail_receipts_by_time` entries, in its bounded transactions, **except each system's newest
  current receipt** (the one `mail_newest` names), as 0017 keeps it, and raises
  `meta/hub/mail_prune_floor` to the cutoff in the same transaction. A report older than the
  floor is refused (step 4 above), so a pruned receipt's replay is always stale, never new. Retired receipts are pruned by the window alone.
- **The overdue sweep** reads `mail_newest` through `read_catalog` (MVCC: it never waits for
  the writer) on the blocking pool, and calls 0017's `mail_status` on the receipt's
  `received_at` (0010 §10). The mark is a write-free transaction that re-reads `mail_newest`
  inside `f` and reports an overdue mark only while it still names the receipt the sweep read;
  the commit hook applies it to `LiveStatus` in commit order with the scans (0010 §10). So a
  report committed after the read keeps the system online, as 0017's single-statement
  `mark_mail_overdue` guarantees today (a compare-and-set keyed on the receipt, not on
  `last_contact`, which application rounds also move). `MailNewest` therefore carries
  `received_at` too. A mail record with no `mail_newest` entry is overdue, as 0017 §7 says.
- **No sealed credential.** RFC 0017 offered this RFC's sealed tokens as an alternative to its
  derived mail keys. Not taken: a derived key stores nothing per system and is already bound to
  its id, while a sealed mail credential would add a secret per system to the store for no
  property the derivation lacks.

## Domain impact

- **Fleet Registry**:
  - `System` and `Source` (domain), `SystemRecord` (persistence, `storage/` only), `SystemUrl`,
    `PollInterval`, `PollToken`, `AgentToken`, `SealedToken`, `Generation`, tombstones (0010);
  - the Registry in memory, maintained by 0010's commit hook in commit order;
  - the info rule stated; the poll interval honoured;
  - RFC 0008's limit inside the registration transaction; every registration source-checked
    inside it (push, poll, mail);
  - `db.rs`'s registry half is gone.
- **Fleet History**: alert records with a generation, `last_seen_active`, the `record_key`
  function, the alert tables and indexes, `events` retention on the retention clock, the edge
  limits and caps. This closes "the hub's `alerts` table has no retention", "an agent alert
  without an `id` … becomes a new record on every poll", and "alert-record ids are unbounded".
- **Ingestion**: the poller's per-system timers; the mail intake's report transaction on the
  catalog (§7), its pure decisions unchanged (RFC 0017).
- **Fleet Storage** (0010): the catalog tables' place in the one file.
- **Glossary**: added catalog, commit class (durable, batched), commit hook, generation,
  tombstone, source (push / mail / poll), current and retired receipt, system URL, poll interval, poll token, sealed token, secret
  key, key id, key reset, key rotation, record key, API id, last seen active, `events`
  retention; changed **alert record** (keyed by `record_key`; evicted by last-seen-active on the
  retention clock; kept while active; bounded), **system** (a source, not a URL sentinel), and
  **poll** (on the system's own interval).
- **Published contracts**: `SystemInfo` keeps `url` (`push://` for push systems, `mail://` for
  mail systems) and loses the never-serialised `token`; a push or mail system's
  `poll_interval_secs` is `null`. The mail report (`mail-report.v1`) is untouched. `POST`/`PUT` gain
  400s for URLs, field sizes, invalid tokens and poll settings on push systems, and clamp the
  interval. `/api/alerts` clamps `limit`. Alert API ids are unchanged.
- **Mixed-version fleet**: agents unaffected, mail agents included.

## Alternatives considered

- **Our own catalog log** (the previous drafts): a custom log, snapshots, a pending overlay for
  group commit, an install hook in log order. Two passes found lost updates, orphans and lock
  gaps in it. redb's single-writer transactions make all of that unnecessary.
- **Alert records in memory** (the previous drafts). A 50,000-record worst case was ≈ 95 MB of
  RAM, and grew with every cap change. In redb they cost page cache only.
- **Keep SQLite for the registry and alerts.** Two engines, plaintext tokens, dynamic SQL. The
  owner chose to replace everything.
- **The API id as the store key.** It is unbounded in length and can't be split. `record_key`
  bounds the key, and an index finds it from the API id.
- **An inert poll interval** (validated, stored, ignored). A validated value no rule reads.
  Honouring it costs one timer per system and makes the field mean what it says.
- **Rejecting `url` and `enabled` on push `PUT`s outright** (the previous draft). It broke every
  GET-then-PUT script. The values a `GET` shows are now accepted as no-ops.
- **Tombstone by key only.** A re-registered system's history would be invisible, or the deleted
  one would come back. Generations separate them.
- **Evict by `stored_at`.** Acknowledged long-running incidents came back unacknowledged.
- **Global-cap victims chosen by age alone** (the previous draft). A still-firing incident whose
  refresh was coalesced could be evicted and come back unacknowledged.
- **`url::Url` as the stored URL.** It accepts `push://` and `javascript:`, rewrites the text, and
  drops a path prefix on `join`.
- **Whole-store encryption.** Declined by the owner.
- **A reset switch without a key id.** Left set in a manifest, it dropped every token on a later
  misconfigured start.

## Security implications

**OWASP Top 10 (2021)**

- **A01 Broken Access Control:** every registry write needs the admin token (0012). `PUT` can't
  switch a source or set poll settings on a push or mail system. Every registration checks the
  stored source inside its transaction, so no transport can write into another's system. A late poll or a closing connection
  can't write into a deleted or re-registered system.
- **A02 Cryptographic Failures:** XChaCha20-Poly1305 with random 192-bit nonces; fixed-width
  associated data binding the token to its system, generation and URL; key-file checks applied
  to a generated key at once; atomic key publication; a key id; a one-shot reset; rotation at
  start. Old ciphertexts may stay in freed pages until reused or compacted, useless without the
  old key; the README says so. The default key location beside the data is a stated limitation,
  with a Compose example of the better one. The legacy `system-hub.db` warning.
- **A03 Injection:** no SQL. `SystemUrl` refuses `javascript:`, non-http schemes, whitespace and
  control characters; info fields have control characters stripped.
- **A04 Insecure Design:** atomic deletes, registrations, mail reports (receipt, points and
  alert records) and cap evictions in one writer's transactions; receipts retired by generation,
  so a delete can't reopen a replay; the Registry updated in commit order; alert retention on the retention clock;
  global-cap victims never recently seen.
- **A05 Security Misconfiguration:** fail-closed key handling with explicit, one-shot recovery;
  no key generated at a configured path; the key generated only after the store's lock.
- **A06 Vulnerable Components:** `postcard`, `chacha20poly1305`, `hmac` and `sha2` (RustCrypto)
  added; `rusqlite` and its bundled C library removed. Run `cargo audit`.
- **A07 Identification & Authentication Failures:** agent tokens sealed, cached unsealed in
  memory only, redacted in `Debug`, never serialised, and refused if they can't be sent as a
  header.
- **A08 Software & Data Integrity Failures:** version bytes on every value, unknown versions
  refused; AEAD makes a tampered token or URL fail to open; redb's checksums and 2-phase commit
  (0010).
- **A09 Logging & Monitoring Failures:** alert-cap refusals, edge truncations, skipped id-less
  alerts, key failures, resets and mismatched reset values are logged and counted, never with a
  token or key.
- **A10 SSRF:** `Source::Poll { url }` is still the standing SSRF surface. `SystemUrl` narrows the
  schemes and registration needs the admin token (0012); the host isn't validated. Honouring the
  poll interval lets an admin poll a URL every 5 s, as today's `POST` minimum already implied.

**OWASP API Security Top 10 (2023)**

- **API1:** reserving push ids is 0012's; `Source` gives it a sentinel-free test. A mail system
  is bound to its id by its derived key (RFC 0017); a push token holder can still register a mail
  agent's id before its first report, which RFC 0012 §3 records as standing.
- **API2:** N/A.
- **API3:** `SystemInfo` carries no `token`; `SystemRecord` is never a DTO or a domain value.
- **API4:** 100 new alerts per poll, a 1 MiB alerts body, field limits, 1,000 records per system
  and 50,000 in total, `/api/alerts` `limit` clamped to 1,000, the Registry's stated worst case,
  identical writes free, polls spread by interval.
- **API5:** see 0012.
- **API6:** N/A.
- **API7:** see A10.
- **API8:** see A05.
- **API9:** `HUB_SECRET_KEY_FILE`, `HUB_SECRET_KEY_FILE_PREVIOUS`, `HUB_SECRET_KEY_RESET` and
  `events=` go into the README, with the recovery and rotation procedures; the 400s, the interval
  (now honoured, default 30 s), the `null` interval of push systems and the `/api/alerts` clamp
  are documented; compose and `.env.example` show the key mount.
- **API10:** agent alert JSON is bounded and parsed at the edge; a mail report's alerts arrive
  typed and bounded by RFC 0017 and pass the same edge limits.

## Testing plan

- **Catalog on the store**: `transact` returns after its commit and the hook; `Durable` forces an
  immediate commit, `Batched` waits for the next; two transactions in one group commit compose (the
  second reads the first's writes); an identical write writes nothing; the hook sees commits in
  order; a v1 fixture of every value decodes; an unknown version byte refuses to open, naming the
  table and key; keys of system `a` and system `a/b` never collide under a prefix scan.
- **Registry**:
  - a push rename racing a `PUT` of the name: the `PUT` survives;
  - a short hostname equal to its default name: repeated frames write nothing; a 256-byte
    hostname renames to its first 255 bytes, once;
  - the info fill writes once; control characters in agent info are stripped;
  - re-encoding an unchanged system with a token writes nothing;
  - the per-source table row by row, including **a push system `GET` then `PUT` back unchanged
    (accepted)** and a push `PUT` with `enabled: false` or another URL (400);
  - `token: ""` on `POST` (none) and on `PUT` (cleared); a token with a control character (400);
  - intervals 4, 5, 86,400 and 86,401 on `POST` and `PUT` (clamped); a `POST` without an
    interval gets 30;
  - `SystemUrl`: `push://`, `javascript:alert(1)`, `file:///etc/passwd`, no host, userinfo, a
    query, a fragment, a space, a tab, a trailing newline, an IPv6 zone id, 2 KiB + 1 (all 400);
    a trailing slash trimmed;
  - a path prefix: `http://proxy/agents/web01` polls `…/agents/web01/api/system` and
    `…/api/alerts`; the URL joined matches today's concatenation for a table of accepted URLs;
  - `name` 255 and 256 bytes; `token` 1 KiB and 1 KiB + 1;
  - `SystemInfo` JSON unchanged except `token` and a push system's `poll_interval_secs` (a golden
    test), ordered by name then id; `Debug` of both payloads never shows the token.
- **Poller**: each system polled at its own interval (an injected clock); offsets spread by id;
  disabled, push and mail systems never polled.
- **Mail systems**:
  - the per-source table's mail column row by row: `GET` then `PUT` back unchanged (accepted),
    `PUT enabled: false` (stored), `PUT` of a URL, an interval or a token (400), `POST` with
    `mail://` (400); `PUT` from each source to each other (400);
  - a report for an unseen id registers a `Mail` system in the same transaction as its receipt,
    points and alerts; for a push or poll id, `TransportMismatch` with nothing written; a push
    registration for a mail id answers `Reserved(Mail)`;
  - a duplicate `(run, seq)` is skipped with nothing staged or written, including after the
    system was deleted (a retired receipt), when the duplicate is the first report after the
    delete, and when both copies are in the same scan; the scan's other reports commit;
  - a new report after a delete registers a new generation, `Newest`, with no `mail_newest`
    carried over;
  - pruning at exactly the receipt window on the retention clock and one second before; a +1
    year system clock step prunes nothing early; `mail_newest` survives the prune;
  - the overdue sweep over `mail_newest`: on time, overdue, no entry (overdue), disabled
    (skipped); an overdue mark sets `last_error = "mail overdue"`, and the next report clears it;
  - a report committed between the overdue sweep's read and its write leaves the system online
    (`overdue.rs::a_report_stored_since_the_read_isnt_marked_overdue`, its guarantee kept, its
    `last_seen` assertion released above);
  - a retention pass pruning between the scan's freshness check and its transaction: the
    replay of a deleted system's report is refused as stale, nothing registered; **the same
    after the retention clock steps back an hour behind a prune**: the floor holds;
  - `newest_of_scan` and `stale_before` as tables (ties on `created_at` across runs and within
    one; the floor's edge); the global cap full of 1,001 mail records seen a day ago and one
    polled record seen 3 hours ago: the polled record is the victim, in one read; a refreshed
    record and a changed interval move the entry's key; after a forward fault, a prune, the
    correction and a rewind, a current report is accepted;
  - the prune keeps each system's newest current receipt
    (`db/mail.rs::the_prune_keeps_the_newest_current_receipt`, ported);
  - two `Newest` reports of one system in one scan, in both orders, and with equal `created_at`
    across two runs: points, metrics, info, round and `mail_newest` all come from the one
    `newest_of_scan` choice; a non-current choice writes `mail_newest` and the status but no
    point and no round;
  - a mail report's newest sample fills the system's info and default name;
  - a mail system's acknowledged incident unseen for 3 hours survives the global cap; unseen for
    7 days + 1 s it is a victim;
  - the ported tests of `db/mail.rs`, `mail_intake/ingest.rs`, `mail_intake/overdue.rs` and
    `mail_intake/scan.rs` keep their meaning, except these, each released under the
    test-contract guard with `--authority user` when implemented (the owner's decisions of
    2026-10-03), and quoted in the change summary:
    - *newest snapshot only, at hub time*: `db/mail.rs::a_first_report_registers_its_system_and_stores_everything`,
      `mail_intake/ingest.rs::a_first_report_registers_and_stores_its_system`,
      `db/mail.rs::a_backfill_report_adds_points_but_leaves_the_status`,
      `mail_intake/ingest.rs::a_backfill_report_leaves_the_live_metrics`, and
      `mail_intake/scan.rs::a_backlog_is_stored_whole` (its count holds; its doc and name change
      to "a backlog's reports are all accepted");
    - *expiry removed* (points are stamped at hub now): `db/mail.rs::a_point_past_its_retention_is_left_out_and_counted`,
      and the `expired` field asserted in `db/mail.rs` and `mail_intake/ingest.rs`
      (`MailStored::Stored`, `Ingested::Stored`);
    - *`last_seen` and overdue on hub time*: `overdue.rs::an_overdue_mail_system_is_marked_offline`
      (`last_seen == CREATED` becomes `received_at`) and
      `overdue.rs::a_report_stored_since_the_read_isnt_marked_overdue` (ported for its guarantee,
      but its `last_seen == CREATED + 60` assertion becomes `received_at`, so it is released
      too);
    - *no SQL*: `overdue.rs::a_mail_row_with_no_receipt_is_overdue_once` (its row can't be
      re-created by hand; its `last_error` assertion is kept by the row above);
    - *fail-stop instead of rollback*: `db/mail.rs::a_failure_partway_leaves_nothing_and_a_retry_stores_once`
      and `db/mail.rs::a_delete_that_fails_partway_deletes_nothing` (a SQLite trigger with no
      redb equivalent).
- **Generations, deletion and tombstones**: delete then append to the old generation
  (`SystemGone` at that commit); **a poll's alert transaction in the same group commit as the
  delete, before and after it**: no alert record of the deleted system survives; delete while a
  poll is in flight (its writes refused or ignored); delete removes the retention override and
  pending shortening and decrements the push counter; delete then re-register (new generation,
  empty history); delete an online push system (it re-registers, empty); the tombstone kept until
  the purge finished and one raw-retention period passed.
- **Sealed tokens and the key**: seal and open; a wrong key; a token copied to another system, to
  a new generation, or kept while the URL is changed directly in the file, fails to open; **a
  token `PUT` then a URL `PUT` in one group commit**: the new token opens under the new URL; key
  files (mode 0600 accepted; 0640 with a primary or supplementary hub gid accepted; 0640 with
  another gid, 0644, another owner refused; root accepted; a symlink to a valid file accepted; 31
  and 33 bytes refused); a configured missing file refused, nothing generated; a generated key's
  mode checked at once; a filesystem that ignores modes refused at generation; the key durable
  before the first seal (a child killed between them); a lost key: the refusal names
  `HUB_SECRET_KEY_FILE_PREVIOUS` and `HUB_SECRET_KEY_RESET=<id>`; the reset with the right id
  drops exactly the unopenable tokens; with a wrong id, ignored; left set, ignored with a `warn`;
  rotation re-seals every token and updates the key id; a token opening with neither refuses; the
  key and tokens never appear in any log line.
- **Alert records**:
  - `record_key` for a table of system ids and incident ids, including `_` in the system id; the
    API id finds its record through `alerts_api_id`; acknowledging by API id works for a system id
    containing `_`;
  - first values kept on repeat; a repeated acknowledgement writes nothing;
  - `last_seen_active` coalescing exactly at 1 h, and 1 s before;
  - `events` retention exactly at the period on the retention clock, 1 s before kept, never while
    reported; **a +1 year system clock step evicts nothing** (the retention clock bounds it);
    batches of 1,000;
  - caps: 1,000 per system and 50,000 in total, exactly at and one past; **a global-cap victim is
    never a record seen within 2 hours or twice its system's interval** (a still-firing,
    acknowledged incident whose refresh was coalesced 59 minutes ago survives, and the new record
    is refused instead); two polls in one group commit at 49,999 never exceed 50,000;
  - edge limits: 100 and 101 **new** alerts per poll, existing records beyond position 100 still
    refreshed; a 1 MiB + 1 body; field cuts on character boundaries; an incident id of 256 and 257
    bytes;
  - id-less alerts: two polls make one record; the message changing still makes one; two systems
    with identical alerts make two; two disk rules differing only by mount point make two;
    ("ab","c") and ("a","bc") differ; no `fired_at` is skipped and counted;
  - `/api/alerts` `limit` 1,000 and 1,001 (clamped); `count_active_alerts` from the counter.
- **The legacy file warning**: logged at every start while `system-hub.db` exists, not after.
- **Compose key example**: the smoke test starts the stack with a key mounted as the example says.
- **Ported tests**: every registry and alert test from `db.rs` and `routes/api.rs` keeps its
  meaning, except the documented clamps. A ported test whose expectation changes (the backfill
  points, a mail system's `last_seen`, a push connection for a polled id) is released under the
  test-contract guard, with the authority its RFC names, and quoted in the change summary.

## Impact on `docs/ARCHITECTURE.md`

- **Components**: the registry in memory over the catalog tables; the poller's per-system timers.
- **Data flow**: `db.rs` replaced by `storage/` over 0010's store.
- **Storage**: the catalog tables and their keys, commit classes, the commit hook, sealed tokens
  and the key id, `record_key` and the alert indexes, retention and caps; the mail receipt tables
  replacing `mail_receipts`.
- **Domain model**: Fleet Registry, Fleet History and Ingestion rows (`SystemSource` becomes
  `Source`, with `Mail`), and the glossary: **mail system** (`Source::Mail`; its push refusal is
  still answered `transport mismatch`, decided as `Reserved(Mail)`), **mail receipt** (retired by
  generation rather than a column; pruned by the retention clock except the newest current one),
  current and retired receipt.
- **Trust boundaries**: the secret key file and its checks; sealed tokens bound to their URL; the
  legacy plaintext file; *Hub → Agent (poll)*: `SystemUrl`'s rules, path-prefix joining, and the
  honoured interval.
- **Testing architecture**: the `db.rs` tempfile paragraph replaced by the catalog's tests.
- **Open questions**: closes dynamic SQL, unbounded alerts, alerts without an id making a record
  per poll, unbounded alert ids, the `INSERT OR REPLACE` cascade hazard, the `push://` sentinel,
  push systems being polled, and the registry's blocking SQLite calls; adds the default key
  location inside the data directory, and that a deleted online push or mail system re-registers
  unless its agent is stopped.

## Rollout / migration notes

- Implemented together with 0010; shipped with 0008, 0010 and 0012 in one release. No
  migration: the registry starts empty (0010's Rollout).
- **Behaviour changes:** `/api/alerts` clamps `limit` to 1,000; `POST`/`PUT` refuse
  non-http(s) URLs, whitespace in URLs, invalid tokens and oversize fields with 400, and poll
  settings on push systems unless unchanged; the poll interval is honoured (default 30 s);
  a push or mail system's `poll_interval_secs` is `null`; a mail system's `PUT` refuses poll
  settings; at most 1,000 alert records per system and
  50,000 in total; the API has no `token` field; the hub image's user has uid 10001.
- Implementation, each step through the full gate:
  1. the catalog tables, commit classes and the hook (on 0010's writer);
  2. the registry, sources, `SystemUrl`, the poller's timers, sealed tokens and the key
     procedures;
  3. tombstones, generations and the delete transaction;
  3a. the mail receipt tables, the report transaction and the overdue sweep on the catalog
     (with 0010's `CatalogTxn::append`);
  4. alert records with the edge limits, `record_key`, the indexes and the caps;
  5. removal of `db.rs`, the Dockerfile uid, the compose example and its smoke test.

## Review

The first two passes and the first pass on the undivided draft reviewed a custom catalog; their
tables are kept as written under **Earlier passes** at the end of this section. The **redb
rewrite** maps every finding of the latest (third) pass, and every earlier finding still
relevant:

| Finding | Verdict | Now |
|---|---|---|
| global-cap victims could be another system's acknowledged, still-firing incident (third) | CONFIRMED | **resolved**: a global victim must be unseen for 2 h or twice its system's interval; otherwise the new record is refused (§5) |
| alert identity unspecified across poll, acknowledgement and import (third) | CONFIRMED | **resolved**: one `record_key(system, incident)`; the API id kept and indexed; no import (§5) |
| GET→PUT of a push system returns 400; the 8 KiB cap and escaped info fields (third) | CONFIRMED | **resolved**: `GET`'s values accepted as no-ops; control characters stripped from info fields (§2) |
| a delete never told the store's policies; a re-registered push system inherited the override (third) | CONFIRMED | **resolved**: retention is store-owned, removed in the delete transaction (§3; 0010 §9) |
| a URL `PUT` re-sealed from a Registry cache that lagged the overlay (third) | CONFIRMED | **resolved**: re-sealed inside `f` from the record `f` reads; no overlay exists (§4) |
| when `transact` returns was unstated; possible deadlock with Registry guards (third) | PLAUSIBLE | **resolved**: returns after its commit and the hook; no Registry guard across a store call (§1) |
| the (now rejected) import skipped legacy URLs with userinfo (third) | CONFIRMED | **moot**: no import |
| `events` retention's clock unstated; a forward step evicts active incidents (third) | PLAUSIBLE | **resolved**: on 0010's retention clock, in bounded batches (§5) |
| the ≈ 210 MB budget assumed 10,000 systems while the push limit went to 1,000,000 (third) | CONFIRMED | **resolved**: alert records moved out of RAM; 0008's limit capped at 10,000; Registry ≤ 60 MB worst (§1) |
| `PollInterval` validated but never read (third) | CONFIRMED | **resolved**: honoured (author's decision) (§2) |
| id-less alert hash left out `mount_point`; no `fired_at` merged incidents (third) | PLAUSIBLE | **resolved**: every rule field hashed; no `fired_at` skipped and counted (§5) |
| `<system id>/` keys weren't prefix-free (third) | CONFIRMED | **resolved**: length-prefixed system keys (§1) |
| seams: `meta` ownership; name vs hostname limits; "leaf" locks; `xss.mjs` in the gate; header-invalid tokens (third) | CONFIRMED | **resolved**: hub keys under `hub/`; hostname cut to 255; lock order in 0010 §9; `xss.mjs` in 0012's plan for the delete confirmation; `AgentToken` must be a header value (§1, §2) |
| the compose secret example vs the hub's dynamic uid (third) | PLAUSIBLE | **resolved**: uid and gid pinned to 10001, the example says how to own the key, smoke-tested (§6) |
| key loss bricks the hub; rotation; the generated key's durability (first, second) | CONFIRMED | **resolved** as before: one-shot reset naming the key id; rotation at start; atomic publish and check (§4) |
| in-memory catalog RAM open to hostile growth (first, second) | CONFIRMED | **resolved**: alert records in redb; edge limits and caps kept (§1, §5) |
| `SystemUrl` accepted `push://` and rewrote the text (first) | CONFIRMED | **resolved** as before (§2) |
| writes per frame for short hostnames, info fill per poll, id-less alerts per poll, repeated acknowledgements (first) | CONFIRMED | **resolved** as before: identical writes free (§1, §2, §5) |
| generation allocation atomicity (first) | PLAUSIBLE | **resolved**: one transaction, counter raised at open (§2) |
| delete orphans, overrides, early tombstone drops (first, second) | CONFIRMED | **resolved**: one transaction on the single writer; tombstone dropped after the purge (§3) |
| lock order and lost updates under the custom group commit (first, second) | CONFIRMED | **moot**: redb's single writer (§1) |
| a push system's meaningless poll settings; unversioned values (first) | CONFIRMED | **resolved** as before (§2, §1) |
| a deleted online push system re-registers (first) | CONFIRMED | **stated** in the README and the confirmation (§3) |
| token secrecy in DTOs and `Debug`; secret mounts (first) | CONFIRMED | **resolved** as before (§2, §4) |
| "within 14 days" false for the hour tier (second) | CONFIRMED | **resolved**: removed from the store within one pass; freed-page bytes stated (§3; 0010 §5) |

**Mail amendment (2026-10-03).** RFC 0017 (Implemented) added mail systems after the passes
above; RFC 0012's fourth and fifth passes listed what this RFC must carry for them:

| Change | Where |
|---|---|
| `Source::Mail { enabled }`, and its column in the per-source API table (`mail://`, no poll settings, `enabled` settable) | §2 |
| every registration source-checked inside its transaction; RFC 0008's `Reserved` and RFC 0017's `TransportMismatch` decided there | §2 |
| mail receipts as three catalog tables; retired by generation instead of a column; pruned by the retention pass on the retention clock | §1, §7 |
| a scan's reports in one transaction, each report's refusals decided by reads before its writes; duplicates skipped whatever the receipt's generation | §7 |
| delete keeps the receipts and removes `mail_newest` | §3 |
| alert records from mail reports, with the mail interval in the global-cap rule | §5 |
| mail systems bounded by operator-derived keys in the memory budget | §1 |
| sealed credentials for mail not taken | §7 |

`rfc-adversary`, first pass on the mail amendment. Every finding was acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| the overdue sweep could overwrite a report committed after its read (today's compare-and-set dropped) | CONFIRMED | offline only while `last_contact` equals the `received_at` read; 0017's test ported (§7) |
| a prune between the scan's freshness check and its transaction let a deleted system's replay register it anew | CONFIRMED | the transaction refuses `created_at` older than the persisted retention clock minus the window (§7) |
| mail incidents delayed hours by a relay became global-cap victims and came back unacknowledged | CONFIRMED | for mail systems, evictable only after the receipt window (§5) |
| three tests in `HEAD` contradicted the design without being named; `overdue.rs` and `scan.rs` missing | CONFIRMED | the newest receipt kept (test ported); the overdue test ported; the partial-failure test named for the owner's release; both files in the port list (Testing plan) |
| after-commit steps of two `Newest` reports in one scan unordered | PLAUSIBLE | once per system, for its newest `Newest` by `created_at` (§7) |
| the info fill from mail reports not carried | PLAUSIBLE | added to the info rule; a row (§2) |
| ARCHITECTURE glossary lines for mail system and mail receipt | CONFIRMED (low) | listed (Impact) |
| `MailNewest.receipt` could name a pruned receipt | PLAUSIBLE (low) | moot: the newest current receipt is never pruned (§7) |

`rfc-adversary`, pass on the simplified amendment (findings written with "rcpt" for the receipt
table, which a session filter redacted). Every finding was acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| after a corrected forward fault the retention clock steps back, so a pruned rcpt's replay passes as new | CONFIRMED | a monotone `mail_prune_floor` written by each prune; step 4 refuses below it (§7) |
| tests in `HEAD` the simplification contradicts weren't named | CONFIRMED | each named under its owner's decision, `--authority user` (Testing) |
| §7 needed a retention-clock read and a prune hook 0010 §9 lacked | CONFIRMED | the floor replaces the clock read; `Store::on_retention_pass` in 0010 §9 (§7) |
| three rules picked the newest report of a scan | CONFIRMED | one `newest_of_scan` for points, after-commit steps and `mail_newest` (§7; 0010 §2) |
| the overdue mark dropped `last_error = "mail overdue"` | CONFIRMED | kept; cleared by the next report (0010 §10) |
| the global-cap victim search was unbounded | PLAUSIBLE | one search per transaction, at most 1,000 entries (§5) |
| a test row couldn't tell the 7-day rule from the old | CONFIRMED (low) | removed (Testing) |
| `meta/mail/in_flight` had no owner | CONFIRMED (low) | **moot**: the quarantine was cut (0010 §10) |
| the new rules had no pure home | PLAUSIBLE (low) | `newest_of_scan` and `stale_before` in `receipt.rs` (§7) |

Came closest and survived: a message that fails every scan, and a delete sharing a group commit
with a scan (store errors after a write stop the hub, and messages are deleted only after the
commit, so committed reports return as duplicates).

**Simplification (owner's decision, 2026-10-03).** After RFC 0010's third pass, a mail report
stores only its newest snapshot's points, at hub time (0010 §2 *Mail points*). Here: `MailNewest`
loses the clock lead; points are staged only for a system's newest `Newest` report of a scan; the
overdue mark is a transaction applied by the commit hook (0010 §10). A second pass on this
amendment ran before the simplification and its report never reached the session; the next pass
covers both.

`rfc-adversary`, final pass of 2026-10-03, resolved in one round with 0008's and 0010's:

| Finding | Verdict | Resolution |
|---|---|---|
| the bounded victim search read the oldest, unevictable, mail entries and refused every new alert | CONFIRMED | `alerts_seen` keyed by `evictable_at`; one read per search; re-keyed on refresh and interval change; a mixed-class row (§1, §5) |
| `mail_prune_floor` wasn't lowered by the rewind | CONFIRMED | the rewind sets it to `min(floor, rewound clock − window)`; shown in `/api/storage`; a row (§7; 0010 §2) |
| three answers to when `mail_newest` is written; two orders | CONFIRMED | once per system at the end of `f` for the `newest_of_scan` choice; one order (§7) |
| the currency gate's scope | CONFIRMED | status, `mail_newest`, info and alerts never gated; points, round and live metrics gated (§7; 0010 §2) |
| releases naming categories; a "ported" test whose assertion changes; `a_backlog_is_stored_whole` | CONFIRMED (low) | each named `file::test` (Testing) |
| `on_retention_pass` couldn't signal more work; prune throughput | PLAUSIBLE | `More`/`Done` looped under the bound; the throughput stated (§7) |

**Still open**: whether one timer per polled system scales to 10,000 polled systems (it is one
tokio timer each, which is cheap, but the performance test checks it).

**Earlier passes, as written against the custom catalog.** Section references in these tables
point at that draft, not at this one; the redb rewrite above says what each finding is now.

This RFC holds these findings from the first `rfc-adversary` pass on the undivided draft (the
full table is in RFC 0010's Review):

| Finding | Verdict | Resolution |
|---|---|---|
| catalog API lacks get/compare-and-swap/batch: lost updates, non-atomic cascades | CONFIRMED | `get`, `update(FnOnce)`, `batch` as one `fsync`ed record; registry in memory, written through (§1, §2) |
| delete: tombstone scope, recreation, lingering data | CONFIRMED | generations, tombstones with deletion time, `SystemGone` in the store, purge rewrite (§3) |
| catalog `fsync` and AEAD work per frame | CONFIRMED | configuration-only writes; unsealed-token cache (§1, §4) |
| alert retention and cap resurface acknowledged active incidents | CONFIRMED | eviction by `last_seen_active`, never while reported (§5) |
| `/api/alerts` `limit` uncapped | CONFIRMED | capped (§5) |
| plaintext tokens outlive the import | CONFIRMED | a warning and README guidance (§6) |

`rfc-adversary`, first pass on this RFC after the split. Three of the resolutions above didn't
hold as written (configuration-only writes, no resurfacing at the import seam, atomic
cascades). Every finding was acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| losing the key file bricks the hub; no recovery or rotation; the generated key's write isn't atomic | CONFIRMED | `HUB_SECRET_KEY_RESET=1` drops only unopenable tokens (author); rotation with `HUB_SECRET_KEY_FILE_PREVIOUS` in one batch (author); the key published atomically before the first seal; the series table no longer in the catalog, so nothing forces deleting it (§4, §1) |
| the in-memory catalog lets unauthenticated clients and hostile agents grow RAM | CONFIRMED | 100 alerts per poll, field limits, 1,000 records per system and 100,000 in total, `name`/`url`/`token` limits, a stated ≤ 190 MB budget; registration admin-gated (0012) and 0008 named (§1, §2, §5) |
| `SystemSource::Poll { url: Url }` accepts `push://` and rewrites the operator's URL | CONFIRMED | `SystemUrl`: http(s) with a host, no userinfo, ≤ 2 KiB, rendering the trimmed original (§2) |
| the import seam brings back resurfacing acknowledged incidents; 90-day history loss not in Rollout | CONFIRMED | imported records get `last_seen_active` = import time (here and in 0013); the eviction is listed in Rollout (§5) |
| rename per frame for short hostnames, info fill per poll, a record per poll for id-less alerts, unlimited acknowledgements: not configuration-only | CONFIRMED / PLAUSIBLE | identical writes are `Keep`; the info rule writes on change only; one record per poll; stable ids for id-less alerts; repeated acknowledgements free (§1, §2, §5) |
| generation allocation can't be atomic with this API | PLAUSIBLE | `update_many` writes counter and record in one record; the counter is also raised at open (§2) |
| delete leaves orphans: in-flight poll inserts, the retention override, tombstones dropped too early | CONFIRMED | alert records carry the generation and inserts check it; the override removed in the batch; tombstones kept until no data holds them and one raw-retention period passes (§3, §5) |
| catalog locks held across `fsync` and snapshots; the series namespace left out of the write rate; no lock order | PLAUSIBLE | readers never wait on `fsync`; one writer mutex with a 2 ms group commit; snapshots off-lock; the series table moved to the engine; the lock order stated (§1) |
| invalid states representable: a push system's token and interval; the info rule changed silently; no version byte on alert records | CONFIRMED | `Poll { url, token, interval }`; the info rule stated; version bytes on every hub-owned value, with fixture tests (§2, §5) |
| deleting an online push system only wipes its history: it re-registers at once | CONFIRMED | said in the README and the delete confirmation; a test row (§3) |
| token secrecy past the sealed record; key-file checks against real secret mounts | CONFIRMED / PLAUSIBLE | `token` removed from the response DTO; redacted `Debug` on the payloads; group-readable with the hub's gid, root ownership and symlinks accepted per stated rules (§2, §4) |
| test rows moot or missing | CONFIRMED | the race row is rename vs `PUT`; 1,001 clamps; ordering pinned; the missing rows added (Testing plan) |
| documentation inventory gaps; "latest poll or push" | CONFIRMED | README and ARCHITECTURE lists completed; alerts come from polls only (§5, Impact) |

Came closest and survived: the mixed-version fleet (no agent-facing change), the AEAD
construction itself (now with fixed-width associated data), and `update(FnOnce)`'s no-lost-update
claim for single-key writes.

`rfc-adversary`, second pass on this RFC. Four first-pass resolutions held (the unsealed-token
cache, the `/api/alerts` clamp, the plaintext warning, import-time `last_seen_active`), and
removing `token` from the response was confirmed safe. Four held only in part (atomic cascades,
no lost update, key loss, the RAM bound). Every finding was acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| the ≤ 190 MB budget is wrong by more than 2×, and the system count it multiplies has no cap in this release | CONFIRMED | RFC 0008's limit joins the release (owner); the budget recomputed from encoded and decoded sizes (≈ 210 MB worst, ≈ 50 MB typical); the total alert cap lowered to 50,000 and messages to 512 B (author); `AlertSeen` split out; a frozen copy that shares `Arc`s (§1, §5) |
| `delete_system` as a blind `batch` races a poll's inserts, and `update_many` can't choose global-cap victims | CONFIRMED | one `transact` API: reads, prefix scans and victim choice happen inside `f` under the writer mutex; delete scans `<id>/` inside it; victims chosen from `AlertSeen` inside it (§1, §3, §5) |
| the writer mutex and 2 ms group commit contradict each other (lost updates or no group commit); the lock order omits the hub's locks | CONFIRMED | the mutex covers `f` and the append only; `f` reads through a pending overlay; install and the hub's hook run in log order under one install lock; the full lock order stated; polls spread over the tick (§1, §2) |
| `HUB_SECRET_KEY_RESET=1` is persistent and irreversible; "wrong key" and "lost key" look alike; unspecified generation and group cases | CONFIRMED / PLAUSIBLE | a stored key id; the reset must name it (one-shot, author); the refusal names both `_PREVIOUS` and the reset; no key generated at a configured path; a generated key checked at once; groups from `getgroups` (§4) |
| "gone from disk within 14 days" is false for the hour tier | CONFIRMED | block builds skip tombstoned generations; the bound stated as 30 days, here and in the README text (§3; 0010 §5) |
| stable ids for id-less alerts not scoped to the system, ambiguous hash input, unstable for this repo's messages | CONFIRMED / PLAUSIBLE | `anon-` ids over length-prefixed system id, rule fields and `fired_at`, without the message; records keyed under `<system id>/` (§5) |
| the 100-alerts-per-poll cap lets an acknowledged, still-firing incident be evicted and resurface | PLAUSIBLE | adopted: the cap applies to new inserts; every reported existing record is refreshed (§5) |
| `SystemUrl` "the parsed form for requests" breaks path-prefixed agents; text and polled host can differ; query and fragment accepted | PLAUSIBLE | adopted: the text is stored, path segments are appended per poll; whitespace, control characters, query, fragment and zone ids refused (§2) |
| `Poll { url, token, interval }` versus its readers: response shape and `PUT` semantics undefined | CONFIRMED | one table per source for every response field and `PUT` field; `""` means none or clear; the interval clamped on `POST` and `PUT`; `enabled` moved into `Poll` (§2) |
| the in-memory `Registry` holds the persistence struct, and re-encoding re-seals | CONFIRMED | a domain `System` in the `Registry`, `SystemRecord` only in `storage/`; the sealed bytes kept, re-sealed only on change (§2, §4) |
| rotation leaves old-key ciphertexts in the log; the associated data doesn't bind the URL | PLAUSIBLE | adopted: a snapshot forced after rotation and reset deletes the old log; the URL text added to the associated data, re-sealed on a URL `PUT` (§1, §4) |
| late writers bring `live_status` back after a delete | PLAUSIBLE | adopted: `LiveStatus` carries the generation, and writes for another generation are ignored (§3; 0010 §10) |
| unknown version bytes: behaviour undefined; store-owned values unversioned | PLAUSIBLE | adopted: an unknown version refuses to open, naming the namespace and key; store-owned values versioned (§1) |
| inventory: compose and `.env.example` missing | CONFIRMED | a Compose secret example and an `.env.example` line (§6, Impact, Rollout) |

Came closest and survived: removing `token` from the response (nothing reads it), the
mixed-version fleet, and the fixed-width associated data (now with the URL added).
