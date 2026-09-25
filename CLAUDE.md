# CLAUDE.md

Instructions for Claude Code (and any other agent) working in this repository.
These rules are mandatory, not suggestions. If a request conflicts with them, follow the
rule and tell the user why, rather than silently skipping it.

## Project overview

Two independent Rust binaries, **not** a Cargo workspace (each has its own `Cargo.toml` /
`Cargo.lock`):

- **`system-agent`** (repo root, `src/`) — runs on every monitored host, exposes a REST/SSE/WS
  API over local system metrics (CPU, memory, disk, network, processes, packages, services,
  containers, ports), and can push snapshots to a hub over WebSocket + MessagePack.
- **`system-hub`** (`system-hub/`) — aggregates data from many agents (HTTP poll or WS push),
  persists it to SQLite, and serves a fleet dashboard.

Both are Axum services. Full endpoint/protocol reference lives in `README.md` — keep it in
sync when endpoints change, but architectural *reasoning* belongs in `docs/ARCHITECTURE.md`
(see below), not the README.

## Toolchain & edition policy

- Target the **latest stable Rust** (`rustup update stable`), currently tracking 1.9x /
  edition 2024. Do not add a `rust-toolchain.toml` pinning to an older channel without asking
  the user first — the intent is to always ride latest stable.
- Before finishing any change, run, per crate touched:
  ```sh
  cargo fmt --all
  cargo clippy --all-targets --all-features -- -D warnings
  cargo test
  cargo build --release
  ```
  A change is not done until these pass in every crate it touched. If `system-hub` and the
  root crate are both affected, run the full sequence in both directories (`cd system-hub`).
- Prefer idiomatic, modern std/Axum 0.7/Tokio patterns over hand-rolled alternatives:
  - Use `?` and typed errors (`thiserror` for library-style errors, `anyhow` only at the
    edges/`main`) instead of `.unwrap()`/`.expect()`/`panic!` outside of tests and true
    "cannot happen, verified at startup" invariants.
  - No `unwrap()` on I/O, network, parsing, or user-controlled input — that includes request
    bodies, query params, headers, WebSocket frames, and DB rows. `src/main.rs` currently
    `.unwrap()`s on `TcpListener::bind`/`axum::serve` and `db.rs` has `format!`-built SQL for
    dynamic `UPDATE`/`SELECT` clauses — treat these as tech debt to clean up opportunistically
    when you touch nearby code, not as a pattern to copy into new code.
  - Use `tracing` (already a dependency) instead of `println!`/`eprintln!`.
  - Don't hand-write parsers for things std or an existing dependency already does correctly —
    e.g. `auth.rs::urlencoding` is a bespoke percent-decoder; if you touch auth/query parsing,
    prefer a proper crate (`url`/`percent-encoding`, already pulled in transitively) instead of
    extending the hand-rolled version.
  - Async: no blocking calls (`std::fs`, `std::process::Command`, blocking locks) inside async
    fns on the Tokio runtime without `spawn_blocking` — several collectors already shell out
    (`dpkg`, `rpm`, `systemctl`, `ss`, `docker`), and **none of them are offloaded today**.
    The hub's `rusqlite` calls hold a `std::sync::Mutex` from async code everywhere except
    the push receiver, which runs its SQLite work through `spawn_blocking`
    (`system-hub/src/push.rs::on_blocking_pool`, awaited so work stays in order). Don't copy
    the blocking pattern. New shell-outs use `tokio::process::Command` or `spawn_blocking`,
    and fix the existing ones when you touch them.
  - Keep `unsafe` at zero. If a change seems to need it, stop and ask.

## Development discipline: TDD + DDD + adversaries + the Rosette

These four are mandatory for every non-trivial change to the Rust crates, not only "big"
ones. They build on the sections below: TDD replaces "write tests alongside the code" with
"write the test first", DDD sets where code lives, the Rosette sets how it reads, and the
adversaries make sure the author isn't the one grading the work.

Scope, so it isn't decided anew in each session:

- **Trivial** means no behaviour change: comments, docs, formatting, the wording of a log
  message, or a dependency bump with no API change. A trivial change skips the TDD loop and
  the adversaries, but still runs the toolchain gate.
- **Dashboards** (`static/index.html` in both crates) have no JS test harness. A dashboard
  change gets the XSS review from the Security section plus `rosette-auditor`, but no
  red-test requirement. The Rust code that serves the dashboard's data follows the full loop.

### Domain-Driven Design

The context map — which module belongs to which bounded context — and the glossary of
domain terms (the ubiquitous language) live in `docs/ARCHITECTURE.md` § Domain model. Read it
before adding a type, and update it in the same change when you add a context or a term, or
move a module between contexts.

- **Use the ubiquitous language.** Types, functions, test names and API fields use the
  glossary's terms. A new concept gets a glossary entry before it gets a type. Never coin a
  synonym for an existing term (e.g. `node`/`machine`/`target` for *system*, `sample` for
  *metric point*).
- **Keep the domain core pure.** Domain logic (alert evaluation and threshold/duration/cooldown
  rules, system status derivation, retention policy, snapshot → metric mapping) takes values
  and returns values. It contains no Axum types, `rusqlite`, `tokio`, `std::process`, `std::fs`,
  env reads or clock reads: pass `now` and config in as arguments. I/O lives in adapters:
  `routes/*` (HTTP/SSE/WS), `db.rs` (SQLite), `collectors/*` (sysinfo and shell-outs),
  `push.rs` and `collector.rs` (network).
- **Keep adapters thin.** A handler or adapter only parses input into domain values, calls
  the domain, and maps the result or error back to the wire. A business decision inside a
  handler closure, an SQL string or a collector's shell-out wrapper is a defect.
- **Parse, don't validate, at the boundary.** Untrusted input (request bodies, query params,
  headers, env vars, push frames, DB rows, agent JSON consumed by the hub) is converted into
  domain types once, at the edge, via `TryFrom` or constructors that reject invalid values.
  Use newtypes with private fields for constrained values (a percentage, a poll interval, a
  system URL) so an invalid one can't be constructed. Security validation (e.g. the SSRF URL
  check below) belongs in these constructors.
- **Wire and storage shapes are not the domain model.** Serde DTOs, SQLite rows and
  MessagePack push frames form an anti-corruption layer: convert explicitly to and from
  domain types at the edge. Today `models.rs` in both crates mixes the two (see
  `docs/ARCHITECTURE.md` § Open architectural questions). Separate them in code you touch,
  and don't add domain behaviour to a serde-derived DTO.
- **The push frame is a published contract between two contexts,** defined independently on
  each side (`src/push.rs` and `system-hub/src/push.rs` each declare their own
  `PushPayload`). The agent encodes it with `rmp_serde::to_vec`, which is **positional**:
  structs become MessagePack arrays with no field names, so field *order* is the contract.
  Renaming a field is harmless on the wire. Reordering, removing, or inserting a field
  anywhere but the end shifts every later value. `#[serde(default)]` only rescues a field
  that is missing at the very end, and an old hub may reject a frame with extra trailing
  elements. The hub silently drops frames that fail to decode. Change both sides in the same
  change, and add a round-trip test across the two declarations. Treat any frame change as
  mixed-version-breaking unless tested otherwise, and give it an RFC. Switching to
  `to_vec_named` (map encoding) is itself an RFC-level change.
- **Retrofit what you touch,** with the same ratchet as tests: you don't have to remodel the
  codebase in one pass, but every file you touch must end with a cleaner domain boundary than
  it started with, never a muddier one.

### The Rosette of Beautiful Code

Judge every diff on these eight dimensions: yourself during TDD phase 4 (refactor), and
the `rosette-auditor` before the change is called done.

1. **Storytelling** — A handler reads top to bottom as the request's story: extract →
   authorize → parse into domain → decide → respond. A collector reads as gather → parse →
   snapshot. A domain module reads as the rules it enforces.
2. **Simplicity** — Low cognitive load matters more than compact syntax. One function, one
   responsibility, within 30–50 lines of code (comments, blank lines and `#[cfg(test)]`
   modules don't count). Past that, extract the helper hiding inside the function rather than
   nesting another `match` or loop. A file nearing 500 lines of non-test code is a smell to
   investigate, not a quota to fill: never split into `db_part1.rs`/`db_part2.rs`, split only
   along a boundary that would exist anyway, and leave a cohesive file alone. An
   `#[allow(clippy::…)]` that silences a complexity lint (`too_many_arguments`,
   `type_complexity`, `cognitive_complexity`) is a finding, not a fix: introduce the value
   object it's asking for. Nothing enforces this mechanically yet; you and the auditor do.
3. **Clarity of Intent** — Model state and absence explicitly: enums instead of boolean flags
   or combinations of `Option`s, newtypes instead of bare `String`/`f32`/`u64`, and no
   sentinel values (`""`, `0`, `-1`, `"unknown"`). Make invalid states unrepresentable. Errors
   are typed enums, not strings.
4. **Expressiveness** — Idiomatic Rust: iterators where they read better than loops, `?`,
   `From`/`TryFrom` at boundaries, and exhaustive `match` over domain enums with no `_ =>`
   catch-all, so a new variant forces every site to decide. Comments explain *why*, never
   *what*.
5. **Purity** — Side effects (I/O, clock, env, shell-outs, DB, network, randomness) stay at
   the edges. Domain functions are deterministic given their arguments, so plain values are
   enough to test them.
6. **Sustainability** — Tests are table-driven: a `cases` array of `(name, input, expected)`,
   iterated, with the case name in the assertion message. Include rows for boundaries
   (exactly at the threshold), empty and malformed input, and each error variant. Test names
   state behaviour in the ubiquitous language.
7. **Durability** — A new capability (collector, alert metric, route, stored metric) attaches
   by adding a module or an enum variant, not by restructuring core modules. Agents and hub
   are deployed independently, so assume a mixed-version fleet on the push/poll contract.
8. **Creativity** — An elegant synthesis within the hard constraints (zero `unsafe`, no
   blocking on the runtime, OWASP, two independent crates), not the first thing that
   compiled.

### Test-Driven Development

All production code is written test-first, in a closed loop per behaviour:

1. **Contract and red test.** State the behaviour in the ubiquitous language, then write the
   test before any production code. Test the domain function directly where you can; use the
   `Router` (`oneshot`) or a real ephemeral server only when the behaviour lives in the
   adapter.
2. **Prove it red, then prove the red means something.** Run the targeted test
   (`cargo test <name>`); it must fail **on an assertion**, not on compilation. If the
   behaviour needs a new function or type, add its signature first with a stub body that
   compiles and returns a wrong value (`Default::default()`, an empty `Vec`, the wrong
   variant) — not `todo!()`, whose panic proves nothing about behaviour. Then launch the
   `red-test-adversary` subagent on the test. `DECORATION` (a cheat implementation passed) →
   rewrite the test and run the adversary again. `WEAK-RED` (fails on compilation or a panic
   rather than an assertion) → add the stub and re-run.
3. **Minimal green.** Write the least code that satisfies the test: no tidying nearby code,
   no speculative generality.
4. **Refactor under green.** Improve the code you just wrote against the Rosette and the DDD
   rules. The tests stay green throughout.
5. **Gate.** Run the toolchain sequence above in every touched crate, then run the
   `rosette-auditor` on the diff. Fix whatever the compiler, clippy or tests report; never
   report a change done while the gate is red.

Cases where red-first works differently:

- **Bug fixes** start with a test that reproduces the bug and fails *because of* the bug.
- **Characterisation tests** (backfilling tests for existing untested behaviour) pass on the
  first run by design: they pin current behaviour, so red-first doesn't apply. Instead,
  `red-test-adversary` attacks them in mutation mode: it breaks the behaviour in a temp copy
  and checks the test goes red. If you find surprising behaviour, report it; don't "fix" it
  inside the characterisation test, and don't describe it as intended in the docs.
- **Pure refactors** need no new red test. The existing tests are the contract and must be
  green before and after.

### Adversarial review

The author doesn't grade their own work. Three adversary subagents live in `.claude/agents/`.
They are read-only: they may run read-only commands and throwaway experiments in a temp
directory, and must never edit the repository. Running them is required:

| Adversary | When | Blocks the change on |
|---|---|---|
| `red-test-adversary` | after every new red test, before implementing (TDD phase 2), and on characterisation tests (mutation mode) | `DECORATION` |
| `rfc-adversary` | after drafting or materially amending an RFC, before it becomes `Accepted` | any unaddressed `CONFIRMED` |
| `rosette-auditor` | before reporting any non-trivial change done (TDD phase 5) | any `VIOLATED` |

- `CONFIRMED` / `VIOLATED`: fix it, by amending the RFC or changing the code.
- `PLAUSIBLE` / `AT-RISK`: decide, and write the decision down (in the RFC, or in your
  summary). A settled question that's recorded is worth more than one settled silently.
- Clean pass: say so in one line and name the attack that came closest.
- Don't re-run `rfc-adversary` on wording or factual corrections you just made to satisfy
  it; a second pass on your own fixes is theatre. Do re-run it if an amendment changes the
  design itself. `red-test-adversary` *is* re-run after a `DECORATION` or `WEAK-RED` rewrite.
- If a subagent can't be launched in the current environment, say so in your summary. Never
  substitute your own self-review and present it as the adversary's verdict.

Report each adversary's verdict in the change summary, next to the OWASP findings.

## Security: OWASP review on every change

Every change that touches routing, auth, request/response handling, WebSocket/SSE framing,
SQL, environment/config parsing, or the static dashboard HTML must be explicitly checked
against **OWASP Top 10 (Web, 2021)** and **OWASP API Security Top 10 (2023)** before you call
the work done. State in your summary which categories you checked and what you found — "N/A"
is fine, silence is not.

Known standing risks in this codebase — don't reintroduce or extend these patterns, and
flag/fix them if a change touches the surrounding code:

- **CORS is wide open** (`src/main.rs`, `system-hub/src/main.rs`): `CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any)`.
  This is API1/API8-adjacent (Broken Object Level Authorization has no origin boundary to help
  it) and Top-10 A05 (Security Misconfiguration). Don't widen it further; if you add
  credentialed requests anywhere, this combination becomes actively unsafe (browsers reject
  `Any` + credentials, but don't rely on that as your only control).
- **Token comparison must stay constant-time**, on both sides: `src/auth.rs::tokens_match`
  for `SYSTEM_AGENT_TOKEN` (RFC 0001) and `system-hub/src/push.rs::PushToken::accepts` for
  `HUB_PUSH_TOKEN` (RFC 0003) both use `subtle::ConstantTimeEq`. Never replace either with
  `==`/`!=`/`as_deref() ==`, which reopens a timing side-channel on the shared secret. No
  test can catch that regression, so review must. Top-10 A02 / API2.
- **The push system id is self-asserted** (`system-hub/src/push.rs::authenticate`): the hub
  trusts whatever `system_id` the handshake presents, and `GET /api/systems` lists every id.
  Anyone with the shared push token (or anyone, when `HUB_PUSH_TOKEN` is unset) can push as
  any registered system. This is API1 / A01. Don't build anything that relies on the push
  id as proof of identity; binding ids to per-system credentials needs an RFC.
- **Hub-side SSRF surface**: `POST /api/systems` on `system-hub` accepts an arbitrary `url`
  that the hub's poller (`collector.rs`) will then fetch on a schedule. This is API7 (SSRF) /
  Top-10 A10. Any change to system registration or the poller must consider whether the URL
  needs validation (block loopback/link-local/metadata-endpoint targets unless that's an
  intentional feature for this deployment model — ask the user if unsure).
- **Dynamic SQL string assembly** in `system-hub/src/db.rs` (`format!("UPDATE systems SET {}
  WHERE id = ?", sets.join(", "))` and the `WHERE {}` conditions builder) — values are bound
  as parameters so this isn't classic SQL injection today, but column/clause names are
  string-joined. Never let user input reach the joined fragment itself, only bound
  `?`-parameters; if a change adds a new dynamic field, keep the field *name* from a fixed
  Rust-side allowlist, never from request data.
- **Secrets in env vars** (`SYSTEM_AGENT_TOKEN`, `PUSH_TOKEN`, `HUB_PUSH_TOKEN`): never log
  them, never echo them back in API responses/error messages, never write them into
  `docs/ARCHITECTURE.md` or RFC examples with real-looking values.
- **No rate limiting / no request size limits** on either API — relevant to API4 (Unrestricted
  Resource Consumption). If you add an endpoint that accepts a body (registration, alert
  config), consider whether it needs a size cap.
- The dashboards (`static/index.html`) render server-provided strings (hostnames, process
  names, package names) into the DOM — check any change there for XSS (Top-10 A03) if it
  changes how data is inserted (prefer `textContent`/escaping over `innerHTML` with
  interpolated values).

Checklist to actually run through (skip categories that are genuinely not touched, but say
so):

**OWASP Top 10 Web (2021):** A01 Broken Access Control · A02 Cryptographic Failures · A03
Injection (incl. XSS) · A04 Insecure Design · A05 Security Misconfiguration · A06 Vulnerable
Components (check `cargo audit` / `cargo deny` if deps changed) · A07 Identification &
Authentication Failures · A08 Software & Data Integrity Failures · A09 Logging & Monitoring
Failures · A10 SSRF.

**OWASP API Security Top 10 (2023):** API1 Broken Object Level Authorization · API2 Broken
Authentication · API3 Broken Object Property Level Authorization (over/under-posting on JSON
bodies) · API4 Unrestricted Resource Consumption · API5 Broken Function Level Authorization ·
API6 Unrestricted Access to Sensitive Business Flows · API7 SSRF · API8 Security
Misconfiguration · API9 Improper Inventory Management (undocumented/zombie endpoints — keep
the README table current) · API10 Unsafe Consumption of APIs (the hub consuming agent
responses, and vice versa for push frames).

If `cargo-audit` or `cargo-deny` are available, run them when dependencies change:
```sh
cargo audit
```

## Testing: full coverage, including retrofitting existing code

The order of work is set by the TDD loop above. This section sets what the finished test
suite must cover.

- **New or modified code must ship with tests in the same change,** written first (see TDD).
  No exceptions for "just a small fix."
- **When you touch a file that has no tests, add tests for the existing untested behavior in
  that file first (or in the same commit), not just for your new lines.** The goal is
  monotonically increasing coverage — every file you touch should leave the repo with *more*
  tested surface than it found, never the same or less. You don't have to backfill the entire
  codebase in one pass; backfill what you touch, and prioritize `alerts.rs`, `auth.rs`,
  `collectors/*`, `routes/*`, and `system-hub/src/db.rs` first since they hold the actual
  logic (parsing, thresholds, SQL, auth) rather than glue.
- **Structure:**
  - Unit tests: `#[cfg(test)] mod tests { ... }` colocated at the bottom of the file under
    test, per standard Rust convention.
  - Integration tests: `tests/` directory at each crate root (create it — it doesn't exist
    yet) for anything that needs a running `Router`/`axum::serve` or a temp SQLite file. Use
    `axum::body::Body` + `tower::ServiceExt::oneshot` to test routes without binding a real
    socket; use `tempfile` (already a dev-dependency) for `db.rs` tests instead of touching the
    real `system-hub.db`.
  - Prefer pure, testable functions over logic buried in handler closures — e.g. alert
    threshold evaluation, MessagePack frame construction/parsing, and SQL clause building
    should be extractable and tested without spinning up a server.
- **What "covered" means here:** every public function with non-trivial logic (branches, error
  paths, parsing, threshold/comparison logic) has at least one test for the happy path and one
  for each realistic failure/edge case (empty input, malformed data, boundary values like
  exactly-90%-CPU, auth token present-but-wrong, DB row missing, etc).
- Run `cargo test` (and `cargo test` inside `system-hub/`) before considering any change done.
  If `cargo llvm-cov` or `cargo tarpaulin` is installed, use it to sanity-check coverage isn't
  regressing; don't install new tooling without asking first.

## Architecture documentation: `docs/ARCHITECTURE.md`

`docs/ARCHITECTURE.md` is the living architectural record — components, data flow, protocols,
storage schema, trust boundaries, and the rationale behind them.

- **Update it in the same change** whenever you: add/remove/rename a component, module, or
  route group; change the push/poll protocol or the MessagePack frame shape; change the SQLite
  schema; change auth/trust boundaries; or make a decision an RFC (below) covers.
- Keep it a *description of current reality*, not a changelog — don't append "as of 2026-07-26
  we changed X"; edit the relevant section in place so it always reads as "this is how the
  system works today." History belongs in git log and RFCs, not in this file.
- If a change has no architectural effect (bug fix, refactor with identical external behavior,
  test-only change), you don't need to touch it — say so explicitly rather than leaving it
  ambiguous whether you forgot.

## RFCs for new changes: `rfcs/`

Any **new feature, protocol change, schema change, breaking API change, or other design
decision with lasting consequences** gets an RFC before (or alongside, for small-enough
same-session work) implementation. Trivial fixes, dependency bumps, formatting, and pure
test-additions do not need one.

- File naming: `rfcs/NNNN-short-kebab-title.md`, zero-padded 4-digit sequential number. Check
  the highest existing number in `rfcs/` before assigning the next one.
- Use `rfcs/0000-template.md` as the starting structure.
- An RFC covers: problem/motivation, proposed design, domain impact (bounded contexts
  touched, glossary terms added or changed), alternatives considered, security implications
  (explicitly run through the OWASP categories above), testing plan, and impact on
  `docs/ARCHITECTURE.md` (which sections will change).
- Run `rfc-adversary` on the draft before moving it to `Accepted` (see Adversarial review).
- Mark RFC status at the top: `Draft` → `Accepted` → `Implemented` (or `Rejected`/`Superseded`).
  Update the status as the corresponding work lands; don't leave an implemented change with a
  stale `Draft` RFC.

## Per-change workflow checklist

For any non-trivial change, before reporting it as done:

1. If the change is a new feature/protocol/schema/breaking change, write or update the
   corresponding `rfcs/NNNN-*.md`, run `rfc-adversary` on it, act on the findings, and set its
   status.
2. Backfill characterisation tests for untested existing behaviour in the files you'll touch.
3. For each behaviour: write the red test, prove it red, and have `red-test-adversary` attack
   it (TDD phases 1–2).
4. Implement minimally, then refactor against the Rosette and the DDD rules, following the
   Rust conventions above (TDD phases 3–4).
5. Run `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`,
   `cargo test`, `cargo build --release` in every crate touched, then run `rosette-auditor`
   on the diff (TDD phase 5).
6. Run the OWASP checklist above and state findings, even if "no relevant category touched."
7. Update `docs/ARCHITECTURE.md` if the change affects architecture (including § Domain
   model when contexts or glossary terms change); otherwise note explicitly that it doesn't.
8. Update `README.md`'s endpoint/config tables if you added, removed, or changed an
   externally-visible endpoint or environment variable.
9. In the summary, report each adversary's verdict next to the OWASP findings.
