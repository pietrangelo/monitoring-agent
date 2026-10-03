#!/usr/bin/env python3
"""Tests for the test-contract guard. Run: python3 .claude/hooks/test_tdd_guard.py

Each case builds a throwaway git repository, freezes a tree, then presents an edit the way
Claude Code's hooks would, and asserts whether the guard refuses it.
"""

from __future__ import annotations

import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import tdd_guard as g  # noqa: E402

ALERTS = '''
pub fn over(v: f32, t: f32) -> bool { v > t }

#[cfg(test)]
mod tests {
    use super::*;

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
        done = subprocess.run(
            [sys.executable, str(Path(g.__file__)), cmd], input=json.dumps(payload), capture_output=True, text=True, env=env
        )
        return done.returncode, done.stderr

    def cli(self, *args: str) -> tuple[int, str]:
        env = {**os.environ, "CLAUDE_PROJECT_DIR": str(self.root)}
        done = subprocess.run([sys.executable, str(Path(g.__file__)), *args], capture_output=True, text=True, env=env)
        return done.returncode, done.stdout + done.stderr

    def freeze(self) -> None:
        code, _ = self.hook("post-bash", {"tool_name": "Bash", "tool_input": {"command": "cargo test"}})
        assert code == 0

    def edit(self, rel: str, old: str, new: str) -> tuple[int, str]:
        return self.hook(
            "pre-edit",
            {"tool_name": "Edit", "tool_input": {"file_path": str(self.root / rel), "old_string": old, "new_string": new}},
        )


class ParsingTests(unittest.TestCase):
    def test_finds_every_test_fn_and_survives_braces_in_literals(self):
        fns = {fn.name: fn for fn in g.rust_test_fns(ALERTS)}
        self.assertEqual(set(fns), {"fires_above_threshold", "json_fixture_has_braces"})
        lines = ALERTS.splitlines()
        self.assertTrue(lines[fns["json_fixture_has_braces"].end].strip() == "}", "closing brace of the async test")
        self.assertTrue(lines[fns["json_fixture_has_braces"].start].strip() == "#[tokio::test]")

    def test_digest_ignores_formatting(self):
        cases = [
            ("reflowed", ALERTS.replace("    ", "  "), True),
            ("threshold flipped", ALERTS.replace("v > t", "v >= t"), True),  # production code: not in the test fn
            ("expectation flipped", ALERTS.replace('("at", 90.0, false)', '("at", 90.0, true)'), False),
            ("row dropped", ALERTS.replace('("at", 90.0, false), ', ""), False),
        ]
        before = {fn.name: fn.digest for fn in g.rust_test_fns(ALERTS)}
        for name, text, same in cases:
            after = {fn.name: fn.digest for fn in g.rust_test_fns(text)}
            self.assertEqual(before["fires_above_threshold"] == after["fires_above_threshold"], same, name)

    def test_helper_lines_exclude_test_fns(self):
        surface = g.surface_of("src/alerts.rs", ALERTS)
        self.assertIn("fn fixture() -> f32 { 90.0 }", surface.helper_lines)
        self.assertNotIn('assert_eq!(over(v, 90.0), expected, "{name}");', surface.helper_lines)
        self.assertNotIn("pub fn over(v: f32, t: f32) -> bool { v > t }", surface.helper_lines)

    def test_integration_file_is_entirely_test_surface(self):
        surface = g.surface_of("tests/startup.rs", INTEGRATION)
        self.assertIn("fn spawn() -> u16 { 8080 }", surface.helper_lines)
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

    def test_shell_write_detection(self):
        cases = [
            ("sed -i 's/a/b/' src/alerts.rs", True),
            ("perl -pi -e 's/a/b/' src/x.rs", True),
            ("cat > src/alerts.rs <<'EOF'", True),
            ("echo x >> tests/startup.rs", True),
            ("tee system-hub/src/db.rs", True),
            ("cargo test -- alerts 2>&1 | head -n 40", False),
            ("grep -n assert src/alerts.rs", False),
            ("git diff -- src/alerts.rs > /tmp/out.diff", False),
            ("sed -n '1,40p' src/alerts.rs", False),
            ("cargo clippy --all-targets 2>&1 | tee /tmp/clippy.log", False),
            ("sed -i 's/a/b/' notes.py && grep -rn x --include=*.rs src", False),
            ("cp src/alerts.rs /tmp/x && sed -i 's/a/b/' /tmp/x/alerts.rs", True),
        ]
        for command, denied in cases:
            self.assertEqual(g.writes_source_in_place(command), denied, command)

    def test_test_run_and_commit_detection(self):
        self.assertTrue(g.TEST_RUN.search("cd system-hub && cargo test --locked -- db::"))
        self.assertTrue(g.TEST_RUN.search("node system-hub/dashboard-tests/xss.mjs"))
        self.assertFalse(g.TEST_RUN.search("cargo build --release"))
        self.assertTrue(g.GIT_COMMIT.search("git commit -m x"))
        self.assertTrue(g.GIT_COMMIT.search("git -C /x commit --amend"))
        self.assertFalse(g.GIT_COMMIT.search("git log --oneline"))


class EditGateTests(unittest.TestCase):
    def setUp(self):
        self.repo = Repo()
        self.repo.freeze()

    def tearDown(self):
        self.repo.dir.cleanup()

    def test_edits_to_frozen_tests(self):
        cases = [
            ("flip expectation", "src/alerts.rs", '("at", 90.0, false)', '("at", 90.0, true)', True),
            ("drop a row", "src/alerts.rs", '("at", 90.0, false), ', "", True),
            ("ignore a frozen test", "src/alerts.rs", "#[test]\n    fn fires", '#[test]\n    #[ignore = "later"]\n    fn fires', True),
            ("rename a test", "src/alerts.rs", "fn fires_above_threshold", "fn fires_over_threshold", True),
            ("delete a test", "src/alerts.rs", "#[tokio::test]", "#[cfg(any())]", True),
            ("weaken a fixture helper", "src/alerts.rs", "fn fixture() -> f32 { 90.0 }", "fn fixture() -> f32 { 0.0 }", True),
            ("change production code", "src/alerts.rs", "v > t", "v >= t", False),
            ("add a new test", "src/alerts.rs", "    #[tokio::test]", "    #[test]\n    fn new_one() { assert!(true); }\n\n    #[tokio::test]", False),
            ("add a row", "src/alerts.rs", '("over", 90.5, true)]', '("over", 90.5, true), ("far", 99.0, true)]', True),
            ("add a helper", "src/alerts.rs", "    fn fixture()", "    fn other() {}\n    fn fixture()", False),
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
        code, err = self.repo.hook(
            "pre-edit", {"tool_name": "Write", "tool_input": {"file_path": str(self.repo.root / "src/alerts.rs"), "content": content}}
        )
        self.assertEqual(code, 2, err)
        self.assertIn("fires_above_threshold", err)

    def test_unfrozen_file_is_free_until_run(self):
        self.repo.write("src/fresh.rs", "#[cfg(test)]\nmod tests {\n    #[test]\n    fn a() { assert!(false); }\n}\n")
        code, _ = self.repo.edit("src/fresh.rs", "assert!(false)", "assert!(true)")
        self.assertEqual(code, 0)
        self.repo.freeze()
        code, _ = self.repo.edit("src/fresh.rs", "assert!(false)", "assert!(true)")
        self.assertEqual(code, 2)

    def test_release_opens_exactly_the_named_test(self):
        code, out = self.repo.cli("release", "src/alerts.rs", "fires_above_threshold", "--reason", "DECORATION: cheat passed", "--authority", "adversary")
        self.assertEqual(code, 0, out)
        code, _ = self.repo.edit("src/alerts.rs", '("at", 90.0, false)', '("at", 90.0, true)')
        self.assertEqual(code, 0)
        code, _ = self.repo.edit("src/alerts.rs", "let s = \"{\";", "let s = \"x\";")
        self.assertEqual(code, 2, "the other test stays frozen")
        self.assertIn("authority=adversary", (self.repo.root / g.RELEASE_LOG).read_text())

    def test_release_requires_reason_and_authority(self):
        code, _ = self.repo.cli("release", "src/alerts.rs", "fires_above_threshold", "--reason", "x")
        self.assertNotEqual(code, 0)
        code, _ = self.repo.cli("release", "src/alerts.rs", "--authority", "user")
        self.assertNotEqual(code, 0)

    def test_shell_write_is_refused_before_it_runs(self):
        code, err = self.repo.hook("pre-bash", {"tool_name": "Bash", "tool_input": {"command": "sed -i 's/false/true/' src/alerts.rs"}})
        self.assertEqual(code, 2)
        self.assertIn("Edit or Write", err)


class CommitGateTests(unittest.TestCase):
    def setUp(self):
        self.repo = Repo()
        self.repo.freeze()

    def tearDown(self):
        self.repo.dir.cleanup()

    def commit(self) -> tuple[int, str]:
        return self.repo.hook("pre-bash", {"tool_name": "Bash", "tool_input": {"command": "git commit -m 'x'"}})

    def test_clean_tree_commits(self):
        self.assertEqual(self.commit()[0], 0)

    def test_bypassed_test_edit_is_caught_at_commit(self):
        self.repo.write("src/alerts.rs", ALERTS.replace('("at", 90.0, false)', '("at", 90.0, true)'))
        code, err = self.commit()
        self.assertEqual(code, 2)
        self.assertIn("fires_above_threshold", err)
        self.assertIn("vs HEAD", err)

    def test_new_test_must_be_run_then_attacked_then_audited(self):
        self.repo.write("src/alerts.rs", ALERTS.replace("    #[tokio::test]", "    #[test]\n    fn new_one() { assert!(over(1.0, 0.0)); }\n\n    #[tokio::test]"))
        code, err = self.commit()
        self.assertEqual(code, 2)
        self.assertIn("has not been run", err)

        self.repo.freeze()
        code, err = self.commit()
        self.assertEqual(code, 2)
        self.assertIn("not been attacked", err)

        report = {"content": [{"type": "text", "text": "MODE: red\nVERDICT: EVIDENCE\nCheat that passed: none"}]}
        code, err = self.repo.hook(
            "post-agent", {"tool_name": "Agent", "tool_input": {"subagent_type": "red-test-adversary", "prompt": "attack"}, "tool_response": report}
        )
        self.assertEqual(code, 0, err)
        code, err = self.commit()
        self.assertEqual(code, 2)
        self.assertIn("rosette-auditor", err)

        code, out = self.repo.cli("audited", "--in-process", "--verdict", "HOLDS")
        self.assertEqual(code, 0, out)
        self.assertEqual(self.commit()[0], 0)

    def test_in_process_attack_declaration_counts_and_is_logged(self):
        self.repo.write("src/alerts.rs", ALERTS.replace("    #[tokio::test]", "    #[test]\n    fn new_one() { assert!(over(1.0, 0.0)); }\n\n    #[tokio::test]"))
        self.repo.freeze()
        code, out = self.repo.cli("attacked", "--in-process", "--verdict", "EVIDENCE", "--closest", "return true")
        self.assertEqual(code, 0, out)
        self.assertIn("src/alerts.rs::new_one", out)
        self.assertIn("in-process", (self.repo.root / g.EVENT_LOG).read_text())

    def test_release_lets_a_changed_test_commit_and_is_consumed(self):
        self.repo.cli("release", "src/alerts.rs", "fires_above_threshold", "--reason", "user asked: threshold is inclusive", "--authority", "user")
        self.repo.write("src/alerts.rs", ALERTS.replace('("at", 90.0, false)', '("at", 90.0, true)'))
        self.repo.freeze()
        code, err = self.commit()
        self.assertEqual(code, 2, "rewritten test must be attacked again")
        self.assertIn("not been attacked", err)
        self.repo.cli("attacked", "--in-process", "--verdict", "EVIDENCE", "--closest", "always true")
        self.repo.cli("audited", "--in-process", "--verdict", "HOLDS")
        self.assertEqual(self.commit()[0], 0)
        code, _ = self.repo.hook("post-bash", {"tool_name": "Bash", "tool_input": {"command": "git commit -m x"}, "tool_response": {"exit_code": 0}})
        self.assertEqual(code, 0)
        self.assertIn("consumed", (self.repo.root / g.RELEASE_LOG).read_text())
        self.assertEqual(g.Guard(self.repo.root).released_keys(), set())

    def test_freeze_after_bypass_warns(self):
        self.repo.write("src/alerts.rs", ALERTS.replace('("at", 90.0, false)', '("at", 90.0, true)'))
        code, err = self.repo.hook("post-bash", {"tool_name": "Bash", "tool_input": {"command": "cargo test"}})
        self.assertEqual(code, 2)
        self.assertIn("bypassed", err)


if __name__ == "__main__":
    unittest.main(verbosity=1)
