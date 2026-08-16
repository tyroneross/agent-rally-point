#!/usr/bin/env python3
"""Hermetic contract tests for scripts/release-readiness.sh."""

from __future__ import annotations

import os
import shutil
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SOURCE_SCRIPT = ROOT / "scripts/release-readiness.sh"


def make_executable(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)


class ReleaseReadinessTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name) / "fixture"
        self.root.mkdir()
        self.log = self.root / "commands.log"
        self.bin = self.root / "bin"
        self.bin.mkdir()

        script_dest = self.root / "scripts/release-readiness.sh"
        script_dest.parent.mkdir(parents=True)
        shutil.copy2(SOURCE_SCRIPT, script_dest)

        make_executable(
            self.root / "scripts/run-quality-gate.sh",
            "#!/usr/bin/env bash\nprintf 'quality:%s\\n' \"${CARGO_TARGET_DIR:-}\" >> \"$RELEASE_READINESS_LOG\"\n",
        )
        make_executable(
            self.root / "scripts/check-release-parity.sh",
            "#!/usr/bin/env bash\nprintf 'parity:%s\\n' \"${RALLY_RELEASE_TAG:-}\" >> \"$RELEASE_READINESS_LOG\"\nexit \"${RELEASE_READINESS_PARITY_EXIT:-0}\"\n",
        )
        make_executable(
            self.root / "scripts/generate_host_surfaces.py",
            "#!/usr/bin/env python3\nimport os\nfrom pathlib import Path\nPath(os.environ['RELEASE_READINESS_LOG']).open('a').write('generator\\n')\n",
        )
        make_executable(
            self.root / "tests/hooks/test_prepush_alpha.sh",
            "#!/usr/bin/env bash\nprintf 'prepush:alpha\\n' >> \"$RELEASE_READINESS_LOG\"\n",
        )
        make_executable(
            self.root / "tests/hooks/test_prepush_beta.sh",
            "#!/usr/bin/env bash\nprintf 'prepush:beta\\n' >> \"$RELEASE_READINESS_LOG\"\n",
        )
        (self.root / "dynamic-workflows").mkdir()
        (self.root / "dynamic-workflows/package.json").write_text("{}\n", encoding="utf-8")
        (self.root / "CHANGELOG.md").write_text("# Changelog\n\n## v1.2.3\n", encoding="utf-8")

        make_executable(
            self.bin / "git",
            """#!/usr/bin/env bash
set -euo pipefail
if [ "$1" = "rev-parse" ] && [ "$2" = "--show-toplevel" ]; then
  printf '%s\\n' "$RELEASE_READINESS_ROOT"
  exit 0
fi
if [ "$1" = "rev-parse" ] && [ "$2" = "HEAD" ]; then
  state="$RELEASE_READINESS_ROOT/.git-head-calls"
  calls="$(cat "$state" 2>/dev/null || printf 0)"
  calls=$((calls + 1))
  printf '%s' "$calls" > "$state"
  if [ "${RELEASE_READINESS_GIT_HEAD_DRIFTS:-}" = "1" ] && [ "$calls" -ge 2 ]; then
    printf '%s\\n' "drifted-head"
  else
    printf '%s\\n' "candidate-head"
  fi
  exit 0
fi
if [ "$1" = "status" ]; then
  state="$RELEASE_READINESS_ROOT/.git-status-calls"
  calls="$(cat "$state" 2>/dev/null || printf 0)"
  calls=$((calls + 1))
  printf '%s' "$calls" > "$state"
  if [ "${RELEASE_READINESS_GIT_STATUS_DRIFTS:-}" = "1" ] && [ "$calls" -ge 2 ]; then
    printf '%s\\n' " M README.md"
    exit 0
  fi
  printf '%s' "${RELEASE_READINESS_GIT_STATUS:-}"
  exit 0
fi
if [ "$1" = "diff" ]; then
  printf 'git:%s\\n' "$*" >> "$RELEASE_READINESS_LOG"
  exit 0
fi
exit 1
""",
        )
        make_executable(
            self.bin / "npm",
            "#!/usr/bin/env bash\nprintf 'npm:%s\\n' \"$*\" >> \"$RELEASE_READINESS_LOG\"\n",
        )
        make_executable(
            self.bin / "mktemp",
            """#!/usr/bin/env bash
set -euo pipefail
target="$RELEASE_READINESS_ROOT/isolated-target"
mkdir -p "$target"
printf '%s\\n' "$target"
""",
        )
        make_executable(
            self.bin / "actionlint",
            "#!/usr/bin/env bash\nprintf 'actionlint:%s\\n' \"$*\" >> \"$RELEASE_READINESS_LOG\"\n",
        )

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def run_script(self, *args: str, extra_env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
        env = {
            "PATH": f"{self.bin}:/usr/bin:/bin",
            "RELEASE_READINESS_LOG": str(self.log),
            "RELEASE_READINESS_ROOT": str(self.root),
        }
        if extra_env:
            env.update(extra_env)
        return subprocess.run(
            ["bash", "scripts/release-readiness.sh", *args],
            cwd=self.root,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )

    def commands(self) -> list[str]:
        if not self.log.exists():
            return []
        return self.log.read_text(encoding="utf-8").splitlines()

    def test_default_check_only_runs_read_only_candidate_checks(self) -> None:
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self.commands(),
            ["parity:", "git:diff --check", "git:diff --cached --check"],
        )

    def test_fix_generated_is_the_only_repair_path(self) -> None:
        result = self.run_script("--fix-generated")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self.commands(),
            ["generator", "parity:", "git:diff --check", "git:diff --cached --check"],
        )

    def test_check_and_fix_generated_are_mutually_exclusive(self) -> None:
        result = self.run_script("--check", "--fix-generated")
        self.assertEqual(result.returncode, 2)
        self.assertIn("--check and --fix-generated are mutually exclusive", result.stderr)
        self.assertEqual(self.commands(), [])

    def test_tag_forwards_validation_without_creating_a_tag(self) -> None:
        result = self.run_script("--tag", "v1.2.3")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self.commands(),
            ["parity:v1.2.3", "git:diff --check", "git:diff --cached --check"],
        )

    def test_tag_requires_a_pristine_checkout(self) -> None:
        result = self.run_script(
            "--tag",
            "v1.2.3",
            extra_env={"RELEASE_READINESS_GIT_STATUS": " M README.md\\n"},
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("--tag requires a pristine checkout", result.stderr)
        self.assertEqual(self.commands(), [])

    def test_fix_generated_cannot_be_combined_with_tag(self) -> None:
        result = self.run_script("--fix-generated", "--tag", "v1.2.3")
        self.assertEqual(result.returncode, 2)
        self.assertIn("--fix-generated cannot be combined with --tag", result.stderr)
        self.assertEqual(self.commands(), [])

    def test_tag_requires_a_matching_changelog_heading(self) -> None:
        (self.root / "CHANGELOG.md").write_text("# Changelog\n\n## Unreleased\n", encoding="utf-8")
        result = self.run_script("--tag", "v1.2.3")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("CHANGELOG.md needs a versioned v1.2.3 heading", result.stderr)
        self.assertEqual(self.commands(), [])

    def test_tag_rejects_a_near_miss_changelog_version(self) -> None:
        (self.root / "CHANGELOG.md").write_text("# Changelog\n\n## v1x2x3\n", encoding="utf-8")
        result = self.run_script("--tag", "v1.2.3")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("CHANGELOG.md needs a versioned v1.2.3 heading", result.stderr)
        self.assertEqual(self.commands(), [])

    def test_full_runs_every_additional_candidate_gate(self) -> None:
        result = self.run_script("--full")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self.commands(),
            [
                "parity:",
                "git:diff --check",
                "git:diff --cached --check",
                f"quality:{self.root / 'isolated-target'}",
                "prepush:alpha",
                "prepush:beta",
                "npm:--prefix dynamic-workflows test",
                "actionlint:",
            ],
        )
        self.assertFalse((self.root / "isolated-target").exists())

    def test_full_preserves_an_explicit_cargo_target(self) -> None:
        explicit_target = self.root / "caller-target"
        explicit_target.mkdir()
        result = self.run_script(
            "--full", extra_env={"CARGO_TARGET_DIR": str(explicit_target)}
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"quality:{explicit_target}", self.commands())
        self.assertTrue(explicit_target.exists())

    def test_full_rejects_a_changed_head(self) -> None:
        result = self.run_script(
            "--full", extra_env={"RELEASE_READINESS_GIT_HEAD_DRIFTS": "1"}
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("candidate HEAD changed during --full", result.stderr)
        self.assertIn("actionlint:", self.commands())

    def test_full_rejects_a_changed_worktree(self) -> None:
        result = self.run_script(
            "--full", extra_env={"RELEASE_READINESS_GIT_STATUS_DRIFTS": "1"}
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("candidate worktree changed during --full", result.stderr)
        self.assertIn("actionlint:", self.commands())

    def test_failure_stops_before_later_gates(self) -> None:
        result = self.run_script(extra_env={"RELEASE_READINESS_PARITY_EXIT": "7"})
        self.assertEqual(result.returncode, 7)
        self.assertEqual(self.commands(), ["parity:"])

    def test_invalid_tag_is_rejected_before_running_commands(self) -> None:
        result = self.run_script("--tag", "0.2.5")
        self.assertEqual(result.returncode, 2)
        self.assertIn("--tag must use vX.Y.Z", result.stderr)
        self.assertEqual(self.commands(), [])


if __name__ == "__main__":
    unittest.main()
