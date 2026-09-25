# RFC 0002: TDD, DDD, Adversarial Review and the Rosette

- Status: Implemented
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-25
- Affects: both (development process; no runtime behaviour changes)

## Motivation

The repository's rules required tests *alongside* each change, but not *before* it. They also
said nothing about where domain logic lives, and every check was run by the same agent that
wrote the code. That leaves three gaps:

- A test written after the implementation tends to pin whatever the code happens to do
  (a photograph, not a specification). The same author then judges whether it "fails for
  the right reason".
- Domain rules (alert thresholds, status, retention) drift into handlers, SQL strings and
  poll loops. `collector.rs` already parses agent alerts from untyped JSON, maps them and
  stores them in one function. `models.rs` in both crates serves as wire format, storage
  shape and domain model at once.
- With no shared quality bar, a review comes down to taste.

The team already runs this discipline successfully in another repository. This RFC adopts it
here, adapted to Rust/Axum and to this system's two independently deployed binaries.

## Proposed design

Four mandatory practices, stated in `CLAUDE.md` § Development discipline:

1. **Test-Driven Development**, as a five-phase loop per behaviour: contract and red test →
   prove red plus adversarial check → minimal green → refactor under green → gate. Explicit
   exceptions for bug fixes (a reproducing red test), characterisation tests (green on first
   run by design) and pure refactors (existing tests are the contract).
2. **Domain-Driven Design**: bounded contexts and a glossary (`docs/ARCHITECTURE.md` § Domain
   model), a pure domain core with I/O confined to adapters, parse-don't-validate at the
   boundary, DTOs kept separate from domain types, and the push frame treated as a published
   contract that must stay mixed-version compatible. Existing code is retrofitted with the
   same ratchet as test coverage: only in files a change touches.
3. **The Rosette of Beautiful Code**: eight review dimensions (Storytelling, Simplicity,
   Clarity of Intent, Expressiveness, Purity, Sustainability, Durability, Creativity), made
   concrete for Rust. For example: a 30–50 line function budget excluding comments, blanks
   and test modules; complexity-lint `#[allow]`s count as findings; exhaustive `match` over
   domain enums; table-driven tests with boundary rows.
4. **Adversarial review** by three read-only subagents in `.claude/agents/`:
   - `red-test-adversary`: tries to pass a new red test with a cheat implementation in a
     temp copy of the crate. Verdict `EVIDENCE` / `DECORATION` / `WEAK-RED`.
   - `rfc-adversary`: attacks a draft RFC on this repository's failure modes (mixed-version
     fleet, the silent push-decode drop, the SQLite file already on disk, security "N/A"s,
     blocking calls, domain leakage, unbounded growth, untestable claims, stale inventory).
   - `rosette-auditor`: reviews a diff for what clippy and the tests can't see (purity,
     thin adapters, boundary parsing, DTO/domain separation, error modelling, the Rosette,
     the glossary).

   Blocking verdicts are `DECORATION`, an unaddressed `CONFIRMED`, and `VIOLATED`. Each
   verdict is reported in the change summary next to the OWASP findings.

Scope: the rules apply to every non-trivial change to the Rust crates. *Trivial* means no
behaviour change (comments, docs, formatting, log wording, dependency bumps with no API
change); trivial changes skip the loop and the adversaries but still run the toolchain gate.
The dashboards have no JS test harness, so a dashboard change gets an XSS review plus
`rosette-auditor` but no red-test requirement.

A red test must fail on an assertion against a compiling stub, not on a missing symbol or a
`todo!()` panic. Characterisation tests, which are green by design, are attacked by
`red-test-adversary` in mutation mode instead.

The RFC template gains a *Domain impact* section, and the per-change checklist in `CLAUDE.md`
is reordered to follow the loop.

## Domain impact

No bounded context changes. This RFC introduces the context map and glossary themselves
(`docs/ARCHITECTURE.md` § Domain model), describing current reality, and records the existing
places where code doesn't meet the new rules as open questions rather than changing them.
There is no change to the push frame or the poll contract.

## Alternatives considered

- **Keep "tests alongside" only.** Rejected: it doesn't prevent tests from being derived from
  the implementation, which is the failure the adversary targets.
- **Enforce the Rosette mechanically** (a function-length script, a ratchet allowlist like
  the other repository's `scripts/invariants.sh`). Deferred, not rejected: worth its own RFC
  once the budget has been applied by hand for a while and the allowlist can be seeded from
  real data. Until then the auditor enforces it.
- **Split the domain into a shared crate used by both binaries.** Rejected for now: the
  crates are deliberately independent (not a workspace) and are deployed independently. A
  shared crate would change the build and release model and deserves its own RFC. The
  mixed-version rule addresses the contract risk without it.
- **Remodel `models.rs` now.** Rejected: a big-bang rewrite with no behaviour change to
  justify it. The retrofit ratchet spreads the work across the changes that touch those files.

## Security implications

No runtime code changes: no routes, auth, SQL, framing, config parsing or dashboard HTML.
`CLAUDE.md` § Security *does* change. The agent-side constant-time bullet now says to keep
it that way (RFC 0001), and a new standing-risk bullet records that the hub's
`HUB_PUSH_TOKEN` comparison is still not constant-time (A02/API2) and that the handshake's
`system_id[..8]` can panic (API4/API8, reachable unauthenticated when the token is unset).
Both are recorded, not fixed; each deserves its own RFC and TDD change.

- A01–A10, API1–API10: no runtime surface changes. The DDD boundary rule ("security
  validation belongs in boundary constructors") and `rfc-adversary` attack 3 (security
  "N/A"s) *strengthen* future reviews of A03/A10/API3/API4/API7, but change nothing today.
- The subagents are read-only by instruction. `red-test-adversary` writes only to a fixed
  sandbox under `$XDG_CACHE_HOME` (never `/tmp`), copied with `*.db`, `.env*` and `.git`
  excluded so the hub database's per-system tokens don't leave the tree, and it proves the
  real tree is untouched with `git status --porcelain`. Cargo runs use `--locked`.
- A06: no dependency changes.

## Testing plan

No production code changes, so there are no new tests. Follow-up before relying on the
recipe: a dry run of `red-test-adversary` in mutation mode against an existing alert
threshold test in `src/alerts.rs`, to validate the sandbox recipe end to end (first cold
build included). After that, the first change made under these rules exercises them; its
summary must report each adversary's verdict.

## Impact on `docs/ARCHITECTURE.md`

Adds § Domain model (bounded contexts, published contracts including the JSON handshake and
the positional push frame, glossary). Adds to § Open architectural questions: DTOs doubling
as domain types; `AlertManager::evaluate`'s nine positional arguments; `collector.rs`'s
interleaved alert parsing with a discarded `insert_alert` error; the `ongoing_rule_N`
alert-id collision; no blocking work offloaded anywhere; retention policy decided in the SQL
adapter; and the hub handshake's non-constant-time comparison and `[..8]` panic.
`README.md`'s push-protocol section now states that frames are positional arrays.

## Rollout / migration notes

None at runtime. Subagent definitions in `.claude/agents/` are loaded when a Claude Code
session starts, so sessions already open when this lands must be restarted to see them.

## Adversarial review

`rfc-adversary` ran on the first draft; it did not survive. All nine findings were acted on
in this RFC and the files it changes:

1. CONFIRMED — the mixed-version rule assumed a keyed map, but `rmp_serde::to_vec` is
   positional. The rule was rewritten around field order in `CLAUDE.md`, ARCHITECTURE,
   README and the adversary exemplar. Verified separately: both crates' `PushPayload`,
   `DiskPayload`/`DiskItem` and `ProcessPayload`/`ProcessItem` currently declare identical
   field order.
2. CONFIRMED — the glossary called alert records "deduplicated", which hid an id collision.
   Now described as it behaves, and added to open questions.
3. CONFIRMED — the edited security bullet hid the hub's non-constant-time comparison.
   Restored as a standing risk.
4. PLAUSIBLE — the sandbox recipe could write to the real tree or copy the token database.
   Decided: replaced with a fixed cache-dir sandbox, absolute paths, excludes, `--locked`
   and a `git status` proof.
5. CONFIRMED — the status was `Implemented` before review or merge. Now `Accepted`.
6. CONFIRMED — WEAK-RED and the characterisation-test exemption hollowed out TDD. Now a red
   must be an assertion failure against a stub, and characterisation tests get mutation mode.
7. CONFIRMED — "trivial" was undefined and dashboards were in scope with no harness. Both
   are now scoped.
8. CONFIRMED — open questions missed the largest existing breaches. Added, and the argument
   count corrected.
9. CONFIRMED — handshake vs frame confusion, a misattributed standing risk, the re-run rule,
   and an unverifiable testing plan. All adjusted.

Per `CLAUDE.md`, the adversary was not re-run on these amendments: they correct facts and
wording, and don't change the design.
