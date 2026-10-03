# RFC 0008: Push Registry Limit and Push System Lifecycle

- Status: Draft
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-25 (revised 2026-09-26 for the catalog-backed registry, then for the redb store;
  2026-09-29, §5 and §7 split into RFC 0016; 2026-10-03, amended for RFC 0017's mail systems and
  RFC 0012's reserved ids)
- Affects: `system-hub`
- Depends on: RFC 0006 (implemented: the handshake answer send, `PushAuth` parsing in
  `main`), RFC 0011 (the catalog tables, `Source`, generations, `transact`), RFC 0010 (the redb
  store, `LiveStatus`, `/api/storage`)
- **Part of the release that replaces SQLite (owner's decision):** this RFC ships together
  with RFCs 0010, 0011 and 0012 in one release. RFC 0013 (the SQLite import) is Rejected: the
  new hub starts with an empty store. RFC 0011 keeps every system in memory, so the registry
  must be bounded, and this RFC is what bounds push registration. Polled systems are bounded by
  the operator: registering one needs the admin token (RFC 0012).
- Related: RFC 0007 (Implemented; ingestion cost per frame). It shipped first, on SQLite, and
  introduced the `registry unavailable` answer (`Refusal::RegistryUnavailable`), which §4
  reuses for store errors. Split from an earlier, wider draft of RFC 0006. The owner chose the
  default limit, 1000.
- **Mail systems (RFC 0017, Implemented)** are a third source. RFC 0017 asked whether this
  limit should count them: it doesn't (§2), because a mail registration needs a key the operator
  derives for that id. RFC 0012 §3 moved the reserved-id rule into this RFC's registration
  transaction (§3, §4).
- **Split (owner's decision, recorded in RFC 0007's header):** the push system lifecycle
  (former §5) and the agent's id (former §7) moved to RFC 0016, which ships on today's SQLite
  hub before this release. This RFC keeps the registry limit.

## Motivation

1. **Unbounded auto-registration (API4).** Every accepted handshake with an unseen id
   registers a permanent system. The ids are self-asserted, so with `HUB_PUSH_TOKEN` unset
   anyone can fill the registry, and with a token set any token holder can. Under RFC 0011 the
   registry lives in memory, so an unbounded registry is unbounded RAM, not only disk.
2. **The limit is only as good as the agent's id is stable.** An agent that draws a new id
   per restart takes a new slot per restart. RFC 0016 makes the id stable (its §6), and ships
   first.

RFC 0011 already fixes two problems the first revision of this RFC had to handle itself: push
systems are no longer polled (the poller visits only `Source::Poll`; RFC 0016 §3 already stops
it on SQLite), and "push system" is no longer the `url = "push://"` sentinel (it is
`Source::Push`, and `PUT` can't change a system's source).

## Proposed design

### 1. Push system

A **push system** is a system whose source is `Source::Push` (RFC 0011 §2): one registered by a
push handshake. It is never polled. `POST /api/systems` can't create one (0011's `SystemUrl`
refuses `push://`), and `PUT` can't turn a system into one or out of one. A push handshake may
feed only a push system (RFC 0012 §3): an id held by a poll or mail system is a *reserved push
id*.

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

**The Registry's memory, per source.** 0011 §1 budgets about 6 KiB per system at every field's
limit. This limit bounds the push systems; polled systems (admin token) and mail systems (keys
the operator derives) are bounded by the operator, not by a number here. So the Registry's worst
case is **6 KiB × (this limit + the polled and mail systems the operator registers)**: ≤ 60 MB
for 10,000 systems of any mix, and proportionally beyond. `/api/storage` reports the count per
source (`registry: {push, poll, mail}`) and that estimate, so an operator sees the total. A
total cap across sources was considered and not added: the operator already controls the two
uncounted sources one registration at a time.

**Mail systems aren't counted.** A mail report opens only under the key derived from its own
id (RFC 0017 §3), and only the holder of `HUB_MAIL_KEY` derives keys, so every mail system is one
the operator issued a key for: bounded by the operator, as polled systems are by the admin token.
Counting them here would let a full push registry turn away a mail agent the operator set up, for
no bound the key doesn't already give.

### 3. Exact registration: a read for known ids, a transaction for new ones

```rust
/// Fleet Registry (`registry.rs`): the push registration rule, pure.
pub fn decide_push_registration(existing: Existing, limit: PushRegistryLimit) -> Registration;
pub enum Existing {
    Held { source: SourceKind, generation: Generation },
    Absent { push_systems: u32 },
}
pub enum SourceKind { Push, Poll, Mail }
pub enum Registration {
    AlreadyRegistered { generation: Generation },
    /// Write the record, the counters and the generation (the transaction does it).
    Register,
    /// The id is held by another source (RFC 0012 §3).
    Reserved(ReservedSource),
    RegistryFull,
}
/// Fleet Registry. No `Push` variant: a push record is never reserved.
pub enum ReservedSource { Poll, Mail }
```

`decide_push_registration` matches exhaustively: `Held { Push }` → `AlreadyRegistered`; `Held {
Poll }` → `Reserved(Poll)`; `Held { Mail }` → `Reserved(Mail)`; `Absent` → `Register` if
`limit.admits(push_systems)`, else `RegistryFull`. It is the whole rule, table-tested; the
adapter only feeds it.

**Two steps, in the hub's `storage/` adapter, on the blocking pool:**
1. **A read, for every handshake.** `read_catalog` (0010 §9: a redb read transaction, MVCC,
   never waiting for the writer) reads `systems/<key>`. A held id is decided there:
   `AlreadyRegistered`, or `Reserved`; an **absent id with `meta/hub/push_systems` already at
   the limit** (read in the same transaction) is decided `RegistryFull` there too, so only
   "absent with room" reaches the writer. **No commit, no writer work.** A read can be stale only
   across a delete (a source changes only by deletion and re-registration, which gives a new
   generation): a stale `Push` read's generation is fenced (0011 §3: its writes are refused,
   `SystemGone` ends the connection, and the agent reconnects); a stale `Reserved` is retried
   by the agent after 5 s.
2. **A transaction, only for an absent id.** `Store::transact(Commit::Durable, …)` (0010 §9)
   re-reads `systems/<key>` and `meta/hub/push_systems` inside `f` on the writer thread and
   asks `decide_push_registration` again, so the decision is made on the committed state:
   `Register` writes the new `systems/<key>` record, the incremented `meta/hub/push_systems`
   and the incremented `meta/hub/generation`, committed together (`Registered { generation
   }`); `RegistryFull`, or an id registered meanwhile (`AlreadyRegistered`, `Reserved`), writes
   nothing. **An `f` that writes nothing forces no commit.** When it is answered depends on
   what it answers (0010 §6, `Answer::AfterCommit` or `Answer::AtOnce`): an **accepting**
   outcome, `AlreadyRegistered` decided here, may rest on another handshake's uncommitted
   `Register`, so it is answered after the commit that holds the transaction it read; a
   **refusal** (`RegistryFull`, `Reserved`) is answered at once, since one decided on state that
   then fails to commit costs only the agent's 5 s retry. An `AlreadyRegistered` is answered at
   once too when the open transaction held no uncommitted **catalog** write by an earlier `f`
   when `f` ran (appended or staged points don't count: a live hub always has some). So a refusal never
   waits for a commit; a flood of new ids **with room** still reaches the writer's bounded
   channel and waits on it, which RFC 0006's connection limits and `HUB_PUSH_TOKEN` bound, as for
   any push traffic.

- **The limit is exact.** redb has one writer, and 0010 runs every transaction on one thread, so
  two handshakes racing for the last slot are serialised inside step 2, and the second sees the
  incremented count. An id racing itself gets `AlreadyRegistered`, answered only once the
  registration it read has committed; if that commit fails, the store fails stop (0010 §6) and
  both answers are `registry unavailable`. A mail scan or a delete
  committing between step 1 and step 2 is seen by step 2.
- **Cost.** A fleet reconnecting after a hub restart costs one MVCC read per handshake. Only new
  ids reach the writer, and only `Register` commits: durable transactions that arrive while a
  commit is in flight share the next one (the group commit), so the first registration of a
  1,000-agent fleet after the upgrade costs well under the measured ≈ 69 commits a second. A
  refused id, over the limit or held by another source, costs only the step-1 read and nothing
  on the writer.
- **The presence lock** (RFC 0016 §2) is taken only to accept the connection, after step 2,
  never across a store call. RFC 0016 held it across registration because the SQLite mutex
  serialised every store anyway; on redb a registration waits for a commit, and holding the
  presence lock through it would stall every connection's frame claims. RFC 0010 §9's lock order
  is amended to match: **the presence lock comes before the Registry's read lock**; `claim`, the
  end and the sweep read the Registry's current generation while holding the presence lock, so
  the check and the act are one step; the commit hook takes the Registry's write lock and never
  the presence lock. Accept takes currency when no live current connection exists, and
  otherwise a snapshot claim moves it (RFC 0016 §2).
- **Presence carries the generation** (a change to RFC 0016's implemented presence, in this
  release). A delete and re-registration between the read and accept, or later, must fence
  everything the stale connection does, not only its appends: `ConnectionLease` and the
  presence entry carry the generation the connection was accepted for; the pure
  `PushPresence::claim` and `end` take the Registry's `current: Option<Generation>` for the id
  and act only when it equals the connection's; the offline write passes the generation to
  `LiveStatus`, which ignores another (0010 §10). **The live-metrics entry stores the generation
  it was filled for**, and **one generation-checked fill**, `keep_live_metrics`, serves push, mail
  and poll alike: it takes the Registry's read lock, checks the current generation equals the
  frame's, report's or poll's, and only then takes the live-metrics write lock, inside it.
  `LiveMetrics::following` ignores a previous entry of another generation. Eviction compares the
  generation. **The same fence covers `live_applications` and `LiveStatus`.** `SystemApplications`
  carries the generation, and `append_round`'s `on_stored` (RFC 0009 §5, Implemented; amended
  here) splits in two:
  - **record**, still under the admission lock: `recent.remember(round)` and the spent **poll**
    pace are written into the `Arc<Mutex<ApplicationAdmission>>` itself, which therefore holds
    `recent` and the system's poll pace (a push connection's pace stays in its `ConnectionState`
    and is returned by *record*, as `store_round` returns `after` today, so a second connection
    for one id can't spend the honest one's tokens), so `decide` and the record stay one step
    and two overlapping polls, or a poll and a push, can't both store one round (RFC 0009's
    guarantee, pinned by `round_intake.rs::overlapping_polls_of_one_system_share_one_pace`).
    *Record* returns an **ordinal** (a counter in the admission, incremented per recorded
    round), and `SystemApplications::shown` carries it: *show* replaces `shown` only with a
    higher ordinal, so two overlapping polls whose *show* steps run out of order can't show the
    older round. An **empty round** is recorded and shown like any other (0010 §10);
  - **show**, after the admission is released: the Registry read lock, the generation check, and
    **while still holding it** the `live_applications` write lock, to set `shown`; the Registry
    guard is held across the insert, so a delete's hook can't run between the check and the
    insert.
  The admission is obtained the same way: `append_round` takes the Registry read lock, checks the
  generation, and under it clones or **creates the entry at that generation** (replacing one of
  another generation, whose `recent` would otherwise mark a re-registered agent's re-sent round a
  duplicate); it never calls `entry().or_default()` for an id the Registry doesn't hold.
  `collector/application_poll.rs::log_refused_round`'s hourly throttle moves out of
  `live_applications`: `poll_every`'s loop (the one long-lived poller; a `poll_system` task lives
  one tick) owns a `HashMap<SystemId, Instant>` of warned-at instants, pruned each tick to the
  tick's `polled_systems` (so it is bounded by the registry and drops deleted ids) and handed to
  each task behind an `Arc<Mutex<_>>`; it creates no `live_applications` entry.
  `forget_shown_round` clears `shown` by id and creates nothing; a stale poll clearing a
  re-registered id's `shown` is restored by the next poll. A `LiveStatus` write for an id with no
  entry is dropped, since entries are created only by the registration hook, which replaces an
  entry of another generation; the delete's hook removes only the deleted generation's entries,
  and nothing removes entries by id after the transaction (0011 §3, amended). So a fill or a
  round racing a delete can't leave an orphan for any source, and a stale end evicts nothing. **The lock order** is 0010 §9's, which lists every path that
  holds two hub locks; the commit hook takes only the Registry's write lock while holding any.
  No test can show it; review must. The delete's hook evicts, by generation;
  `routes/api.rs::delete_system`'s eviction by id after the delete goes. **An end always removes its own presence entry** when its connection number
  matches, whatever the generation; only its side effects (the offline write, the eviction) are
  generation-gated, so a deleted id leaves no dead entry. A connection whose append answered
  `SystemGone` ends with no side effect beyond that.
- **The disconnection sweep's grace** (RFC 0016 §4, 120 s from the store's open) counts, for a
  system registered after open, from its registration, which the commit hook records in the
  Registry's `System` as `registered: Registered { BeforeOpen, At(Instant) }` (in memory only;
  every system loaded at open is `BeforeOpen`, so nothing persisted needs a placeholder; it goes
  with the system on delete). RFC 0011 §2's `System` gains the field.
  So a new push system can't be marked offline in the moment between its registration's commit
  and its accept.
- RFC 0011's delete transaction decrements `meta/hub/push_systems` when it deletes a push system.
- **At open, the adapter recomputes the counter** from `systems` in one transaction and corrects
  it, with a `warn` if it differed, as defence in depth. The store doesn't know what a push
  system is; the adapter does.
- `transact` and `read_catalog` block, so the async handshake calls them through
  `spawn_blocking` (the existing `on_blocking_pool`). `f` runs on the writer thread, and a panic
  there is 0010's writer fail-stop, which ends the hub. The blocking unit also runs adapter code
  of its own (decoding the read, the decision, accept); a panic there, or runtime shutdown,
  is a `JoinError`, which maps to `registry unavailable` (`RegistryFailure::Panicked`), as today.

### 4. Handshake outcomes

The push handshake checks, in order: the shape, the token (constant time), the id's `SystemId`
rule (RFC 0005), then registration (§3), which decides the reserved-id rule (RFC 0012 §3) on the
catalog's committed state: by its read for a held id, inside its transaction for a new one. Nothing else; there
is no import in progress to refuse (RFC 0013 is Rejected).

`Refusal` (RFC 0006: `Rejected(HandshakeRejection)` and `Timeout`; RFC 0007:
`RegistryUnavailable`; RFC 0017: `TransportMismatch`, widened by RFC 0012 to `{ id, held:
ReservedSource }`) gains one variant, `RegistryFull`. Store errors reuse RFC 0007's
`registry unavailable`:

| Outcome | Answer |
|---|---|
| `AlreadyRegistered` | accepted, as today |
| `Registered` (after `Register` committed) | accepted |
| `Reserved(Poll)`, `Reserved(Mail)` | `auth_error` / `transport mismatch` (`Refusal::TransportMismatch`, RFC 0017's answer, RFC 0012 §3); logged and counted as RFC 0012 §3 says |
| `RegistryFull` | `auth_error` / `registry full` (`Refusal::RegistryFull`) |
| a `StoreError` (`Closed` during shutdown, `Failed`, `Io`) | `auth_error` / `registry unavailable` (RFC 0007's `Refusal::RegistryUnavailable`), so the agent takes its 5 s backoff instead of reconnecting at once |

Logging (A09):
- the first `RegistryFull` is logged at `warn`, then **again every hour while refusals go on**,
  with the count of refusals since the last line. The hourly slot is **cleared by a
  `Registered` outcome or a push system's delete**, so a registry that drains and fills again
  within the hour logs the refill at once;
- a store error is logged at `error` at most once per hour, with the error kind and no id.
  **This replaces today's line**: `Refusal::log` logs every `RegistryUnavailable` at `warn`, once
  per handshake, with the id (`push/mod.rs`), which a store outage under a retrying fleet would
  turn into hundreds of lines a second. `RegistryFailure { Database(rusqlite::Error), Panicked
  }` becomes `RegistryFailure { Store(StoreError), Panicked }`, `Panicked` from a `JoinError`
  (a panic in the registration unit's own code, or runtime shutdown, §3). `on_blocking_pool`'s
  per-call `error!` with the id is routed, for registration, through the same hourly id-free
  line;
- `RegistryFull` and `registry unavailable` refusals never log the id. RFC 0012 §3's hourly
  `transport mismatch` line, per id, is the one per-id refusal line, and it is rate-limited;
- each refusal is counted in `/api/storage` (`push_registry: {limit, count, refused_full,
  refused_unavailable}`; `transport mismatch` is RFC 0012's `push_transport_mismatches`).

### 5. Lifecycle: RFC 0016

Moved to RFC 0016, which ships it on SQLite: see its §2 for which connection is current and
how currency moves, and its §4 for the disconnection sweep; offline marking is by the current
connection's end only. RFC 0010 §10 carries it into
`LiveStatus`. This RFC relies on it only in that "delete offline" (RFC 0012) then sees a push
system as offline when its current connection has ended, or it has none.

### 6. Where the limit holds

Everywhere. `POST /api/systems` needs the admin token and can't create a push system, and `PUT`
can't change a system's source (RFCs 0011, 0012). So the only way to add a push system is a
push handshake, and every handshake goes through §3.

### 7. The agent's id: RFC 0016

Moved to RFC 0016 §6: the id is resolved once, in the agent's synchronous `start`, from
`SYSTEM_AGENT_ID_FILE`, the machine id, the host name (no `hostname` binary) or a random UUID,
under the hub's `SystemId` rule, and `SYSTEM_AGENT_ID_FILE`, when set, keeps it across
re-creations.

## Domain impact

- **Fleet Registry:** `PushRegistryLimit`, the pure `decide_push_registration` with `Existing`,
  `SourceKind`, `Registration` and `ReservedSource`, and the `meta/hub/push_systems` counter.
  The `storage/` adapter only feeds the rule: its read and its transaction.
- **Ingestion:** `Refusal::RegistryFull`, and store errors mapped to RFC 0007's
  `Refusal::RegistryUnavailable` (`RegistryFailure::Store`); `Registration::Reserved` mapped to RFC 0017's
  `Refusal::TransportMismatch`; the handshake order stated in §4.
  **Registration never aborts**: a step-2 refusal is `Ok((refusal, Answer::AtOnce))`, so its
  `transact` is instantiated with `TransactError<Infallible>` (0010 §9) and `RegistryFailure` has
  no abort arm to invent.
- **Glossary:**
  - **push system** (RFC 0016's term): its source becomes `Source::Push` (RFC 0011);
  - **push registry limit**: the most push systems the hub will auto-register; mail systems
    aren't counted;
  - **reserved push id** (RFC 0012's term): decided in the registration transaction.
- **Published contracts:** the push frame is untouched. The handshake gains one `auth_error`
  message, `registry full`, and answers store errors with RFC 0007's `registry unavailable`.
  Mixed-version fleets:
  - *any agent*: every `auth_error` gets the same 5 s backoff;
  - *agents older than RFC 0016 without a stable id source*: a new UUID per restart until
    upgraded, so churn can fill the limit; the hourly `warn` and "delete offline" handle it;
  - *old hub*: ignores `HUB_MAX_PUSH_SYSTEMS`; *old agent*: unchanged behaviour.

## Alternatives considered

- **Unlimited by default.** No change on upgrade, but unbounded RAM under RFC 0011. The owner
  chose 1000.
- **A limit up to 1,000,000** (the previous revision). RFC 0011's memory budget holds for 10,000.
- **Count push systems by scanning the registry.** O(n) per registration on the writer thread. A
  counter in the same transaction is exact and O(1).
- **One total cap on every system** (RFC 0011). Polled systems are already bounded by the admin
  token; the limit targets the self-asserted path.
- **Pre-registration only.** The strongest control, but it needs an API that accepts a chosen
  id, and belongs with per-system push credentials.
- **Expire stale push systems automatically.** Recorded as an open question: automatic deletion
  of history needs an owner decision of its own.

## Security implications

- **API4:** push registration is bounded, exactly, at 10,000 at most, and REST can't bypass it
  (§6). The Registry's memory is bounded per source (§2), and its total is shown in
  `/api/storage`. A refused handshake costs an MVCC read and nothing on the writer (§3), so
  refusals can't load the commit path.
- **A04 / API6: onboarding lock-out, accepted.** Registration is permanent and happens at the
  handshake. Anyone who can push (every token holder, or anyone while the token is unset) can
  fill the registry with 1000 handshakes. New and recreated hosts then get `registry full`, and
  the attacker refills freed slots faster than agents retry. The remedies: set
  `HUB_PUSH_TOKEN`; raise the limit; expose `/api/push` only to the fleet's networks. The trade
  is accepted: an unbounded registry costs every operator, while the lock-out needs an attacker
  who already has push access.
- **A05 / API8:** a malformed or out-of-range limit refuses startup.
- **A07 / API2:** the `registry full` and `registry unavailable` answers come after
  authentication. The registry's fullness is **not** a secret, though: `/api/storage` is an open
  read and shows `push_registry`'s limit and count, as `GET /api/systems` already shows every
  system. Nothing here reveals the token.
- **A09:** an hourly `warn` while refusals go on, its slot cleared by a registration or a
  delete; store errors logged at `error` once per hour, replacing today's per-handshake `warn`;
  the configuration error names the variable, never its value; no `registry full` or
  `registry unavailable` refusal logs an id (RFC 0012's hourly `transport mismatch` line is the
  one per-id line).
- **A03:** no SQL.
- **API1:** push ids stay self-asserted. The reserved-id rule, decided on committed state (a
  read for a held id, the transaction for a new one), keeps a push connection out of poll and
  mail systems; a stale read is fenced by generations.
- **API4 (mail):** mail registrations are bounded by the keys the operator derives, not by this
  limit (§2).
- **API9:** `HUB_MAX_PUSH_SYSTEMS` and the `registry full` message go into the README; the
  README's existing `registry unavailable` row (RFC 0007: "a database error, or a registration
  that panicked") is reworded for store errors; `/api/storage`'s `push_registry` and `registry`
  fields are documented.
- Everything else is unchanged.

## Testing plan

TDD, per `CLAUDE.md`, with `red-test-adversary` and `rosette-auditor`.

- **Pure:**
  - `PushRegistryLimit::from_env` over unset, empty, `1`, `1000`, `10000`, `10001`, `0`, `-1`,
    `1,000`, `18446744073709551616` and non-UTF-8, each with its outcome;
  - `admits` below the limit, at it, and above it;
  - `decide_push_registration` as a table over `Held { Push | Poll | Mail }` and `Absent` ×
    {room, exactly full}, each with its `Registration`;
  - `PushPresence::claim` and `end` as tables over {current generation equal, stale, id absent}
    × {connection number current, not current};
  - the sweep's grace as a table over {registered before open, after open} × {119 s, 120 s}, and
    a fenced connection (a presence entry of a stale generation, still claimed) counted as no
    connection;
  - `LiveMetrics::following` with a previous entry of another generation: nothing carried over;
  - with points appended and the record already committed (handshake seam), a step-2
    `AlreadyRegistered` is answered before the next group commit;
  - **Registration-time rows that need seams** are under *Registration* below.
- **Call-site follow-through** (released with authority author, quoted in the change summary):
  the `presence.rs`, `push/sweep.rs` and `push/ingest.rs` tests in `HEAD` whose calls gain the
  generation argument keep every expectation.
- **Registration** (a temporary store, with a **writer gate**: a test seam in `StoreOptions`
  that holds the writer before it runs the next submitted transaction until the test releases
  it, so two transactions are ordered deterministically):
  - a limit of 1 with one polled system present admits an unseen push id; a limit of 1 with
    one mail system present admits one too (mail isn't counted);
  - a push handshake for a poll id and for a mail id answers `Reserved(Poll)` and
    `Reserved(Mail)` from the read, submits no transaction (the writer gate sees none) and
    leaves the counter as it was;
  - a mail scan and a push handshake for the same new id, both submitted behind the gate, **in
    both orders**: mail first → one mail record, the push answer `Reserved(Mail)` from step 2;
    push first → one push record, the mail report `TransportMismatch`;
  - a limit of 1 with one push system present refuses an unseen id from the step-1 read: **the
    gate's submission count stays at zero** and nothing is written;
  - a known push id answers `AlreadyRegistered { generation }` with the stored generation, from
    step 1, with no submission;
  - two handshakes for different new ids racing for the last slot, both submitted behind the
    gate before either runs: exactly one `Registered`, and a hook in `f` asserting the second
    `f` read the first one's increment (the check-then-insert regression would read the same
    count);
  - a stale `Push` read, through a **handshake seam** between `read_catalog` and accept: the id
    deleted and re-registered by mail at the seam; the connection's first frame is refused
    `SystemGone`, and its end leaves the mail system's `LiveStatus`, live metrics and presence
    unchanged; the same with the delete after `auth_ok`;
  - push to push: connection A (generation 1) still open while the id is deleted and B
    registers generation 2 and claims; A's last frame claims nothing, and A's end neither marks
    B offline nor evicts B's live metrics;
  - **A ends idle while B (generation 2) is accepted but hasn't claimed**, so A's number is
    still current: A's end removes its entry and marks nothing offline and evicts nothing;
  - a delete between a frame's append and its live-metrics fill (a **frame seam** between the
    append and the fill) leaves no live-metrics entry; the same for a mail report's and a poll's
    fill; a later registration of the id by another source starts without the old entry;
  - a deleted id with an open connection leaves no presence entry after that connection ends;
  - two rounds of one system through a **round seam** placed after the admission is released
    (between *record* and *show*): the second is `Duplicate` or `TooSoon`, never stored twice;
    `overlapping_polls_of_one_system_share_one_pace` stays deterministic (its empty rounds are
    `Stored`, as today); two rounds whose *show* steps run in reverse order through the seam
    leave `shown` on the newer; an empty round is `Stored` and replaces `shown`; a refused
    round on two consecutive ticks logs one `warn` and one `debug`;
  - a delete through a seam **inside *show***, between the generation check and the insert: the
    delete's hook waits for the Registry guard, and no orphan entry exists afterwards;
  - a delete between a round's `append` and *show*: no `live_applications` entry; a later
    registration of the id stores its agent's re-sent round (a fresh admission at the new
    generation); a refused poll round across a delete creates no entry;
  - a late `Online` status write after a delete creates no entry; a delete followed by a
    re-registration committed before the delete's caller resumes keeps generation 2's
    `LiveStatus`;
  - a refusal (`RegistryFull`, `Reserved` from step 2) is answered while a `Batched` **catalog**
    write (a system-info fill) is pending, before the next commit; an `AlreadyRegistered` from
    step 2 waits for it; the gate counts submitted transactions, and every wait on an answer has
    a timeout, so a cheat that routes a refusal through step 2 fails rather than hangs;
  - two handshakes for one new id behind the gate with an injected commit failure: neither is
    accepted, both answer `registry unavailable`;
  - a sweep pass with `up_for` past `RECONNECT_GRACE`, between a new id's registration commit
    and its accept, doesn't mark it offline;
  - a handshake holds no presence lock across a store call (a test that blocks the writer gate
    while another connection's frame claim completes);
  - deleting a push system decrements the counter; a counter written wrong through a test seam
    is corrected at open, with a `warn`;
  - a child process killed right after a registration answered: at reopen, the record, the
    counter and the generation are all there; killed before the commit, none is.
- **Real server:**
  - a full registry answers `registry full`, and logs one `warn`, then another after an hour of
    injected time; a delete then a refill within the hour logs the refill at once;
  - a registration unit that panics through a seam answers `registry unavailable` (today's
    `push/mod.rs::a_registration_that_panics_answers_registry_unavailable`, kept, its panic moved
    from the poisoned database mutex to the seam: a call-site follow-through released with
    authority author). `push/ingest.rs::registration_panics_on_a_poisoned_database_mutex` pins
    a mutex this release removes; retiring it is a contract change, released only with the
    owner's authority when implemented, quoted in the change summary;
  - a store closed through a test seam answers `registry unavailable`; two store errors within
    an hour log one `error` line without an id; `refused_full` and `refused_unavailable` reach
    `/api/storage`, and so do the per-source counts.

## Impact on `docs/ARCHITECTURE.md`

- § Components: push registration is bounded.
- § Domain model: the Fleet Registry row (`decide_push_registration`, `ReservedSource`) and
  the Ingestion row, and the glossary (**push registry limit**; **mail system**'s line that mail
  registrations are unbounded "as push's are" becomes "bounded by the keys the operator
  derives, uncounted by the push limit").
- § Components (`system-hub`): the presence lock is no longer held across registration; leases,
  presence entries, live-metrics and live-applications entries carry the generation; the
  lock-order sentence ("the presence lock, then the database mutex, then the live state", also
  in `state.rs`'s `AppState` comments) becomes 0010 §9's order.
- `rfcs/0016-current-push-connection.md` (Implemented) gains a header note: "§2 and §4 amended by
  RFC 0008 (generation-fenced currency, sweep grace from registration)".
- `rfcs/0009-spring-boot-application-telemetry.md` (Implemented) gains a header note: "§5's
  `on_stored` is split by RFC 0008 §3 into *record* (under the admission) and *show* (under the
  Registry), and §8's delete-then-evict order is superseded by the generation fence"; its
  guarantee is unchanged. `routes/api.rs::delete_system`'s eviction by id is removed.
- RFC 0011 §3's eviction sentence (by id, after the transaction) becomes the delete hook's, by
  generation.
- § Domain model, glossary: **push handshake**'s refusals become rejection, timeout, registry
  unavailable, registry full and transport mismatch; **current connection** fenced by
  generation.
- § Trust boundaries, Agent → Hub (push): the limit, the handshake order, and that the limit
  holds everywhere.
- § Open architectural questions:
  - removed: unbounded auto-registration (RFC 0016 has already removed push systems being
    polled, the stale connection's offline marking and the `hostname` shell-out);
  - added: expiring stale push systems;
  - the mail item saying "Mail registrations are unbounded, as push's are (RFC 0008)" is
    corrected the same way.
- `README.md`: the `HUB_MAX_PUSH_SYSTEMS` row; `registry full` in
  the `auth_error` table, beside RFC 0007's `registry unavailable` (reworded for store errors); "they appear
  automatically" (up to the limit).
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
- Agents should run RFC 0016's id (shipped before this release) so a restart doesn't take a new
  slot; `SYSTEM_AGENT_ID_FILE` on a volume keeps the id across container re-creations.

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

**Mail and reserved-id amendment (2026-10-03).** RFC 0017 (Implemented) added mail systems, and
RFC 0012's fourth pass found that §3 step 1 answered `AlreadyRegistered` "whatever its source"
after a reserved-id check that ran before the transaction: a mail scan registering the id in
between would let a push connection write into a mail system, which 0017 closed. Changed: §3
step 1 answers `Reserved(ReservedSource)` for a poll or mail record inside the transaction;
§4's order and table gain it as RFC 0017's `transport mismatch`; mail systems aren't counted
(§2); Domain impact, Security and Testing follow.

`rfc-adversary`, first pass on the redb revision with the mail amendment. Every finding was
acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| every handshake, refused or known, forced a durable commit while holding the presence lock: 500 agents over the limit would exceed the commit rate and stall every frame claim | CONFIRMED | a held id decided by an MVCC read with no writer work; only an absent id reaches the transaction; an `f` that writes nothing forces no commit; the presence lock only for accept, after registration (§3) |
| the Registry's memory budget isn't a stated number once poll and mail are uncounted | CONFIRMED | stated per source (6 KiB × systems), counts and estimate in `/api/storage`; a total cap considered and not added (§2) |
| logging claims contradict today's per-handshake `warn` with the id | CONFIRMED | the change to `Refusal::log` and `RegistryFailure` named; A09 narrowed; test rows (§4) |
| the reserved-id rule is a business decision inside the adapter's closure | CONFIRMED | the pure `decide_push_registration`, table-tested; `ReservedSource` in the Fleet Registry (§3, Domain impact) |
| the race rows can't be ordered; the `admits` hook was lost | CONFIRMED / PLAUSIBLE | a writer-gate test seam; both orders asserted; the hook restored (Testing plan) |
| the hourly `RegistryFull` slot has no reset | PLAUSIBLE | cleared by a registration or a push system's delete; a test row (§4) |
| inventory: ARCHITECTURE's mail line and glossary, the README `registry unavailable` row, RFC 0007's status | CONFIRMED | listed; RFC 0007 is Implemented (header, API9, Impact) |

Came closest and survived: minting unbounded mail ids with one key (each key opens only its
own id's reports).

`rfc-adversary`, second pass (the first pass's resolutions). Every finding was acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| a write-free `f` answered at once can accept a connection on another handshake's uncommitted registration, which a failed commit then loses | CONFIRMED | answered after the commit holding the transaction it read, unless that transaction had no writes; a commit-failure row (§3; 0010 §6) |
| presence entries and live-metrics eviction are keyed by id, so a fenced connection's end acts on the new holder | CONFIRMED | the generation in the lease, the presence entry, the end's offline write and the live-metrics entry; an end after `SystemGone` has no effect; rows (§3) |
| the stale-read row can't be staged by the writer gate; the hook assertion can't hold | CONFIRMED | a handshake seam between read and accept; the hook reworded (Testing plan) |
| the sweep can mark a new system offline between registration and accept | PLAUSIBLE | the sweep's grace counts from registration for systems registered after open; "currency moves only by claims" corrected (§3) |
| `JoinError` overclaimed; two tests in `HEAD` silently retired | CONFIRMED / PLAUSIBLE | the claim corrected; one test kept through a seam (author release), the mutex test's retirement named for the owner's release (§3, Testing plan) |
| inventory: the push handshake glossary row; 0010 §9's lock order | CONFIRMED | listed; 0010 §9 amended in the same change (Impact) |

Came closest and survived: a flood of fresh-id handshakes with the token unset through the
shared writer channel (two point reads per `f`, no commit, bounded channel back-pressure).

`rfc-adversary`, third pass (the second pass's resolutions). Every finding was acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| waiting for the commit on every write-free outcome parked a blocking thread per refused handshake: a new-id flood could exhaust the pool | CONFIRMED | only an accepting outcome waits; refusals are answered at once (§3; 0010 §6) |
| the live-metrics fill wasn't generation-checked: a fill racing a delete left an orphan a later mail system inherited | CONFIRMED | the fill checks the current generation under the live-metrics lock; the delete evicts after its hook (§3) |
| the generation check and act weren't atomic; a registration-time map in presence could deadlock with the hook | CONFIRMED / PLAUSIBLE | presence before Registry; checks under the presence lock; the hook never takes it; `registered_at` in the Registry's `System` (§3; 0010 §9) |
| 0010 §10 and RFC 0016 still state the old grace and currency rules | CONFIRMED | 0010 §10 amended; a header note in 0016 (Impact) |
| test rows could pass without the behaviour; follow-through releases unnamed | PLAUSIBLE | `up_for` past grace; the idle-end row; pure tables; releases named (Testing plan) |
| `on_blocking_pool` still logged the id per panic; §3 and §4 disagreed on `Panicked` | CONFIRMED (low) | routed through the hourly id-free line; §4 matches §3 |
| an end gated by generation left dead presence entries | PLAUSIBLE (low) | an end always removes its own entry; only side effects are gated (§3) |

Came closest and survived: generation-gating the end (today's connection-number check and
`LiveStatus`'s generation filter already cover every case but the unclaimed-B window, now a row).

`rfc-adversary`, fourth pass (the third pass's resolutions). Every finding was acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| the fill took `live_metrics` then the Registry, the reverse of every other path: a writer-thread deadlock with a queued hook | CONFIRMED | Registry read lock first, then `live_metrics`; the full order stated here, in 0010 §9 and ARCHITECTURE (§3) |
| a new-id flood over the limit still went through the writer's channel | PLAUSIBLE | `RegistryFull` decided in step 1's read too; the claim narrowed (§3) |
| only the push fill was generation-checked | CONFIRMED (low) | one generation-checked `keep_live_metrics` for every source; `following` ignores another generation (§3) |
| `registered_at` existed nowhere else and needed a placeholder at open | CONFIRMED (low) | `Registered { BeforeOpen, At(Instant) }` in 0011 §2's `System` (§3) |
| 0010 §9's `transact` signature couldn't express the answer timing | CONFIRMED (low) | the signature carries `Answer`; `Abort` answered at once (0010 §9) |
| an `AlreadyRegistered` on committed state waited for the next commit | PLAUSIBLE (low) | answered at once when the open transaction held no uncommitted write (§3) |
| the append-to-fill row had no seam; lock order untested | PLAUSIBLE | a frame seam; lock order a review item (Testing plan, §3) |

Came closest and survived: generation fencing between a stale and a re-registered connection.

`rfc-adversary`, fifth pass. Every finding was acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| the generation fence didn't cover `live_applications` or a status write for a removed entry | CONFIRMED (medium) | `SystemApplications` carries the generation; `on_stored` fenced after releasing the admission lock; status writes for absent entries dropped (§3; 0010 §10) |
| the Cost and API4 sentences and a test row still described step-2 `RegistryFull` | CONFIRMED (low) | corrected; the row asserts no submitted transaction (§3, Security, Testing) |
| `transact` couldn't return an abort; the admission-lock sentence contradicted itself | CONFIRMED (low) | `TransactError { Aborted, Store }`; the admission exception stated (0010 §9) |
| ARCHITECTURE's and `state.rs`'s lock-order text not in the inventory | CONFIRMED (low) | listed (Impact) |
| paths that could invert the order (sweeps, candidate lists, recursive Registry reads) | PLAUSIBLE | 0010 §9 lists every nested path and forbids holding the Registry guard while taking presence or re-taking it |
| "uncommitted write" undefined | PLAUSIBLE (low) | catalog writes only; a row for the at-once branch (§3, Testing) |
| no rows for `following` and a stale presence entry in the sweep | PLAUSIBLE (low) | added (Testing) |

Came closest and survived: the Registry lagging redb between a commit and its hook (`transact`
answers after the hook, and the next frame heals the window).

`rfc-adversary`, sixth pass. Every finding was acted on, in the single round of 2026-10-03
that closed the session's open findings across 0008, 0010 and 0011:

| Finding | Verdict | Resolution |
|---|---|---|
| releasing the admission lock before `on_stored` let overlapping polls store one round twice | CONFIRMED (medium) | `on_stored` split: *record* under the admission (which now holds `recent` and the paces), *show* under the Registry; RFC 0009 noted (§3, Impact) |
| the Registry guard must be held across the `live_applications` insert | CONFIRMED | stated; a seam inside *show* (§3, Testing; 0010 §9) |
| `append_round`'s admission clone and `log_refused_round` created entries unchecked | CONFIRMED | the admission created at the checked generation, replacing another's; the throttle in the poll task (§3) |
| 0011 §3 evicted by id after the transaction, wiping a re-registered generation | CONFIRMED | the delete hook evicts by generation only; the registration hook replaces (§3; 0011 §3) |
| `Abort` undefined; no arm for `TransactError::Aborted` in registration | CONFIRMED (low) | registration never aborts, `TransactError<Infallible>`; `Abort` defined in 0010 §9 (§4) |
| the writer gate couldn't count submissions; merged and contradictory rows; a misfiled row | PLAUSIBLE | a submission counter and timeouts; rows split and reworded; the round rows under Registration (Testing) |

Came closest and survived: the at-once `AlreadyRegistered` (only the id's own uncommitted
`Register` can produce one, and that is a catalog write, so the wait applies).

`rfc-adversary`, seventh pass. Every finding was acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| 0010 §10's "no point accepted → `NotStored`" dropped empty rounds, which the pinned tests send and an agent sends when its last application goes | CONFIRMED (medium) | a round *with points* none accepted is `NotStored`; an empty round is `Stored` and shown (0010 §10; Testing) |
| *show* out from under the admission let an older round overwrite `shown` | CONFIRMED | *record* returns an ordinal; *show* replaces only a lower one; a row (§3) |
| the throttle "in the poll task" lived one tick | CONFIRMED (low) | in `poll_every`'s loop, keyed by id, pruned to the tick's systems (§3) |
| `transact` used `A` undeclared | CONFIRMED (low) | `transact<T: Send, A: Send>` (0010 §9) |
| "the paces" could mean the push pace in the admission | PLAUSIBLE (low) | the poll pace only; the push pace stays per connection (§3) |
| inventory: line 208, `delete_system`'s eviction, 0009 §8, `forget_shown_round` | PLAUSIBLE (low) | reworded and listed (§3, Impact) |

**Still open**: nothing CONFIRMED.

**Split into RFC 0016 (2026-09-29).** §5 and §7 moved to RFC 0016, with the findings above
that concerned them (the stale connection, the offline backstop, the agent's id). RFC 0016 also
revises §7: a set `SYSTEM_AGENT_ID_FILE` that is missing captures the id resolved from the other
sources, since on Linux the host name always answers and the UUID step is practically never
reached.
