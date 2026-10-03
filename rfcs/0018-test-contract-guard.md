# RFC 0018: The Test-Contract Guard

- Status: Draft
- Author: Claude (pairing with pietrangelo.masala)
- Date: 2026-10-03
- Affects: both (development process and Claude Code configuration; no runtime behaviour changes)

## Motivation

RFC 0002 made TDD, DDD and adversarial review mandatory, and `CLAUDE.md` says the test is
written first and the implementation is written to meet it. Nothing enforces the second half.
An agent in phase 3 ("minimal green") that cannot make a test pass has a cheaper move than
fixing the code: edit the expectation, drop the row that fails, loosen `assert_eq!` to
`assert!(result.is_ok())`, or add `#[ignore]`. The test still "passes", the summary still says
"all green", and the contract the red phase established is gone. The same move is available on
characterisation tests and on tests that already exist in `HEAD`. RFC 0002 § Alternatives
deferred mechanical enforcement until the rules had been applied by hand for a while; that
time has passed, and the failure mode above is exactly the one a prose rule cannot stop,
because it is the author grading their own work.

The adversaries have the same gap. `CLAUDE.md` requires `red-test-adversary` before the
implementation and `rosette-auditor` before "done", but nothing records whether either ran,
so a summary can claim a verdict that was never produced.

## Proposed design

A single Python 3 script, `.claude/hooks/tdd_guard.py` (standard library only), run by Claude
Code hooks declared in `.claude/settings.json`, which is committed and so applies to every
checkout. It keeps a ledger in `.claude/tdd-state/` (gitignored). Three mechanisms:

### 1. Frozen tests

`PostToolUse` on `Bash`: when the command ran tests (`cargo test`, `cargo nextest run`,
`dashboard-tests/xss.mjs`, the guard's own tests), the guard scans every protected file and
records, per test function, a digest of its source with all whitespace removed (so
`cargo fmt` is not a change). Protected files: `src/**/*.rs`, `tests/*.rs`,
`system-hub/src/**/*.rs`, `system-hub/tests/*.rs`, `system-hub/dashboard-tests/*.mjs`,
`.claude/hooks/test_*.py`. A *test function* is a `fn` preceded by `#[test]`,
`#[tokio::test(…)]`, `#[rstest]`, `#[test_case]` or `#[test_log::test]`; its extent is found
by brace matching that skips string, raw-string and char literals and comments. The rest of a
`#[cfg(test)] mod` region (fixtures, helpers), the rest of an integration-test file, and the
whole of an `.mjs` or guard-test file form the *helper surface*, recorded as a list of lines.

`PreToolUse` on `Edit | Write | MultiEdit`: the guard projects the file as it would read after
the call and refuses (exit 2, reason on stderr) when a frozen test function would be modified,
renamed or removed; when a helper-surface line would be removed; when a `.mjs` file would gain
a line matching an escape hatch (`process.exit(0)`, `.only(`, `skip`, `todo`, …); or when a
bare `#[ignore]` (no reason) would be added. Adding a test, a row, a helper or a check is not
refused. Changing production code in the same file is not refused.

`PreToolUse` on `Bash`: a pipeline segment that writes in place (`sed -i`, `perl -i`, `tee`,
`>`/`>>`, `cp`/`mv`/`install`) **and** names a `.rs`/`.mjs` path is refused, so test edits go
through the tools the guard can read.

### 2. Releases

A frozen test is changed by releasing it first:

    python3 .claude/hooks/tdd_guard.py release <file> [<test> …] --reason TEXT --authority adversary|user

The release is appended to `.claude/tdd-state/releases.log` and opens only the named tests
(or the file's whole surface when no test is named). The next successful `git commit` consumes
open releases, logging them as consumed. A released test is re-frozen by its next run and,
because its digest changed, loses its *attacked* record (below) and needs the adversary again.

### 3. The commit gate and adversary records

`PreToolUse` on `Bash` matching `git commit`: the guard refuses the commit while

- a test function in the tree differs from, or is missing from, its `HEAD` version without an
  open release (this catches edits that bypassed the hooks: Python one-liners, editors);
- a test function that is new or changed versus `HEAD` has a digest the ledger has not seen,
  i.e. it has not been run since its last edit;
- such a test has no *attacked* record;
- any `.rs` file is modified and no `rosette-auditor` record is newer than the last Edit/Write
  of a `.rs` file (`PostToolUse` on `Edit | Write | MultiEdit` records the last edit).

Records: `PostToolUse` on `Agent` with `subagent_type` `red-test-adversary` marks every
frozen test without a record as attacked (the report's `VERDICT:` line is stored);
`rosette-auditor` sets the last-audit record. The in-process checks `CLAUDE.md` permits at
medium effort are recorded by explicit declaration, which stores what it is given:

    python3 .claude/hooks/tdd_guard.py attacked --in-process --verdict EVIDENCE --closest '…'
    python3 .claude/hooks/tdd_guard.py audited  --in-process --verdict HOLDS

Tests that exist byte-for-byte (modulo whitespace) in `HEAD` are marked attacked on freeze:
they were reviewed when they landed.

### Threat model, stated plainly

The guard stops the author's shortcuts, not a determined bypass: the ledger can be deleted,
the hook edited, `git commit --no-verify` is irrelevant because the hook is Claude Code's,
not git's, and a tool the hooks don't wrap (an editor) is invisible until the commit gate.
Its value is that a shortcut becomes an explicit, logged act (`release`, `attacked`,
`audited`) that the summary has to quote, instead of a silent edit. `CLAUDE.md` forbids
editing the guard or `settings.json` inside a change to the crates.

## Domain impact

None at runtime. No bounded context, glossary term, push frame or poll response changes.
Development vocabulary gains *frozen test*, *release*, *attacked record* and *commit gate*,
defined in `CLAUDE.md` § Tests are the contract, not in the domain glossary.

## Alternatives considered

- **Prose only (status quo).** Rejected: this is the rule a prose instruction demonstrably
  cannot hold, because the actor it constrains is the one applying it.
- **A git `pre-commit` hook.** Rejected as the primary mechanism: it fires after the damage
  is written and cannot distinguish adding a test from weakening one without the ledger;
  and `.git/hooks` is not committed, so it would not travel with the repository. The commit
  gate here is the same idea inside the Claude hook, with the ledger to tell the two apart.
- **Freeze on `red-test-adversary` completion instead of on every test run.** Rejected: it
  leaves the window between proving red and running the adversary open, and tests that are
  never attacked (characterisation, pre-existing) would never freeze. "A test that has been
  run is a contract" is simpler to state and to check.
- **Block production edits while a new test is pending the adversary.** Rejected: phase 2
  legitimately adds a compiling stub to production code before the adversary runs. The
  commit gate enforces the same requirement at the point where it can be told apart.
- **Hash the whole `#[cfg(test)]` region.** Rejected: adding a test would then be
  indistinguishable from modifying one. Per-function digests plus an additive-only helper
  surface keep additions free.
- **Diff-based "additive only" for Rust tests.** Rejected: an addition can weaken a Rust
  test (`return;` as the first statement, `#[ignore]`). Additive-only is used only where the
  alternative is no protection at all (`.mjs`, fixtures), with the escape-hatch list on top.
- **A Rust syntax parser (`syn`) instead of line scanning.** Rejected for now: it would need a
  compiled helper or a Python dependency; the scanner finds all 682 test functions in the
  current tree and is tested against braces in string and char literals and comments. If it
  misses a shape, the commit gate's `HEAD` comparison still sees the file.

## Security implications

No runtime code changes: no routes, auth, SQL, framing, config parsing or dashboard HTML.
A01–A10 and API1–API10 therefore do not apply to the system's surface. The change does add
code that runs on the developer's machine on every tool call, so:

- **Input the hook reads** is Claude Code's hook JSON (tool name, file path, edit strings,
  shell command) and the working tree. File paths are resolved and must lie inside the
  repository root, or the hook does nothing. The guard never executes anything from the
  input; `git` is invoked with fixed argument lists, never through a shell.
- **The ledger** holds digests, test names, file paths, reasons and verdicts: no secrets. It
  is gitignored and never read by the binaries. Hook stderr can contain test names and the
  first few removed helper lines; a fixture that embedded a secret would surface a fragment
  of it in the refusal text. Fixtures must not hold real secrets (already a rule).
- **A08 (integrity)**: project hooks run for anyone who opens the checkout in Claude Code,
  without a trust prompt. The hook is committed, readable, stdlib-only and tested; changing it
  is reviewable in the diff like any code. This is the same trust model as the committed
  `.claude/agents/`.
- **Denial of service on the developer**: the hooks are bounded by `timeout` in
  `settings.json` (30 s edits, 60 s shell); the full-tree freeze takes about 1.6 s and the
  commit gate about 2.8 s on the current tree (682 tests). A timeout fails the hook, which
  Claude Code treats as non-blocking, so a slow machine degrades to no guard, not to a locked
  session.
- A06: no dependency changes (Python standard library only; Python 3 is already required for
  nothing else, and the hooks exit 0 if the script cannot run — see Rollout).

## Testing plan

`.claude/hooks/test_tdd_guard.py` (plain `unittest`, no dependencies), table-driven: parsing
(test-function extent across braces in literals, digest stability under reformatting, helper
surface, path kinds, shell-write and commit detection); the edit gate (fifteen edits to a
frozen tree, each with the expected verdict; Write projection; an unfrozen file is free until
run; release opens exactly the named test; release needs reason and authority; shell writes
refused); the commit gate (clean tree commits; a bypassed edit is caught against `HEAD`; a new
test must be run, then attacked, then audited; in-process declarations count and are logged;
a release lets a changed test commit and is consumed; a freeze after a bypass warns). Each
case runs the real script as a subprocess with the hook's JSON on stdin, in a throwaway git
repository.

The tests are green by design (they pin the guard's behaviour), so per `CLAUDE.md` they were
attacked in mutation mode: eight mutants (empty `compare`, digest keeping whitespace,
modification check disabled, helper-removal check disabled, shell guard disabled, attacked
check removed, audit check removed, escape-hatch list emptied) each fail at least one test.
The result is reported in the change summary.

The guard was also run against the real tree: it freezes 682 test functions, the same count
as `grep` finds `#[test]`/`#[tokio::test]` attributes, and the commit gate is clean on `HEAD`.

## Impact on `docs/ARCHITECTURE.md`

None. The document describes the running system; the guard is development tooling and is
documented in `CLAUDE.md` § Tests are the contract and in the script's docstring.

## Rollout / migration notes

- `.claude/settings.json` is read when a Claude Code session starts; sessions already open
  when this lands must be restarted to get the hooks.
- The ledger starts empty in every checkout. Until the first test run, the edit gate has
  nothing to protect; the commit gate's comparison against `HEAD` applies from the first
  commit regardless.
- Python 3 (3.9+) must be on `PATH` as `python3`. If it is missing, Claude Code reports the
  hook command as failed and continues, so the guard is absent rather than blocking; the
  summary rule in `CLAUDE.md` (quote releases, name adversary records) is what makes that
  absence visible.
- Organisations that set `allowManagedHooksOnly` disable project hooks entirely.

## Adversarial review

`rfc-adversary` ran on the draft; findings and their resolution are recorded below once it
has.
