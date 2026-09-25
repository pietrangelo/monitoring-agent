# RFC 0008: Push Registry Limit and Push System Lifecycle

- Status: Draft
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-25
- Affects: `system-hub`
- Depends on: RFC 0006 (the handshake answer send, `PushAuth` parsing in `main`)
- Related: RFC 0007. Split from an earlier, wider draft of RFC 0006 (see git history). This RFC
  keeps the part that belongs to the Fleet Registry. The owner chose the default limit, 1000.

## Motivation

1. **Unbounded auto-registration (API4).** Every accepted handshake with an unseen id inserts
   a permanent, enabled `systems` row. The ids are self-asserted, so with the token unset
   anyone can fill the registry, and with a token set any token holder can.
2. **Push systems are polled.** The poller visits every enabled row every 30 s, including push
   systems. Their url is `push://`, which reqwest rejects, so each poll marks a live push
   system offline until its next frame. So:
   - the dashboard shows push systems flapping between online and offline;
   - "delete offline" can delete a live push system, with its history;
   - a push system that died goes offline only through this accident.
3. **Some exits leave a push system online.** A system that registers and then fails before
   its first frame returns early without being marked offline, and so does a registration
   whose blocking task panics (`JoinError`). The accidental polling in 2 is the only thing
   that marks it offline later. The same holds after a hub restart, since the hub has no
   graceful shutdown.
4. **"Push system" is a sentinel.** The literal `"push://"` in `register_if_new` is the only
   thing that tells a push system apart, and `PUT /api/systems/:id` can rewrite it.

## Proposed design

### 1. Push system

A **push system** is a system registered from a push handshake. It is recognised by the
constant `PUSH_SYSTEM_URL = "push://"`, which replaces the literal. (A separate column recording
how a system was registered is the cleaner model, but it needs a schema change: see
Alternatives.)

### 2. Registry limit

```rust
/// The most push systems the hub will auto-register. Never zero.
pub struct PushRegistryLimit(NonZeroUsize);

impl PushRegistryLimit {
    pub const DEFAULT: usize = 1000;
    pub fn from_env(value: Result<String, VarError>) -> Result<Self, PushRegistryLimitError>;
    /// Whether a registry holding `push_systems` push systems may register one more.
    pub fn admits(&self, push_systems: usize) -> bool;
}

pub enum PushRegistryLimitError { NotUnicode, Zero, OutOfRange, NotANumber }
```

`HUB_MAX_PUSH_SYSTEMS` is parsed in `main` together with `HUB_PUSH_TOKEN` (RFC 0006), before
`Database::new` and `start_collectors`:

| Value | Outcome |
|---|---|
| unset or empty | the default, 1000 |
| a whole number from 1 to `usize::MAX` | that limit |
| not UTF-8, `0`, negative or overflowing (`OutOfRange`), or anything else (`NotANumber`) | the hub refuses to start, logging the variable's name and the reason, never the value |

### 3. Exact registration

```rust
pub enum Registration { AlreadyRegistered, Registered, RegistryFull }

pub fn register_push_system(          // on Database
    &self,
    system: &SystemInfo,
    admits: impl FnOnce(usize) -> bool,
) -> Result<Registration, rusqlite::Error>;
```

- It holds the single connection guard across three steps: the existence check,
  `SELECT COUNT(*) FROM systems WHERE url = ?1` (with `PUSH_SYSTEM_URL` bound), and the plain
  `INSERT`. It calls `admits` between the count and the insert.
- It must not call `get_system` or `insert_system`: a std `Mutex` isn't reentrant, and
  `insert_system`'s `INSERT OR REPLACE` would cascade-delete history.
- The rule stays in `admits`, not in SQL.
- The limit is exact, and an id racing itself gets `AlreadyRegistered`.
- `on_blocking_pool` becomes generic over its return value, to carry the `Registration` back.

### 4. Handshake outcomes

- A known id is unaffected.
- An unseen id beyond the limit is answered `auth_error` / `registry full`.
- A database error, or a `JoinError` from the blocking task, is answered `auth_error` /
  `registry unavailable`, so the agent takes its 5 s backoff. Today there's no answer, and the
  agent reconnects at once.
- Logging: the first refusal after the registry fills is logged at `warn`, once, until there is
  room again (an `AtomicBool` in the app state). Each refused id is logged at `debug`, in
  `Debug` form.

### 5. Lifecycle

- **Not polled.** The poller's selection of systems to poll becomes a pure function that skips
  rows whose url is `PUSH_SYSTEM_URL`. `start_collectors` calls it on every tick.
- **Offline at startup.** No push connection survives a restart, so `main` marks every push
  system offline before serving. Each is marked online again by its first frame.
- **Offline on every exit.** Every path that leaves `handle_push` after registration marks the
  system offline. That includes a failed or timed-out `auth_ok` (RFC 0006), a `JoinError`, and
  the paths of RFC 0007. RFC 0007 also evicts the live entry.
- **"Delete offline"** therefore deletes only systems that really are offline. Deleting a push
  system that is still connected is undone within seconds: RFC 0007 ends its connection, and
  the agent registers again.

### 6. Where the limit holds

The limit counts push systems. `POST /api/systems` stays an unbounded registration path, and
`PUT /api/systems/:id` can rewrite a push system's url so that it stops counting. The hub has
no client authentication, so the limit **holds only where push clients can't reach the REST
API**. `README.md` § Deployment patterns gains an nginx example that exposes `/api/push` to
agent networks and serves the rest only to operators. It also says the limit is advisory
wherever the REST API is reachable.

## Domain impact

- **Fleet Registry:**
  - `PUSH_SYSTEM_URL`, `PushRegistryLimit` and `register_push_system`;
  - the poll selection rule;
  - startup offline marking.
- **Ingestion:** the handshake's `Refusal` gains `RegistryFull` and `RegistryUnavailable`.
- **Glossary:**
  - adds **push system**: a system registered from a push handshake, with url `push://`. It is
    never polled, and the hub marks it offline at startup and whenever its connection ends;
  - adds **push registry limit**: the most push systems the hub will auto-register.
- **Published contracts:** the push frame is untouched. The handshake gains two `auth_error`
  messages, `registry full` and `registry unavailable`. Mixed-version fleets:
  - *Any agent:* every `auth_error` gets the same 5 s backoff.
  - *Agents whose id changes* (a recreated container without `/etc/machine-id` falls back to
    its hostname) leave an offline push system behind at each change. Nothing expires those
    rows, so churn fills the limit over time. The registry-full `warn` and "delete offline"
    handle it. Expiring stale push systems is recorded as an open question.
  - *Old hub:* ignores `HUB_MAX_PUSH_SYSTEMS`.

## Alternatives considered

- **Unlimited by default.** No change on upgrade, but it stays unbounded. The owner chose
  1000.
- **The registry rule as one SQL statement.** It puts a business rule in SQL, and it can't
  tell "full" from "registered meanwhile".
- **A registration-origin column.** Cleaner than a url marker, but it needs a schema change
  with a backfill. It waits for a schema RFC that needs the column anyway.
- **Pre-registration only.** This is the strongest control, but it needs an API that accepts a
  chosen id. It belongs with per-system push credentials.
- **Register on the first frame instead of the handshake.** It filters out handshake-only
  probes, but it isn't a bound.

## Security implications

- **API4:** push registration is bounded. `POST /api/systems` is not (see §6).
- **A04 / API6: onboarding lock-out, accepted.** Registration is permanent and happens before
  `auth_ok`. So anyone who can push (every token holder, or anyone while the token is unset)
  can fill the registry with 1000 handshakes. After that, new and recreated hosts get
  `registry full`, and the attacker refills freed slots faster than agents retry. The
  remedies:
  - set `HUB_PUSH_TOKEN`;
  - raise the limit;
  - expose `/api/push` only to the fleet's networks.

  The trade is accepted: unbounded rows cost every operator, while the lock-out needs an
  attacker who already has push access.
- **A05 / API8:** a malformed limit refuses startup.
- **A07 / API2:** `registry full` and `registry unavailable` come after authentication, so
  only an authenticated client learns about the registry.
- **A09:** one `warn` when the registry fills, per-id `debug` lines in `Debug` form, and the
  configuration error names the variable, never its value.
- **A03:** the count query is a fixed string with a bound parameter.
- **API9:** the README's deployment patterns change to match.
- Everything else is unchanged.

## Testing plan

TDD, per `CLAUDE.md`, with `red-test-adversary` and `rosette-auditor`.

- **Pure:**
  - `PushRegistryLimit::from_env` over unset, empty, `1`, `1000`, `0`, `-1`, `1,000`,
    `18446744073709551616` and non-UTF-8, each with its outcome;
  - `admits` below the limit, at it, and above it;
  - the poll selection skips a push system and keeps an enabled `http://` system.
- **`register_push_system`** (temp SQLite):
  - a limit of 1 with one polled system present admits an unseen push id;
  - a limit of 1 with one push system present refuses an unseen id, inserts nothing, and
    returns `AlreadyRegistered` for the known id;
  - it never replaces an existing row.
- **Startup marking** touches push systems only.
- **Real server:**
  - a full registry answers `registry full`;
  - a registry whose `systems` table was dropped answers `registry unavailable`;
  - a client that registers and then resets before `auth_ok` leaves its system offline.

## Impact on `docs/ARCHITECTURE.md`

- § Components: registration is bounded, and push systems aren't polled.
- § Domain model: the Fleet Registry row, and the glossary (**push system**, **push registry
  limit**).
- § Trust boundaries, Agent → Hub (push): the registry limit and where it holds.
- § Open architectural questions:
  - Removed: unbounded auto-registration; push systems being polled; the reliance on the
    poller in the stored-id entry from RFC 0005.
  - Added:
    - expiring stale push systems;
    - REST-created systems as an unbounded registration path;
    - a registration-origin column.
- `README.md`:
  - a `HUB_MAX_PUSH_SYSTEMS` row;
  - `registry full` and `registry unavailable` in the `auth_error` table;
  - "they appear automatically" (up to the limit);
  - § Deployment patterns (the push-only nginx example, and the advisory note).
- `docker-compose.yml`: `HUB_MAX_PUSH_SYSTEMS: ${HUB_MAX_PUSH_SYSTEMS:-}` on the hub.
  `.env.example`: a commented line.

## Rollout / migration notes

Hub-only. No schema change. Rolling back is safe, since an old hub ignores the variable.

- The limit counts every row whose url is `push://`, live or stale. Before upgrading, run
  `sqlite3 system-hub.db "SELECT COUNT(*) FROM systems WHERE url = 'push://';"`. A hub at or
  over 1000 keeps its rows, but refuses new ids until it's under the limit.
- Push systems start offline after the upgrade. Each turns online with its first frame:
  within one push interval, plus up to 5 s if its agent first has to notice the old connection
  died.
- A malformed `HUB_MAX_PUSH_SYSTEMS` refuses startup, and the log names it.

## Review status

The first `rfc-adversary` pass left these findings. They must be addressed before this RFC can
be `Accepted`. It stays `Draft` while RFC 0006 is implemented first.

**CONFIRMED:**
- **`PUT` still decides what a push system is.** A polled row re-pointed to `push://` stays
  online forever, and a push row re-pointed away stops counting. Fix: parse `url` at the REST
  edge into an endpoint enum (`Push` / `Poll(SystemUrl)`). `POST` and `PUT` then refuse
  `push://`, and refuse any change to a push system's url (API3).
- **Removing the poller removes the only backstop.** Offline detection moves from about 30 s
  to RFC 0006's 90 s idle deadline. Fix: declare the dependency on that deadline, state the
  change, and log a failed offline write at `warn`.
- **A stale connection marks a live system offline.** A host back within 90 s is marked
  offline, and its live entry evicted, when its old connection times out. Fix: a per-id
  connection generation, so only the current connection marks offline or evicts. Drop the
  "undone" and "only really offline" claims.
- **The agent recomputes its id on every reconnect.** In its random-UUID fallback (distroless
  images), one agent uses one registry slot per reconnect. Fix: compute the id once per
  process and persist the fallback. That is an agent change, so `system-agent` goes into
  Affects.
- **The push-only nginx pattern can't isolate REST by itself.** The hub binds `0.0.0.0:9091`
  and compose publishes it, CORS is `Any`, and SSE needs `proxy_buffering off`. Fix:
  firewall 9091 from agent networks (or add a bind-address setting), put the upgrade headers
  only on the push location, and name CORS in §6.
- **Tests that can't fail:**
  - exactness needs an `admits`-hook concurrency test;
  - "not polled" needs a collector tick test, not only the pure selection;
  - the reset test is already green on HEAD and belongs to RFC 0006;
  - the limit needs a seam in `PushConfig`;
  - the `JoinError` branch is best-effort.
- **A09.** A database error logs nothing, and per-id `debug` lines are dropped at the default
  INFO level. Fix: log the database error once, rate-limited, and repeat the "full" `warn`
  periodically with a refusal count.
- **Inventory:**
  - define `Refusal`'s new variants against RFC 0006's enum;
  - add the Ingestion row, the `insert_system` open question and README:281;
  - an API-based pre-check (`curl … | jq`), since the container has no `sqlite3`;
  - the motivation's "resets before `auth_ok`" claim is false on HEAD (see RFC 0006 §2).

**PLAUSIBLE:**
- **Startup marking on poisoned rows.** Use a single `UPDATE … WHERE url = ?1` and a
  `SELECT 1` existence check, with no row mapping. A marking failure refuses startup. Test
  with a `..` row and a -1 `poll_interval_secs` row.
- **The limit bounds rows, not load.** Cite RFC 0007's capacity, and index `systems(url)`.
