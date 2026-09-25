# RFC 0004: Alert Incident Identity

- Status: Implemented
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-25
- Affects: `system-agent` (plus one characterisation test in `system-hub`, whose alert
  history is what gets fixed)

## Motivation

An active alert's id doesn't identify anything stable. `AlertManager::evaluate`
(`src/alerts.rs`) gives an alert a fresh `uuid::Uuid::new_v4()` only on a tick where it
*notifies*: the tick its rule's duration is first satisfied, and each later tick where the
cooldown has elapsed. On every other tick of the same breach the id is
`ongoing_rule_<index>`, and that string is the same for every breach of that rule, forever.

The hub polls `/api/alerts` and stores each active alert under `<system id>_<agent alert id>`
with `INSERT OR IGNORE` (`system-hub/src/collector.rs`, `system-hub/src/db.rs::insert_alert`).
The agent evaluates every 2 s, and a poll lands on a notifying tick only by chance. So in
practice:

1. The first poll that sees any breach of rule N stores `<system>_ongoing_rule_N`. Every
   later breach of rule N, on that system, for the life of the database, is silently ignored.
   An operator who acknowledged the first one never sees the next CPU spike.
2. When a poll does land on a notifying tick, the same breach is stored a second time under
   a UUID. A breach that renotifies after its cooldown gets a third row, and so on.
3. `fired_at` also changes during a breach: it is the notifying tick's time on a notifying
   tick and the breach start on the others. Whichever tick the hub sees first is what it
   keeps.

So the hub's alert history is both lossy (1) and duplicated (2). Doing nothing leaves the
hub's alert panel unable to show the second occurrence of any problem.

## Proposed design

The JSON shape of `ActiveAlert` is unchanged (`id` stays a string); only the values of `id`
and `fired_at` change. Changes are in `src/alerts.rs`, plus `src/state.rs` (mint the agent
run) and the three route files that read the manager (`api.rs` calls `replace_rules`; `api.rs`,
`sse.rs` and `ws.rs` read through accessors instead of public fields).

### Alert incidents

An **alert incident** is one uninterrupted breach of one alert rule. It becomes *active* on
the first tick where the breach has lasted the rule's duration, and ends on the first tick
the rule no longer breaches. Replacing the rule set (`POST /api/alerts/config`) or
restarting the agent also ends it, because the state that remembers it is gone.

Each incident carries one **incident id**, fixed when it becomes active. Every active alert
reported during the incident carries that id, including ticks inside the cooldown and
re-notifications after it. `fired_at` is likewise the tick the incident became active, on
every tick.

### Ids from an agent run and a sequence, not from the clock

```rust
/// One lifetime of the agent process. Minted once at startup, so incident ids from
/// different runs of the same agent never meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentRun(Uuid);

/// Identifies one alert incident: the agent run that saw it and its position in that
/// run's sequence of incidents (starting at 1).
#[derive(Debug, Clone, PartialEq, Eq)]
struct IncidentId { run: AgentRun, sequence: u64 }   // private to alerts.rs

impl Display for IncidentId { /* "<run uuid, hyphenated>-<sequence>" */ }

/// Mints the incident ids of one agent run; never rewinds.
struct IncidentSequence { run: AgentRun, minted: u64 }
```

`AlertManager` holds the run and the next sequence number (its `IncidentSequence`). `AlertManager::new(run, rules)`
and `with_defaults(run)` take the run. There is deliberately no accessor that reads it back:
code that could read the run could build a second manager on it and restart the sequence at
1, which would re-emit ids the hub already holds. Code that can't read it can only mint a
fresh run, which is safe. `AppState::new`
(`src/state.rs`) mints it with `Uuid::new_v4()`: `AppState::new` is wiring that assembles the
shared state, not Host Telemetry's domain core (that is `MetricsHistory`), and the context map
says so. `evaluate` itself never calls the RNG or the clock, so it is deterministic given its
arguments and the manager's state (Rosette: Purity). A test pins that the same breach seen
through two `AppState::new()` calls gets different incident ids, since that one line is the
whole of cross-restart uniqueness.
`AlertManager` stops deriving `Clone` (nothing clones it), so a copy can't fork the sequence.
`AgentRun::new` accepts any `Uuid`, not only a random one. This is accepted: its only
production caller is `AppState::new`, which draws `Uuid::new_v4()`, and the state test asserts
that the run in every incident id is a version-4 UUID. Rejecting non-random UUIDs in the
constructor would make every test build runs from random bytes and read less clearly.

Uniqueness: within one run, the sequence never repeats (it is not reset by `replace_rules`
either). Across runs, the run's UUID differs. Nothing depends on the wall clock, so a clock
stepped backwards, two incidents activating in the same second, or a host without a
real-time clock booting at the same time on every boot can't make two incidents share an id.

`ActiveAlert` stays the wire DTO with `id: String`, filled from `IncidentId`'s `Display` when
the DTO is built. The domain type has no serde impl. That keeps the wire format decided in one
place, at the DTO, and a test pins it through `serde_json` as a JSON *string*: if it ever
stopped being a string, the hub would mint a new random id on every poll
(`collector.rs`, `unwrap_or_else(|| Uuid::new_v4())`).

After an agent restart an incident that is still breaching gets a new id, because the agent
has lost its state, and the hub records it as a new incident. This is a new, accepted cost:
today a restart mostly adds no row, because the next non-notifying tick reports the
`ongoing_rule_N` id the hub already holds. After this change each restart adds one row per
rule still breaching.

### Per-rule state as an enum

The per-rule state is today `breached_since: Option<u64>` plus `last_notified: u64`, where
`0` means "never notified", keyed by the string `"rule_<index>"`. It becomes:

```rust
struct Incident { id: IncidentId, activated_at: u64 }  // an alert incident that is active

enum Breach {
    Clear,
    Pending { since: u64 },                  // duration not yet satisfied
    Active { since: u64, incident: Incident },
}

struct RuleState { breach: Breach, last_notified: Option<u64> }
```

keyed by rule index (`HashMap<usize, RuleState>`). The transition is one pure function of
the previous state, whether the rule breaches, the rule's duration and `now`
(`Breach::advance`). `RuleState::tick` runs one rule's tick and returns a `Report`, either
`Quiet` or `Notify`, carrying the active alert. The notification is decided and recorded in
one place (`RuleState::take_notification`): an `Active` incident notifies when there is no previous notification for
that rule, or the cooldown has elapsed since it. The cooldown spans incidents, as today, so an
incident that becomes active inside the previous incident's cooldown is reported as active but
not notified until the cooldown ends.

Two edges change:

- **No sentinel.** With `0` as "never", a first incident at a clock value below
  `cooldown_secs` did not notify (`now - 0 < cooldown`). That happens on a host whose clock
  starts at the epoch before NTP syncs. With `Option` it notifies.
- **Clock stepped backwards.** `now - since` and `now - last_notified` are plain `u64`
  subtractions today: they panic in debug builds (killing the background collector task for
  the life of the process) and wrap in release (an instant notification "for
  18446744073709551xxx s"). They become re-anchoring: when `now` is earlier than a recorded
  tick (the breach start or the last notification), that tick moves back to `now`. So after a
  step back, a pending breach activates one duration later and the cooldown ends one cooldown
  later, measured on the new clock. Saturating the subtraction instead would stall both
  until the clock caught up with the old reading, a blind spot as long as the step.
  Re-anchoring never changes the `Breach` variant: an active incident stays active with the
  same id and `fired_at`, only its breach start moves, so after a step back `fired_at` can be
  later than the clock. The last notification re-anchors on every tick of an enabled rule,
  breaching or not.

  For the cooldown, the alternative was to treat a last notification in the future as "never
  notified" and notify at once. Re-anchoring was chosen because the agent can't tell how much
  real time a step hid, and it treats the breach start and the cooldown alike. Its cost is
  bounded: a genuinely new incident right after a step back logs its notification up to one
  cooldown late, while it is reported as active on `/api/alerts`, SSE and WS immediately
  (notifications only drive the `🚨 ALERT` log line). Forward clock steps are out of scope:
  as today, a pending breach activates on the tick after a forward step.

### Replacing the rule set is a domain operation

`set_alert_config` (`src/routes/api.rs`) currently assigns `mgr.rules` and clears
`active_alerts` and `states` itself. That is the "replacing the rule set ends every incident"
rule, living in a handler. Under index keying it matters more: a leftover `Active` state at
index *i* would lend its incident id to whatever rule sits at *i* afterwards. It becomes
`AlertManager::replace_rules(rules)`, which ends every incident and keeps the run and the
sequence, and the handler calls it. The manager's fields (`rules`, `active_alerts` and the
per-rule state) become private, with `rules()` and `active_alerts()` read accessors; every use
outside `alerts.rs` is already a read. So nothing outside the domain can swap the rules without
ending the incidents.

Replacing the rule set ends *every* incident, including those of rules the new set leaves
unchanged, and resets every cooldown and pending duration. Those incidents come back one
duration later (up to 300 s for the default disk warning), under new ids, so the hub shows them
as new unacknowledged records, and they notify when they come back. Until then they are absent
from `/api/alerts`. This was already true before this RFC (the handler cleared the state), but
new ids make it visible on the hub. It is accepted: rule sets change rarely and by hand. A
deployment that re-POSTs an identical rule set on every configuration run would pay it on every
run; the alternatives (below) are deferred.

### What stays

`evaluate` is split into helpers (the rule's reading, the effective threshold, the
comparison, one rule's tick, the notification, the message) so no function exceeds the
Rosette's size limit. The tick's readings are packed into a private `Readings` value object;
only the public signature stays positional. Its nine-argument
signature and `#[allow(clippy::too_many_arguments)]` stay. This is a recorded exception, not
an oversight: replacing them with a readings value object changes the only caller,
`collectors/mod.rs::background_collector`. Touching that file obliges fixing its blocking
`sysinfo` collection on the runtime, which is its own change. It stays in § Open
architectural questions.

A second recorded exception: a disk rule with no disk reading reads `0.0`, a sentinel. That
covers a named mount point missing from the snapshot (pinned by
`evaluate_disk_missing_mount_point_defaults_to_zero`) and a rule without a mount point when no
disks were reported at all (the default disk rules, and an empty disk list happens in
containers). With
incident ids this now has a visible cost: a mount that briefly disappears ends a `Gt`
incident, and it comes back as a second hub record; for `Lt`/`Lte` it opens a phantom
incident. Turning an absent reading into "no transition this tick" is a behaviour change of
its own; it is added to § Open architectural questions.

## Domain impact

- **Alerting** (agent) changes: new glossary terms **agent run**, **alert incident**,
  **incident id**, **tick**, **metric readings** and **notification**; **active alert** is redefined as the report of an active incident on one
  tick. Replacing the rule set moves from the adapter into the domain.
- **Host Telemetry** (agent): `state.rs`'s `AppState::new` mints the agent run. The context
  map is corrected to list `AppState` as wiring, with `MetricsHistory` as the domain core.
- **Ingestion** (hub): no code change; it reads the id as an opaque string. Its poll tests in
  `collector.rs` pin one record per incident.
- **Fleet History** (hub) has no code change, but the glossary entry **alert record** changes
  meaning: it is now one record per alert incident, keyed by `<system id>_<incident id>`.
- The published contract that changes is the *poll response* `/api/alerts` (and the same
  `active` array on `/api/stream/alerts` and the WebSocket). The shape is the same; the
  meaning of `id` and `fired_at` is new. The push frame carries no alerts and is untouched.

Mixed-version fleet:

- **New agent + old or new hub:** fixed. The hub treats the agent's id as an opaque string,
  so the hub needs no change and there is no deployment order.
- **Old agent + any hub:** unchanged, keeps today's bug until the agent is upgraded.
- **Rows already in the hub's `alerts` table** stay as they are. A new key ends in
  `_<uuid>-<digits>`, which can't equal an old one ending in `_ongoing_rule_<digits>` or
  `_<uuid>`, so no migration is needed. A stale unacknowledged `<system>_ongoing_rule_N` row
  remains until someone acknowledges it or deletes the system.
- **Keys across systems.** System ids are not all hub-minted: a push-registered system's id is
  self-asserted and may contain `_`, and `PUT /api/systems/:id` can give it a URL to poll. The
  key `<system id>_<incident id>` still can't be shared by two honest agents, because the
  incident id starts with a random run UUID. A hostile agent can already choose an agent id
  that forges another system's key; that is unchanged (API10, below).

## Alternatives considered

- **Derive the id from the rule index and the activation second** (the first draft). Pure
  and readable, but the wall clock repeats: a host without a real-time clock boots at the
  same time on every boot, and a clock step or a burst of ticks can repeat a second. Each
  repeat merges a new incident into an old, possibly acknowledged, record: the bug this RFC
  removes. Rejected for the run + sequence id.
- **A random id per incident, drawn inside `evaluate`.** Also unique, but puts the RNG in the
  domain function and makes ids untestable by value. The run UUID is drawn once, when
  `AppState::new` wires the shared state, instead.
- **A sequence alone**, without a run. Resets to 1 on every restart, so it repeats across
  runs. Rejected.
- **Fix it in the hub**: key records on `<system>_<agent id>_<fired_at>`. That would partly
  rescue old agents, but the hub would still store notifying ticks separately, and it would be
  parsing another context's id grammar. Rejected: the agent owns alert identity.
- **Keep the state of rules that `replace_rules` leaves unchanged** (derive `PartialEq` on
  `AlertRule`, keep state *i* when new rule *i* equals old rule *i*). Would stop a config edit
  from re-opening unrelated incidents, but makes a rule's position plus its contents its
  identity, which breaks when a rule is inserted before it. Deferred until rules have ids.
- **Make `replace_rules` a no-op when the whole new set equals the current one.** Keys on the
  whole set, not on positions, so it has no rule-identity problem, and it would make an
  idempotent configuration push free. Deferred: it changes today's behaviour of an identical
  POST, which is outside this RFC.
- **An open/closed alert lifecycle in the hub** (store "resolved", update rows as an incident
  continues). Real value, but it needs a schema change and a hub-side notion of an incident
  ending. Out of scope; this RFC gives it the stable id it would need.
- **One id per notification** (each re-notification after the cooldown is a new record). A
  long breach would fill the hub with one row per cooldown period, and acknowledging it would
  not stay acknowledged. Rejected: one incident, one record.
- **Do nothing.** Rejected, see Motivation.

## Security implications

- **A01 Broken Access Control / API1 / API5:** no change. The hub's
  `POST /api/alerts/:alert_id/acknowledge` stays unauthenticated, as today, and
  `GET /api/alerts` lists every id without auth, so the ids were never a capability.
- **A02 Cryptographic Failures:** N/A. The run UUID is an identifier, not a secret; it is
  visible in every alert.
- **A03 Injection (incl. XSS):** the agent's ids are now only hex digits and `-`.
  Pre-existing and not fixed here: the hub dashboard interpolates `a.id` unescaped into an
  inline `onclick="ackAlert('…')"`, and `a.severity` unescaped into a class attribute
  (`system-hub/static/index.html`, `loadAlerts`). `a.severity` and the agent half of `a.id`
  come from agent JSON. The system half of `a.id` is the system id, which for a
  push-registered system is self-asserted in the handshake. So any agent, anyone who can
  register or re-point a system on the unauthenticated hub, and any push-token holder can
  plant stored XSS on the hub dashboard. This RFC doesn't change that surface; it is recorded
  as an open question and should be fixed separately.
- **A04 Insecure Design:** the change *removes* one: silently dropped incidents are a
  monitoring blind spot.
- **A05 / API8 Security Misconfiguration:** no change (CORS stays as it is).
- **A06 Vulnerable Components:** no dependency added or removed (`uuid` is already a
  dependency of the agent).
- **A07 / API2 Authentication:** N/A, no auth code touched.
- **A08 Data Integrity:** the hub's alert history becomes accurate.
- **A09 Logging & Monitoring Failures:** improved: a second incident of a rule is now
  recorded, and a clock step no longer kills the alert evaluation task. The agent's
  `🚨 ALERT` log line is unchanged.
- **A10 / API7 SSRF:** N/A.
- **API3 Object Property Level Authorization:** `POST /api/alerts/config` keeps its body
  shape; only the handler's body changes.
- **API4 Unrestricted Resource Consumption:** the hub now stores one row per incident instead
  of one per rule forever. The hub polls every 30 s, so every incident that stays active
  across a poll becomes one row: a rule that flaps with a period of a few minutes adds a few
  hundred rows a day per system. Today the same rule adds rows only when a poll lands in a
  2 s notifying window. The hub's `alerts` table has no retention (pre-existing); that is
  now a more pressing open question and is recorded as one.
- **API6 Sensitive Business Flows:** N/A.
- **API9 Inventory:** no endpoint added or removed; README's endpoint table is unaffected.
- **API10 Unsafe Consumption of APIs:** the hub still reads agent alerts from untyped JSON and
  concatenates the agent id into its key (pre-existing). This RFC changes only what an honest
  agent sends.

## Testing plan

Unit tests in `src/alerts.rs`, table-driven, driving `evaluate` through a sequence of ticks
with plain values (no clock, no RNG), using rules with `duration_secs > 0` wherever the
activation tick and the breach start must be told apart (breach at 1000, activation at 1060).

- Characterisation (pass on first run; `red-test-adversary` in mutation mode): the cooldown
  spans incidents, with duration 0 and 5 s; alerts of identical rules active on the same
  tick have distinct ids.
- Red, then green:
  - an incident keeps one incident id on its activating tick, on every tick inside the
    cooldown, and on a re-notification after it;
  - two incidents of the same rule, separated by a clear tick, have different ids;
  - `fired_at` is the activation tick (not the breach start, nor breach start + duration) on
    every tick of the incident;
  - the id serialises through `serde_json` as the JSON string `"<run>-<sequence>"`, and two
    runs seeing the same ticks produce different ids;
  - a first incident notifies even when the clock reads less than `cooldown_secs`;
  - a clock stepped backwards neither panics nor wraps; a pending breach then activates one
    duration after the step, the cooldown ends one cooldown after it, and a backwards tick
    that no longer breaches clears the incident;
  - an active incident keeps its id and `fired_at` across a step back (duration > 0);
  - `fired_at` of an incident activating inside the previous cooldown is its activation tick,
    not its first notification;
  - a notification at clock 0 starts the cooldown (no "never notified" sentinel);
  - a breach that ends before its duration doesn't consume a sequence number;
  - `replace_rules` ends every active incident, and the next incident after it gets an id
    no earlier incident had.
- The existing `evaluate_ongoing_alert_present_but_not_renotified_within_cooldown` asserts
  the `ongoing_` prefix; it is rewritten to assert the incident id instead. The route test
  `set_alert_config_replaces_rules_and_clears_active_state` makes an incident active before
  the POST, asserts none is active after it, and that the next breaching tick reports a
  different incident id (catching a handler that assigns the rules without ending incidents).
- `src/state.rs`: the same breach seen through fresh `AppState::new()` calls gets different
  incident ids.
- Hub, characterisation: `poll_system_stores_active_alerts_from_agent` uses a realistic
  incident id instead of `rule_0`, and `poll_system_stores_one_alert_record_per_incident`
  runs a sequence of polls: a new incident is recorded; the same id again changes nothing
  (first-seen values and the acknowledgement stay); two incidents on one poll get a record
  each; an incident the agent stops reporting stays unacknowledged. It passes today; it is the
  cross-context proof that distinct ids are enough.

## Impact on `docs/ARCHITECTURE.md`

- § Components (hub): "deduplicated alert history" becomes "alert history, one record per
  alert incident". `README.md` § Storage (`alerts` row) gets the same wording.
- § Domain model, glossary: add **agent run**, **alert incident**, **incident id**, **tick**,
  **metric readings** and **notification**;
  redefine **active alert**; reword **alert record** (one per incident, keyed by
  `<system id>_<incident id>`).
- § Domain model, bounded contexts: Alerting's domain core lists `AgentRun`, `IncidentId` and
  `AlertManager::replace_rules`; Host Telemetry lists `state.rs`'s `AppState` as wiring and
  `MetricsHistory` as the domain core.
- § Storage: the `alerts` row is one per alert incident.
- § Open architectural questions: remove the alert-id collision item; add the hub
  dashboard's unescaped `a.id`/`a.severity` (with both sources of `a.id`), the `alerts`
  table's missing retention, the `0.0` disk reading when there is none, and the hub's random
  fallback id for an agent alert without an `id` (a new record on every poll).

## Rollout / migration notes

Agent-only. Upgrading agents in any order, before or after the hub, is safe. No schema
migration. Old `ongoing_rule_N` rows stay in existing hub databases until acknowledged.

## Adversarial review

`rfc-adversary` ran on the first draft (a clock-derived id). Findings and what was done:

1. CONFIRMED — the planned wire-form test could pass on `Display` alone while the JSON stopped
   being a string, and no test crossed into the hub. The test now goes through `serde_json`;
   the hub gets a two-incident characterisation test.
2. CONFIRMED — the rewritten transition kept `u64` subtractions that panic or wrap when the
   clock steps back. Now handled, with its own red test (first as `saturating_sub`; the
   second pass replaced that with re-anchoring).
3. CONFIRMED — "replacing the rule set ends the incident" lived untested in a handler. Now
   `AlertManager::replace_rules`, tested, called by the handler.
4. PLAUSIBLE — a clock-derived id repeats on RTC-less hosts across reboots. Decided: the id
   became agent run + sequence, which removes the clock from identity entirely.
5. CONFIRMED — "polled system ids are hub-minted UUIDs" was false. Replaced with the
   run-UUID argument and a note on self-asserted ids.
6. PLAUSIBLE — duration-0 tests can't tell activation from breach start. Decided: rows with
   `duration_secs > 0` are required.
7. PLAUSIBLE — serde on a domain type, and the retained lint allow. Decided: `ActiveAlert.id`
   stays a `String` DTO field; the allow is kept as a recorded exception with its reason.
8. PLAUSIBLE — API4 growth understated. Decided: the growth rate is now stated, and alert
   retention is recorded as an open question.
9. CONFIRMED — two doc locations missing and the XSS source understated. Both fixed.

Closest attack that failed: the mixed-version fleet analysis. The hub treats the id as
opaque and the push frame carries no alerts.

A second pass attacked the run + sequence design:

1. CONFIRMED — the route test couldn't catch a handler that assigns the rules without ending
   incidents. It now makes an incident active first and checks the next id differs.
2. CONFIRMED — cross-restart uniqueness rested on one untested line in `state.rs`. Now
   tested, with a `run()` accessor, and `AlertManager` no longer derives `Clone`.
3. CONFIRMED — `saturating_sub` stalls activation and cooldown for as long as the clock step.
   Replaced by re-anchoring, with rows pinning activation a duration after the step.
4. CONFIRMED — public fields let any handler bypass `replace_rules`. Now private, with read
   accessors.
5. CONFIRMED — the RNG sat in a module the context map called domain core, and two contexts
   were missing from Domain impact. The context map is corrected, and Host Telemetry and
   Ingestion are listed.
6. PLAUSIBLE — a rule-set replacement ends unchanged rules' incidents. Decided: accepted and
   stated; keeping unchanged rules' state is listed as a deferred alternative.
7. PLAUSIBLE — the missing-mount `0.0` sentinel. Decided: recorded exception and open question.
8. CONFIRMED — the restart baseline was wrong. Reworded as a new, accepted cost.

Closest attack that failed: wire-form injectivity. A hyphenated UUID has a fixed length, so
`<run>-<sequence>` can't collide with another pair or an old key.

A third pass attacked only the second pass's amendments:

1. CONFIRMED — no test stepped the clock back while an incident with a duration was active,
   so ending and re-opening it would pass. Added, and the RFC now says re-anchoring never
   changes the `Breach` variant.
2. CONFIRMED — the `run()` accessor let code restart the sequence within a run. Removed; the
   state test now compares incident ids instead.
3. PLAUSIBLE — the cost of `replace_rules` was misstated. Decided: corrected to "one duration
   later", and the whole-set no-op is listed as a deferred alternative.
4. PLAUSIBLE — re-anchoring the cooldown delays a new incident's notification. Decided: kept,
   with the reason and its bound written down; forward steps are named out of scope.
5. CONFIRMED — the `0.0` exception missed the no-disks case. Widened.

The adversary was not re-run on these: they add a test, remove an accessor, and correct
statements, without adding behaviour. Closest attack that failed: the reclassification of
`AppState::new` as wiring, which holds (one production caller, `src/main.rs`).
