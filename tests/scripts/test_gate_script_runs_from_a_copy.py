#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""The quality gate must work when it is run as a copy from another folder.

Run: python3 tests/scripts/test_gate_script_runs_from_a_copy.py

The pre-push hook does not run `scripts/run-quality-gate.sh` in place. It copies
the remote baseline's version into a per-run pin folder and runs that copy
against a clean worktree of the pushed commit. On 2026-11-09 a new gate step
addressed a script as `"$(dirname "$0")/lint_test_temp_dir.py"`. That resolved
inside the pin folder, where the file does not exist, and every push failed.
The push that introduced the step had passed, because it ran the pushed tree's
own copy under a gate-change acknowledgement, so nothing exercised the copy.

This test is that exercise: it runs a copy of the gate script from an unrelated
folder, with stand-ins for cargo so it takes under a second.
"""

from __future__ import annotations

import os
import re
import shutil
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
GATE = REPO / "scripts/run-quality-gate.sh"
HOOK = REPO / ".githooks/pre-push"


def pinned_executed_scripts() -> list[str]:
    """Scripts the hook copies into its pin folder and executes from there."""
    text = HOOK.read_text(encoding="utf-8")
    paths = re.search(r'^GATE_SCRIPT_PATHS="([^"]+)"', text, re.M).group(1).split()
    compare_only = re.search(r'^GATE_COMPARE_ONLY_PATHS="([^"]*)"', text, re.M).group(1).split()
    return [path for path in paths if path not in compare_only]


class GateScriptRunsFromACopy(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        stubs = self.root / "bin"
        stubs.mkdir()
        # cargo "passes" everything; rustup reports no pinned toolchain so the
        # script falls back to plain `cargo`; nextest is absent.
        for name, body in (("cargo", "exit 0"), ("rustup", "exit 1")):
            stub = stubs / name
            stub.write_text(f"#!/bin/sh\n{body}\n", encoding="utf-8")
            stub.chmod(stub.stat().st_mode | stat.S_IXUSR)
        self.env = dict(os.environ)
        self.env["PATH"] = f"{stubs}:/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin"

    def run_copy(self, source_text: str) -> subprocess.CompletedProcess:
        pin = self.root / "pin"
        pin.mkdir(exist_ok=True)
        copy = pin / "scripts__run-quality-gate.sh"
        copy.write_text(source_text, encoding="utf-8")
        copy.chmod(copy.stat().st_mode | stat.S_IXUSR)
        return subprocess.run(
            ["bash", str(copy)], cwd=REPO, env=self.env, capture_output=True, text=True, timeout=120
        )

    def test_the_gate_passes_when_run_as_a_copy_from_another_folder(self):
        result = self.run_copy(GATE.read_text(encoding="utf-8"))
        self.assertEqual(result.returncode, 0, result.stderr[-2000:])
        self.assertIn("test scratch-folder lint", result.stderr)
        self.assertIn("all checks green", result.stderr)

    def test_a_step_that_depends_on_the_scripts_own_folder_fails_this_test(self):
        # The exact line that broke every push. If this stops failing, the
        # test above no longer proves anything.
        broken = GATE.read_text(encoding="utf-8").replace(
            'python3 "$repo_root/scripts/lint_test_temp_dir.py"',
            'python3 "$(dirname "$0")/lint_test_temp_dir.py"',
        )
        self.assertNotEqual(broken, GATE.read_text(encoding="utf-8"), "fixture line not found")
        result = self.run_copy(broken)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("lint_test_temp_dir.py", result.stderr)

    def test_no_pinned_script_addresses_files_through_its_own_location(self):
        for rel in pinned_executed_scripts():
            for number, line in enumerate((REPO / rel).read_text(encoding="utf-8").splitlines(), 1):
                code = line.split("#", 1)[0]
                for needle in ('dirname "$0"', "dirname $0", "BASH_SOURCE"):
                    self.assertNotIn(
                        needle,
                        code,
                        f"{rel}:{number} resolves a path from the script's own folder; "
                        f"the hook runs a pinned copy from elsewhere: {line.strip()}",
                    )


if __name__ == "__main__":
    unittest.main()
