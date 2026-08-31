#!/usr/bin/env python3
"""Hermetic contract tests for scripts/run-release-auxiliary-gate.sh."""

from __future__ import annotations

import os
import shutil
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SOURCE_SCRIPT = ROOT / "scripts/run-release-auxiliary-gate.sh"


def make_executable(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)


class ReleaseAuxiliaryGateTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name) / "fixture"
        self.root.mkdir()
        self.log = self.root / "commands.log"
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.built_binary = self.root / "isolated-target/release/rally"
        self.npm_output = self.root / "npm-output.txt"

        script_dest = self.root / "scripts/run-release-auxiliary-gate.sh"
        script_dest.parent.mkdir(parents=True)
        shutil.copy2(SOURCE_SCRIPT, script_dest)
        (self.root / "scripts/check_command_conformance.py").write_text(
            """import os
import sys

binary = sys.argv[sys.argv.index("--binary") + 1]
with open(os.environ["AUX_LOG"], "a", encoding="utf-8") as log:
    log.write(f"conformance:{binary}\\n")
if binary != os.environ["AUX_BUILT_BINARY"]:
    raise SystemExit(93)
""",
            encoding="utf-8",
        )
        (self.root / "dynamic-workflows").mkdir()
        (self.root / "dynamic-workflows/package.json").write_text("{}\n", encoding="utf-8")
        self.write_npm_output(tests=3, skipped=0)

        make_executable(
            self.bin / "git",
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$AUX_ROOT\"\n",
        )
        make_executable(
            self.bin / "cargo",
            """#!/usr/bin/env bash
set -euo pipefail
printf 'cargo:%s\n' "$*" >> "$AUX_LOG"
if [ "${AUX_CARGO_EXIT:-0}" -ne 0 ]; then
  printf 'cargo build diagnostic\n'
  exit "$AUX_CARGO_EXIT"
fi
if [ "${AUX_CREATE_BINARY:-1}" = "1" ]; then
  mkdir -p "$(dirname "$AUX_BUILT_BINARY")"
  printf '#!/usr/bin/env bash\nexit 0\n' > "$AUX_BUILT_BINARY"
  chmod +x "$AUX_BUILT_BINARY"
fi
printf '{"reason":"compiler-artifact","target":{"name":"rally","kind":["bin"]},"executable":"%s"}\n' "$AUX_BUILT_BINARY"
""",
        )
        make_executable(
            self.bin / "npm",
            """#!/usr/bin/env bash
set -euo pipefail
printf 'npm:%s\n' "$*" >> "$AUX_LOG"
printf 'npm-binary:%s\n' "${RALLY_PACKET_EMPIRICAL_BIN:-}" >> "$AUX_LOG"
if [ "${RALLY_PACKET_EMPIRICAL_BIN:-}" != "$AUX_BUILT_BINARY" ]; then
  printf 'wrong empirical binary: %s\n' "${RALLY_PACKET_EMPIRICAL_BIN:-}" >&2
  exit 91
fi
cat "$AUX_NPM_OUTPUT"
exit "${AUX_NPM_EXIT:-0}"
""",
        )
        make_executable(
            self.root / "scripts/scale_reliability_test.sh",
            """#!/usr/bin/env bash
set -euo pipefail
printf 'scale:%s\n' "$*" >> "$AUX_LOG"
printf 'scale-binary:%s\n' "${RALLY_BIN:-}" >> "$AUX_LOG"
if [ "${RALLY_BIN:-}" != "$AUX_BUILT_BINARY" ]; then
  printf 'wrong scale binary: %s\n' "${RALLY_BIN:-}" >&2
  exit 92
fi
exit "${AUX_SCALE_EXIT:-0}"
""",
        )
        make_executable(
            self.root / "tests/hooks/test_prepush_alpha.sh",
            "#!/usr/bin/env bash\nprintf 'prepush:alpha\\n' >> \"$AUX_LOG\"\n",
        )
        make_executable(
            self.root / "tests/hooks/test_prepush_beta.sh",
            "#!/usr/bin/env bash\nprintf 'prepush:beta\\n' >> \"$AUX_LOG\"\n",
        )

        stale_binary = self.root / "target/release/rally"
        make_executable(stale_binary, "#!/usr/bin/env bash\nexit 99\n")

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def write_npm_output(self, *, tests: int, skipped: int, extra: str = "") -> None:
        self.npm_output.write_text(
            f"TAP version 13\n1..{tests}\n# tests {tests}\n# pass {tests - skipped}\n# skipped {skipped}\n{extra}",
            encoding="utf-8",
        )

    def run_gate(
        self,
        *args: str,
        extra_env: dict[str, str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        env = {
            "PATH": f"{self.bin}:/usr/bin:/bin",
            "AUX_ROOT": str(self.root),
            "AUX_LOG": str(self.log),
            "AUX_BUILT_BINARY": str(self.built_binary),
            "AUX_NPM_OUTPUT": str(self.npm_output),
        }
        if extra_env:
            env.update(extra_env)
        return subprocess.run(
            ["bash", "scripts/run-release-auxiliary-gate.sh", *args],
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

    def test_success_uses_cargo_reported_binary_and_runs_every_gate(self) -> None:
        result = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self.commands(),
            [
                "cargo:build --release -p rally-cli --bin rally --message-format=json-render-diagnostics",
                f"conformance:{self.built_binary}",
                "npm:--prefix dynamic-workflows test",
                f"npm-binary:{self.built_binary}",
                "scale:--mode both --scales 2,4,6 --max-wall-s 20",
                f"scale-binary:{self.built_binary}",
                "prepush:alpha",
                "prepush:beta",
            ],
        )
        self.assertIn("# tests 3", result.stdout)
        self.assertIn("# skipped 0", result.stdout)

    def test_product_only_runs_user_acceptance_without_maintainer_suites(self) -> None:
        result = self.run_gate("--product-only")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            "scale:--mode both --scales 2,4,6 --max-wall-s 20",
            self.commands(),
        )
        self.assertNotIn("prepush:alpha", self.commands())
        self.assertIn("product acceptance green", result.stderr)

    def test_scale_failure_blocks_product_acceptance(self) -> None:
        result = self.run_gate(
            "--product-only", extra_env={"AUX_SCALE_EXIT": "17"}
        )
        self.assertEqual(result.returncode, 17)
        self.assertNotIn("prepush:alpha", self.commands())

    def test_missing_current_binary_does_not_fall_back_to_stale_default(self) -> None:
        result = self.run_gate(extra_env={"AUX_CREATE_BINARY": "0"})
        self.assertEqual(result.returncode, 1)
        self.assertIn("missing or not executable", result.stderr)
        self.assertEqual(
            self.commands(),
            ["cargo:build --release -p rally-cli --bin rally --message-format=json-render-diagnostics"],
        )
        self.assertTrue((self.root / "target/release/rally").exists())

    def test_npm_failure_always_prints_diagnostics_and_preserves_exit(self) -> None:
        self.npm_output.write_text("npm exploded with useful diagnostics\n", encoding="utf-8")
        result = self.run_gate(extra_env={"AUX_NPM_EXIT": "7"})
        self.assertEqual(result.returncode, 7)
        self.assertIn("npm exploded with useful diagnostics", result.stdout)
        self.assertIn("failed with exit 7", result.stderr)
        self.assertNotIn("prepush:alpha", self.commands())

    def test_skipped_node_tests_fail_the_gate(self) -> None:
        self.write_npm_output(tests=3, skipped=1)
        result = self.run_gate()
        self.assertEqual(result.returncode, 1)
        self.assertIn("exactly zero skipped tests", result.stderr)
        self.assertNotIn("prepush:alpha", self.commands())

    def test_zero_executed_node_tests_fail_the_gate(self) -> None:
        self.write_npm_output(tests=0, skipped=0)
        result = self.run_gate()
        self.assertEqual(result.returncode, 1)
        self.assertIn("positive executed-test count", result.stderr)
        self.assertNotIn("prepush:alpha", self.commands())

    def test_no_prepush_suites_cannot_pass_vacuously(self) -> None:
        (self.root / "tests/hooks/test_prepush_alpha.sh").unlink()
        (self.root / "tests/hooks/test_prepush_beta.sh").unlink()
        result = self.run_gate()
        self.assertEqual(result.returncode, 1)
        self.assertIn("refusing to pass vacuously", result.stderr)

    def test_prepush_failure_is_preserved_after_every_suite_runs(self) -> None:
        make_executable(
            self.root / "tests/hooks/test_prepush_beta.sh",
            "#!/usr/bin/env bash\nprintf 'prepush:beta\\n' >> \"$AUX_LOG\"\nexit 23\n",
        )
        make_executable(
            self.root / "tests/hooks/test_prepush_gamma.sh",
            "#!/usr/bin/env bash\nprintf 'prepush:gamma\\n' >> \"$AUX_LOG\"\n",
        )
        result = self.run_gate()
        self.assertEqual(result.returncode, 23)
        self.assertIn("failed with exit 23", result.stderr)
        self.assertEqual(self.commands()[-3:], ["prepush:alpha", "prepush:beta", "prepush:gamma"])


if __name__ == "__main__":
    unittest.main()
