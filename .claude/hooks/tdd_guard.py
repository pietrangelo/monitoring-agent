#!/usr/bin/env python3
"""The test-contract guard: Claude Code hooks that keep tests from bending to the code.

CLAUDE.md § Development discipline says the test is written first and is the contract the
implementation has to meet. This script is the mechanical half of that rule (RFC 0018). It
runs as a hook (see `.claude/settings.json`) and keeps a local ledger in `.claude/tdd-state/`.

A test is *under contract* once it has been graded: `red-test-adversary` returned EVIDENCE
on it (recorded automatically from the subagent, or declared with `attacked --in-process`),
or it already exists in `HEAD`. A draft that nobody has graded is the author's to change.

- every `cargo test` / `xss.mjs` run *freezes* each test function in the tree: a digest of
  its source with whitespace removed outside string literals, so `cargo fmt` is not a change
  but an expected `"1.5 GB"` becoming `"1.5GB"` is;
- an Edit/Write that changes, renames, deletes or `#[ignore]`s a test under contract is
  refused unless the test was *released* first, with a reason and an authority
  (`release --authority adversary|user|author`); a release is logged and must be quoted in
  the change summary;
- fixture and helper code around the tests (the rest of a `#[cfg(test)]` region or an
  integration-test file, the hub dashboard's `xss.mjs`, this guard's own tests) is
  *additive-only* once the file is in `HEAD`: items may be added, never removed, and an
  addition that could neutralise the tests (`#[cfg(…)]`, `#[should_panic]`, `#[ignore]`,
  `macro_rules!`, a `fn` shadowing a production function, `process.exit(0)` …) is refused;
- `git commit` is refused while a test differs from `HEAD` (in the index or the worktree)
  without a release, while a test file in `HEAD` is gone without a release, while a new or
  changed test has not been run or has no EVIDENCE record, or while a `.rs` file changed and
  no `rosette-auditor` record (or `trivial` declaration) is newer than the last edit;
- a shell command that writes a `.rs`/`.mjs` file *inside the repository* in place is
  refused, so test edits go through Edit/Write where the guard can read them. Writing a
  copy elsewhere (the adversaries' sandbox) is fine. The commit gate is the backstop.

The guard protects against the author's own shortcuts, not against a determined attacker:
anyone can delete the ledger. Its value is that a shortcut becomes a visible, logged act.

Subcommands (hooks read Claude Code's JSON from stdin):

    pre-edit        PreToolUse  Edit | Write | MultiEdit
    post-edit       PostToolUse Edit | Write | MultiEdit (records the last .rs edit)
    pre-bash        PreToolUse  Bash
    post-bash       PostToolUse Bash        (freezes after a test run; consumes releases after a commit)
    post-agent      PostToolUse Agent       (records an adversary's verdict)
    freeze          manual: freeze every test in the tree now
    release FILE [TEST ...] --reason TEXT --authority adversary|user|author
    attacked --in-process --verdict V --closest TEXT [--mode red|mutation] [--tests FILE::NAME ...]
    audited  --in-process --verdict V
    trivial  --reason TEXT      (a no-behaviour-change .rs edit, in place of an audit)
    status | check-commit

Exit code 2 with a message on stderr is how a hook refuses a tool call.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import re
import shlex
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

STATE_DIR = Path(".claude") / "tdd-state"
LEDGER = STATE_DIR / "ledger.json"
RELEASE_LOG = STATE_DIR / "releases.log"
EVENT_LOG = STATE_DIR / "events.log"
LEDGER_VERSION = 3  # bump when the digest or the surface changes shape; an older ledger is rebuilt

TEST_ATTRIBUTE = re.compile(r"^\s*#\[\s*(tokio::test|test|rstest|test_case|test_log::test)\b")
ATTRIBUTE_LINE = re.compile(r"^\s*#\[")
COMMENT_LINE = re.compile(r"^\s*//")
FN_DECL = re.compile(r"(?:^|\s)(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)")
CFG_TEST = re.compile(r"^\s*#\[\s*cfg\s*\(\s*test\s*\)\s*\]")
MOD_LINE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+[A-Za-z_][A-Za-z0-9_]*\s*\{")
IGNORE_WITHOUT_REASON = re.compile(r"^\s*#\[\s*ignore\s*\]")

ADDITIVE_ONLY_GLOBS = ("system-hub/dashboard-tests/*.mjs", ".claude/hooks/test_*.py")
INTEGRATION_TEST_DIRS = ("tests", "system-hub/tests")
RUST_SOURCE_DIRS = ("src", "tests", "system-hub/src", "system-hub/tests")

# An added helper chunk that could neutralise the tests around it (chunks are whitespace-free).
RUST_HATCH = re.compile(r"^#\[(?:cfg|should_panic|ignore)|macro_rules!")
JS_HATCH = re.compile(r"process\.exit\(0\)|\.only\(|\bxit\(|\bxdescribe\(|\bskip\b|\btodo\b")

WRITING_COMMAND = re.compile(
    r"^(?:sed|perl)\b.*\s-[a-zA-Z0-9]*i\b|--in-place|>>?\s*['\"]?[^\s'\"]+\.(?:rs|mjs)\b|^(?:tee|cp|mv|install|rm|patch)\s|^git\s+apply\b"
)
SOURCE_TOKEN = re.compile(r"[^\s'\"<>|;&]+\.(?:rs|mjs)\b")
GIT_OPTION_WITH_ARG = {"-c", "-C", "--git-dir", "--work-tree", "--namespace"}

VERDICTS_ATTACK = ("EVIDENCE", "DECORATION", "WEAK-RED")
VERDICTS_AUDIT = ("HOLDS", "AT-RISK", "VIOLATED")


# --------------------------------------------------------------------------- scanning Rust

CODE, STRING, COMMENT = "c", "s", "/"


def classify(lines: list[str]) -> list[list[str]]:
    """Per character of each line: code, inside a string/char literal, or inside a comment.

    Rust test bodies routinely hold braces in JSON fixtures, format strings and chars; the
    brace matcher and the digest both need to know what is code and what is literal.
    """
    kinds: list[list[str]] = []
    in_block_comment = False
    in_string = False
    raw_hashes: int | None = None
    for line in lines:
        row = [CODE] * len(line)
        j = 0
        while j < len(line):
            ch = line[j]
            if in_block_comment:
                row[j] = COMMENT
                if line.startswith("*/", j):
                    row[j + 1] = COMMENT
                    in_block_comment = False
                    j += 2
                    continue
                j += 1
            elif raw_hashes is not None:
                row[j] = STRING
                if ch == '"' and line.startswith("#" * raw_hashes, j + 1):
                    for k in range(j + 1, j + 1 + raw_hashes):
                        row[k] = STRING
                    j += 1 + raw_hashes
                    raw_hashes = None
                    continue
                j += 1
            elif in_string:
                row[j] = STRING
                if ch == "\\" and j + 1 < len(line):
                    row[j + 1] = STRING
                    j += 2
                    continue
                if ch == '"':
                    in_string = False
                j += 1
            elif line.startswith("//", j):
                for k in range(j, len(line)):
                    row[k] = COMMENT
                break
            elif line.startswith("/*", j):
                in_block_comment = True
                row[j] = row[j + 1] = COMMENT
                j += 2
            else:
                raw = re.match(r"b?r(#*)\"", line[j:])
                char = re.match(r"'(?:\\.|[^\\'])'", line[j:])
                if raw:
                    raw_hashes = len(raw.group(1))
                    for k in range(j, j + len(raw.group(0))):
                        row[k] = STRING
                    j += len(raw.group(0))
                elif ch == '"':
                    in_string = True
                    row[j] = STRING
                    j += 1
                elif char:
                    for k in range(j, j + len(char.group(0))):
                        row[k] = STRING
                    j += len(char.group(0))
                else:
                    j += 1
        kinds.append(row)
    return kinds


def brace_end(lines: list[str], kinds: list[list[str]], start: int) -> int:
    """Index of the line holding the `}` closing the first code `{` at or after `start`."""
    depth = 0
    for i in range(start, len(lines)):
        for ch, kind in zip(lines[i], kinds[i]):
            if kind != CODE:
                continue
            if ch == "{":
                depth += 1
            elif ch == "}":
                depth -= 1
                if depth == 0:
                    return i
    return len(lines) - 1


def normalize(lines: list[str], kinds: list[list[str]], a: int, b: int) -> str:
    """Lines a..b with comments dropped and, outside string literals, whitespace reduced to the
    one space that keeps two words apart (`as over` stays two tokens; `{ 90.0 }` loses its spaces)."""
    out: list[str] = []
    pending_space = False
    for i in range(a, b + 1):
        for ch, kind in zip(lines[i] + "\n", kinds[i] + [CODE]):
            if kind == STRING:
                out.append(ch)
                pending_space = False
            elif kind == CODE:
                if ch.isspace():
                    pending_space = True
                else:
                    if pending_space and out and _word(out[-1]) and _word(ch):
                        out.append(" ")
                    out.append(ch)
                    pending_space = False
    return "".join(out)


def _word(ch: str) -> bool:
    return ch.isalnum() or ch == "_"


def digest_of(text: str) -> str:
    return hashlib.sha256(text.encode()).hexdigest()[:16]


# --------------------------------------------------------------------------- the test surface


@dataclass(frozen=True)
class TestFn:
    name: str
    start: int  # first line of its attribute block
    end: int  # line of its closing brace
    digest: str


@dataclass
class TestSurface:
    """What the guard protects in one file: its test functions and its helper chunks."""

    fns: dict[str, TestFn] = field(default_factory=dict)
    helpers: list[str] = field(default_factory=list)
    production_fns: set[str] = field(default_factory=set)


def _attribute_block_start(lines: list[str], i: int) -> int:
    while i > 0 and (ATTRIBUTE_LINE.match(lines[i - 1]) or COMMENT_LINE.match(lines[i - 1])):
        i -= 1
    return i


def rust_test_fns(text: str) -> list[TestFn]:
    lines = text.splitlines()
    kinds = classify(lines)
    found: list[TestFn] = []
    i = 0
    while i < len(lines):
        if not TEST_ATTRIBUTE.match(lines[i]) or kinds[i][0:1] == [COMMENT]:
            i += 1
            continue
        start = _attribute_block_start(lines, i)
        j = i
        while j < len(lines) and not FN_DECL.search(lines[j]):
            if lines[j].strip() and not (ATTRIBUTE_LINE.match(lines[j]) or COMMENT_LINE.match(lines[j])):
                break
            j += 1
        decl = FN_DECL.search(lines[j]) if j < len(lines) else None
        if not decl:
            i += 1
            continue
        end = brace_end(lines, kinds, j)
        found.append(TestFn(decl.group(1), start, end, digest_of(normalize(lines, kinds, start, end))))
        i = end + 1
    return found


def _cfg_test_regions(lines: list[str], kinds: list[list[str]]) -> list[tuple[int, int]]:
    regions = []
    i = 0
    while i < len(lines):
        if CFG_TEST.match(lines[i]):
            j = i + 1
            while j < len(lines) and (ATTRIBUTE_LINE.match(lines[j]) or COMMENT_LINE.match(lines[j]) or not lines[j].strip()):
                j += 1
            if j < len(lines) and MOD_LINE.match(lines[j]):
                end = brace_end(lines, kinds, j)
                regions.append((i, end))
                i = end + 1
                continue
        i += 1
    return regions


def _chunks(normalized: str) -> list[str]:
    return [c for c in re.split(r"(?<=[{};])", normalized) if c]


def _helper_chunks(lines: list[str], kinds: list[list[str]], regions: list[tuple[int, int]], fns: list[TestFn]) -> list[str]:
    covered = set()
    for fn in fns:
        covered.update(range(fn.start, fn.end + 1))
    chunks: list[str] = []
    for a, b in regions:
        text = "".join(normalize(lines, kinds, k, k) for k in range(a, b + 1) if k not in covered)
        chunks += _chunks(text)
    return chunks


def surface_of(rel: str, text: str) -> TestSurface:
    """The protected surface of a file, by kind. Files that carry no tests are empty."""
    if is_additive_only(rel):
        return TestSurface(helpers=[re.sub(r"\s+", "", ln) for ln in text.splitlines() if ln.strip() and not COMMENT_LINE.match(ln)])
    if not rel.endswith(".rs"):
        return TestSurface()
    lines = text.splitlines()
    kinds = classify(lines)
    fns = rust_test_fns(text)
    regions = [(0, len(lines) - 1)] if lines and is_integration_test(rel) else _cfg_test_regions(lines, kinds)
    in_region = {k for a, b in regions for k in range(a, b + 1)}
    production = {m.group(1) for k, ln in enumerate(lines) if k not in in_region for m in [FN_DECL.search(ln)] if m}
    return TestSurface({fn.name: fn for fn in fns}, _helper_chunks(lines, kinds, regions, fns), production)


def is_additive_only(rel: str) -> bool:
    return any(Path(rel).match(g) for g in ADDITIVE_ONLY_GLOBS)


def is_integration_test(rel: str) -> bool:
    return any(rel.startswith(d + "/") for d in INTEGRATION_TEST_DIRS) and rel.endswith(".rs")


def is_protected_path(rel: str) -> bool:
    return (rel.endswith(".rs") and any(rel.startswith(d + "/") for d in RUST_SOURCE_DIRS)) or is_additive_only(rel)


# --------------------------------------------------------------------------- comparing surfaces


@dataclass
class Violation:
    path: str
    what: str

    def __str__(self) -> str:
        return f"{self.path}: {self.what}"


def _multiset_minus(before: list[str], after: list[str]) -> list[str]:
    remaining = list(after)
    missing = []
    for item in before:
        try:
            remaining.remove(item)
        except ValueError:
            missing.append(item)
    return missing


def _shadowing(chunk: str, production_fns: set[str]) -> bool:
    """A helper `fn`, or a `use` not from `super`, that takes a production function's name."""
    decl = re.match(r"^(?:pub(?:\([^)]*\))? )?(?:async )?fn ([A-Za-z_][A-Za-z0-9_]*)\(", chunk)
    if decl:
        return decl.group(1) in production_fns
    if chunk.startswith("use ") and not chunk.startswith("use super::"):
        return any(ident in production_fns for ident in re.findall(r"[A-Za-z_][A-Za-z0-9_]*", chunk))
    return False


def compare_fns(rel: str, frozen: dict[str, str], new: TestSurface, released: set[str], contract: set[str]) -> list[Violation]:
    out = []
    for name, digest in frozen.items():
        key = f"{rel}::{name}"
        if key in released or key not in contract:
            continue
        now = new.fns.get(name)
        if now is None:
            out.append(Violation(rel, f"test under contract `{name}` was removed or renamed"))
        elif now.digest != digest:
            out.append(Violation(rel, f"test under contract `{name}` was modified"))
    return out


def compare_helpers(rel: str, frozen: list[str], new: TestSurface, released: set[str]) -> list[Violation]:
    if f"{rel}::*" in released:
        return []
    out = []
    removed = _multiset_minus(frozen, new.helpers)
    if removed:
        shown = "; ".join(c[:60] for c in removed[:3]) + (" …" if len(removed) > 3 else "")
        out.append(Violation(rel, f"{len(removed)} chunk(s) of test fixture/helper code removed: {shown}"))
    added = _multiset_minus(new.helpers, frozen)
    hatch = JS_HATCH if rel.endswith(".mjs") else RUST_HATCH if rel.endswith(".rs") else None
    bad = [c for c in added if (hatch and hatch.search(c)) or _shadowing(c, new.production_fns)]
    if bad:
        out.append(Violation(rel, f"added helper code that could neutralise the tests: {'; '.join(c[:60] for c in bad[:3])}"))
    return out


def ignore_without_reason(text: str) -> list[str]:
    return [ln.strip() for ln in text.splitlines() if IGNORE_WITHOUT_REASON.match(ln)]


# --------------------------------------------------------------------------- shell commands


def _segments(command: str) -> list[str]:
    return [s.strip() for s in re.split(r"\|\|?|&&|;|\n", command) if s.strip()]


def writes_source_in_place(command: str, root: Path) -> bool:
    """True when a pipeline segment writes in place and names a .rs/.mjs path inside `root`."""
    for seg in _segments(command):
        if not WRITING_COMMAND.search(seg):
            continue
        for token in SOURCE_TOKEN.findall(seg):
            if _inside(root, token):
                return True
    return False


def _inside(root: Path, token: str) -> bool:
    try:
        (root / token).resolve().relative_to(root.resolve())
        return True
    except (ValueError, OSError):
        return False


def is_git_commit(command: str) -> bool:
    for seg in _segments(command):
        try:
            words = shlex.split(seg)
        except ValueError:
            words = seg.split()
        if not words or Path(words[0]).name != "git":
            continue
        k = 1
        while k < len(words) and words[k].startswith("-"):
            k += 2 if words[k] in GIT_OPTION_WITH_ARG else 1
        if k < len(words) and words[k] == "commit":
            return True
    return False


def is_test_run(command: str) -> bool:
    for seg in _segments(command):
        words = seg.split()
        if not words:
            continue
        head = Path(words[0]).name
        if head in ("cargo", "cross") and any(w in ("test", "nextest") for w in words[1:3]):
            return True
        if head in ("node", "python3", "python") and len(words) > 1 and words[1].endswith(("dashboard-tests/xss.mjs", "test_tdd_guard.py")):
            return True
    return False


# --------------------------------------------------------------------------- the ledger


class LedgerCorrupt(Exception):
    pass


class Guard:
    def __init__(self, root: Path):
        self.root = root
        self.ledger = self._load()

    # -- persistence
    def _load(self) -> dict:
        path = self.root / LEDGER
        empty = {"version": LEDGER_VERSION, "files": {}, "released": {}, "attacked": {}, "last_adversary": None, "last_audit": None, "last_edit": None}
        if not path.exists():
            return empty
        try:
            loaded = json.loads(path.read_text())
        except json.JSONDecodeError as e:
            self.log(EVENT_LOG, f"ledger unreadable ({e}); refusing rather than resetting")
            raise LedgerCorrupt(f"{LEDGER} is unreadable: {e}. Delete it deliberately (and say so) to start over.") from e
        if loaded.get("version") != LEDGER_VERSION:
            self.log(EVENT_LOG, f"ledger version {loaded.get('version')} -> {LEDGER_VERSION}: frozen digests dropped, releases and records kept")
            return {**empty, "released": loaded.get("released", {}), "last_audit": loaded.get("last_audit"), "last_edit": loaded.get("last_edit")}
        return {**empty, **loaded}

    def save(self) -> None:
        (self.root / STATE_DIR).mkdir(parents=True, exist_ok=True)
        tmp = self.root / LEDGER.with_suffix(f".{os.getpid()}.tmp")
        tmp.write_text(json.dumps(self.ledger, indent=1, sort_keys=True) + "\n")
        os.replace(tmp, self.root / LEDGER)

    def log(self, which: Path, line: str) -> None:
        (self.root / STATE_DIR).mkdir(parents=True, exist_ok=True)
        with (self.root / which).open("a") as f:
            f.write(f"{_now()} {line}\n")

    # -- git helpers
    def _git(self, *args: str) -> str:
        done = subprocess.run(["git", "-C", str(self.root), *args], capture_output=True, text=True, check=False)
        return done.stdout if done.returncode == 0 else ""

    def worktree_files(self) -> list[str]:
        listed = self._git("ls-files", "--cached", "--others", "--exclude-standard", "-z")
        return [p for p in listed.split("\0") if p and is_protected_path(p) and (self.root / p).is_file()]

    def head_files(self) -> list[str]:
        return [p for p in self._git("ls-tree", "-r", "--name-only", "-z", "HEAD").split("\0") if p and is_protected_path(p)]

    def text_at(self, ref: str, rel: str) -> str | None:
        done = subprocess.run(["git", "-C", str(self.root), "show", f"{ref}:{rel}"], capture_output=True, text=True, check=False)
        return done.stdout if done.returncode == 0 else None

    # -- the contract
    def contract(self, rel: str, head_fns: dict[str, str] | None = None) -> set[str]:
        """Keys of the tests in `rel` that are under contract: graded, or already in HEAD."""
        if head_fns is None:
            head = self.text_at("HEAD", rel)
            head_fns = {fn.name: fn.digest for fn in rust_test_fns(head)} if head and rel.endswith(".rs") else {}
        keys = {f"{rel}::{n}" for n in head_fns}
        keys |= {k for k in self.ledger["attacked"] if k.startswith(f"{rel}::")}
        return keys

    # -- freezing
    def freeze_all(self) -> list[str]:
        warnings = []
        for rel in self.worktree_files():
            warnings += self.freeze_file(rel, (self.root / rel).read_text(errors="replace"))
        self.save()
        return warnings

    def freeze_file(self, rel: str, text: str) -> list[str]:
        surface = surface_of(rel, text)
        entry = self.ledger["files"].get(rel, {"fns": {}, "helpers": []})
        head = self.text_at("HEAD", rel)
        head_fns = {fn.name: fn.digest for fn in rust_test_fns(head)} if head and rel.endswith(".rs") else {}
        contract = self.contract(rel, head_fns)
        released = set(self.ledger["released"])
        warnings = []
        for name, fn in surface.fns.items():
            key = f"{rel}::{name}"
            old = entry["fns"].get(name)
            if old is not None and old != fn.digest and key in contract and key not in released:
                warnings.append(f"{key} changed since it was frozen, without a release (an edit bypassed the guard?)")
            if old != fn.digest:
                if head_fns.get(name) == fn.digest:
                    self.ledger["attacked"][key] = {"at": _now(), "how": "pre-existing in HEAD"}
                else:
                    self.ledger["attacked"].pop(key, None)  # new or rewritten: needs grading again
            entry["fns"][name] = fn.digest
        for name in list(entry["fns"]):
            if name not in surface.fns:
                del entry["fns"][name]
        entry["helpers"] = surface.helpers
        entry["frozen_at"] = _now()
        self.ledger["files"][rel] = entry
        return warnings

    # -- releases and records
    def release(self, rel: str, names: list[str], reason: str, authority: str) -> list[str]:
        keys = [f"{rel}::{n}" for n in names] or [f"{rel}::*", *[f"{rel}::{n}" for n in self.ledger["files"].get(rel, {}).get("fns", {})]]
        for key in keys:
            self.ledger["released"][key] = {"reason": reason, "authority": authority, "at": _now()}
            self.log(RELEASE_LOG, f"release {key} authority={authority} reason={reason!r}")
        self.save()
        return keys

    def record_attack(self, verdict: str, how: str, detail: str, tests: list[str] | None) -> list[str]:
        targets = [t for t in (tests or []) if t in self.all_keys()]
        if not targets:
            # Unnamed: every ungraded test, and, for a failing verdict, every test graded in this
            # checkout (never the ones that were already in HEAD).
            targets = self.pending_attack()
            if verdict != "EVIDENCE":
                targets += sorted(k for k, v in self.ledger["attacked"].items() if v.get("how") != "pre-existing in HEAD")
        if verdict == "EVIDENCE":
            for key in targets:
                self.ledger["attacked"][key] = {"at": _now(), "how": how, "detail": detail}
        else:
            for key in targets:
                self.ledger["attacked"].pop(key, None)
        self.ledger["last_adversary"] = {"at": _now(), "how": how, "verdict": verdict, "detail": detail, "tests": targets}
        self.log(EVENT_LOG, f"red-test-adversary {how} {verdict}: {detail!r} on {targets}")
        self.save()
        return targets

    def record_audit(self, verdict: str, how: str, detail: str) -> None:
        if verdict == "VIOLATED":
            self.ledger["last_audit"] = None
        else:
            self.ledger["last_audit"] = {"at": _now(), "how": how, "verdict": verdict, "detail": detail}
        self.log(EVENT_LOG, f"rosette-auditor {how} {verdict}: {detail!r}")
        self.save()

    def record_edit(self, rel: str) -> None:
        self.ledger["last_edit"] = {"at": _now(), "path": rel}
        self.save()

    def all_keys(self) -> set[str]:
        return {f"{rel}::{n}" for rel, e in self.ledger["files"].items() for n in e.get("fns", {})}

    def pending_attack(self) -> list[str]:
        return sorted(k for k in self.all_keys() if k not in self.ledger["attacked"])

    def released_keys(self) -> set[str]:
        return set(self.ledger["released"])

    def clear_releases(self) -> None:
        for key, rel in self.ledger["released"].items():
            self.log(RELEASE_LOG, f"consumed {key} by commit (reason={rel['reason']!r})")
        self.ledger["released"] = {}
        self.save()

    # -- the edit gate
    def check_edit(self, rel: str, new_text: str) -> list[Violation]:
        violations = []
        old = (self.root / rel).read_text(errors="replace") if (self.root / rel).exists() else ""
        if len(ignore_without_reason(new_text)) > len(ignore_without_reason(old)):
            violations.append(Violation(rel, "`#[ignore]` without a reason: use `#[ignore = \"why\"]`, or don't"))
        frozen = self.ledger["files"].get(rel)
        if not frozen:
            return violations
        new = surface_of(rel, new_text)
        released = self.released_keys()
        violations += compare_fns(rel, frozen["fns"], new, released, self.contract(rel))
        if self.text_at("HEAD", rel) is not None:
            violations += compare_helpers(rel, frozen["helpers"], new, released)
        return violations

    # -- the commit gate
    def check_commit(self) -> list[str]:
        problems: list[str] = []
        released = self.released_keys()
        worktree = self.worktree_files()
        for rel in sorted(set(worktree) | set(self.head_files())):
            problems += self._check_file_for_commit(rel, released, rel in worktree)
        if self._production_changed() and not self._audited_after_last_edit():
            problems.append("production code changed but rosette-auditor has not run since the last edit (subagent, `audited --in-process`, or `trivial --reason`)")
        return problems

    def _check_file_for_commit(self, rel: str, released: set[str], present: bool) -> list[str]:
        head = self.text_at("HEAD", rel)
        head_surface = surface_of(rel, head) if head is not None else TestSurface()
        head_fns = {n: f.digest for n, f in head_surface.fns.items()}
        if not present:
            return [f"{rel}: file in HEAD is gone ({len(head_fns)} test(s)) with no release logged ({rel}::*)"] if head_fns and f"{rel}::*" not in released else []
        problems = []
        versions = {"worktree": (self.root / rel).read_text(errors="replace")}
        staged = self.text_at("", rel)
        if staged is not None and staged != versions["worktree"]:
            versions["index"] = staged
        for label, text in versions.items():
            new = surface_of(rel, text)
            contract = self.contract(rel, head_fns)
            found = compare_fns(rel, head_fns, new, released, contract)
            if head is not None:
                found += compare_helpers(rel, head_surface.helpers, new, released)
            problems += [f"{v} ({label} vs HEAD, no release logged)" for v in found]
            problems += self._ungraded(rel, new, head_fns)
        return problems

    def _ungraded(self, rel: str, new: TestSurface, head_fns: dict[str, str]) -> list[str]:
        ledger_fns = self.ledger["files"].get(rel, {}).get("fns", {})
        out = []
        for name, fn in new.fns.items():
            if head_fns.get(name) == fn.digest:
                continue
            key = f"{rel}::{name}"
            if ledger_fns.get(name) != fn.digest:
                out.append(f"{key}: new or changed test has not been run (`cargo test`) since its last edit")
            elif key not in self.ledger["attacked"]:
                out.append(f"{key}: new or rewritten test has no EVIDENCE record from red-test-adversary")
        return out

    def _production_changed(self) -> bool:
        return any(line.strip() for line in self._git("status", "--porcelain", "--", "*.rs").splitlines())

    def _audited_after_last_edit(self) -> bool:
        audit, edit = self.ledger.get("last_audit"), self.ledger.get("last_edit")
        if not audit:
            return False
        return not edit or audit["at"] >= edit["at"]


# --------------------------------------------------------------------------- hook entry points


def _now() -> str:
    return dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def _read_hook_input() -> dict:
    try:
        return json.load(sys.stdin)
    except (json.JSONDecodeError, OSError):
        return {}


def _root(hook: dict) -> Path:
    for candidate in (os.environ.get("CLAUDE_PROJECT_DIR"), hook.get("cwd")):
        if candidate:
            top = subprocess.run(["git", "-C", candidate, "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=False)
            if top.returncode == 0:
                return Path(top.stdout.strip())
    return Path.cwd()


def _relative(root: Path, file_path: str) -> str | None:
    try:
        return Path(file_path).resolve().relative_to(root.resolve()).as_posix()
    except ValueError:
        return None


def projected_text(tool: str, tool_input: dict, current: str) -> str | None:
    """The file as it would read after the tool call, or None if the edit cannot apply."""
    if tool == "Write":
        return tool_input.get("content", "")
    edits = tool_input.get("edits") if tool == "MultiEdit" else [tool_input]
    text = current
    for edit in edits or []:
        old, new = edit.get("old_string", ""), edit.get("new_string", "")
        if old not in text:
            return None
        text = text.replace(old, new) if edit.get("replace_all") else text.replace(old, new, 1)
    return text


def refuse(lines: list[str]) -> None:
    sys.stderr.write("tdd-guard: refused.\n" + "\n".join(f"  - {ln}" for ln in lines) + "\n")
    sys.exit(2)


def _guard(root: Path) -> Guard:
    try:
        return Guard(root)
    except LedgerCorrupt as e:
        refuse([str(e)])
        raise AssertionError("unreachable")


RELEASE_HELP = [
    "A graded test is the contract (CLAUDE.md § Tests are the contract). The implementation changes to",
    "meet it, not the other way round. If the contract itself is wrong, release the test first:",
    "  python3 .claude/hooks/tdd_guard.py release <file> <test> --reason '<why>' --authority adversary|user|author",
    "(`adversary`: red-test-adversary said DECORATION/WEAK-RED or named a missing row, quote it; `user`: the user",
    "asked, in their words; `author`: a strengthening only — a new row, a new assertion, a call-site follow-through",
    "of a signature change — never a changed expectation). Every release is logged and quoted in the summary.",
]


def cmd_pre_edit() -> None:
    hook = _read_hook_input()
    root = _root(hook)
    tool_input = hook.get("tool_input", {})
    rel = _relative(root, tool_input.get("file_path", ""))
    if rel is None or not is_protected_path(rel):
        return
    path = root / rel
    current = path.read_text(errors="replace") if path.exists() else ""
    new_text = projected_text(hook.get("tool_name", ""), tool_input, current)
    if new_text is None:
        return  # the tool itself will fail on the unmatched old_string
    violations = _guard(root).check_edit(rel, new_text)
    if violations:
        refuse([str(v) for v in violations] + RELEASE_HELP)


def cmd_post_edit() -> None:
    hook = _read_hook_input()
    root = _root(hook)
    rel = _relative(root, hook.get("tool_input", {}).get("file_path", ""))
    if rel and rel.endswith(".rs"):
        _guard(root).record_edit(rel)


def cmd_pre_bash() -> None:
    hook = _read_hook_input()
    root = _root(hook)
    command = hook.get("tool_input", {}).get("command", "")
    if writes_source_in_place(command, root):
        refuse(["this shell command writes a .rs/.mjs file of the repository in place; use the Edit or Write tool so the test-contract guard can read the change."])
    if is_git_commit(command):
        problems = _guard(root).check_commit()
        if problems:
            refuse(problems + ["Fix these, or release the test with a reason, before committing (CLAUDE.md § Tests are the contract)."])


def cmd_post_bash() -> None:
    hook = _read_hook_input()
    root = _root(hook)
    command = hook.get("tool_input", {}).get("command", "")
    guard = _guard(root)
    if is_test_run(command):
        warnings = guard.freeze_all()
        if warnings:
            sys.stderr.write("tdd-guard: tests frozen, with warnings:\n" + "\n".join(f"  - {w}" for w in warnings) + "\n")
            sys.exit(2)
    if is_git_commit(command) and _succeeded(hook):
        guard.clear_releases()


def _succeeded(hook: dict) -> bool:
    response = hook.get("tool_response", {})
    if isinstance(response, dict):
        if "exit_code" in response:
            return response["exit_code"] == 0
        return not response.get("interrupted") and "fatal" not in str(response.get("stderr", ""))
    return True


def cmd_post_agent() -> None:
    hook = _read_hook_input()
    root = _root(hook)
    tool_input = hook.get("tool_input", {})
    kind = tool_input.get("subagent_type", "")
    if kind not in ("red-test-adversary", "rosette-auditor"):
        return
    verdict = _verdict_of(hook.get("tool_response"), VERDICTS_ATTACK if kind == "red-test-adversary" else VERDICTS_AUDIT)
    guard = _guard(root)
    if verdict is None:
        sys.stderr.write(f"tdd-guard: no VERDICT line found in the {kind} report; nothing recorded. Re-run it or record the in-process check explicitly.\n")
        sys.exit(2)
    if kind == "red-test-adversary":
        named = [k for k in guard.all_keys() if k in tool_input.get("prompt", "")]
        covered = guard.record_attack(verdict, "subagent", "report", named)
        sys.stderr.write(f"tdd-guard: red-test-adversary {verdict} recorded for {covered or 'no pending tests'}\n")
    else:
        guard.record_audit(verdict, "subagent", "report")
        sys.stderr.write(f"tdd-guard: rosette-auditor {verdict} recorded\n")


def _verdict_of(response, allowed: tuple[str, ...]) -> str | None:
    text = response if isinstance(response, str) else json.dumps(response)
    found = re.findall(r"VERDICT:\s*([A-Z-]+)", text or "")
    verdicts = [v for v in found if v in allowed]
    if not verdicts:
        return None
    # The worst verdict in the report decides: one VIOLATED among several HOLDS is a VIOLATED.
    return max(verdicts, key=allowed.index)


def main(argv: list[str]) -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)
    for name in ("pre-edit", "post-edit", "pre-bash", "post-bash", "post-agent", "freeze", "status", "check-commit"):
        sub.add_parser(name)
    rel = sub.add_parser("release")
    rel.add_argument("file")
    rel.add_argument("tests", nargs="*")
    rel.add_argument("--reason", required=True)
    rel.add_argument("--authority", required=True, choices=("adversary", "user", "author"))
    att = sub.add_parser("attacked")
    att.add_argument("--in-process", action="store_true", required=True)
    att.add_argument("--mode", default="red", choices=("red", "mutation"))
    att.add_argument("--verdict", required=True, choices=VERDICTS_ATTACK)
    att.add_argument("--closest", required=True, help="the cheat or mutant that came closest")
    att.add_argument("--tests", nargs="*", default=[], help="file::name keys; default: every ungraded test")
    aud = sub.add_parser("audited")
    aud.add_argument("--in-process", action="store_true", required=True)
    aud.add_argument("--verdict", required=True, choices=VERDICTS_AUDIT)
    triv = sub.add_parser("trivial")
    triv.add_argument("--reason", required=True, help="why this .rs change has no behaviour (comments, fmt, log wording)")
    args = parser.parse_args(argv)

    hooks = {"pre-edit": cmd_pre_edit, "post-edit": cmd_post_edit, "pre-bash": cmd_pre_bash, "post-bash": cmd_post_bash, "post-agent": cmd_post_agent}
    if args.cmd in hooks:
        hooks[args.cmd]()
        return
    guard = _guard(_root({}))
    if args.cmd == "freeze":
        warnings = guard.freeze_all()
        print("frozen" + (" with warnings:\n" + "\n".join(warnings) if warnings else ""))
    elif args.cmd == "release":
        keys = guard.release(_relative(guard.root, args.file) or args.file, args.tests, args.reason, args.authority)
        print("released: " + ", ".join(keys))
    elif args.cmd == "attacked":
        covered = guard.record_attack(args.verdict, "in-process", f"mode={args.mode} closest={args.closest!r}", args.tests)
        print(f"recorded in-process red-test-adversary {args.verdict} for: " + (", ".join(covered) or "no pending tests"))
    elif args.cmd == "audited":
        guard.record_audit(args.verdict, "in-process", "declared")
        print(f"recorded in-process rosette-auditor {args.verdict}")
    elif args.cmd == "trivial":
        guard.record_audit("HOLDS", "trivial", args.reason)
        print("recorded trivial change: " + args.reason)
    elif args.cmd == "status":
        files = guard.ledger["files"]
        print(f"frozen files: {len(files)}, frozen tests: {sum(len(e.get('fns', {})) for e in files.values())}")
        print(f"ungraded (not under contract yet): {guard.pending_attack() or 'none'}")
        print(f"open releases: {sorted(guard.released_keys()) or 'none'}")
        print(f"last adversary: {guard.ledger.get('last_adversary')}")
        print(f"last audit: {guard.ledger.get('last_audit')}")
    elif args.cmd == "check-commit":
        problems = guard.check_commit()
        print("\n".join(problems) if problems else "commit gate: clean")
        sys.exit(1 if problems else 0)


if __name__ == "__main__":
    main(sys.argv[1:])
