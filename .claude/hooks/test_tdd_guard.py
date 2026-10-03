#!/usr/bin/env python3
"""Tests for the test-contract guard. Run: python3 .claude/hooks/test_tdd_guard.py

Each case builds a throwaway git repository, freezes a tree, then presents an edit the way
Claude Code's hooks would, and asserts whether the guard refuses it.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import tdd_guard as g  # noqa: E402

ALERTS = '''
pub fn over(v: f32, t: f32) -> bool { v > t }

pub fn fmt(v: f32) -> String { format!("{v} GB") }

#[cfg(test)]
mod tests {
    use super::*;

    // a fixture comment
    fn fixture() -> f32 { 90.0 }

    #[test]
    fn fires_above_threshold() {
        let cases = [("at", 90.0, false), ("over", 90.5, true)];
        for (name, v, expected) in cases {
            assert_eq!(over(v, 90.0), expected, "{name}");
        }
    }

    #[tokio::test]
    async fn json_fixture_has_braces() {
        let raw = r#"{"a": "}"}"#;
        let s = "{";
        let c = '{';
        assert!(raw.contains(s) && c == '{');
    }

    #[test]
    fn formats_with_a_space() {
        assert_eq!(fmt(1.5), "1.5 GB");
    }
}
'''

INTEGRATION = '''
fn spawn() -> u16 { 8080 }

#[tokio::test(start_paused = true)]
async fn starts() {
    assert_eq!(spawn(), 8080);
}
'''

XSS = """const checks = [];
checks.push('<img onerror>');
if (!ok) process.exit(1);
"""

NEW_TEST = "    #[test]\n    fn new_one() { assert!(over(1.0, 0.0)); }\n\n    #[tokio::test]"
ADVERSARY_OK = {"content": [{"type": "text", "text": "MODE: red\nVERDICT: EVIDENCE\nCheat that passed: none"}]}


def git(root: Path, *args: str) -> str:
    return subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True, check=True).stdout


class Repo:
    def __init__(self):
        self.dir = tempfile.TemporaryDirectory()
        self.root = Path(self.dir.name)
        git(self.root, "init", "-q")
        git(self.root, "config", "user.email", "t@t")
        git(self.root, "config", "user.name", "t")
        self.write("src/alerts.rs", ALERTS)
        self.write("tests/startup.rs", INTEGRATION)
        self.write("system-hub/dashboard-tests/xss.mjs", XSS)
        git(self.root, "add", "-A")
        git(self.root, "commit", "-qm", "base")

    def write(self, rel: str, text: str) -> None:
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def read(self, rel: str) -> str:
        return (self.root / rel).read_text()

    def hook(self, cmd: str, payload: dict) -> tuple[int, str]:
        """Run a hook subcommand as Claude Code would; returns (exit code, stderr)."""
        payload = {"cwd": str(self.root), **payload}
        env = {**os.environ, "CLAUDE_PROJECT_DIR": str(self.root)}
        done = subprocess.run([sys.executable, str(Path(g.__file__)), cmd], input=json.dumps(payload), capture_output=True, text=True, env=env)
        return done.returncode, done.stderr

    def cli(self, *args: str) -> tuple[int, str]:
        env = {**os.environ, "CLAUDE_PROJECT_DIR": str(self.root)}
        done = subprocess.run([sys.executable, str(Path(g.__file__)), *args], capture_output=True, text=True, env=env)
        return done.returncode, done.stdout + done.stderr

    def bash(self, command: str) -> tuple[int, str]:
        return self.hook("pre-bash", {"tool_name": "Bash", "tool_input": {"command": command}})

    def freeze(self) -> tuple[int, str]:
        return self.hook("post-bash", {"tool_name": "Bash", "tool_input": {"command": "cargo test"}})

    def edit(self, rel: str, old: str, new: str) -> tuple[int, str]:
        return self.hook("pre-edit", {"tool_name": "Edit", "tool_input": {"file_path": str(self.root / rel), "old_string": old, "new_string": new}})

    def apply(self, rel: str, old: str, new: str) -> None:
        text = self.read(rel)
        assert old in text, old
        self.write(rel, text.replace(old, new, 1))

    def agent(self, kind: str, report, prompt: str = "attack") -> tuple[int, str]:
        return self.hook("post-agent", {"tool_name": "Agent", "tool_input": {"subagent_type": kind, "prompt": prompt}, "tool_response": report})

    def commit_gate(self) -> tuple[int, str]:
        return self.bash("git commit -m 'x'")

    def ledger(self) -> dict:
        return json.loads((self.root / g.LEDGER).read_text())


class ScannerTests(unittest.TestCase):
    def test_finds_every_test_fn_and_survives_braces_in_literals(self):
        fns = {fn.name: fn for fn in g.rust_test_fns(ALERTS)}
        self.assertEqual(set(fns), {"fires_above_threshold", "json_fixture_has_braces", "formats_with_a_space"})
        lines = ALERTS.splitlines()
        self.assertEqual(lines[fns["json_fixture_has_braces"].end].strip(), "}")
        self.assertEqual(lines[fns["json_fixture_has_braces"].start].strip(), "#[tokio::test]")

    def test_attribute_block_above_the_test_attribute_belongs_to_the_test(self):
        text = ALERTS.replace("    #[test]\n    fn fires", "    #[should_panic]\n    /// doc\n    #[test]\n    fn fires")
        fn = {f.name: f for f in g.rust_test_fns(text)}["fires_above_threshold"]
        self.assertEqual(text.splitlines()[fn.start].strip(), "#[should_panic]")

    def test_unusual_shapes(self):
        cases = [
            ("one-line test", "#[test] fn x() { assert!(true); }", ["x"]),
            ("doc comment between attribute and fn", "#[test]\n/// why\nfn y() {\n}", ["y"]),
            ("attribute then non-fn item", "#[test]\nstruct S;\nfn z() {}", []),
            ("commented-out attribute", "// #[test]\nfn w() {}", []),
            ("block comment with brace", "#[test]\nfn v() {\n  /* { */\n  assert!(true);\n}", ["v"]),
        ]
        for name, text, expected in cases:
            self.assertEqual([f.name for f in g.rust_test_fns(text)], expected, name)

    def test_digest_ignores_formatting_but_not_string_contents(self):
        cases = [
            ("reflowed", ALERTS.replace("    ", "  "), "fires_above_threshold", True),
            ("threshold flipped in production", ALERTS.replace("v > t", "v >= t"), "fires_above_threshold", True),
            ("expectation flipped", ALERTS.replace('("at", 90.0, false)', '("at", 90.0, true)'), "fires_above_threshold", False),
            ("row dropped", ALERTS.replace('("at", 90.0, false), ', ""), "fires_above_threshold", False),
            ("comment added inside", ALERTS.replace("let cases", "// note\n        let cases"), "fires_above_threshold", True),
            ("space inside a string literal", ALERTS.replace('"1.5 GB"', '"1.5GB"'), "formats_with_a_space", False),
            ("format arg spaced", ALERTS.replace('"{name}"', '"{ name }"'), "fires_above_threshold", False),
        ]
        before = {fn.name: fn.digest for fn in g.rust_test_fns(ALERTS)}
        for name, text, test, same in cases:
            after = {fn.name: fn.digest for fn in g.rust_test_fns(text)}
            self.assertEqual(before[test] == after[test], same, name)

    def test_helper_chunks_exclude_test_fns_and_comments_and_survive_reflow(self):
        surface = g.surface_of("src/alerts.rs", ALERTS)
        self.assertIn("fn fixture()->f32{", surface.helpers)
        self.assertNotIn("//afixturecomment", "".join(surface.helpers))
        self.assertFalse(any("assert_eq!(over" in c for c in surface.helpers))
        self.assertFalse(any("pub fn over" in c for c in surface.helpers))
        reflowed = g.surface_of("src/alerts.rs", ALERTS.replace("fn fixture() -> f32 { 90.0 }", "fn fixture() -> f32 {\n        90.0\n    }"))
        self.assertEqual(surface.helpers, reflowed.helpers)
        self.assertEqual({"over", "fmt"}, surface.production_fns)

    def test_integration_file_is_entirely_test_surface(self):
        surface = g.surface_of("tests/startup.rs", INTEGRATION)
        self.assertIn("fn spawn()->u16{", surface.helpers)
        self.assertEqual(set(surface.fns), {"starts"})

    def test_path_kinds(self):
        cases = [
            ("src/alerts.rs", True, False, False),
            ("system-hub/src/db.rs", True, False, False),
            ("tests/startup.rs", True, True, False),
            ("system-hub/tests/fail_closed.rs", True, True, False),
            ("system-hub/dashboard-tests/xss.mjs", True, False, True),
            (".claude/hooks/test_tdd_guard.py", True, False, True),
            ("README.md", False, False, False),
            ("target/debug/x.rs", False, False, False),
        ]
        for rel, protected, integration, additive in cases:
            self.assertEqual(g.is_protected_path(rel), protected, rel)
            self.assertEqual(g.is_integration_test(rel), integration, rel)
            self.assertEqual(g.is_additive_only(rel), additive, rel)


class CommandTests(unittest.TestCase):
    def setUp(self):
        self.repo = Repo()

    def tearDown(self):
        self.repo.dir.cleanup()

    def test_shell_write_detection_is_scoped_to_the_repository(self):
        outside = tempfile.gettempdir()
        cases = [
            ("sed -i 's/a/b/' src/alerts.rs", True),
            ("perl -pi -e 's/a/b/' src/x.rs", True),
            ("perl -0pi -e 's/a/b/' src/x.rs", True),
            ("cat > src/alerts.rs <<'EOF'", True),
            ("echo x >> tests/startup.rs", True),
            ("tee system-hub/src/db.rs", True),
            ("rm tests/startup.rs", True),
            ("git apply fix.patch -- src/alerts.rs", True),
            (f"cp src/alerts.rs {outside}/x.rs && sed -i 's/a/b/' {outside}/x.rs", True),  # the cp reads a repo .rs — refused
            (f"sed -i 's/a/b/' {outside}/tree/src/alerts.rs", False),  # the adversaries' sandbox
            (f"cat > {outside}/tree/src/alerts.rs <<'EOF'", False),
            ("cargo test -- alerts 2>&1 | head -n 40", False),
            ("grep -n assert src/alerts.rs", False),
            ("git diff -- src/alerts.rs > /tmp/out.diff", False),
            ("sed -n '1,40p' src/alerts.rs", False),
            ("cargo clippy --all-targets 2>&1 | tee /tmp/clippy.log", False),
            ("sed -i 's/a/b/' notes.py && grep -rn x --include=*.rs src", False),
            ("git checkout -- src/alerts.rs", False),
        ]
        for command, denied in cases:
            self.assertEqual(g.writes_source_in_place(command, self.repo.root), denied, command)

    def test_test_run_detection(self):
        cases = [
            ("cd system-hub && cargo test --locked -- db::", True),
            ("cargo +nightly test", True),
            ("cargo nextest run", True),
            ("node system-hub/dashboard-tests/xss.mjs", True),
            ("python3 .claude/hooks/test_tdd_guard.py", True),
            ("cargo build --release", False),
            ("cargo test --no-run", True),
            ("echo cargo test", False),
            ("grep -rn 'cargo test' docs/", False),
            ("python3 .claude/hooks/tdd_guard.py release .claude/hooks/test_tdd_guard.py --reason x --authority user", False),
        ]
        for command, expected in cases:
            self.assertEqual(g.is_test_run(command), expected, command)

    def test_commit_detection(self):
        cases = [
            ("git commit -m x", True),
            ("git -C /x commit --amend", True),
            ("git -c user.name=t commit -m x", True),
            ("git --no-pager commit -m x", True),
            ("git add -A && git commit -q -F - <<'EOF'", True),
            ("git log --oneline", False),
            ("grep -rn 'git commit' CLAUDE.md", False),
            ("echo 'git commit'", False),
        ]
        for command, expected in cases:
            self.assertEqual(g.is_git_commit(command), expected, command)


class EditGateTests(unittest.TestCase):
    def setUp(self):
        self.repo = Repo()
        self.assertEqual(self.repo.freeze()[0], 0)

    def tearDown(self):
        self.repo.dir.cleanup()

    def test_edits_to_tests_under_contract(self):
        cases = [
            ("flip expectation", "src/alerts.rs", '("at", 90.0, false)', '("at", 90.0, true)', True),
            ("drop a row", "src/alerts.rs", '("at", 90.0, false), ', "", True),
            ("add a row", "src/alerts.rs", '("over", 90.5, true)]', '("over", 90.5, true), ("far", 99.0, true)]', True),
            ("ignore below #[test]", "src/alerts.rs", "#[test]\n    fn fires", '#[test]\n    #[ignore = "later"]\n    fn fires', True),
            ("ignore above #[test]", "src/alerts.rs", "    #[test]\n    fn fires", '    #[ignore = "later"]\n    #[test]\n    fn fires', True),
            ("should_panic above #[test]", "src/alerts.rs", "    #[test]\n    fn fires", "    #[should_panic]\n    #[test]\n    fn fires", True),
            ("cfg above #[test]", "src/alerts.rs", "    #[test]\n    fn fires", "    #[cfg(any())]\n    #[test]\n    fn fires", True),
            ("cfg on the whole module", "src/alerts.rs", "#[cfg(test)]\nmod tests", "#[cfg(test)]\n#[cfg(any())]\nmod tests", True),
            ("shadow a production fn", "src/alerts.rs", "    use super::*;\n", "    use super::*;\n    fn over(_v: f32, _t: f32) -> bool { true }\n", True),
            ("shadow with a renaming use", "src/alerts.rs", "    use super::*;\n", "    use super::*;\n    use crate::other::always as over;\n", True),
            ("shadow a macro", "src/alerts.rs", "    use super::*;\n", "    use super::*;\n    macro_rules! assert_eq { ($($t:tt)*) => {} }\n", True),
            ("rename a test", "src/alerts.rs", "fn fires_above_threshold", "fn fires_over_threshold", True),
            ("delete a test", "src/alerts.rs", "#[tokio::test]", "#[cfg(any())]", True),
            ("weaken a fixture helper", "src/alerts.rs", "fn fixture() -> f32 { 90.0 }", "fn fixture() -> f32 { 0.0 }", True),
            ("space inside an expected string", "src/alerts.rs", '"1.5 GB"', '"1.5GB"', True),
            ("change production code", "src/alerts.rs", "v > t", "v >= t", False),
            ("add a new test", "src/alerts.rs", "    #[tokio::test]", NEW_TEST, False),
            ("add a helper", "src/alerts.rs", "    fn fixture()", "    fn other() {}\n    fn fixture()", False),
            ("reflow a helper", "src/alerts.rs", "fn fixture() -> f32 { 90.0 }", "fn fixture() -> f32 {\n        90.0\n    }", False),
            ("delete a fixture comment", "src/alerts.rs", "    // a fixture comment\n", "", False),
            ("edit integration test", "tests/startup.rs", "8080);", "8081);", True),
            ("extend xss.mjs", "system-hub/dashboard-tests/xss.mjs", "checks.push('<img onerror>');", "checks.push('<img onerror>');\nchecks.push('<svg onload>');", False),
            ("remove an xss check", "system-hub/dashboard-tests/xss.mjs", "checks.push('<img onerror>');\n", "", True),
            ("escape hatch in xss.mjs", "system-hub/dashboard-tests/xss.mjs", "if (!ok)", "process.exit(0);\nif (!ok)", True),
            ("bare ignore on a new test", "src/alerts.rs", "    #[tokio::test]", "    #[test]\n    #[ignore]\n    fn later() {}\n\n    #[tokio::test]", True),
        ]
        for name, rel, old, new, denied in cases:
            self.assertIn(old, self.repo.read(rel), f"{name}: fixture must contain old_string")
            code, err = self.repo.edit(rel, old, new)
            self.assertEqual(code == 2, denied, f"{name}: {err}")

    def test_write_tool_is_projected_from_content(self):
        content = self.repo.read("src/alerts.rs").replace('("at", 90.0, false)', '("at", 90.0, true)')
        code, err = self.repo.hook("pre-edit", {"tool_name": "Write", "tool_input": {"file_path": str(self.repo.root / "src/alerts.rs"), "content": content}})
        self.assertEqual(code, 2, err)
        self.assertIn("fires_above_threshold", err)

    def test_a_draft_is_free_until_graded(self):
        self.repo.write("src/fresh.rs", "#[cfg(test)]\nmod tests {\n    fn helper() {}\n    #[test]\n    fn a() { assert!(false); }\n}\n")
        self.repo.freeze()
        code, _ = self.repo.edit("src/fresh.rs", "assert!(false)", "assert!(true)")
        self.assertEqual(code, 0, "run but not graded: still the author's draft")
        code, _ = self.repo.edit("src/fresh.rs", "    fn helper() {}\n", "")
        self.assertEqual(code, 0, "helpers of a file not in HEAD are free")
        self.repo.agent("red-test-adversary", ADVERSARY_OK)
        code, err = self.repo.edit("src/fresh.rs", "assert!(false)", "assert!(true)")
        self.assertEqual(code, 2, err)
        self.assertIn("under contract", err)

    def test_a_new_test_in_a_head_file_is_free_until_graded(self):
        self.repo.apply("src/alerts.rs", "    #[tokio::test]", NEW_TEST)
        self.repo.freeze()
        code, _ = self.repo.edit("src/alerts.rs", "assert!(over(1.0, 0.0))", "assert!(true)")
        self.assertEqual(code, 0)
        self.repo.cli("attacked", "--in-process", "--verdict", "EVIDENCE", "--closest", "return true")
        code, _ = self.repo.edit("src/alerts.rs", "assert!(over(1.0, 0.0))", "assert!(true)")
        self.assertEqual(code, 2)

    def test_release_opens_exactly_the_named_test(self):
        code, out = self.repo.cli("release", "src/alerts.rs", "fires_above_threshold", "--reason", "DECORATION: cheat passed", "--authority", "adversary")
        self.assertEqual(code, 0, out)
        code, _ = self.repo.edit("src/alerts.rs", '("at", 90.0, false)', '("at", 90.0, true)')
        self.assertEqual(code, 0)
        code, _ = self.repo.edit("src/alerts.rs", 'let s = "{";', 'let s = "x";')
        self.assertEqual(code, 2, "the other test stays under contract")
        self.assertIn("authority=adversary", (self.repo.root / g.RELEASE_LOG).read_text())

    def test_release_requires_reason_and_a_known_authority(self):
        self.assertNotEqual(self.repo.cli("release", "src/alerts.rs", "fires_above_threshold", "--reason", "x")[0], 0)
        self.assertNotEqual(self.repo.cli("release", "src/alerts.rs", "--authority", "user")[0], 0)
        self.assertNotEqual(self.repo.cli("release", "src/alerts.rs", "--reason", "x", "--authority", "me")[0], 0)

    def test_shell_write_is_refused_before_it_runs(self):
        code, err = self.repo.bash("sed -i 's/false/true/' src/alerts.rs")
        self.assertEqual(code, 2)
        self.assertIn("Edit or Write", err)

    def test_a_corrupt_ledger_refuses_rather_than_resetting(self):
        path = self.repo.root / g.LEDGER
        path.write_text(path.read_text()[:100])
        code, err = self.repo.hook("post-edit", {"tool_name": "Edit", "tool_input": {"file_path": str(self.repo.root / "src/alerts.rs")}})
        self.assertEqual(code, 2)
        self.assertIn("unreadable", err)
        self.assertEqual(len(path.read_text()), 100, "the truncated ledger was not overwritten")
        self.assertIn("unreadable", (self.repo.root / g.EVENT_LOG).read_text())

    def test_an_older_ledger_is_rebuilt_keeping_releases(self):
        path = self.repo.root / g.LEDGER
        old = json.loads(path.read_text())
        old["version"] = 1
        old["released"] = {"src/alerts.rs::fires_above_threshold": {"reason": "r", "authority": "user", "at": "t"}}
        path.write_text(json.dumps(old))
        code, out = self.repo.cli("status")
        self.assertEqual(code, 0, out)
        self.assertIn("frozen tests: 0", out)
        self.assertIn("fires_above_threshold", out)


class RecordTests(unittest.TestCase):
    def setUp(self):
        self.repo = Repo()
        self.repo.apply("src/alerts.rs", "    #[tokio::test]", NEW_TEST)
        self.repo.freeze()

    def tearDown(self):
        self.repo.dir.cleanup()

    def test_only_evidence_grades_and_a_later_verdict_can_ungrade(self):
        key = "src/alerts.rs::new_one"
        self.repo.agent("red-test-adversary", {"content": [{"type": "text", "text": "VERDICT: DECORATION"}]})
        self.assertNotIn(key, self.repo.ledger()["attacked"])
        self.repo.agent("red-test-adversary", ADVERSARY_OK)
        self.assertIn(key, self.repo.ledger()["attacked"])
        self.repo.agent("red-test-adversary", {"content": [{"type": "text", "text": "VERDICT: WEAK-RED"}]})
        self.assertNotIn(key, self.repo.ledger()["attacked"])
        code, out = self.repo.cli("attacked", "--in-process", "--verdict", "DECORATION", "--closest", "x")
        self.assertEqual(code, 0, out)
        self.assertNotIn(key, self.repo.ledger()["attacked"])

    def test_a_report_without_a_verdict_records_nothing_and_says_so(self):
        code, err = self.repo.agent("red-test-adversary", {"content": [{"type": "text", "text": "I could not run cargo."}]})
        self.assertEqual(code, 2)
        self.assertIn("no VERDICT", err)
        self.assertEqual(self.repo.ledger()["last_adversary"], None)

    def test_a_subagent_grades_only_the_tests_its_prompt_names(self):
        self.repo.write("src/other.rs", "#[cfg(test)]\nmod tests {\n    #[test]\n    fn b() { assert!(true); }\n}\n")
        self.repo.freeze()
        self.repo.agent("red-test-adversary", ADVERSARY_OK, prompt="Attack src/other.rs::b in red mode")
        attacked = self.repo.ledger()["attacked"]
        self.assertIn("src/other.rs::b", attacked)
        self.assertNotIn("src/alerts.rs::new_one", attacked)
        self.repo.cli("attacked", "--in-process", "--verdict", "EVIDENCE", "--closest", "x", "--tests", "src/alerts.rs::new_one")
        self.assertIn("src/alerts.rs::new_one", self.repo.ledger()["attacked"])

    def test_the_worst_verdict_in_a_report_decides(self):
        report = {"content": [{"type": "text", "text": "Verdict: HOLDS\n...\nVerdict: VIOLATED\n...\nVerdict: HOLDS"}]}
        self.repo.cli("audited", "--in-process", "--verdict", "HOLDS")
        self.assertIsNotNone(self.repo.ledger()["last_audit"])
        self.repo.agent("rosette-auditor", {"content": [{"type": "text", "text": report["content"][0]["text"].upper()}]})
        self.assertIsNone(self.repo.ledger()["last_audit"])


class CommitGateTests(unittest.TestCase):
    def setUp(self):
        self.repo = Repo()
        self.repo.freeze()

    def tearDown(self):
        self.repo.dir.cleanup()

    def test_clean_tree_commits(self):
        self.assertEqual(self.repo.commit_gate()[0], 0)

    def test_bypassed_test_edit_is_caught_at_commit(self):
        self.repo.write("src/alerts.rs", ALERTS.replace('("at", 90.0, false)', '("at", 90.0, true)'))
        code, err = self.repo.commit_gate()
        self.assertEqual(code, 2)
        self.assertIn("fires_above_threshold", err)
        self.assertIn("worktree vs HEAD", err)

    def test_a_staged_weakening_is_caught_even_when_the_worktree_is_restored(self):
        self.repo.write("src/alerts.rs", ALERTS.replace('("at", 90.0, false)', '("at", 90.0, true)'))
        git(self.repo.root, "add", "src/alerts.rs")
        git(self.repo.root, "restore", "--worktree", "--source=HEAD", "src/alerts.rs")
        code, err = self.repo.commit_gate()
        self.assertEqual(code, 2)
        self.assertIn("index vs HEAD", err)

    def test_a_deleted_test_file_is_caught(self):
        (self.repo.root / "tests/startup.rs").unlink()
        code, err = self.repo.commit_gate()
        self.assertEqual(code, 2)
        self.assertIn("tests/startup.rs: file in HEAD is gone", err)
        self.repo.cli("release", "tests/startup.rs", "--reason", "user asked: moved to system-hub", "--authority", "user")
        self.repo.cli("audited", "--in-process", "--verdict", "HOLDS")
        self.assertEqual(self.repo.commit_gate()[0], 0)

    def test_new_test_must_be_run_then_graded_then_audited(self):
        self.repo.apply("src/alerts.rs", "    #[tokio::test]", NEW_TEST)
        code, err = self.repo.commit_gate()
        self.assertEqual(code, 2)
        self.assertIn("has not been run", err)

        self.repo.freeze()
        code, err = self.repo.commit_gate()
        self.assertEqual(code, 2)
        self.assertIn("no EVIDENCE record", err)

        self.assertEqual(self.repo.agent("red-test-adversary", ADVERSARY_OK)[0], 0)
        code, err = self.repo.commit_gate()
        self.assertEqual(code, 2)
        self.assertIn("rosette-auditor", err)

        self.assertEqual(self.repo.cli("audited", "--in-process", "--verdict", "HOLDS")[0], 0)
        self.assertEqual(self.repo.commit_gate()[0], 0)

    def test_an_edit_after_the_audit_needs_a_new_audit(self):
        self.repo.apply("src/alerts.rs", "v > t", "v >= t")
        self.repo.cli("audited", "--in-process", "--verdict", "HOLDS")
        self.assertEqual(self.repo.commit_gate()[0], 0)
        self.repo.hook("post-edit", {"tool_name": "Edit", "tool_input": {"file_path": str(self.repo.root / "src/alerts.rs")}})
        code, err = self.repo.commit_gate()
        self.assertEqual(code, 2)
        self.assertIn("rosette-auditor", err)

    def test_a_trivial_change_is_declared_instead_of_audited(self):
        self.repo.apply("src/alerts.rs", "pub fn over", "// compares\npub fn over")
        self.assertEqual(self.repo.commit_gate()[0], 2)
        code, out = self.repo.cli("trivial", "--reason", "comment only")
        self.assertEqual(code, 0, out)
        self.assertEqual(self.repo.commit_gate()[0], 0)
        self.assertIn("trivial", (self.repo.root / g.EVENT_LOG).read_text())

    def test_in_process_attack_declaration_counts_and_is_logged(self):
        self.repo.apply("src/alerts.rs", "    #[tokio::test]", NEW_TEST)
        self.repo.freeze()
        code, out = self.repo.cli("attacked", "--in-process", "--verdict", "EVIDENCE", "--closest", "return true")
        self.assertEqual(code, 0, out)
        self.assertIn("src/alerts.rs::new_one", out)
        self.assertIn("in-process", (self.repo.root / g.EVENT_LOG).read_text())

    def test_release_lets_a_changed_test_commit_and_is_consumed(self):
        self.repo.cli("release", "src/alerts.rs", "fires_above_threshold", "--reason", "user asked: threshold is inclusive", "--authority", "user")
        self.repo.write("src/alerts.rs", ALERTS.replace('("at", 90.0, false)', '("at", 90.0, true)'))
        self.repo.freeze()
        code, err = self.repo.commit_gate()
        self.assertEqual(code, 2, "rewritten test must be graded again")
        self.assertIn("no EVIDENCE record", err)
        self.repo.cli("attacked", "--in-process", "--verdict", "EVIDENCE", "--closest", "always true")
        self.repo.cli("audited", "--in-process", "--verdict", "HOLDS")
        self.assertEqual(self.repo.commit_gate()[0], 0)
        code, _ = self.repo.hook("post-bash", {"tool_name": "Bash", "tool_input": {"command": "git commit -m x"}, "tool_response": {"exit_code": 0}})
        self.assertEqual(code, 0)
        self.assertIn("consumed", (self.repo.root / g.RELEASE_LOG).read_text())
        self.assertEqual(g.Guard(self.repo.root).released_keys(), set())

    def test_freeze_after_a_bypass_warns_only_for_tests_under_contract(self):
        self.repo.write("src/alerts.rs", ALERTS.replace('("at", 90.0, false)', '("at", 90.0, true)'))
        code, err = self.repo.freeze()
        self.assertEqual(code, 2)
        self.assertIn("bypassed", err)
        self.repo.write("src/draft.rs", "#[cfg(test)]\nmod tests {\n    #[test]\n    fn d() { assert!(false); }\n}\n")
        self.repo.freeze()
        self.repo.write("src/draft.rs", "#[cfg(test)]\nmod tests {\n    #[test]\n    fn d() { assert!(true); }\n}\n")
        code, err = self.repo.freeze()
        self.assertEqual(code, 0, err)


if __name__ == "__main__":
    unittest.main(verbosity=1)
