#!/usr/bin/env python3
"""The test-contract guard: Claude Code hooks that keep tests from bending to the code.

CLAUDE.md § Development discipline says the test is written first and is the contract the
implementation has to meet. This script is the mechanical half of that rule. It runs as a
hook (see `.claude/settings.json`) and keeps a local ledger in `.claude/tdd-state/`:

- every `cargo test` / `xss.mjs` run *freezes* each test function it could have executed
  (a hash of its source, whitespace removed, so `cargo fmt` is not a change);
- an Edit/Write that changes, renames, deletes or `#[ignore]`s a frozen test is refused,
  unless that test was *released* first, with a reason and an authority
  (`release --authority adversary|user`); a release is logged and must be quoted in the
  change summary;
- fixture and helper code in a `#[cfg(test)]` region, an integration-test file, the hub
  dashboard's `xss.mjs` and this guard's own tests are *additive-only*: lines may be added,
  never removed, without a release;
- `git commit` is refused while a test function differs from `HEAD` without a release, while
  a new test has never been run, or while a new test has not been attacked by
  `red-test-adversary` (as a subagent, recorded automatically, or in-process, recorded with
  `attacked --in-process`);
- shell commands that write a `.rs` / `.mjs` file in place are refused, so every test edit
  goes through Edit/Write where the guard can read it. The commit gate is the backstop for
  anything that slips past.

The guard protects against the author's own shortcuts, not against a determined attacker:
anyone can delete the ledger. Its value is that a shortcut becomes a visible, logged act
instead of a silent one.

Subcommands (all read the hook's JSON from stdin unless noted):

    pre-edit        PreToolUse  Edit | Write | MultiEdit
    pre-bash        PreToolUse  Bash
    post-bash       PostToolUse Bash        (freezes after a test run; clears releases after a commit)
    post-agent      PostToolUse Agent | Task (records an adversary run)
    freeze          manual: freeze every test in the tree now
    release FILE [TEST ...] --reason TEXT --authority adversary|user
    attacked --in-process --verdict V --closest TEXT [--mode red|mutation]
    audited  --in-process --verdict V
    status          manual: print the ledger
    check-commit    manual: run the commit gate against HEAD

Exit code 2 with a message on stderr is how a hook refuses a tool call.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

STATE_DIR = Path(".claude") / "tdd-state"
LEDGER = STATE_DIR / "ledger.json"
RELEASE_LOG = STATE_DIR / "releases.log"
EVENT_LOG = STATE_DIR / "events.log"

TEST_ATTRIBUTE = re.compile(r"^\s*#\[\s*(tokio::test|test|rstest|test_case|test_log::test)\b")
ATTRIBUTE_LINE = re.compile(r"^\s*#\[")
FN_LINE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)")
CFG_TEST = re.compile(r"^\s*#\[\s*cfg\s*\(\s*test\s*\)\s*\]")
MOD_LINE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+[A-Za-z_][A-Za-z0-9_]*\s*\{")
IGNORE_WITHOUT_REASON = re.compile(r"^\s*#\[\s*ignore\s*\]")

ADDITIVE_ONLY_SUFFIXES = (".mjs",)
ADDITIVE_ONLY_GLOBS = ("dashboard-tests/*.mjs", ".claude/hooks/test_*.py")
INTEGRATION_TEST_DIRS = ("tests", "system-hub/tests")
RUST_SOURCE_DIRS = ("src", "tests", "system-hub/src", "system-hub/tests")

# Added lines that would let a JS/Python test skip its own checks.
ESCAPE_HATCH = re.compile(r"process\.exit\(\s*0\s*\)|\.only\(|\bxit\(|\bxdescribe\(|\bskip\b|\btodo\b|@unittest\.skip|pytest\.skip")

TEST_RUN = re.compile(r"\bcargo\s+(?:\+\S+\s+)?(?:test|nextest\s+run)\b|dashboard-tests/xss\.mjs|test_tdd_guard\.py")
GIT_COMMIT = re.compile(r"\bgit\s+(?:-C\s+\S+\s+)?(?:-c\s+\S+\s+)*commit\b")
SOURCE_PATH = re.compile(r"\S+\.(?:rs|mjs)\b")
WRITING_COMMAND = re.compile(
    r"(?:^|\s)(?:sed|perl)\b.*\s-[a-zA-Z]*i\b|--in-place|>>?\s*['\"]?[^\s'\"]+\.(?:rs|mjs)\b|(?:^|\s)(?:tee|cp|mv|install)\s"
)


def writes_source_in_place(command: str) -> bool:
    """True when some pipeline segment both writes in place and names a .rs/.mjs path."""
    segments = re.split(r"\|\|?|&&|;|\n", command)
    return any(WRITING_COMMAND.search(seg) and SOURCE_PATH.search(seg) for seg in segments)


# --------------------------------------------------------------------------- source model


@dataclass(frozen=True)
class TestFn:
    name: str
    start: int  # 0-based line of the first attribute
    end: int  # 0-based line of the closing brace (inclusive)
    digest: str


@dataclass
class TestSurface:
    """What the guard protects in one file: its test functions and its helper lines."""

    fns: dict[str, TestFn] = field(default_factory=dict)
    helper_lines: list[str] = field(default_factory=list)


def _digest(text: str) -> str:
    return hashlib.sha256(re.sub(r"\s+", "", text).encode()).hexdigest()[:16]


def _brace_end(lines: list[str], start: int) -> int:
    """Index of the line holding the `}` that closes the first `{` at or after `start`.

    Skips string literals (including raw strings), char literals and comments, which in a
    test body routinely contain braces (JSON fixtures, format strings).
    """
    depth = 0
    opened = False
    in_block_comment = False
    raw_hashes: int | None = None
    in_string = False
    for i in range(start, len(lines)):
        line = lines[i]
        j = 0
        while j < len(line):
            ch = line[j]
            if in_block_comment:
                if line.startswith("*/", j):
                    in_block_comment = False
                    j += 2
                    continue
                j += 1
                continue
            if raw_hashes is not None:
                if ch == '"' and line.startswith("#" * raw_hashes, j + 1):
                    raw_hashes = None
                    j += 1 + raw_hashes if raw_hashes else 1
                    continue
                j += 1
                continue
            if in_string:
                if ch == "\\":
                    j += 2
                    continue
                if ch == '"':
                    in_string = False
                j += 1
                continue
            if line.startswith("//", j):
                break
            if line.startswith("/*", j):
                in_block_comment = True
                j += 2
                continue
            raw = re.match(r"b?r(#*)\"", line[j:])
            if raw:
                raw_hashes = len(raw.group(1))
                j += len(raw.group(0))
                continue
            if ch == '"':
                in_string = True
                j += 1
                continue
            if ch == "'":
                char = re.match(r"'(?:\\.|[^\\'])'", line[j:])
                if char:
                    j += len(char.group(0))
                    continue
            if ch == "{":
                depth += 1
                opened = True
            elif ch == "}":
                depth -= 1
                if opened and depth == 0:
                    return i
            j += 1
    return len(lines) - 1


def rust_test_fns(text: str) -> list[TestFn]:
    lines = text.splitlines()
    found: list[TestFn] = []
    i = 0
    while i < len(lines):
        if not TEST_ATTRIBUTE.match(lines[i]):
            i += 1
            continue
        start = i
        while i < len(lines) and (ATTRIBUTE_LINE.match(lines[i]) or not lines[i].strip()):
            i += 1
        fn = FN_LINE.match(lines[i]) if i < len(lines) else None
        if not fn:
            continue
        end = _brace_end(lines, i)
        body = "\n".join(lines[start : end + 1])
        found.append(TestFn(fn.group(1), start, end, _digest(body)))
        i = end + 1
    return found


def _cfg_test_regions(lines: list[str]) -> list[tuple[int, int]]:
    regions = []
    i = 0
    while i < len(lines):
        if CFG_TEST.match(lines[i]):
            j = i + 1
            while j < len(lines) and (ATTRIBUTE_LINE.match(lines[j]) or not lines[j].strip()):
                j += 1
            if j < len(lines) and MOD_LINE.match(lines[j]):
                end = _brace_end(lines, j)
                regions.append((i, end))
                i = end + 1
                continue
        i += 1
    return regions


def _helper_lines(lines: list[str], regions: list[tuple[int, int]], fns: list[TestFn]) -> list[str]:
    covered = set()
    for fn in fns:
        covered.update(range(fn.start, fn.end + 1))
    helpers = []
    for a, b in regions:
        for k in range(a, b + 1):
            if k not in covered and lines[k].strip():
                helpers.append(lines[k].strip())
    return helpers


def surface_of(rel: str, text: str) -> TestSurface:
    """The protected surface of a file, by kind. Files that carry no tests are empty."""
    if is_additive_only(rel):
        return TestSurface(helper_lines=[ln.strip() for ln in text.splitlines() if ln.strip()])
    if not rel.endswith(".rs"):
        return TestSurface()
    lines = text.splitlines()
    fns = rust_test_fns(text)
    if is_integration_test(rel):
        regions = [(0, len(lines) - 1)] if lines else []
    else:
        regions = _cfg_test_regions(lines)
    return TestSurface({fn.name: fn for fn in fns}, _helper_lines(lines, regions, fns))


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


def _removed_lines(before: list[str], after: list[str]) -> list[str]:
    remaining = list(after)
    removed = []
    for line in before:
        try:
            remaining.remove(line)
        except ValueError:
            removed.append(line)
    return removed


def compare(rel: str, frozen: dict, new: TestSurface, released: set[str]) -> list[Violation]:
    """Everything `new` does to the frozen surface of `rel` that needs a release."""
    out: list[Violation] = []
    for name, digest in frozen.get("fns", {}).items():
        key = f"{rel}::{name}"
        if key in released:
            continue
        now = new.fns.get(name)
        if now is None:
            out.append(Violation(rel, f"frozen test `{name}` was removed or renamed"))
        elif now.digest != digest:
            out.append(Violation(rel, f"frozen test `{name}` was modified"))
    if f"{rel}::*" not in released:
        removed = _removed_lines(frozen.get("helpers", []), new.helper_lines)
        if removed:
            shown = "; ".join(removed[:3]) + (" …" if len(removed) > 3 else "")
            out.append(Violation(rel, f"{len(removed)} line(s) of test fixture/helper code removed: {shown}"))
        if rel.endswith(ADDITIVE_ONLY_SUFFIXES):
            added = _removed_lines(new.helper_lines, frozen.get("helpers", []))
            hatches = [ln for ln in added if ESCAPE_HATCH.search(ln)]
            if hatches:
                out.append(Violation(rel, f"added line(s) that could skip checks: {'; '.join(hatches[:3])}"))
    return out


def ignore_without_reason(text: str) -> list[str]:
    return [ln.strip() for ln in text.splitlines() if IGNORE_WITHOUT_REASON.match(ln)]


# --------------------------------------------------------------------------- the ledger


class Guard:
    def __init__(self, root: Path):
        self.root = root
        self.ledger = self._load()

    # -- persistence
    def _load(self) -> dict:
        path = self.root / LEDGER
        if path.exists():
            try:
                return json.loads(path.read_text())
            except json.JSONDecodeError:
                pass
        return {"files": {}, "released": {}, "attacked": {}, "last_adversary": None, "last_audit": None, "last_edit": None}

    def save(self) -> None:
        (self.root / STATE_DIR).mkdir(parents=True, exist_ok=True)
        (self.root / LEDGER).write_text(json.dumps(self.ledger, indent=2, sort_keys=True) + "\n")

    def log(self, which: Path, line: str) -> None:
        (self.root / STATE_DIR).mkdir(parents=True, exist_ok=True)
        with (self.root / which).open("a") as f:
            f.write(f"{_now()} {line}\n")

    # -- git helpers
    def _git(self, *args: str) -> str:
        try:
            return subprocess.run(["git", "-C", str(self.root), *args], capture_output=True, text=True, check=False).stdout
        except OSError:
            return ""

    def tracked_and_untracked(self) -> list[str]:
        listed = self._git("ls-files", "--cached", "--others", "--exclude-standard", "-z")
        return [p for p in listed.split("\0") if p and is_protected_path(p) and (self.root / p).is_file()]

    def head_text(self, rel: str) -> str | None:
        done = subprocess.run(["git", "-C", str(self.root), "show", f"HEAD:{rel}"], capture_output=True, text=True, check=False)
        return done.stdout if done.returncode == 0 else None

    # -- freezing
    def freeze_all(self) -> list[str]:
        """Freeze every protected file. Returns warnings about edits that bypassed the guard."""
        warnings = []
        for rel in self.tracked_and_untracked():
            warnings += self.freeze_file(rel, (self.root / rel).read_text(errors="replace"))
        self.save()
        return warnings

    def freeze_file(self, rel: str, text: str) -> list[str]:
        surface = surface_of(rel, text)
        entry = self.ledger["files"].get(rel, {"fns": {}, "helpers": []})
        head = self.head_text(rel)
        head_fns = {fn.name: fn.digest for fn in rust_test_fns(head)} if head and rel.endswith(".rs") else {}
        warnings = []
        released = set(self.ledger["released"])
        for name, fn in surface.fns.items():
            key = f"{rel}::{name}"
            old = entry["fns"].get(name)
            if old is not None and old != fn.digest and key not in released:
                warnings.append(f"{key} changed since it was frozen, without a release (an edit bypassed the guard?)")
                self.ledger["attacked"].pop(key, None)
            if old is None or old != fn.digest:
                # New, or legitimately rewritten after a release: it needs the adversary again,
                # unless it is byte-for-byte what HEAD already has (a pre-existing test).
                if head_fns.get(name) == fn.digest:
                    self.ledger["attacked"][key] = {"at": _now(), "how": "pre-existing in HEAD"}
                else:
                    self.ledger["attacked"].pop(key, None)
            entry["fns"][name] = fn.digest
        for name in list(entry["fns"]):
            if name not in surface.fns:
                del entry["fns"][name]
        entry["helpers"] = surface.helper_lines
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

    def record_adversary(self, how: str, detail: str) -> list[str]:
        pending = self.pending_attack()
        for key in pending:
            self.ledger["attacked"][key] = {"at": _now(), "how": how, "detail": detail}
        self.ledger["last_adversary"] = {"at": _now(), "how": how, "detail": detail, "tests": pending}
        self.log(EVENT_LOG, f"red-test-adversary {how}: {detail!r} covers {pending}")
        self.save()
        return pending

    def record_audit(self, how: str, detail: str) -> None:
        self.ledger["last_audit"] = {"at": _now(), "how": how, "detail": detail}
        self.log(EVENT_LOG, f"rosette-auditor {how}: {detail!r}")
        self.save()

    def record_edit(self, rel: str) -> None:
        self.ledger["last_edit"] = {"at": _now(), "path": rel}
        self.save()

    def pending_attack(self) -> list[str]:
        out = []
        for rel, entry in self.ledger["files"].items():
            for name in entry.get("fns", {}):
                key = f"{rel}::{name}"
                if key not in self.ledger["attacked"]:
                    out.append(key)
        return sorted(out)

    def released_keys(self) -> set[str]:
        return set(self.ledger["released"])

    def clear_releases(self) -> None:
        for key, rel in self.ledger["released"].items():
            self.log(RELEASE_LOG, f"consumed {key} by commit (reason={rel['reason']!r})")
        self.ledger["released"] = {}
        self.save()

    # -- checks
    def check_edit(self, rel: str, new_text: str) -> list[Violation]:
        violations = []
        hatches = ignore_without_reason(new_text)
        old = (self.root / rel).read_text(errors="replace") if (self.root / rel).exists() else ""
        if hatches and len(hatches) > len(ignore_without_reason(old)):
            violations.append(Violation(rel, "`#[ignore]` without a reason: use `#[ignore = \"why\"]`, or don't"))
        frozen = self.ledger["files"].get(rel)
        if frozen:
            violations += compare(rel, frozen, surface_of(rel, new_text), self.released_keys())
        return violations

    def check_commit(self) -> list[str]:
        problems: list[str] = []
        released = self.released_keys()
        for rel in self.tracked_and_untracked():
            text = (self.root / rel).read_text(errors="replace")
            head = self.head_text(rel)
            head_surface = surface_of(rel, head) if head is not None else TestSurface()
            frozen_like = {"fns": {n: f.digest for n, f in head_surface.fns.items()}, "helpers": head_surface.helper_lines}
            problems += [f"{v} (vs HEAD, no release logged)" for v in compare(rel, frozen_like, surface_of(rel, text), released)]
            ledger_fns = self.ledger["files"].get(rel, {}).get("fns", {})
            for name, fn in surface_of(rel, text).fns.items():
                if head_surface.fns.get(name) and head_surface.fns[name].digest == fn.digest:
                    continue
                key = f"{rel}::{name}"
                if ledger_fns.get(name) != fn.digest:
                    problems.append(f"{key}: new or changed test has not been run (`cargo test`) since its last edit")
                elif key not in self.ledger["attacked"]:
                    problems.append(f"{key}: new or rewritten test has not been attacked by red-test-adversary")
        if self._production_changed() and not self._audited_after_last_edit():
            problems.append("production code changed but rosette-auditor has not run since the last edit (subagent, or `audited --in-process`)")
        return problems

    def _production_changed(self) -> bool:
        changed = self._git("status", "--porcelain", "--", "*.rs")
        return any(line.strip() for line in changed.splitlines())

    def _audited_after_last_edit(self) -> bool:
        audit, edit = self.ledger.get("last_audit"), self.ledger.get("last_edit")
        if not audit:
            return False
        return not edit or audit["at"] >= edit["at"]


# --------------------------------------------------------------------------- hook entry points


def _now() -> str:
    return dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


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
    violations = Guard(root).check_edit(rel, new_text)
    if violations:
        refuse(
            [str(v) for v in violations]
            + [
                "A test that has been run is the contract (CLAUDE.md § Tests are the contract). The implementation",
                "changes to meet it, not the other way round. If the contract itself is wrong, release the test first:",
                "  python3 .claude/hooks/tdd_guard.py release <file> <test> --reason '<why>' --authority adversary|user",
                "(`adversary`: red-test-adversary said DECORATION/WEAK-RED, quote it; `user`: the user asked, in their words).",
                "Every release is logged and must be quoted in the change summary.",
            ]
        )


def cmd_post_edit() -> None:
    hook = _read_hook_input()
    root = _root(hook)
    rel = _relative(root, hook.get("tool_input", {}).get("file_path", ""))
    if rel and rel.endswith(".rs"):
        Guard(root).record_edit(rel)


def cmd_pre_bash() -> None:
    hook = _read_hook_input()
    root = _root(hook)
    command = hook.get("tool_input", {}).get("command", "")
    if writes_source_in_place(command):
        refuse(
            [
                "this shell command writes a .rs/.mjs file in place; use the Edit or Write tool so the test-contract guard can read the change.",
            ]
        )
    if GIT_COMMIT.search(command):
        problems = Guard(root).check_commit()
        if problems:
            refuse(problems + ["Fix these, or release the test with a reason, before committing (CLAUDE.md § Tests are the contract)."])


def cmd_post_bash() -> None:
    hook = _read_hook_input()
    root = _root(hook)
    command = hook.get("tool_input", {}).get("command", "")
    guard = Guard(root)
    if TEST_RUN.search(command):
        warnings = guard.freeze_all()
        if warnings:
            sys.stderr.write("tdd-guard: tests frozen, with warnings:\n" + "\n".join(f"  - {w}" for w in warnings) + "\n")
            sys.exit(2)
    if GIT_COMMIT.search(command) and _succeeded(hook):
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
    detail = _verdict_of(hook.get("tool_response"))
    guard = Guard(root)
    if kind == "red-test-adversary":
        covered = guard.record_adversary("subagent", detail)
        sys.stderr.write(f"tdd-guard: red-test-adversary run recorded for {covered or 'no pending tests'}\n")
    elif kind == "rosette-auditor":
        guard.record_audit("subagent", detail)


def _verdict_of(response) -> str:
    text = json.dumps(response) if not isinstance(response, str) else response
    found = re.search(r"VERDICT:\s*([A-Z-]+)", text or "")
    return found.group(1) if found else "verdict not found in report"


def cmd_release(args: argparse.Namespace) -> None:
    root = _root({})
    rel = _relative(root, args.file) or args.file
    keys = Guard(root).release(rel, args.tests, args.reason, args.authority)
    print("released: " + ", ".join(keys))


def cmd_attacked(args: argparse.Namespace) -> None:
    guard = Guard(_root({}))
    covered = guard.record_adversary("in-process", f"mode={args.mode} verdict={args.verdict} closest={args.closest!r}")
    print("recorded in-process red-test-adversary check for: " + (", ".join(covered) or "no pending tests"))


def cmd_audited(args: argparse.Namespace) -> None:
    Guard(_root({})).record_audit("in-process", f"verdict={args.verdict}")
    print("recorded in-process rosette-auditor check")


def cmd_status() -> None:
    guard = Guard(_root({}))
    files = guard.ledger["files"]
    print(f"frozen files: {len(files)}, frozen tests: {sum(len(e.get('fns', {})) for e in files.values())}")
    print(f"pending adversary: {guard.pending_attack() or 'none'}")
    print(f"open releases: {list(guard.released_keys()) or 'none'}")
    print(f"last adversary: {guard.ledger.get('last_adversary')}")
    print(f"last audit: {guard.ledger.get('last_audit')}")


def main(argv: list[str]) -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)
    for name in ("pre-edit", "post-edit", "pre-bash", "post-bash", "post-agent", "freeze", "status", "check-commit"):
        sub.add_parser(name)
    rel = sub.add_parser("release")
    rel.add_argument("file")
    rel.add_argument("tests", nargs="*")
    rel.add_argument("--reason", required=True)
    rel.add_argument("--authority", required=True, choices=("adversary", "user"))
    att = sub.add_parser("attacked")
    att.add_argument("--in-process", action="store_true", required=True)
    att.add_argument("--mode", default="red", choices=("red", "mutation"))
    att.add_argument("--verdict", required=True, choices=("EVIDENCE", "DECORATION", "WEAK-RED"))
    att.add_argument("--closest", required=True, help="the cheat or mutant that came closest")
    aud = sub.add_parser("audited")
    aud.add_argument("--in-process", action="store_true", required=True)
    aud.add_argument("--verdict", required=True, choices=("HOLDS", "AT-RISK", "VIOLATED"))
    args = parser.parse_args(argv)

    if args.cmd == "pre-edit":
        cmd_pre_edit()
    elif args.cmd == "post-edit":
        cmd_post_edit()
    elif args.cmd == "pre-bash":
        cmd_pre_bash()
    elif args.cmd == "post-bash":
        cmd_post_bash()
    elif args.cmd == "post-agent":
        cmd_post_agent()
    elif args.cmd == "freeze":
        warnings = Guard(_root({})).freeze_all()
        print("frozen" + (" with warnings:\n" + "\n".join(warnings) if warnings else ""))
    elif args.cmd == "release":
        cmd_release(args)
    elif args.cmd == "attacked":
        cmd_attacked(args)
    elif args.cmd == "audited":
        cmd_audited(args)
    elif args.cmd == "status":
        cmd_status()
    elif args.cmd == "check-commit":
        problems = Guard(_root({})).check_commit()
        print("\n".join(problems) if problems else "commit gate: clean")
        sys.exit(1 if problems else 0)


if __name__ == "__main__":
    main(sys.argv[1:])
