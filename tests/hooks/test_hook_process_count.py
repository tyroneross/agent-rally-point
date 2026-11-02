#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Each Rally hook fire launches the rally CLI at most once.

Drives hooks/rally-coordination-hook.sh against the REAL debug binary through
a counting shim (every exec appends its argv to a log, then execs the binary),
the same PATH-shim method used to measure the 6/4/5-call baseline. After the
one-time capabilities probe per binary:

  start / idle / after-write  -> exactly 1 rally process (`hook batch`)
  before-write, mutation      -> exactly 1 (`hook before-write`)
  before-write, read or shell -> 0 (constant `{}` answered by the wrapper)

It also pins byte parity: the batched lifecycle output equals the legacy
multi-call output (RALLY_HOOK_BATCH=off) on identical copies of one room.

Requires a target/debug/rally that advertises `hook batch`
(`cargo build -p rally-cli`); skipped otherwise.
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[2]
HOOK = ROOT / "hooks/rally-coordination-hook.sh"
BIN = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")) / "debug/rally"

EVENTS = {"start": "SessionStart", "idle": "UserPromptSubmit", "after-write": "Stop"}


def _env(**extra):
    env = {k: v for k, v in os.environ.items() if not k.startswith("RALLY_")}
    env.update(RALLY_HOOK_SOURCE="plugin", RALLY_HOOK_MS_BUDGET_SCALE="4")
    env.update(extra)
    return env


def _bin_supports_batch():
    """True when BIN advertises `hook batch`. An older build (for example a
    stale target/ in another checkout) still runs the multi-call path, so
    counting its calls would report failures that are not regressions."""
    if not BIN.exists():
        return False
    try:
        out = subprocess.run([str(BIN), "hook", "capabilities", "--json"],
                             capture_output=True, text=True, timeout=30).stdout
        return "batch" in json.loads(out).get("data", {}).get("hook", {})
    except (OSError, subprocess.SubprocessError, ValueError, AttributeError):
        return False


@unittest.skipUnless(_bin_supports_batch(), f"{BIN} missing or predates `rally hook batch`; run `cargo build -p rally-cli`")
class HookProcessCountTests(unittest.TestCase):
    def setUp(self):
        self.base = Path(tempfile.mkdtemp(dir="/var/tmp", prefix="hook-count-"))
        self.home = self.base / "home"
        self.home.mkdir()
        self.seed = self.base / "seed"
        (self.seed / "src").mkdir(parents=True)
        (self.seed / "src/a.txt").write_text("a\n")
        subprocess.run(["git", "init", "-q"], cwd=self.seed, check=True)
        self._rally(self.seed, "init", "--json")
        self._rally(self.seed, "enter", "--tool", "codex:peer", "--json")
        self._rally(self.seed, "say", "claim", "--tool", "codex:peer", "--path",
                    "src/a.txt", "--subject", "peer editing a", "--json")
        # The shim lives outside every repo (SEC-001 refuses an in-repo binary).
        self.shim = self.base / "bin/rally"
        self.shim.parent.mkdir()
        self.shim.write_text('#!/bin/bash\nprintf \'%s\\n\' "$*" >> "$CALLS"\n'
                             f'exec "{BIN}" "$@"\n')
        self.shim.chmod(0o755)

    def tearDown(self):
        shutil.rmtree(self.base, ignore_errors=True)

    def _rally(self, cwd, *args):
        subprocess.run([str(BIN), *args], cwd=cwd, check=True, capture_output=True,
                       env=_env(HOME=str(self.home), RALLY_GLOBAL_INDEX="1"))

    def _fire(self, repo, phase, payload, calls, **extra):
        calls.write_text("")
        began = time.monotonic()
        result = subprocess.run(
            ["bash", str(HOOK), phase, "claude_code"], cwd=repo,
            input=json.dumps(payload), text=True, capture_output=True, timeout=10,
            env=_env(HOME=str(self.home), RALLY_GLOBAL_INDEX="1",
                     RALLY_BIN=str(self.shim), CALLS=str(calls), **extra))
        elapsed = time.monotonic() - began
        self.assertEqual(result.returncode, 0, result.stderr)
        # A detached child (the legacy heartbeat) may log after exit.
        time.sleep(0.3)
        return result.stdout, calls.read_text().splitlines(), elapsed

    def test_each_hook_fire_launches_rally_at_most_once(self):
        repo = self.base / "repo"
        shutil.copytree(self.seed, repo)
        calls = self.base / "calls.log"
        # Warm the one-time capabilities probe (cached per binary identity).
        self._fire(repo, "start", {"session_id": "warm", "hook_event_name": "SessionStart"}, calls)
        for phase, event in EVENTS.items():
            with self.subTest(phase=phase):
                out, lines, _ = self._fire(
                    repo, phase, {"session_id": "count", "cwd": str(repo),
                                  "hook_event_name": event}, calls)
                self.assertEqual(len(lines), 1, f"{phase} launched rally {len(lines)}x: {lines}")
                self.assertTrue(lines[0].startswith("hook batch "), lines)
                self.assertIsInstance(json.loads(out or "{}"), dict)
        for tool, tool_input, want in (
            ("Edit", {"file_path": str(repo / "src/b.txt"), "old_string": "a", "new_string": "b"}, 1),
            ("Bash", {"command": "ls"}, 0),
            ("exec_command", {"cmd": "ls"}, 0),
            ("Read", {"file_path": str(repo / "src/a.txt")}, 0),
        ):
            with self.subTest(tool=tool):
                out, lines, _ = self._fire(
                    repo, "before-write",
                    {"session_id": "count", "cwd": str(repo), "hook_event_name": "PreToolUse",
                     "tool_name": tool, "tool_input": tool_input}, calls)
                self.assertEqual(len(lines), want, f"{tool}: {lines}")
                if want == 0:
                    self.assertEqual(out, "{}")

    def test_legacy_binary_without_batch_keeps_multi_call_path(self):
        repo = self.base / "repo"
        shutil.copytree(self.seed, repo)
        calls = self.base / "calls.log"
        _, lines, _ = self._fire(repo, "idle", {"session_id": "old", "hook_event_name": "UserPromptSubmit"},
                                 calls, RALLY_HOOK_BATCH="off")
        self.assertGreaterEqual(len(lines), 3, lines)
        self.assertFalse(any(l.startswith("hook batch") for l in lines), lines)

    def test_batched_output_is_byte_identical_to_legacy_multi_call(self):
        legacy, batch = self.base / "legacy", self.base / "batch"
        shutil.copytree(self.seed, legacy)
        shutil.copytree(self.seed, batch)
        calls = self.base / "calls.log"
        for phase, event in EVENTS.items():
            with self.subTest(phase=phase):
                outs = {}
                for name, repo, flag in (("legacy", legacy, "off"), ("batch", batch, "on")):
                    outs[name], lines, _ = self._fire(
                        repo, phase, {"session_id": "parity", "cwd": str(repo),
                                      "hook_event_name": event},
                        calls, RALLY_HOOK_BATCH=flag)
                    if flag == "on":
                        self.assertTrue(any(l.startswith("hook batch") for l in lines), lines)
                self.assertEqual(outs["batch"], outs["legacy"])
                time.sleep(1.1)  # keep both copies in the same wall-clock second


if __name__ == "__main__":
    unittest.main()
