#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Tests for the sibling-asymmetry lint.

Run: python3 tests/scripts/test_lint_sibling_asymmetry.py

The load-bearing test is `test_names_the_known_outlier_on_the_pre_fix_tree`.
A detector that cannot rediscover a cluster we already know is there does not
ship: `referenced_handoff_targeting.rs` was measured flaking 1 run in 20 and
root-caused to the watchdog budget it alone omitted, so the pre-fix tree at
d697be3 is labeled ground truth and this is the planted positive.
"""

from __future__ import annotations

import importlib.util
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
LINT = REPO / "scripts/lint_sibling_asymmetry.py"
PRE_FIX_SHA = "d697be3"


def load_lint():
    spec = importlib.util.spec_from_file_location("lint_sibling_asymmetry", LINT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    # Register before exec: the module defines a @dataclass, and dataclasses
    # resolves annotations through sys.modules[cls.__module__].
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


lint = load_lint()


def rule(**overrides) -> dict:
    base = {
        "id": "synthetic",
        "root": "peers",
        "glob": "*.rs",
        "cohort": r"SPAWNS_BINARY",
        "refine": r"BACKGROUND_CHILD",
        "property": r"SETS_BUDGET",
        "threshold": 0.6,
        "exempt": [],
        "property_name": "the property",
        "remedy": "set it",
    }
    base.update(overrides)
    return base


class SyntheticCorpus(unittest.TestCase):
    """Every file is one line of markers, so the grouping logic is the only
    thing under test."""

    def build(self, files: dict[str, str]) -> Path:
        tmp = Path(tempfile.mkdtemp())
        peers = tmp / "peers"
        peers.mkdir()
        for name, body in files.items():
            (peers / name).write_text(body, encoding="utf-8")
        return tmp

    def test_names_the_minority_that_omits_the_property(self):
        repo = self.build(
            {
                f"peer{i}.rs": "SPAWNS_BINARY BACKGROUND_CHILD SETS_BUDGET"
                for i in range(4)
            }
            | {"odd_one_out.rs": "SPAWNS_BINARY BACKGROUND_CHILD"}
        )
        result = lint.evaluate_rule(repo, rule())
        self.assertTrue(result.fired)
        self.assertEqual(result.outliers, ["odd_one_out.rs"])
        self.assertEqual(result.group_size, 5)
        self.assertEqual(result.group_with_property, 4)

    def test_silent_when_every_peer_sets_the_property(self):
        repo = self.build(
            {
                f"peer{i}.rs": "SPAWNS_BINARY BACKGROUND_CHILD SETS_BUDGET"
                for i in range(5)
            }
        )
        self.assertFalse(lint.evaluate_rule(repo, rule()).fired)

    def test_silent_when_there_is_no_majority(self):
        """No convention exists yet, so nobody is an outlier. This is the
        guard against the detector inventing a norm out of a 50/50 split."""
        repo = self.build(
            {"a.rs": "SPAWNS_BINARY BACKGROUND_CHILD SETS_BUDGET"}
            | {
                f"peer{i}.rs": "SPAWNS_BINARY BACKGROUND_CHILD" for i in range(4)
            }
        )
        result = lint.evaluate_rule(repo, rule())
        self.assertFalse(result.fired)
        self.assertAlmostEqual(result.majority_ratio, 0.2)

    def test_cohort_member_of_a_different_shape_is_not_a_peer(self):
        """A file that invokes the binary but never spawns a background child
        is not wall-clock sensitive in the same way, so it must not be named
        and must not dilute the denominator."""
        repo = self.build(
            {
                f"peer{i}.rs": "SPAWNS_BINARY BACKGROUND_CHILD SETS_BUDGET"
                for i in range(3)
            }
            | {"one_shot.rs": "SPAWNS_BINARY"}
        )
        result = lint.evaluate_rule(repo, rule())
        self.assertFalse(result.fired)
        self.assertEqual(result.group_size, 3)
        self.assertEqual(result.cohort_size, 4)

    def test_non_participant_is_ignored_entirely(self):
        repo = self.build(
            {
                f"peer{i}.rs": "SPAWNS_BINARY BACKGROUND_CHILD SETS_BUDGET"
                for i in range(3)
            }
            | {"unrelated.rs": "nothing to see"}
        )
        result = lint.evaluate_rule(repo, rule())
        self.assertEqual(result.cohort_size, 3)

    def test_exempt_file_leaves_both_numerator_and_denominator(self):
        """A file where the property IS the subject under test is evidence of
        nothing, so it must not prop up the majority it is measuring."""
        repo = self.build(
            {
                "subject_under_test.rs": "SPAWNS_BINARY BACKGROUND_CHILD SETS_BUDGET",
                "a.rs": "SPAWNS_BINARY BACKGROUND_CHILD SETS_BUDGET",
                "b.rs": "SPAWNS_BINARY BACKGROUND_CHILD",
            }
        )
        result = lint.evaluate_rule(repo, rule(exempt=["subject_under_test.rs"]))
        self.assertEqual(result.exempted, ["subject_under_test.rs"])
        self.assertEqual(result.group_size, 2)
        self.assertAlmostEqual(result.majority_ratio, 0.5)
        self.assertFalse(result.fired)

    def test_missing_directory_is_skipped_not_crashed(self):
        result = lint.evaluate_rule(Path(tempfile.mkdtemp()), rule())
        self.assertIsNotNone(result.skipped_reason)
        self.assertFalse(result.fired)


class GroundTruth(unittest.TestCase):
    def extract(self, sha: str) -> Path | None:
        try:
            archive = subprocess.run(
                ["git", "archive", sha],
                cwd=REPO,
                capture_output=True,
                check=True,
            ).stdout
        except (subprocess.CalledProcessError, FileNotFoundError):
            return None
        tmp = Path(tempfile.mkdtemp())
        with tempfile.NamedTemporaryFile(suffix=".tar") as fh:
            fh.write(archive)
            fh.flush()
            with tarfile.open(fh.name) as tar:
                tar.extractall(tmp)
        return tmp

    def test_names_the_known_outlier_on_the_pre_fix_tree(self):
        """The planted positive. `referenced_handoff_targeting.rs` is the file
        that flaked; on the tree before the harness landed it must be named."""
        tree = self.extract(PRE_FIX_SHA)
        if tree is None:
            self.skipTest(f"{PRE_FIX_SHA} not reachable from this checkout")
        results = [lint.evaluate_rule(tree, r) for r in lint.RULES]
        watchdog = next(r for r in results if r.rule_id == "test-watchdog-budget")
        self.assertTrue(watchdog.fired, "detector went quiet on labeled ground truth")
        self.assertIn("referenced_handoff_targeting.rs", watchdog.outliers)

    def test_unrefined_grouping_would_have_missed_it(self):
        """Records why the peer group is shape-refined rather than
        directory-wide: on the same tree the plain grouping is 53%, under the
        60% threshold, so it emits nothing."""
        tree = self.extract(PRE_FIX_SHA)
        if tree is None:
            self.skipTest(f"{PRE_FIX_SHA} not reachable from this checkout")
        watchdog = next(
            lint.evaluate_rule(tree, r)
            for r in lint.RULES
            if r["id"] == "test-watchdog-budget"
        )
        self.assertLess(watchdog.cohort_ratio, 0.6)
        self.assertGreaterEqual(watchdog.majority_ratio, 0.6)

    def test_goes_quiet_for_the_file_once_it_is_migrated(self):
        """The detector must stop naming a file the moment it adopts the
        mechanism, or it becomes noise the week after the fix."""
        results = [lint.evaluate_rule(REPO, r) for r in lint.RULES]
        watchdog = next(r for r in results if r.rule_id == "test-watchdog-budget")
        self.assertNotIn("referenced_handoff_targeting.rs", watchdog.outliers)


class Exit(unittest.TestCase):
    def test_default_is_warn_only(self):
        self.assertEqual(lint.main(["--repo", str(REPO)]), 0)

    def test_strict_is_opt_in(self):
        code = lint.main(["--repo", str(REPO), "--strict", "--json"])
        self.assertIn(code, (0, 1))


if __name__ == "__main__":
    unittest.main(verbosity=2)
