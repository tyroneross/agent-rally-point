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

    def test_supervisor_enforces_internal_deadline_and_orders_heartbeat(self):
        # A tight internal deadline must cut every lifecycle phase off well
        # before the host's 5s, return host-valid JSON, release BOTH pipes
        # (a host waits for EOF), and post the detached idle heartbeat only
        # after the foreground reads it would otherwise contend with.
        import time
        with tempfile.TemporaryDirectory(dir="/var/tmp") as directory:
            base = Path(directory)
            repo = base / "repo"
            (repo / ".rally").mkdir(parents=True)
            stub = base / "rally"
            stub.write_text('#!/bin/bash\n[ "$1" = --warm ] && exit 0\n'
                            'printf "%s\\n" "$*" >> "$CALLS"\n'
                            'sleep 10\n')
            stub.chmod(0o700)
            subprocess.run([str(stub), "--warm"], check=True, timeout=5)
            for phase, event in (("start", "SessionStart"),
                                 ("idle", "UserPromptSubmit"),
                                 ("after-write", "Stop")):
                with self.subTest(phase=phase):
                    calls = base / ("deadline-" + phase)
                    env = {k: v for k, v in os.environ.items()
                           if not k.startswith("RALLY_")}
                    env.update(RALLY_BIN=str(stub), RALLY_HOOKS="on",
                               RALLY_NATIVE_HOOK="off", RALLY_HOOK_MS_BUDGET_SCALE="1",
                               RALLY_HOOK_DEADLINE_MS="1500",
                               RALLY_TOOL_ID="codex:deadline", CALLS=str(calls))
                    began = time.monotonic()
                    result = subprocess.run(
                        ["bash", str(HOOK), phase, "codex"], cwd=repo, env=env,
                        input=json.dumps({"cwd": str(repo), "session_id": "d-" + phase,
                                          "hook_event_name": event}),
                        text=True, capture_output=True, timeout=5)
                    elapsed = time.monotonic() - began
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIsInstance(json.loads(result.stdout), dict)
                    self.assertLess(elapsed, 2.5, f"{phase} took {elapsed:.2f}s")
                    for _ in range(40):
                        if calls.exists() and "status post" in calls.read_text():
                            break
                        time.sleep(0.05)
                    lines = calls.read_text().splitlines() if calls.exists() else []
                    posts = [i for i, l in enumerate(lines) if l.startswith("status post")]
                    reads = [i for i, l in enumerate(lines)
                             if l.startswith(("room ", "next ", "status read"))]
                    # Under the tight deadline later reads may be skipped, but
                    # any heartbeat must follow every read that did run.
                    if posts and reads:
                        self.assertGreater(min(posts), max(reads), lines)

    def test_module_initialization_counts_against_host_deadline(self):
        import time
        with tempfile.TemporaryDirectory(dir="/var/tmp") as directory:
            base = Path(directory)
            repo = base / "repo"
            (repo / ".rally").mkdir(parents=True)
            # POSIX is loaded before the old stopwatch started. Simulate
            # startup contention beyond the host limit, then stall the renderer.
            real_posix = subprocess.check_output(
                ["perl", "-MPOSIX", "-e", 'print $INC{"POSIX.pm"}'], text=True)
            modules = base / "modules"
            modules.mkdir()
            (modules / "POSIX.pm").write_text(
                'select undef, undef, undef, 6;\nrequire ' + json.dumps(real_posix) + ';\n1;\n')
            shim = base / "shim"
            shim.mkdir()
            (shim / "node").write_text("#!/bin/bash\nsleep 10\n")
            (shim / "node").chmod(0o700)
            env = {k: v for k, v in os.environ.items() if not k.startswith("RALLY_")}
            env.update(RALLY_HOOKS="on", RALLY_NATIVE_HOOK="off", PERL5LIB=str(modules),
                       PATH=f"{shim}:{env.get('PATH', '')}")
            began = time.monotonic()
            result = subprocess.run(["bash", str(HOOK), "start", "claude_code"],
                                    cwd=repo, env=env, input="{}", text=True,
                                    capture_output=True, timeout=8)
            self.assertLess(time.monotonic() - began, 5)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout), {})

    def test_concurrent_project_and_plugin_startup_stays_within_host_deadline(self):
        from concurrent.futures import ThreadPoolExecutor
        import time
        with tempfile.TemporaryDirectory(dir="/var/tmp") as directory:
            base = Path(directory)
            repo = base / "repo"
            (repo / ".rally").mkdir(parents=True)
            shim = base / "shim"
            shim.mkdir()
            (shim / "node").write_text("#!/bin/bash\nsleep 10\n")
            (shim / "node").chmod(0o700)
            env = {k: v for k, v in os.environ.items() if not k.startswith("RALLY_")}
            env.update(RALLY_HOOKS="on", RALLY_NATIVE_HOOK="off",
                       PATH=f"{shim}:{env.get('PATH', '')}")
            def run(source):
                began = time.monotonic()
                result = subprocess.run(["bash", str(HOOK), "start", "claude_code"],
                                        cwd=repo, env=dict(env, RALLY_HOOK_SOURCE=source),
                                        input='{"session_id":"paired-startup"}', text=True,
                                        capture_output=True, timeout=5)
                return result, time.monotonic() - began
            with ThreadPoolExecutor(max_workers=4) as pool:
                for result, elapsed in pool.map(run, ["project", "plugin"] * 2):
                    self.assertLess(elapsed, 5)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(json.loads(result.stdout), {})

    def test_budget_overrides_cannot_exceed_host_deadline(self):
        import time
        with tempfile.TemporaryDirectory(dir="/var/tmp") as directory:
            base = Path(directory)
            repo = base / "repo"
            (repo / ".rally").mkdir(parents=True)
            shim = base / "shim"
            shim.mkdir()
            (shim / "node").write_text("#!/bin/bash\nsleep 10\n")
            (shim / "node").chmod(0o700)
            env = {k: v for k, v in os.environ.items() if not k.startswith("RALLY_")}
            env.update(RALLY_HOOKS="on", RALLY_NATIVE_HOOK="off",
                       RALLY_HOOK_DEADLINE_MS="59999", RALLY_HOOK_MS_BUDGET_SCALE="16",
                       PATH=f"{shim}:{env.get('PATH', '')}")
            began = time.monotonic()
            result = subprocess.run(["bash", str(HOOK), "start", "claude_code"],
                                    cwd=repo, env=env, input="{}", text=True,
                                    capture_output=True, timeout=6)
            self.assertLess(time.monotonic() - began, 5)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout), {})

    def test_supervisor_overrun_fails_open_with_empty_object(self):
        with tempfile.TemporaryDirectory(dir="/var/tmp") as directory:
            base = Path(directory)
            repo = base / "repo"
            (repo / ".rally").mkdir(parents=True)
            stub = base / "rally"
            stub.write_text("#!/bin/bash\nsleep 10\n")
            stub.chmod(0o700)
            # Rally calls are clamped to the call deadline, so a hung CLI
            # alone cannot overrun. A hung renderer (node) is not clamped and
            # makes the overrun deterministic.
            shim = base / "shim"
            shim.mkdir()
            (shim / "node").write_text("#!/bin/bash\nsleep 10\n")
            (shim / "node").chmod(0o700)
            env = {k: v for k, v in os.environ.items() if not k.startswith("RALLY_")}
            env.update(RALLY_BIN=str(stub), RALLY_HOOKS="on", RALLY_NATIVE_HOOK="off",
                       RALLY_HOOK_DEADLINE_MS="800", RALLY_TOOL_ID="codex:overrun",
                       PATH=f"{shim}:{env.get('PATH', '')}")
            result = subprocess.run(
                ["bash", str(HOOK), "start", "codex"], cwd=repo, env=env,
                input=json.dumps({"session_id": "overrun"}),
                text=True, capture_output=True, timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "{}")
            self.assertIn("internal deadline", result.stderr)


if __name__ == "__main__":
    unittest.main()
