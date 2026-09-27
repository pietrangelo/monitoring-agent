# RFC 0008: Push Registry Limit and Push System Lifecycle

- Status: Draft
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-25 (revised 2026-09-26 for the catalog-backed registry, then for the redb store)
- Affects: `system-hub`, and `system-agent` (its id, §7)
- Depends on: RFC 0006 (implemented: the handshake answer send, `PushAuth` parsing in
  `main`), RFC 0011 (the catalog tables, `Source`, generations, `transact`), RFC 0010 (the redb
  store, `LiveStatus`, `/api/storage`)
- **Part of the release that replaces SQLite (owner's decision):** this RFC ships together
  with RFCs 0010, 0011 and 0012 in one release. RFC 0013 (the SQLite import) is Rejected: the
  new hub starts with an empty store. RFC 0011 keeps every system in memory, so the registry
  must be bounded, and this RFC is what bounds push registration. Polled systems are bounded by
  the operator: registering one needs the admin token (RFC 0012).
- Related: RFC 0007 (Draft; ingestion cost per frame). Split from an earlier, wider draft of
  RFC 0006. The owner chose the default limit, 1000.

## Motivation

1. **Unbounded auto-registration (API4).** Every accepted handshake with an unseen id
   registers a permanent system. The ids are self-asserted, so with `HUB_PUSH_TOKEN` unset
   anyone can fill the registry, and with a token set any token holder can. Under RFC 0011 the
   registry lives in memory, so an unbounded registry is unbounded RAM, not only disk.
2. **Offline marking isn't tied to the connection that is current.** Since RFC 0006, every
   exit of `handle_push` after registration marks the system offline, whichever connection it
   is. So a stale connection that reaches its 90 s idle deadline marks offline a host that
   already reconnected, and it stays offline until that host's next frame. And a registration
   whose blocking task fails ends the connection with no answer at all (`Handshake::Closed`),
   so the agent reconnects at once instead of taking its 5 s backoff.
3. **The agent's id is recomputed on every reconnect.** `src/push.rs::get_persistent_id` runs
   inside `push_once`, and shells out to `hostname` from async code. Without
   `/etc/machine-id`, `/var/lib/dbus/machine-id` and a `hostname` binary (distroless images),
   it draws a new random UUID on **every reconnect**, so one agent can take one registry slot
   per reconnect.

RFC 0011 already fixes two problems the first revision of this RFC had to handle itself: push
systems are no longer polled (the poller visits only `Source::Poll`), and "push system" is no
longer the `url = "push://"` sentinel (it is `Source::Push`, and `PUT` can't change a system's
source).

## Proposed design

### 1. Push system

A **push system** is a system whose source is `Source::Push` (RFC 0011 §2): one registered by a
push handshake. It is never polled. `POST /api/systems` can't create one (0011's `SystemUrl`
refuses `push://`), and `PUT` can't turn a system into one or out of one.

### 2. Registry limit

```rust
/// The most push systems the hub will auto-register. 1 ..= 10,000.
pub struct PushRegistryLimit(NonZeroU16);

impl PushRegistryLimit {
    pub const DEFAULT: u16 = 1000;
    pub const MAX: u16 = 10_000;
    pub fn from_env(value: Result<String, VarError>) -> Result<Self, PushRegistryLimitError>;
    /// Whether a registry holding `push_systems` push systems may register one more.
    pub fn admits(&self, push_systems: u32) -> bool;
}

pub enum PushRegistryLimitError { NotUnicode, Zero, OutOfRange, NotANumber }
```

`HUB_MAX_PUSH_SYSTEMS` is parsed in `main` together with `HUB_PUSH_TOKEN` (RFC 0006) and
`HUB_ADMIN_TOKEN` (RFC 0012), before the store opens:

| Value | Outcome |
|---|---|
| unset or empty | the default, 1000 |
| a whole number from 1 to 10,000 | that limit |
| not UTF-8 (`NotUnicode`), `0` (`Zero`), negative or above 10,000 (`OutOfRange`), or anything else (`NotANumber`) | the hub refuses to start, logging the variable's name and the reason, never the value |

The upper bound is RFC 0011 §1's: its Registry memory budget (≤ 60 MB worst) is stated for
10,000 systems. A fleet beyond that needs a new budget, and an RFC.

### 3. Exact registration, inside the catalog transaction

Registration of an unseen push id is one RFC 0011 registration transaction
(`Store::transact(Commit::Durable, …)`, 0010 §9), the same one that allocates the generation:

```rust
pub enum Registration {
    AlreadyRegistered { generation: Generation },
    Registered { generation: Generation },
    RegistryFull,
}

/// In the hub's `storage/` adapter; `f` runs on the store's writer thread.
fn register_push_system(store: &Store, id: &SystemId, limit: PushRegistryLimit, now: u64)
    -> Result<Registration, StoreError>;
```

Inside the transaction, on the writer thread:
1. read `systems/<key>`: present → `AlreadyRegistered { generation }`, whatever its source
   (RFC 0012 has already refused a polled id before this point), so the connection writes its
   `LiveStatus` for that generation;
2. read `meta/hub/push_systems`, the count of push systems;
3. ask `limit.admits(count)`: no → `RegistryFull`, and the transaction writes nothing;
4. yes → write the new `systems/<key>` record, the incremented `meta/hub/push_systems` and the
   incremented `meta/hub/generation`, committed together.

- **The limit is exact.** redb has one writer, and 0010 runs every transaction on one thread, so
  two handshakes racing for the last slot are serialised, and the second sees the incremented
  count. An id racing itself gets `AlreadyRegistered`.
- RFC 0011's delete transaction decrements `meta/hub/push_systems` when it deletes a push system.
- **At open, the adapter recomputes the counter** from `systems` in one transaction and corrects
  it, with a `warn` if it differed, as defence in depth. The store doesn't know what a push
  system is; the adapter does.
- The rule stays in `admits`, a pure function; the transaction only feeds it the count.
- `transact` blocks until its commit, so the async handshake calls it through `spawn_blocking`
  (the existing `on_blocking_pool`). The blocking wait itself can't panic: `f` runs on the
  writer thread, and a panic there is 0010's writer fail-stop, which ends the hub. A
  `JoinError` can only come from runtime shutdown; it maps to `registry unavailable` like a
  store error, and has no test row of its own.

### 4. Handshake outcomes

The push handshake checks, in order: the shape, the token (constant time), the id's `SystemId`
rule (RFC 0005), the reserved-id rule (RFC 0012), then registration (§3). Nothing else; there
is no import in progress to refuse (RFC 0013 is Rejected).

`Refusal` (RFC 0006: `Rejected(HandshakeRejection)` and `Timeout`) gains two variants:

| Outcome | Answer |
|---|---|
| `AlreadyRegistered` | accepted, as today |
| `Registered` | accepted |
| `RegistryFull` | `auth_error` / `registry full` (`Refusal::RegistryFull`) |
| a `StoreError` (`Closed` during shutdown, `Failed`, `Io`) | `auth_error` / `registry unavailable` (`Refusal::RegistryUnavailable`), so the agent takes its 5 s backoff instead of reconnecting at once |

Logging (A09):
- the first `RegistryFull` after the registry fills is logged at `warn`, then **again every
  hour while it stays full**, with the count of refusals since the last line;
- a store error is logged at `error` at most once per hour, with the error kind;
- each refused id is counted in `/api/storage` (`push_registry: {limit, count, refused_full,
  refused_unavailable}`), never logged per id at a level that is on by default.

### 5. Lifecycle: the current connection

- **Not polled**: RFC 0011 (the poller visits only enabled `Poll` systems).
- **Connection numbers.** Each accepted push connection takes the next number from one hub-wide
  in-memory counter (a `u64`, never persisted: numbers only compare connections of one process).
- **A frame claims currency.** Every ingested frame sets its system's `LiveStatus` (RFC 0010
  §10) to `Online`, `last_contact` to now, and **`connection` to the frame's connection
  number**. So the current connection is the one that last delivered a frame, or, before any
  frame, the one accepted last (the handshake sets `connection` too).
- **Offline only by the current connection.** Every exit of `handle_push` after registration (a
  failed `auth_ok`, `SystemGone`, the idle deadline, an oversize message, a failed ingestion, a
  clean close) sets `Offline { since: now }` **only if `connection` is still its own number**.
  The decision is a pure function, `on_exit(status, number, now) -> LiveStatus`, taking the
  connection number and hub time as arguments.
  - A host that reconnected within the old connection's 90 s idle deadline isn't marked offline
    when the old connection times out.
  - **Newest exits first.** If the new connection ends while the old one still delivers frames
    (two agents presenting one id, or a half-open old socket that recovers), the new one marks
    offline, and the old one's next frame claims currency and sets `Online` again; its own exit
    later marks offline.
- **Staleness backstop.** Every 30 s, the adapter sets `Offline { since: now }` on every push
  system that is `Online` with a `last_contact` older than **180 s** (twice the 90 s idle
  deadline), so no path that loses an exit can leave a system online for good. The rule is a
  pure function, `stale(status, now) -> bool`. It runs on hub time: a forward clock step can
  mark a live system offline early, until its next frame (at most one push interval), and a
  backward step can't mark anything (hub time holds, 0010 §2). "Delete offline" measures age on
  the retention clock (RFC 0012), so an early `Offline` can't make a system a candidate.
- **After a restart**: RFC 0010 §10's rule. `LiveStatus` is recovered as `Unknown`, and a system
  not heard from within 120 s becomes `Offline`.
- **Offline detection latency** is RFC 0006's 90 s idle deadline, stated, with the 180 s
  backstop behind it.
- **"Delete offline"** (RFC 0012) therefore sees a push system as offline only when its current
  connection has ended, or it has gone silent.

### 6. Where the limit holds

Everywhere. `POST /api/systems` needs the admin token and can't create a push system, and `PUT`
can't change a system's source (RFCs 0011, 0012). So the only way to add a push system is a
push handshake, and every handshake goes through §3.

### 7. The agent's id, computed once

This stays in this RFC: the limit is only as good as the agent's id is stable, and the id
change is small.

The agent computes its push id **once, in its synchronous `main`** (`fn main() -> ExitCode`,
before the Tokio runtime starts, so `std::fs` is fine there), and passes it to the push task,
which reuses it for every reconnect. The sources, in order:

1. the file named by **`SYSTEM_AGENT_ID_FILE`**, if that variable is set and the file exists;
2. `/etc/machine-id`, then `/var/lib/dbus/machine-id`, as today;
3. the **hostname**, as today, but read through `sysinfo::System::host_name()` (a dependency
   already), never by running a `hostname` binary;
4. a new random UUID.

- **The id rule mirrors the hub's `SystemId`** (non-empty after trimming, at most 255 bytes,
  not `.` or `..`), as an agent-side `AgentId` newtype. A machine-id file or hostname that breaks
  it is skipped, as an empty one is today.
- **`SYSTEM_AGENT_ID_FILE` is optional, with no default.** When it is set:
  - an existing file whose content (UTF-8, trimmed) breaks the rule, or that can't be read,
    **refuses startup** with exit code 78 (`EX_CONFIG`, as the agent's other configuration
    errors), naming the variable and the broken rule;
  - when step 4 is reached, the new UUID is written there **only if the file is still
    missing**: `tempfile::NamedTempFile` in the same directory, written, `sync_all`, then
    `persist_noclobber` (never overwrites), then the directory is `fsync`ed. If another process
    created the file first, the agent reads that file instead. Any other failure refuses
    startup (exit 78): the operator asked for a persistent id, and a silent per-process id
    would churn registry slots;
  - an id that comes from steps 2 or 3 is never written to the file.
- **Unset**, step 4's UUID lives for the process only, and the agent logs one `warn` saying so
  and naming `SYSTEM_AGENT_ID_FILE`. It is one id per process, never one per reconnect.
- **The hostname stays a source.** Dropping it would change the id of every agent that uses it
  today, leaving an offline twin behind. Its collisions across hosts are the standing API1
  self-asserted-id risk, unchanged.
- **Container images are unchanged.** The agent's Dockerfile and `docker-compose.yml` set no
  `SYSTEM_AGENT_ID_FILE`; an operator who wants one mounts a volume and sets it. The README says
  so.
- `tempfile` moves from the agent's dev-dependencies to its dependencies (already in its
  lockfile); `cargo audit` runs for that change.

## Domain impact

- **Fleet Registry:** `PushRegistryLimit`, the `meta/hub/push_systems` counter, and
  `register_push_system` inside the registration transaction.
- **Ingestion:** `Refusal::RegistryFull` and `Refusal::RegistryUnavailable`; the handshake
  order stated in §4; connection numbers, frames claiming currency, `on_exit` and the `stale`
  backstop over `LiveStatus`.
- **Telemetry Publishing (agent):** `AgentId`, computed once; `SYSTEM_AGENT_ID_FILE`.
- **Glossary:**
  - **push system**: a system whose source is `Source::Push`, registered by a push handshake,
    never polled;
  - **push registry limit**: the most push systems the hub will auto-register;
  - **current connection**: of a push system's connections, the one that last delivered a frame
    (or, before any frame, was accepted last). A push system's status is set offline only by
    its current connection's end, or by the staleness backstop;
  - **connection number**: the in-memory number that identifies a push connection.
- **Published contracts:** the push frame is untouched. The handshake gains two `auth_error`
  messages, `registry full` and `registry unavailable`. Mixed-version fleets:
  - *any agent*: every `auth_error` gets the same 5 s backoff;
  - *old agents without a stable id source*: a new UUID per reconnect until upgraded, so churn
    can fill the limit; the hourly `warn` and "delete offline" handle it;
  - *old hub*: ignores `HUB_MAX_PUSH_SYSTEMS`; *old agent*: unchanged behaviour.

## Alternatives considered

- **Unlimited by default.** No change on upgrade, but unbounded RAM under RFC 0011. The owner
  chose 1000.
- **A limit up to 1,000,000** (the previous revision). RFC 0011's memory budget holds for 10,000.
- **Count push systems by scanning the registry.** O(n) per registration on the writer thread. A
  counter in the same transaction is exact and O(1).
- **Only the latest-accepted connection is current** (the previous revision). If the newest
  connection ended first, the system went offline while the older one still delivered frames,
  and stayed offline. Frames claiming currency, plus the backstop, fix both orders.
- **Drop the hostname fallback** (the previous revision). It changed existing agents' ids.
- **A default `SYSTEM_AGENT_ID_FILE`** (`/var/lib/system-agent/id`, the previous revision).
  Container images have no writable volume there by default, so every start would warn or fail.
- **One total cap on every system** (RFC 0011). Polled systems are already bounded by the admin
  token; the limit targets the self-asserted path.
- **Pre-registration only.** The strongest control, but it needs an API that accepts a chosen
  id, and belongs with per-system push credentials.
- **Expire stale push systems automatically.** Recorded as an open question: automatic deletion
  of history needs an owner decision of its own.

## Security implications

- **API4:** push registration is bounded, exactly, at 10,000 at most, and REST can't bypass it
  (§6).
- **A04 / API6: onboarding lock-out, accepted.** Registration is permanent and happens at the
  handshake. Anyone who can push (every token holder, or anyone while the token is unset) can
  fill the registry with 1000 handshakes. New and recreated hosts then get `registry full`, and
  the attacker refills freed slots faster than agents retry. The remedies: set
  `HUB_PUSH_TOKEN`; raise the limit; expose `/api/push` only to the fleet's networks. The trade
  is accepted: an unbounded registry costs every operator, while the lock-out needs an attacker
  who already has push access.
- **A05 / API8:** a malformed or out-of-range limit refuses startup; so does an invalid
  `SYSTEM_AGENT_ID_FILE` on the agent.
- **A07 / API2:** the `registry full` and `registry unavailable` answers come after
  authentication. The registry's fullness is **not** a secret, though: `/api/storage` is an open
  read and shows `push_registry`'s limit and count, as `GET /api/systems` already shows every
  system. Nothing here reveals the token.
- **A09:** an hourly `warn` while full, with counts; store errors logged once per hour; the
  configuration error names the variable, never its value; ids never logged by default.
- **A03:** no SQL.
- **A06:** `tempfile` becomes an agent runtime dependency (already locked); `cargo audit` for
  that change.
- **API1:** unchanged: push ids stay self-asserted. Frames claiming currency mean two agents
  presenting one id alternate its status, as they already alternate its metrics.
- **API9:** `HUB_MAX_PUSH_SYSTEMS`, `SYSTEM_AGENT_ID_FILE` and the two `auth_error` messages go
  into the README.
- Everything else is unchanged.

## Testing plan

TDD, per `CLAUDE.md`, with `red-test-adversary` and `rosette-auditor`.

- **Pure:**
  - `PushRegistryLimit::from_env` over unset, empty, `1`, `1000`, `10000`, `10001`, `0`, `-1`,
    `1,000`, `18446744073709551616` and non-UTF-8, each with its outcome;
  - `admits` below the limit, at it, and above it;
  - `on_exit` as a table: the current connection exits (offline, `since` = now); an older one
    exits (unchanged); already offline (unchanged, `since` kept);
  - frames claiming currency as a table, including **newest exits first**: connection 2
    accepted, connection 2 exits (offline), a frame on 1 (online, current 1), 1 exits (offline);
  - `stale`: `Online` at exactly 180 s since `last_contact` and one second under; `Offline` and
    `Unknown` never stale.
- **Registration transaction** (a temporary store):
  - a limit of 1 with one polled system present admits an unseen push id;
  - a limit of 1 with one push system present refuses an unseen id and writes nothing, and
    returns `AlreadyRegistered { generation }` with the stored generation for the known id;
  - two threads racing for the last slot: exactly one `Registered`;
  - deleting a push system decrements the counter; a counter written wrong through a test seam
    is corrected at open, with a `warn`;
  - a child process killed right after a registration answered: at reopen, the record, the
    counter and the generation are all there; killed before the commit, none is.
- **Real server:**
  - a full registry answers `registry full`, and logs one `warn`, then another after an hour of
    injected time;
  - a store closed through a test seam answers `registry unavailable`;
  - a host that reconnects within 90 s isn't marked offline when its old connection times out;
  - a connection that goes silent without closing is `Offline` after the backstop (injected time).
- **Agent** (`main`'s id step, with injected paths and an injected hostname source):
  - the id is computed once per process (a counting seam over the sources);
  - each source in order, each skipped when empty or breaking the rule; the rule as a table
    shared by value with the hub's `SystemId` table;
  - `SYSTEM_AGENT_ID_FILE` set and missing: the UUID is written, and a second run reads it;
    set and invalid: exit 78; set in an unwritable directory: exit 78; the file created by
    another process between the check and `persist_noclobber`: that file's id is used, not
    overwritten; unset with no other source: one `warn`, one id for the process.

## Impact on `docs/ARCHITECTURE.md`

- § Components: push registration is bounded; the agent's id is computed once, in `main`.
- § Domain model: the Fleet Registry and Ingestion rows, the agent's Telemetry Publishing row,
  and the glossary (**push system**, **push registry limit**, **current connection**,
  **connection number**).
- § Trust boundaries, Agent → Hub (push): the limit, the handshake order, and that the limit
  holds everywhere.
- § Open architectural questions:
  - removed: unbounded auto-registration; push systems being polled; the reliance on the poller
    in RFC 0005's stored-id entry; a stale connection marking a live host offline; the agent's
    blocking `hostname` shell-out;
  - added: expiring stale push systems.
- `README.md`: the `HUB_MAX_PUSH_SYSTEMS` and `SYSTEM_AGENT_ID_FILE` rows; `registry full` and
  `registry unavailable` in the `auth_error` table; "they appear automatically" (up to the
  limit).
- `docker-compose.yml`: `HUB_MAX_PUSH_SYSTEMS: ${HUB_MAX_PUSH_SYSTEMS:-}` on the hub; nothing on
  the agent. `.env.example`: a commented line.

## Rollout / migration notes

- Ships in the release that replaces SQLite, with RFCs 0010, 0011 and 0012. The new hub starts
  with an empty store (no migration), so every push agent registers again on its first
  handshake.
- **Size the limit before upgrading**: count the old hub's push systems through its API (the
  container has no `sqlite3`): `curl -s http://hub:9091/api/systems | jq '[.[] | select(.url ==
  "push://")] | length'`. A fleet above 1000 sets `HUB_MAX_PUSH_SYSTEMS`, or its extra agents get
  `registry full`.
- A malformed `HUB_MAX_PUSH_SYSTEMS` refuses startup, and the log names it.
- Agents with no stable id source keep one id per process once upgraded; set
  `SYSTEM_AGENT_ID_FILE` on a volume to keep it across restarts.

## Review

`rfc-adversary`, first pass, on the SQLite-based first revision. The catalog-backed revision
resolved every finding as follows:

| Finding | Verdict | Resolution |
|---|---|---|
| `PUT` still decides what a push system is | CONFIRMED | `SystemSource` (0011): `POST`/`PUT` can't create or change a push system (§1, §6) |
| removing the poller removes the only offline backstop | CONFIRMED | the dependency on RFC 0006's 90 s deadline is declared (§5); offline marking failures can't happen silently (in memory, `live_status`) |
| a stale connection marks a live host offline | CONFIRMED | a per-id connection number; only the current connection marks offline (§5) |
| the agent recomputes its id on every reconnect | CONFIRMED | computed once per process; the fallback persisted; `system-agent` in Affects (§7) |
| the push-only nginx pattern can't isolate REST | CONFIRMED | moot: REST can't bypass the limit once `POST` is admin-gated and can't create push systems; the pattern is dropped (§6) |
| tests that can't fail | CONFIRMED | an `admits`-hook race test; "not polled" is 0011's and has a collector tick test there; the reset test dropped (RFC 0006's); a `PushConfig` seam; `JoinError` through the same seam (Testing plan) |
| A09: a database error logs nothing; per-id `debug` lines invisible | CONFIRMED | catalog errors logged once per hour; the full `warn` repeated hourly with counts; counters in `/api/storage` (§4) |
| inventory: `Refusal` variants, the Ingestion row, README lines, an API pre-check, the "resets before `auth_ok`" claim | CONFIRMED | variants defined against RFC 0006's enum; the Ingestion row; the README rows; a `curl | jq` pre-check; the claim replaced by §5's exit list (§4, Domain impact, Rollout) |
| startup marking on poisoned rows | PLAUSIBLE | moot: no startup marking; RFC 0010's 120 s rule instead (§5) |
| the limit bounds rows, not load | PLAUSIBLE | stated; per-frame cost is RFC 0007's, and the limit's upper bound keeps memory a stated number (§2) |

**redb rewrite.** The owner rebuilt the store on redb (RFC 0010) and chose no migration (RFC 0013
Rejected). Each CONFIRMED finding above, and what it is now:

| Finding | Now |
|---|---|
| `PUT` decides what a push system is | **resolved**, unchanged (§1, §6) |
| no offline backstop without the poller | **resolved**, strengthened: the 180 s staleness backstop (§5) |
| a stale connection marks a live host offline | **resolved**, and the newest-exits-first order that the connection-number rule got wrong is fixed by frames claiming currency (§5) |
| the agent recomputes its id on every reconnect | **resolved**: computed once in `main`; the hostname kept without a shell-out; the id file optional with no default, never overwritten, invalid → exit 78 (§7) |
| the nginx pattern | **moot**, unchanged (§6) |
| tests that can't fail | **resolved**; the `JoinError` row is dropped: registration runs on the writer thread, whose panic ends the hub, so `JoinError` only comes from runtime shutdown (§3) |
| A09 for store errors | **resolved**: `StoreError` logged once per hour (§4) |
| inventory | **resolved**; `Refusal::Timeout` named as the code has it; the handshake order has no import step; the pre-check now sizes the limit (§4, Rollout) |

Also changed in this rewrite, from the round on RFCs 0010–0013: the limit's upper bound is
10,000, to match RFC 0011's memory budget (§2); `AlreadyRegistered` carries the generation
(§3); the counter lives under `meta/hub/push_systems` and is recomputed at open by the adapter
(§3); Motivation 2 now describes the code as RFC 0006 left it; the A07 claim no longer implies
the registry's fullness is hidden (`/api/storage` is open); the glossary gains **current
connection**.

**Still open**: nothing CONFIRMED. This revision has not been reviewed yet; its `rfc-adversary`
pass comes with the next round on RFCs 0008, 0010, 0011 and 0012.
