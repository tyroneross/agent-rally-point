#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Hermetic contracts for scripts/check_command_conformance.py.

The checker is the release-gate control that (1) every command in the
parser's dispatch table appears in the built binary's --help, and (2) every
documented `rally <cmd>` string in command position names a real command.
These tests run it against a fixture repo + stub binary so a regression in
either direction fails before it can pass a release.
"""

from __future__ import annotations

import importlib.util
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CHECKER = ROOT / "scripts/check_command_conformance.py"


def load_checker_module():
    spec = importlib.util.spec_from_file_location("check_command_conformance", CHECKER)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

FIXTURE_CLI_RS = """
pub(crate) const COMMANDS: &[&str] = &[
    "room",
    "say",
    // managed sessions
    "run",
];
"""

COMPLETE_HELP = """rally: fixture
Usage:
  rally room [--json]
  rally say <kind> --tool <tool>
  rally run <agent> [--json]
"""

# Omits `run` — the managed-session discoverability gap this gate exists for.
INCOMPLETE_HELP = """rally: fixture
Usage:
  rally room [--json]
  rally say <kind> --tool <tool>
"""


def make_fixture(root: Path, help_text: str) -> Path:
    cli = root / "crates/rally-cli/src"
    cli.mkdir(parents=True)
    (cli / "cli.rs").write_text(FIXTURE_CLI_RS, encoding="utf-8")
    binary = root / "rally-stub"
    binary.write_text(
        "#!/bin/sh\ncat <<'EOF'\n" + help_text + "EOF\n", encoding="utf-8"
    )
    binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
    (root / "docs").mkdir()
    (root / "skills").mkdir()
    (root / "hooks").mkdir()
    (root / "config").mkdir()
    return binary


def run_checker(root: Path, binary: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(CHECKER), "--binary", str(binary), "--repo-root", str(root)],
        capture_output=True,
        text=True,
        timeout=60,
    )


class CommandTableTest(unittest.TestCase):
    def test_real_command_table_includes_managed_session_family(self):
        """The real cli.rs table parses and carries run/inject/sessions/adopt."""
        checker = load_checker_module()
        commands = checker.load_command_table(ROOT / "crates/rally-cli/src/cli.rs")
        self.assertGreaterEqual(len(commands), 40)
        self.assertTrue({"run", "inject", "sessions", "adopt"} <= commands, commands)


class ConformanceTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def test_clean_fixture_passes(self):
        binary = make_fixture(self.root, COMPLETE_HELP)
        (self.root / "docs/guide.md").write_text(
            "Prose about the per-task rally loop never matches.\n"
            "Run `rally room --json` to see state.\n"
            "```sh\nrally say artifact --tool you\n```\n",
            encoding="utf-8",
        )
        result = run_checker(self.root, binary)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_help_omitting_a_table_command_fails(self):
        binary = make_fixture(self.root, INCOMPLETE_HELP)
        result = run_checker(self.root, binary)
        self.assertEqual(result.returncode, 1)
        self.assertIn("omits registered command `run`", result.stderr)

    def test_unknown_documented_command_fails(self):
        """Regression: the exact historical defect this gate first caught.

        docs/HANDOFFS-AND-LAUNCHING-AGENTS.md shipped a monitoring bullet
        referencing `rally roster` — a command that never existed — as an
        inline backtick span. Both that shape and the fenced-block shape must
        fail.
        """
        binary = make_fixture(self.root, COMPLETE_HELP)
        (self.root / "docs/guide.md").write_text(
            "- `rally roster` — who's live, where, doing what.\n"
            "```sh\nrally bogus --json\n```\n",
            encoding="utf-8",
        )
        result = run_checker(self.root, binary)
        self.assertEqual(result.returncode, 1)
        self.assertIn("`rally roster` does not parse", result.stderr)
        self.assertIn("`rally bogus` does not parse", result.stderr)

    def test_prose_and_comments_never_match(self):
        binary = make_fixture(self.root, COMPLETE_HELP)
        (self.root / "docs/guide.md").write_text(
            "The rally binary lives on PATH; agent-rally-point is the repo.\n"
            "```sh\n# the rally binary auto-checks before each write\n```\n",
            encoding="utf-8",
        )
        (self.root / "hooks/hook.sh").write_text(
            '# rally never gates an edit\necho "install the rally binary first"\n',
            encoding="utf-8",
        )
        result = run_checker(self.root, binary)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_shell_and_json_command_positions_match(self):
        binary = make_fixture(self.root, COMPLETE_HELP)
        (self.root / "hooks/hook.sh").write_text(
            'advice="rally bogus --tool you"\n', encoding="utf-8"
        )
        (self.root / "config/host-integrations.json").write_text(
            '{"cmd": "rally missing --json"}\n', encoding="utf-8"
        )
        result = run_checker(self.root, binary)
        self.assertEqual(result.returncode, 1)
        self.assertIn("`rally bogus` does not parse", result.stderr)
        self.assertIn("`rally missing` does not parse", result.stderr)

    def test_planned_waiver_is_honored_on_the_same_line_only(self):
        binary = make_fixture(self.root, COMPLETE_HELP)
        (self.root / "docs/guide.md").write_text(
            "- `rally onboarding --json` <!-- conformance:planned -->\n"
            "- `rally roster --json`\n",
            encoding="utf-8",
        )
        result = run_checker(self.root, binary)
        self.assertEqual(result.returncode, 1)
        self.assertNotIn("onboarding", result.stderr)
        self.assertIn("`rally roster` does not parse", result.stderr)

    def test_flag_aliases_and_help_are_accepted(self):
        binary = make_fixture(self.root, COMPLETE_HELP)
        (self.root / "docs/guide.md").write_text(
            "```sh\nrally --help\nrally --version\nrally help\n```\n",
            encoding="utf-8",
        )
        result = run_checker(self.root, binary)
        self.assertEqual(result.returncode, 0, result.stderr)


class RealRepoTest(unittest.TestCase):
    def test_repo_docs_conform_against_table_derived_stub(self):
        """The real repo's docs/skills/hooks/config scan clean.

        The stub binary prints a usage line per real table command, so this
        asserts the DOC side against the REAL sources without needing a Cargo
        build; the gate run (run-release-auxiliary-gate.sh) covers the built
        binary's actual --help.
        """
        checker = load_checker_module()
        commands = checker.load_command_table(ROOT / "crates/rally-cli/src/cli.rs")
        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / "rally-stub"
            lines = "\n".join(f"  rally {c} [--json]" for c in sorted(commands))
            binary.write_text(
                "#!/bin/sh\ncat <<'EOF'\nUsage:\n" + lines + "\nEOF\n",
                encoding="utf-8",
            )
            binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
            result = subprocess.run(
                [sys.executable, str(CHECKER), "--binary", str(binary)],
                capture_output=True,
                text=True,
                timeout=120,
                cwd=ROOT,
            )
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
