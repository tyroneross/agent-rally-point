#!/usr/bin/env python3
"""A slow CLI must not consume the host's entire five-second hook deadline."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


HOOK = Path(__file__).resolve().parents[2] / "hooks/rally-coordination-hook.sh"


class LifecycleBudgetTests(unittest.TestCase):
    def test_all_lifecycle_phases_return_json_before_host_deadline(self):
        # The executable lives outside the scanned repo, as SEC-001 requires.
        with tempfile.TemporaryDirectory(dir="/var/tmp") as directory:
            base = Path(directory)
            repo = base / "repo"
            (repo / ".rally").mkdir(parents=True)
            stub = base / "rally"
            stub.write_text('#!/bin/bash\n[ "$1" = --warm ] && exit 0\n'
                            'printf "%s\\n" "$*" >> "$CALLS"\n'
                            'sleep 10\n')
            stub.chmod(0o700)
            # Pay macOS's first-exec evaluation outside the measured probe.
            subprocess.run([str(stub), "--warm"], check=True, timeout=5)
            for phase, event in (("start", "SessionStart"),
                                 ("idle", "UserPromptSubmit"),
                                 ("after-write", "Stop")):
                with self.subTest(phase=phase):
                    calls = base / phase
                    env = {k: v for k, v in os.environ.items()
                           if not k.startswith("RALLY_")}
                    env.update(RALLY_BIN=str(stub), RALLY_HOOKS="on",
                               RALLY_NATIVE_HOOK="off", RALLY_HOOK_MS_BUDGET_SCALE="1",
                               RALLY_TOOL_ID="codex:lifecycle-budget", CALLS=str(calls))
                    result = subprocess.run(
                        ["bash", str(HOOK), phase, "codex"], cwd=repo, env=env,
                        input=json.dumps({"cwd": str(repo), "session_id": phase,
                                          "hook_event_name": event}),
                        text=True, capture_output=True, timeout=5)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIsInstance(json.loads(result.stdout), dict)
                    invoked = calls.read_text()
                    self.assertIn("hooks status", invoked)
                    self.assertIn("next --tool", invoked)
                    if phase == "start":
                        self.assertIn("enter --tool", invoked)
                        self.assertIn("room --json", invoked)
                    if phase == "after-write":
                        self.assertIn("check before-complete", invoked)


if __name__ == "__main__":
    unittest.main()
