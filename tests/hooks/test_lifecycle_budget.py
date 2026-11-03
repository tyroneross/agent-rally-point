#!/usr/bin/env python3
"""Declared caller allowances, explicit cancellation and legacy compatibility."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


HOOK = Path(__file__).resolve().parents[2] / "hooks/rally-coordination-hook.sh"
HOST_TIMEOUT_SECONDS = 30


def declared_host_env():
    env = {k: v for k, v in os.environ.items() if not k.startswith("RALLY_")}
    env["RALLY_HOOK_HOST_TIMEOUT_MS"] = str(HOST_TIMEOUT_SECONDS * 1000)
    return env


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
            subprocess.run([str(stub), "--warm"], check=True, timeout=30)
            for phase, event in (("start", "SessionStart"),
                                 ("idle", "UserPromptSubmit"),
                                 ("after-write", "Stop")):
                with self.subTest(phase=phase):
                    calls = base / phase
                    env = declared_host_env()
                    env.update(RALLY_BIN=str(stub), RALLY_HOOKS="on",
                               RALLY_NATIVE_HOOK="off", RALLY_HOOK_MS_BUDGET_SCALE="1",
                               RALLY_HOOK_TIMEOUT_MS="500",
                               RALLY_TOOL_ID="codex:lifecycle-budget", CALLS=str(calls))
                    result = subprocess.run(
                        ["bash", str(HOOK), phase, "codex"], cwd=repo, env=env,
                        input=json.dumps({"cwd": str(repo), "session_id": phase,
                                          "hook_event_name": event}),
                        text=True, capture_output=True, timeout=HOST_TIMEOUT_SECONDS)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIsInstance(json.loads(result.stdout), dict)
                    invoked = calls.read_text()
                    self.assertIn("hooks status", invoked)
                    # The slow CLI must actually run, but the lifecycle
                    # deadline may skip later calls. Complete call sequences
                    # are graded by the isolated coordination fixtures.

    def test_supervisor_enforces_internal_deadline_and_orders_heartbeat(self):
        # A tight internal deadline must cut every lifecycle phase off well
        # before the declared caller allowance, return valid JSON, release BOTH pipes
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
            subprocess.run([str(stub), "--warm"], check=True, timeout=30)
            for phase, event in (("start", "SessionStart"),
                                 ("idle", "UserPromptSubmit"),
                                 ("after-write", "Stop")):
                with self.subTest(phase=phase):
                    calls = base / ("deadline-" + phase)
                    env = declared_host_env()
                    env.update(RALLY_BIN=str(stub), RALLY_HOOKS="on",
                               RALLY_NATIVE_HOOK="off", RALLY_HOOK_MS_BUDGET_SCALE="1",
                               RALLY_HOOK_DEADLINE_MS="1500",
                               RALLY_TOOL_ID="codex:deadline", CALLS=str(calls))
                    began = time.monotonic()
                    result = subprocess.run(
                        ["bash", str(HOOK), phase, "codex"], cwd=repo, env=env,
                        input=json.dumps({"cwd": str(repo), "session_id": "d-" + phase,
                                          "hook_event_name": event}),
                        text=True, capture_output=True, timeout=HOST_TIMEOUT_SECONDS)
                    elapsed = time.monotonic() - began
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIsInstance(json.loads(result.stdout), dict)
                    self.assertLess(elapsed, HOST_TIMEOUT_SECONDS, f"{phase} took {elapsed:.2f}s")
                    for _ in range(40):
                        if calls.exists() and "status post" in calls.read_text():
                            break
                        time.sleep(0.05)
                    lines = calls.read_text().splitlines() if calls.exists() else []
                    budgets = [int(line.rsplit("--timeout-ms ", 1)[1])
                               for line in lines if "--timeout-ms " in line
                               and not line.startswith("status post ")]
                    self.assertTrue(
                        "1500ms internal deadline" in result.stderr
                        or (budgets and max(budgets) <= 1500),
                        f"short override did not fire: {result.stderr!r}; {lines!r}")
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
            # startup contention beyond the explicit short override, then stall
            # the renderer. The new caller allowance remains30s.
            real_posix = subprocess.check_output(
                ["perl", "-MPOSIX", "-e", 'print $INC{"POSIX.pm"}'], text=True)
            modules = base / "modules"
            modules.mkdir()
            (modules / "POSIX.pm").write_text(
                'select undef, undef, undef, 6;\nrequire ' + json.dumps(real_posix) + ';\n1;\n')
            shim = base / "shim"
            shim.mkdir()
            (shim / "node").write_text("#!/bin/bash\nsleep 60\n")
            (shim / "node").chmod(0o700)
            env = declared_host_env()
            env.update(RALLY_HOOKS="on", RALLY_NATIVE_HOOK="off", PERL5LIB=str(modules),
                       RALLY_HOOK_DEADLINE_MS="1500",
                       PATH=f"{shim}:{env.get('PATH', '')}")
            began = time.monotonic()
            result = subprocess.run(["bash", str(HOOK), "start", "claude_code"],
                                    cwd=repo, env=env, input="{}", text=True,
                                    capture_output=True, timeout=HOST_TIMEOUT_SECONDS)
            self.assertLess(time.monotonic() - began, HOST_TIMEOUT_SECONDS)
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
            (shim / "node").write_text("#!/bin/bash\nsleep 60\n")
            (shim / "node").chmod(0o700)
            env = declared_host_env()
            env.update(RALLY_HOOKS="on", RALLY_NATIVE_HOOK="off",
                       RALLY_HOOK_DEADLINE_MS="1500",
                       PATH=f"{shim}:{env.get('PATH', '')}")
            def run(source):
                began = time.monotonic()
                result = subprocess.run(["bash", str(HOOK), "start", "claude_code"],
                                        cwd=repo, env=dict(env, RALLY_HOOK_SOURCE=source),
                                        input='{"session_id":"paired-startup"}', text=True,
                                        capture_output=True, timeout=HOST_TIMEOUT_SECONDS)
                return result, time.monotonic() - began
            with ThreadPoolExecutor(max_workers=4) as pool:
                for result, elapsed in pool.map(run, ["project", "plugin"] * 2):
                    self.assertLess(elapsed, HOST_TIMEOUT_SECONDS)
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
            (shim / "node").write_text("#!/bin/bash\nsleep 60\n")
            (shim / "node").chmod(0o700)
            env = declared_host_env()
            env.update(RALLY_HOOKS="on", RALLY_NATIVE_HOOK="off",
                       RALLY_HOOK_DEADLINE_MS="59999", RALLY_HOOK_MS_BUDGET_SCALE="16",
                       PATH=f"{shim}:{env.get('PATH', '')}")
            began = time.monotonic()
            result = subprocess.run(["bash", str(HOOK), "start", "claude_code"],
                                    cwd=repo, env=env, input="{}", text=True,
                                    capture_output=True, timeout=HOST_TIMEOUT_SECONDS)
            self.assertLess(time.monotonic() - began, HOST_TIMEOUT_SECONDS)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout), {})

    def test_supervisor_overrun_fails_open_with_empty_object(self):
        with tempfile.TemporaryDirectory(dir="/var/tmp") as directory:
            base = Path(directory)
            repo = base / "repo"
            (repo / ".rally").mkdir(parents=True)
            stub = base / "rally"
            stub.write_text("#!/bin/bash\nsleep 60\n")
            stub.chmod(0o700)
            # Rally calls are clamped to the call deadline, so a hung CLI
            # alone cannot overrun. A hung renderer (node) is not clamped and
            # makes the overrun deterministic.
            shim = base / "shim"
            shim.mkdir()
            (shim / "node").write_text("#!/bin/bash\nsleep 60\n")
            (shim / "node").chmod(0o700)
            env = declared_host_env()
            env.update(RALLY_BIN=str(stub), RALLY_HOOKS="on", RALLY_NATIVE_HOOK="off",
                       RALLY_HOOK_DEADLINE_MS="800", RALLY_TOOL_ID="codex:overrun",
                       PATH=f"{shim}:{env.get('PATH', '')}")
            result = subprocess.run(
                ["bash", str(HOOK), "start", "codex"], cwd=repo, env=env,
                input=json.dumps({"session_id": "overrun"}),
                text=True, capture_output=True, timeout=HOST_TIMEOUT_SECONDS)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "{}")
            self.assertIn("800ms internal deadline", result.stderr)

    def test_declared_allowance_accepts_startup_beyond_legacy_five_seconds(self):
        import shlex
        import shutil
        real_node = shutil.which("node")
        self.assertIsNotNone(real_node)
        with tempfile.TemporaryDirectory(dir="/var/tmp") as directory:
            base = Path(directory)
            repo = base / "repo"
            (repo / ".rally").mkdir(parents=True)
            stub = base / "rally"
            stub.write_text('#!/bin/bash\n[ "$1" = --warm ] && exit 0\n'
                            'case "$1 $2" in "hooks status") '
                            'printf \'{"data":{"hooks":{"enabled":true,"prompt":"once"}}}\';; '
                            '*) printf "{}";; esac\n')
            stub.chmod(0o700)
            shim = base / "shim"
            shim.mkdir()
            (shim / "node").write_text(
                '#!/bin/bash\n[ "$1" = --warm ] && exit 0\n'
                'if [ ! -f "$DELAY_ONCE" ]; then : > "$DELAY_ONCE"; sleep 6; fi\n'
                f'exec {shlex.quote(real_node)} "$@"\n')
            (shim / "node").chmod(0o700)
            for executable in (stub, shim / "node"):
                subprocess.run([str(executable), "--warm"], check=True, timeout=30)
            for declaration in (None, "5000", "30000"):
                with self.subTest(declaration=declaration):
                    env = declared_host_env()
                    if declaration is None:
                        env.pop("RALLY_HOOK_HOST_TIMEOUT_MS")
                    else:
                        env["RALLY_HOOK_HOST_TIMEOUT_MS"] = declaration
                    delay = base / f"delayed-{declaration}"
                    env.update(RALLY_BIN=str(stub), RALLY_NATIVE_HOOK="off",
                               RALLY_TOOL_ID="codex:startup-budget",
                               RALLY_SESSION_ID=f"startup-{declaration}",
                               PATH=f"{shim}:{env.get('PATH', '')}", DELAY_ONCE=str(delay))
                    caller_timeout = 30 if declaration == "30000" else 5
                    result = subprocess.run(
                        ["bash", str(HOOK), "start", "codex"], cwd=repo, env=env,
                        input="{}", text=True, capture_output=True, timeout=caller_timeout)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertTrue(delay.exists(), "startup delay did not execute")
                    envelope = json.loads(result.stdout)
                    if declaration == "30000":
                        self.assertIn("Agent Rally active", result.stdout)
                    else:
                        self.assertEqual(envelope, {})
                        self.assertIn("internal deadline", result.stderr)

    def test_declared_budget_reaches_native_batch_and_each_operation(self):
        import sys
        with tempfile.TemporaryDirectory(dir="/var/tmp") as directory:
            base = Path(directory)
            repo = base / "repo"
            (repo / ".rally").mkdir(parents=True)
            calls = base / "calls"
            stub = base / "rally"
            stub.write_text(
                f'#!{sys.executable}\nimport json,os,sys\n'
                'a=sys.argv[1:]\n'
                'if a == ["--warm"]: sys.exit(0)\n'
                'with open(os.environ["CALLS"],"a") as f: f.write(json.dumps(a)+"\\n")\n'
                'if a[:2] == ["hook","capabilities"]: print(\'{"before-write": {}, "batch": {}}\')\n'
                'elif a[:2] == ["hook","batch"]: sys.exit(124)\n'
                'else: print("{}")\n')
            stub.chmod(0o700)
            subprocess.run([str(stub), "--warm"], check=True, timeout=30)
            shim = base / "shim"
            shim.mkdir()
            gnu_used = base / "gnu-used"
            (shim / "timeout").write_text(
                f'#!/bin/bash\ntouch "{gnu_used}"\nexit 99\n')
            (shim / "timeout").chmod(0o700)
            env = declared_host_env()
            env.update(RALLY_BIN=str(stub), RALLY_NATIVE_HOOK="on", RALLY_HOOK_BATCH="on",
                       RALLY_TOOL_ID="codex:native-budget", RALLY_SESSION_ID="native-budget",
                       CALLS=str(calls), PATH=f"{shim}:{env.get('PATH', '')}")
            result = subprocess.run(["bash", str(HOOK), "start", "codex"], cwd=repo,
                                    env=env, input="{}", text=True, capture_output=True,
                                    timeout=HOST_TIMEOUT_SECONDS)
            self.assertEqual(result.returncode, 0, result.stderr)
            rows = [json.loads(line) for line in calls.read_text().splitlines()]
            batch = next(row for row in rows if row[:2] == ["hook", "batch"])
            budgets = [int(batch[i + 1]) for i, arg in enumerate(batch) if arg == "--timeout-ms"]
            self.assertGreater(len(budgets), 1, "native operation budgets were absent")
            self.assertTrue(all(ms == 25000 for ms in budgets[:-1]), budgets)
            self.assertGreater(budgets[-1], 0)
            self.assertLessEqual(budgets[-1], 25000)
            self.assertFalse(gnu_used.exists(), "lifecycle bypassed its Perl owner")

    def test_foreground_cancellation_stops_only_its_owned_cli_group(self):
        import signal
        import sys
        import time

        def live(pid):
            state = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)],
                                   text=True, capture_output=True).stdout.strip()
            return bool(state) and not state.startswith("Z")

        for cancellation in ("TERM", "INT", "worker-KILL"):
            with self.subTest(cancellation=cancellation), \
                    tempfile.TemporaryDirectory(dir="/var/tmp") as directory:
                base = Path(directory)
                repo = base / "repo"
                (repo / ".rally").mkdir(parents=True)
                trace = base / "foreground.json"
                stub = base / "rally"
                stub.write_text(
                    f'#!{sys.executable}\nimport json,os,subprocess,sys\n'
                    'if sys.argv[1:] == ["--warm"]: sys.exit(0)\n'
                    'child=subprocess.Popen(["sleep","60"])\n'
                    'with open(os.environ["TRACE"],"w") as f:\n'
                    ' json.dump({"pid":os.getpid(),"group":os.getpgrp(),'
                    '"guardian":os.getppid(),"child":child.pid,"argv":sys.argv[1:]},f)\n'
                    'child.wait()\n')
                stub.chmod(0o700)
                subprocess.run([str(stub), "--warm"], check=True, timeout=30)
                env = declared_host_env()
                env.update(RALLY_BIN=str(stub), RALLY_NATIVE_HOOK="off",
                           RALLY_TOOL_ID="codex:cancel-fixture", TRACE=str(trace))
                proc = subprocess.Popen(["bash", str(HOOK), "idle", "codex"],
                                        cwd=repo, env=env, stdin=subprocess.DEVNULL,
                                        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                        text=True, start_new_session=True)
                owned = None
                worker_group = None
                try:
                    until = time.monotonic() + HOST_TIMEOUT_SECONDS
                    while time.monotonic() < until:
                        if trace.exists():
                            try:
                                owned = json.loads(trace.read_text())
                                break
                            except json.JSONDecodeError:
                                pass
                        time.sleep(0.01)
                    self.assertIsNotNone(owned, "foreground CLI never started")
                    self.assertTrue(live(owned["pid"]))
                    self.assertTrue(live(owned["child"]))
                    budgets = [int(owned["argv"][i + 1])
                               for i, arg in enumerate(owned["argv"]) if arg == "--timeout-ms"]
                    self.assertTrue(budgets)
                    began = time.monotonic()
                    if cancellation == "worker-KILL":
                        owner = int(subprocess.check_output(
                            ["ps", "-o", "ppid=", "-p", str(owned["guardian"])], text=True))
                        worker_group = os.getpgid(owner)
                        self.assertNotEqual(worker_group, os.getpgrp())
                        self.assertNotEqual(worker_group, owned["group"])
                        os.killpg(worker_group, signal.SIGKILL)
                    else:
                        proc.send_signal(getattr(signal, "SIG" + cancellation))
                    proc.communicate(timeout=HOST_TIMEOUT_SECONDS)
                    # This must beat the old work deadline, so an ALRM-only
                    # guardian cannot pass by eventually timing out the CLI.
                    until = began + budgets[-1] / 2000
                    while time.monotonic() < until:
                        if not live(owned["pid"]) and not live(owned["child"]):
                            break
                        time.sleep(0.01)
                    self.assertFalse(live(owned["pid"]), "CLI survived cancellation")
                    self.assertFalse(live(owned["child"]), "grandchild survived cancellation")
                finally:
                    if proc.poll() is None:
                        proc.terminate()
                        proc.communicate(timeout=HOST_TIMEOUT_SECONDS)
                    if owned:
                        for pid in (owned["pid"], owned["child"]):
                            try:
                                if os.getpgid(pid) == owned["group"]:
                                    os.killpg(owned["group"], signal.SIGKILL)
                                    break
                            except ProcessLookupError:
                                pass

    def test_module_startup_and_calls_share_the_armed_clock(self):
        import sys
        # Delay the Time::HiRes import itself, before the old stopwatch began.
        # Observe the real armed timer without resetting it in the probe.
        source = HOOK.read_text()
        program = source.split("exec perl -e '", 1)[1].split(
            "' \"$_rally_deadline_ms\"", 1)[0]
        program = program.replace(
            'my ($ms, $phase) = (shift @ARGV, shift @ARGV);',
            'my ($ms, $phase) = (shift @ARGV, shift @ARGV);'
            'my ($armed_left) = Time::HiRes::getitimer(Time::HiRes::ITIMER_REAL());'
            '$ENV{TEST_ARMED_AT_MS} = int((Time::HiRes::time() + $armed_left) * 1000);')
        with tempfile.TemporaryDirectory(dir="/var/tmp") as directory:
            modules = Path(directory) / "Time"
            modules.mkdir()
            real_module = subprocess.check_output(
                ["perl", "-MTime::HiRes", "-e", 'print $INC{"Time/HiRes.pm"}'], text=True)
            (modules / "HiRes.pm").write_text(
                'select undef, undef, undef, 2.2;\nrequire ' + json.dumps(real_module) + ';\n1;\n')
            env = declared_host_env()
            env["PERL5LIB"] = directory
            result = subprocess.run(
                ["perl", "-e", program, "5000", "idle", sys.executable, "-c",
                 "import json,os;print(json.dumps({k:os.environ[k] for k in "
                 "['TEST_ARMED_AT_MS','RALLY_HOOK_CALL_DEADLINE_AT_MS']}))"],
                env=env, text=True, capture_output=True, timeout=HOST_TIMEOUT_SECONDS)
            self.assertEqual(result.returncode, 0, result.stderr)
            observed = json.loads(result.stdout)
            self.assertLessEqual(int(observed["RALLY_HOOK_CALL_DEADLINE_AT_MS"]),
                                 int(observed["TEST_ARMED_AT_MS"]) - 650)

    def test_completed_response_survives_stdout_backpressure(self):
        import sys
        import time
        # Run the actual supervisor program with a controlled JSON worker.
        source = HOOK.read_text()
        program = source.split("exec perl -e '", 1)[1].split(
            "' \"$_rally_deadline_ms\"", 1)[0]
        with tempfile.TemporaryDirectory(dir="/var/tmp") as directory:
            completed = Path(directory) / "completed"
            expected = json.dumps({"payload": "x" * 131072}) + "\n"
            worker = (
                "import json,pathlib,sys;sys.stdout.write(json.dumps({'payload':'x'*131072})+'\\n');"
                + "sys.stdout.flush();pathlib.Path(" + repr(str(completed))
                + ").touch()")
            work_budget_ms = 3000
            started = time.monotonic()
            proc = subprocess.Popen(["perl", "-e", program, str(work_budget_ms), "idle",
                                     sys.executable, "-c", worker],
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                    text=True, start_new_session=True)
            try:
                until = started + work_budget_ms / 1000
                while not completed.exists() and time.monotonic() < until:
                    time.sleep(0.01)
                self.assertTrue(completed.exists(), "worker did not select its response")
                # Leave output blocked beyond twice this controlled work budget.
                time.sleep(2 * work_budget_ms / 1000)
                stdout, stderr = proc.communicate(timeout=HOST_TIMEOUT_SECONDS)
                self.assertEqual(proc.returncode, 0, stderr)
                self.assertEqual(stdout, expected)
                self.assertEqual(json.loads(stdout), json.loads(expected))
                self.assertNotIn("internal deadline", stderr)
            finally:
                if proc.poll() is None:
                    proc.terminate()
                    proc.communicate(timeout=HOST_TIMEOUT_SECONDS)

    def test_idle_phase_spawns_no_avoidable_node(self):
        # Each cold node start costs 100-450ms on a loaded host. Idle needs
        # three (session id, hooks-status parse, renderer); the empty-metadata
        # read and the check-in timestamp used to add two more and must not
        # come back. The heartbeat must still carry a well-formed wake-after.
        import re
        import shutil
        import time
        real_node = shutil.which("node")
        if not real_node:
            self.skipTest("node not installed")
        with tempfile.TemporaryDirectory(dir="/var/tmp") as directory:
            base = Path(directory)
            repo = base / "repo"
            (repo / ".rally").mkdir(parents=True)
            stub = base / "rally"
            stub.write_text('#!/bin/bash\n[ "$1" = --warm ] && exit 0\n'
                            'printf "%s\\n" "$*" >> "$CALLS"\nprintf "{}"\n')
            stub.chmod(0o700)
            shim = base / "shim"
            shim.mkdir()
            (shim / "node").write_text(
                '#!/bin/bash\n[ "$1" = --warm ] && exit 0\n'
                f'echo x >> "$NODE_CALLS"\nexec {real_node} "$@"\n')
            (shim / "node").chmod(0o700)
            # First-exec evaluation is fixture setup, outside the process-count
            # probe. A five-second setup limit was exceeded on a loaded Mac.
            for executable in (stub, shim / "node"):
                subprocess.run([str(executable), "--warm"], check=True, timeout=30)
            calls, node_calls = base / "calls", base / "node-calls"
            env = declared_host_env()
            env.update(RALLY_BIN=str(stub), RALLY_HOOKS="on", RALLY_NATIVE_HOOK="off",
                       RALLY_TOOL_ID="codex:node-count", CALLS=str(calls),
                       RALLY_HOOK_SUPERVISED_CHILD="1", RALLY_HOOK_MS_BUDGET_SCALE="8",
                       NODE_CALLS=str(node_calls), PATH=f"{shim}:{env.get('PATH', '')}")
            result = subprocess.run(
                ["bash", str(HOOK), "idle", "codex"], cwd=repo, env=env,
                input=json.dumps({"cwd": str(repo), "session_id": "node-count",
                                  "hook_event_name": "UserPromptSubmit"}),
                text=True, capture_output=True, timeout=30)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertLessEqual(len(node_calls.read_text().split()), 3)
            for _ in range(40):
                if calls.exists() and "status post" in calls.read_text():
                    break
                time.sleep(0.05)
            post = [l for l in calls.read_text().splitlines() if l.startswith("status post")]
            self.assertTrue(post, calls.read_text())
            match = re.search(r"--wake-after (\S+)", post[0])
            self.assertIsNotNone(match, post[0])
            self.assertRegex(match.group(1), r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$")
            # Same instant the Node formula gives, to within scheduling noise.
            from datetime import datetime, timezone
            wake = datetime.strptime(match.group(1), "%Y-%m-%dT%H:%M:%SZ").replace(
                tzinfo=timezone.utc).timestamp()
            self.assertAlmostEqual(wake, time.time() + 300, delta=30)


if __name__ == "__main__":
    unittest.main()
