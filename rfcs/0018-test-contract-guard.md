# RFC 0018: The Test-Contract Guard

- Status: Implemented
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

### 1. Frozen tests and the contract

`PostToolUse` on `Bash`: when the command ran tests (a pipeline segment whose command is
`cargo test`/`cargo nextest run`, or `node …/xss.mjs`, or `python3 …/test_tdd_guard.py`), the
guard scans every protected file and records, per test function, a digest of its source
normalised so that `cargo fmt` is not a change: comments dropped, whitespace outside string
literals reduced to the one space that keeps two words apart, string literals kept verbatim
(an expected `"1.5 GB"` becoming `"1.5GB"` *is* a change). Protected files: `src/**/*.rs`,
`tests/*.rs`, `system-hub/src/**/*.rs`, `system-hub/tests/*.rs`,
`system-hub/dashboard-tests/*.mjs`, `.claude/hooks/test_*.py`. A *test function* is a `fn`
preceded by `#[test]`, `#[tokio::test(…)]`, `#[rstest]`, `#[test_case]` or
`#[test_log::test]`; its extent runs from the first line of its contiguous attribute block
(so an attribute added *above* `#[test]` is part of the test) to its closing brace, found by
brace matching over a scanner that classifies every character as code, string/char literal
or comment. The rest of a `#[cfg(test)] mod` region (fixtures, helpers), the rest of an
integration-test file, and the whole of an `.mjs` or guard-test file form the *helper
surface*, recorded as the normalised text split into chunks at `{`, `}` and `;`, so a
reflowed helper is the same helper and a deleted comment is not a removal.

A test is **under contract** once it has been graded, i.e. it has an *attacked* record (§ 3),
or it exists by name in `HEAD`. A test that has been run but not graded is a draft: the
guard freezes it (so the commit gate can see whether it was run since its last edit) but lets
the author change it. This is what makes the phase-1 loop (write, run, fix the typo, run
again) and the bug-fix flow (add the reproducing row, run, grade) work without ceremony,
while phase 3 (after `EVIDENCE`) and every pre-existing test are locked.

`PreToolUse` on `Edit | Write | MultiEdit`: the guard projects the file as it would read after
the call and refuses (exit 2, reason on stderr) when a test under contract would be modified,
renamed or removed; when, in a file that exists in `HEAD`, a helper chunk would be removed or
a chunk would be added that could neutralise the tests (`#[cfg(…)]`, `#[should_panic]`,
`#[ignore]`, `macro_rules!`, a `fn` whose name is a production function's in the same file,
a `use` not from `super` that imports a production function's name, and in `.mjs`
`process.exit(0)`, `.only(`, `skip`, `todo`, …); or when a bare `#[ignore]` (no reason)
would be added anywhere. Adding a test, a helper or a check is not refused; adding a row to a
test under contract is a modification of it. Changing production code in the same file is
not refused.

`PreToolUse` on `Bash`: a pipeline segment that writes in place (`sed -i`, `perl -i`, `tee`,
`>`/`>>`, `cp`/`mv`/`install`/`rm`, `patch`, `git apply`) **and** names a `.rs`/`.mjs` path
that resolves inside the repository is refused, so test edits go through the tools the guard
can read. A path outside the repository (the adversaries' sandbox under `$XDG_CACHE_HOME`) is
not the repository's, and such a command is allowed.

### 2. Releases

A test under contract is changed by releasing it first:

    python3 .claude/hooks/tdd_guard.py release <file> [<test> …] --reason TEXT --authority adversary|user|author

`adversary` quotes a `DECORATION`/`WEAK-RED` verdict or a named missing row; `user` quotes
the user; `author` is for a strengthening only (a new row, a new assertion, the call-site
follow-through of a signature change) and never for a changed expectation or a removed row,
which `CLAUDE.md` reserves to the other two. The release is appended to
`.claude/tdd-state/releases.log` and opens only the named tests (or the file's whole surface
when no test is named). The next successful `git commit` consumes open releases, logging them
as consumed. A released test is re-frozen by its next run and, because its digest changed,
loses its *attacked* record and needs the adversary again.

### 3. The commit gate and adversary records

`PreToolUse` on `Bash` whose segment is a `git` command with the `commit` subcommand (after
any `-c`/`-C`/`--no-pager` options): the guard refuses the commit while, over the union of
the protected files in `HEAD`, the index and the worktree,

- a test function differs from, or is missing from, its `HEAD` version in the index or the
  worktree without an open release; a file that `HEAD` has and the worktree does not counts
  as every one of its tests removed (`<file>::*` releases it);
- a helper chunk of a `HEAD` file is gone, or a neutralising chunk was added, without a
  release;
- a test function that is new or changed versus `HEAD` has a digest the ledger has not seen,
  i.e. it has not been run since its last edit;
- such a test has no *attacked* record;
- any `.rs` file is modified and no audit record is newer than the last Edit/Write of a
  `.rs` file (`PostToolUse` on `Edit | Write | MultiEdit` records the last edit).

Records come from verdicts, never from the fact that something ran. `PostToolUse` on
`Agent` with `subagent_type` `red-test-adversary` reads the report's `VERDICT:` lines (the
worst one decides): `EVIDENCE` grades the tests the prompt named as `file::name`, or every
ungraded test when it named none; `DECORATION`/`WEAK-RED` ungrade the named tests, or every
test graded in this checkout. `rosette-auditor`'s `HOLDS`/`AT-RISK` set the audit record and
`VIOLATED` clears it. A report with no verdict records nothing and says so. The in-process
checks `CLAUDE.md` permits at medium effort are declarations with the same vocabulary, so a
declaration carries a verdict and the closest attack, and a `DECORATION` declared in-process
ungrades like the subagent's would:

    python3 .claude/hooks/tdd_guard.py attacked --in-process --verdict EVIDENCE --closest '…' [--tests file::name …]
    python3 .claude/hooks/tdd_guard.py audited  --in-process --verdict HOLDS
    python3 .claude/hooks/tdd_guard.py trivial  --reason '…'

`trivial` records, in place of an audit, that a `.rs` change has no behaviour (comments,
formatting, log wording), which `CLAUDE.md` exempts from the adversaries. Tests that exist
with the same digest in `HEAD` are graded on freeze: they were reviewed when they landed.

### The ledger

`ledger.json` is written atomically (temp file and `os.replace`). It carries a version; an
older version is rebuilt on the next read, keeping releases and the audit and edit records
and logging the rebuild. An unreadable ledger is never reset: the hook refuses and says so,
and deleting it is a deliberate, reportable act.

### Threat model, stated plainly

The guard stops the author's shortcuts, not a determined bypass: the ledger can be deleted,
the hook edited, and a tool the hooks don't wrap (an editor, a Python one-liner) is invisible
until the commit gate, which compares against `HEAD` and so catches what those did unless
the ledger was also tampered with. `git commit --no-verify` is irrelevant: the hook is Claude
Code's, not git's. The in-process declarations are claims; their value is that a claim with
a verdict and a named attack is checkable and a silent edit is not. `CLAUDE.md` forbids
editing the guard or `settings.json` inside a change to the crates.

## Domain impact

None at runtime. No bounded context, glossary term, push frame or poll response changes.
Development vocabulary gains *under contract*, *release*, *attacked record* and *commit
gate*, defined in `CLAUDE.md` § Tests are the contract, not in the domain glossary.

## Alternatives considered

- **Prose only (status quo).** Rejected: this is the rule a prose instruction demonstrably
  cannot hold, because the actor it constrains is the one applying it.
- **A git `pre-commit` hook.** Rejected as the primary mechanism: it fires after the damage
  is written and cannot distinguish adding a test from weakening one without the ledger;
  and `.git/hooks` is not committed, so it would not travel with the repository. The commit
  gate here is the same idea inside the Claude hook, with the ledger to tell the two apart.
- **Freeze every run test, graded or not** (the first draft). Rejected after review: the
  first run of a new test routinely fails on the test's own typo, and the draft would then
  need a release with no authority that applies; the bug-fix row and the backfill row had
  the same problem. Grading is the right moment: it is when a second reader has said the
  test is evidence.
- **Freeze on `red-test-adversary` completion only.** Rejected: tests that are never attacked
  (pre-existing ones) would never be protected, and the ledger would not know whether a
  draft had been run since its last edit. Freezing on every run and locking on grade keeps
  both.
- **Block production edits while a new test is ungraded.** Rejected: phase 2 legitimately
  adds a compiling stub to production code before the adversary runs. The commit gate
  enforces the same requirement at the point where it can be told apart.
- **Hash the whole `#[cfg(test)]` region.** Rejected: adding a test would then be
  indistinguishable from modifying one. Per-function digests plus an additive-only helper
  surface keep additions free.
- **Line-exact helper surface** (the first draft). Rejected after review: `rustfmt` reflows
  and comment deletions read as removals. Chunks of normalised text are formatting-immune.
- **Diff-based "additive only" for Rust tests.** Rejected: an addition can weaken a Rust
  test (`return;` as the first statement, an attribute above `#[test]`). Additive-only is
  used only for the helper surface, with the neutralising-chunk list on top.
- **A Rust syntax parser (`syn`) instead of line scanning.** Rejected for now: it would need a
  compiled helper or a Python dependency; the scanner finds all 682 test functions in the
  current tree and is tested against braces in string, raw-string and char literals and in
  comments, one-line tests, and doc comments between attribute and `fn`. If it misses a
  shape, the commit gate's `HEAD` comparison still sees the file.

## Security implications

No runtime code changes: no routes, auth, SQL, framing, config parsing or dashboard HTML.
A01–A10 and API1–API10 therefore do not apply to the system's surface. The change does add
code that runs on the developer's machine on every tool call, so:

- **Input the hook reads** is Claude Code's hook JSON (tool name, file path, edit strings,
  shell command) and the working tree. File paths are resolved and must lie inside the
  repository root, or the hook does nothing. The guard never executes anything from the
  input; `git` is invoked with fixed argument lists, never through a shell.
- **The ledger** holds digests, test names, file paths, reasons, verdicts, and the
  normalised helper surface of every test region — that is, a copy of the fixture and
  helper source (about 500 KB on the current tree), which is where a test token would live
  if one did. It is gitignored and never read by the binaries. Hook stderr can quote the
  first few removed chunks of helper code, cut to 60 characters, in a refusal. Fixtures must
  not hold real secrets (already a rule); the ledger does not change what a fixture exposes,
  only where a copy of it sits.
- **A08 (integrity)**: project hooks run for anyone who opens the checkout in Claude Code,
  without a trust prompt. The hook is committed, readable, stdlib-only and tested; changing it
  is reviewable in the diff like any code, and `__pycache__/` is gitignored so no bytecode
  ships beside it. This is the same trust model as the committed `.claude/agents/`.
- **Denial of service on the developer**: the hooks are bounded by `timeout` in
  `settings.json` (30 s edits, 60 s shell); the full-tree freeze takes about 4 s and the
  commit gate about 5 s on the current tree (682 tests); an edit check reads one file. A timeout fails the hook, which
  Claude Code treats as non-blocking, so a slow machine degrades to no guard, not to a locked
  session.
- A06: no dependency changes (Python standard library only; see Rollout for the missing-
  Python case).

## Testing plan

`.claude/hooks/test_tdd_guard.py` (plain `unittest`, no dependencies), table-driven, each
case running the real script as a subprocess with the hook's JSON on stdin in a throwaway git
repository: the scanner (extents across braces in literals, the attribute block above
`#[test]`, one-line tests, doc comments, commented-out attributes; digest stability under
reformatting and sensitivity to string contents; helper chunks surviving reflow and ignoring
comments; path kinds); commands (in-place writes scoped to the repository, the sandbox
allowed; test-run and commit detection including `git -c`, `--no-pager` and text that merely
mentions them); the edit gate (twenty-five edits to a frozen tree with the expected verdict,
among them the four neutralising additions and the string-space change; Write projection;
a draft free until graded, in a new file and in a `HEAD` file; release scope; release
validation; shell refusal; the unreadable ledger; the older ledger); records (only
`EVIDENCE` grades, later verdicts ungrade, a report without a verdict records nothing, a
subagent grades only the tests its prompt names, the worst verdict decides); the commit gate
(clean tree; bypassed worktree edit; staged weakening with the worktree restored; deleted
test file; run → graded → audited in order; an edit after the audit; `trivial`; in-process
declaration logged; release consumed by the commit; freeze after a bypass warns only for
tests under contract).

The tests are green by design (they pin the guard's behaviour), so per `CLAUDE.md` they were
attacked in mutation mode: twenty mutants, one per decision the review named (empty
`compare_fns`, word-space normalisation off, modification check off, helper-removal check
off, shell guard off, attacked check off, audit check off, neutralising-chunk list off,
every test "pre-existing in HEAD", audit ordering ignored, every verdict grades, first verdict
instead of worst, `HEAD`-only files skipped, index ignored, `git -c` not skipped, string
literals not preserved, non-atomic save, corrupt ledger reset, attribute block not climbed,
`use` shadowing off). Nineteen fail at least one test; the twentieth (`Path.rename` in
place of `os.replace`) is equivalent on POSIX, where both are an atomic replace, so no test
can tell them apart there, and it is kept as `os.replace` for Windows, where `rename` onto an
existing file fails. The summary reports the run.

The guard was also run against the real tree: it freezes 682 test functions, the same count
as `grep` finds `#[test]`/`#[tokio::test]` attributes, and the commit gate is clean on
`HEAD`. CI (`.github/workflows/ci.yml`) runs the guard's tests as a job of their own.

## Impact on `docs/ARCHITECTURE.md`

§ Testing architecture names the guard beside clippy, `cargo test` and `xss.mjs` as what
gates a change, and that its tests run in CI. Nothing else: the document describes the
running system, and the guard is development tooling documented in `CLAUDE.md` § Tests are
the contract and in the script's docstring.

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
- `#[cfg(test)]` on a single item outside a `mod` (an `impl`, a `fn`, a `pub use`; eleven
  such helpers exist today) is outside the helper surface. The commit gate still sees the
  file against `HEAD` for its test functions; the helper itself is not protected. Recorded
  here as an accepted gap rather than widening the scanner before a case needs it.

## Adversarial review

`rfc-adversary` ran on the first draft (which froze every run test and compared helpers
line by line) and returned twelve `CONFIRMED` findings. All were acted on; the design above
is the result:

1. CONFIRMED — attributes above `#[test]`, `#[cfg(any())]` between `#[cfg(test)]` and
   `mod`, a shadowing `fn` or `macro_rules!` in the test module all passed both gates as
   "added lines". Fixed: the attribute block is part of the test's extent, and added helper
   chunks are checked against a neutralising list and against the file's production
   function names (`fn` and non-`super` `use`).
2. CONFIRMED — the shell guard refused the `red-test-adversary`'s documented sandbox writes.
   Fixed: only paths that resolve inside the repository are refused.
3. CONFIRMED — `DECORATION`/`VIOLATED` were recorded as if they cleared the gate, and one
   adversary run graded every ungraded test. Fixed: verdicts decide, the worst one wins, and
   a subagent grades the tests its prompt names.
4. CONFIRMED — "adding a row is free" was false, and the bug-fix row needed a release with
   no applicable authority. Fixed: the text now says a row changes the test; a draft is free
   until graded; `--authority author` covers strengthening a graded test.
5. CONFIRMED — any text matching `cargo test` froze the half-written draft. Fixed: contract
   starts at grading; test-run detection looks at the segment's command, not its text.
6. CONFIRMED — trivial `.rs` changes demanded an audit record. Fixed: `trivial --reason`.
7. CONFIRMED — a deleted test file and a staged-then-restored weakening passed the commit
   gate; `git --no-pager commit` was missed and `grep 'git commit'` ran the gate. Fixed:
   `HEAD` ∪ index ∪ worktree, tokenised git command detection, `rm`/`patch`/`git apply`
   added to the writers.
8. CONFIRMED — `cargo fmt` and comment deletions were helper "removals". Fixed: normalised
   chunks, comments dropped.
9. CONFIRMED — whitespace inside string literals was stripped from the digest. Fixed: the
   scanner keeps literals verbatim.
10. CONFIRMED — a corrupt ledger was silently reset and saves were not atomic. Fixed.
11. CONFIRMED — five of eight extra mutants survived; ordering, index and verdict logic had
    no tests. Fixed: the cases listed under Testing plan; twenty mutants now die.
12. CONFIRMED — bytecode was committed, the guard's tests ran nowhere in CI, the ledger
    description understated what it copies, ARCHITECTURE's gate sentence was incomplete.
    Fixed: `__pycache__/` ignored and untracked, a CI job, the security section and
    ARCHITECTURE corrected.

Settled and recorded, not fixed: the single-item `#[cfg(test)]` helpers (Rollout, last
bullet), and that the in-process declarations remain claims (Threat model). Closest to
surviving, per the adversary: the brace matcher, which found every test in the tree.
