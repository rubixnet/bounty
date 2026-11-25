"""Tests for scripts/claude_fixtures.py, driven only through its command line.

Run with: python3 -m unittest discover -s scripts -p 'test_*.py'

Seams:
- `--check --out DIR`: validates a fixture folder without calling `claude`.
- capture preflight: `--claude PROG --out DIR` with fake `claude` programs;
  must exit non-zero fast with one reason and write nothing.
"""

import json
import os
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "claude_fixtures.py"
SCENARIOS = [
    "normal-multi-tool",
    "tool-failure",
    "permission-request",
    "subagent",
    "cwd-change",
    "interrupted-turn",
    "api-error-stop",
    "stop-track",
]


def run(*args, timeout=60):
    return subprocess.run(
        [sys.executable, str(SCRIPT), *args],
        capture_output=True,
        text=True,
        timeout=timeout,
    )


def write_clean_fixtures(root: Path):
    for name in SCENARIOS:
        d = root / name
        (d / "hooks").mkdir(parents=True)
        (d / "manifest.json").write_text(
            json.dumps(
                {
                    "scenario": name,
                    "claude_code_version": "2.1.332",
                    "captured_at": "2026-11-25",
                    "os": "macos",
                    "simulated": False,
                    "session_ids": ["11111111-2222-3333-4444-555555555555"],
                }
            )
        )
        (d / "hooks" / "000-SessionStart.json").write_text(
            json.dumps(
                {
                    "hook_event_name": "SessionStart",
                    "session_id": "11111111-2222-3333-4444-555555555555",
                    "cwd": "<REPO>",
                    "transcript_path": "<CLAUDE_CONFIG_DIR>/projects/x/s.jsonl",
                }
            )
        )
        (d / "transcript.jsonl").write_text(
            json.dumps({"type": "user", "message": {"content": "TRACTION-CANARY-" + name}})
            + "\n"
        )


class CheckMode(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name) / "claude-code"
        write_clean_fixtures(self.root)

    def tearDown(self):
        self.tmp.cleanup()

    def assert_check_fails(self, needle):
        r = run("--check", "--out", str(self.root))
        self.assertNotEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn(needle, r.stdout + r.stderr)

    def test_clean_fixtures_pass(self):
        r = run("--check", "--out", str(self.root))
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)

    def test_missing_scenario_fails(self):
        for p in sorted((self.root / "subagent").rglob("*"), reverse=True):
            p.rmdir() if p.is_dir() else p.unlink()
        (self.root / "subagent").rmdir()
        self.assert_check_fails("subagent")

    def test_unparseable_hook_payload_fails(self):
        (self.root / "tool-failure" / "hooks" / "001-Stop.json").write_text("{not json")
        self.assert_check_fails("001-Stop.json")

    def test_invalid_transcript_line_fails(self):
        with open(self.root / "cwd-change" / "transcript.jsonl", "a") as f:
            f.write("{broken\n")
        self.assert_check_fails("transcript.jsonl")

    def test_manifest_missing_version_fails(self):
        m = self.root / "subagent" / "manifest.json"
        data = json.loads(m.read_text())
        del data["claude_code_version"]
        m.write_text(json.dumps(data))
        self.assert_check_fails("claude_code_version")

    def test_leftover_home_path_fails(self):
        (self.root / "normal-multi-tool" / "hooks" / "001-Stop.json").write_text(
            json.dumps({"cwd": "/Users/alice/tmp/repo"})
        )
        self.assert_check_fails("home path")

    def test_leftover_temp_path_fails(self):
        (self.root / "normal-multi-tool" / "hooks" / "001-Stop.json").write_text(
            json.dumps({"cwd": "/var/folders/ab/xyz/T/repo"})
        )
        self.assert_check_fails("temp path")

    def test_leftover_windows_home_path_fails(self):
        (self.root / "normal-multi-tool" / "hooks" / "001-Stop.json").write_text(
            json.dumps({"cwd": "C:\\Users\\alice\\repo"})
        )
        self.assert_check_fails("home path")

    def test_leftover_email_fails(self):
        with open(self.root / "subagent" / "transcript.jsonl", "a") as f:
            f.write(json.dumps({"email": "alice@corp.com"}) + "\n")
        self.assert_check_fails("email")

    def test_leftover_account_identifier_fails(self):
        with open(self.root / "subagent" / "transcript.jsonl", "a") as f:
            f.write(json.dumps({"accountUuid": "9b1f0c2e-0000-4000-8000-000000000000"}) + "\n")
        self.assert_check_fails("account")

    def test_missing_canary_fails(self):
        (self.root / "tool-failure" / "transcript.jsonl").write_text(
            json.dumps({"type": "user"}) + "\n"
        )
        self.assert_check_fails("canary")

    def test_session_ids_and_uuids_are_allowed(self):
        with open(self.root / "subagent" / "transcript.jsonl", "a") as f:
            f.write(
                json.dumps(
                    {
                        "uuid": "0f8e2c1a-1b2c-4d3e-8f90-123456789abc",
                        "sessionId": "11111111-2222-3333-4444-555555555555",
                    }
                )
                + "\n"
            )
        r = run("--check", "--out", str(self.root))
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)


def fake_claude(dirpath: Path, body: str) -> Path:
    p = dirpath / "fake-claude"
    p.write_text("#!" + sys.executable + "\n" + body)
    p.chmod(p.stat().st_mode | stat.S_IXUSR)
    return p


@unittest.skipIf(os.name == "nt", "fake claude programs use a shebang")
class Preflight(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.out = self.dir / "out"

    def tearDown(self):
        self.tmp.cleanup()

    def assert_preflight_fails(self, prog, reason, within=15):
        start = time.monotonic()
        r = run("--claude", str(prog), "--out", str(self.out), "--preflight-timeout", "3")
        elapsed = time.monotonic() - start
        self.assertNotEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn(reason, r.stdout + r.stderr)
        self.assertLess(elapsed, within)
        self.assertFalse(self.out.exists(), "preflight failure must write nothing")

    def test_missing_program(self):
        self.assert_preflight_fails(self.dir / "does-not-exist", "claude-missing")

    def test_unparseable_version(self):
        prog = fake_claude(self.dir, "print('banana')\n")
        self.assert_preflight_fails(prog, "claude-missing")

    def test_not_logged_in(self):
        prog = fake_claude(
            self.dir,
            "import sys, json\n"
            "if '--version' in sys.argv: print('2.1.332 (Claude Code)'); sys.exit(0)\n"
            "print(json.dumps({'type':'result','is_error':True,"
            "'result':'Not logged in · Please run /login'}))\n",
        )
        self.assert_preflight_fails(prog, "not-logged-in")

    def test_no_network(self):
        prog = fake_claude(
            self.dir,
            "import sys, json\n"
            "if '--version' in sys.argv: print('2.1.332 (Claude Code)'); sys.exit(0)\n"
            "print(json.dumps({'type':'result','is_error':True,"
            "'result':'API Error: Connection error.'}))\n",
        )
        self.assert_preflight_fails(prog, "no-network")

    def test_hanging_call_times_out(self):
        prog = fake_claude(
            self.dir,
            "import sys, time\n"
            "if '--version' in sys.argv: print('2.1.332 (Claude Code)'); sys.exit(0)\n"
            "time.sleep(120)\n",
        )
        self.assert_preflight_fails(prog, "timeout")


if __name__ == "__main__":
    unittest.main()
