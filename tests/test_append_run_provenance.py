#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Tests for scripts/append_run_provenance.py (enforce-candidate E3).

Proves the gate with a rigged failing case (the exact wrong SHA "6616b71"
cited in the E3 evidence, .build-loop/proposals/enforce-from-retro/
bl-20260709T193157Z-codex-017210-03.md) followed by a passing case using a
real temporary git repo.

Runnable via:
    python3 -m pytest tests/test_append_run_provenance.py
or directly:
    python3 tests/test_append_run_provenance.py
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile

_TESTS_DIR = os.path.dirname(os.path.abspath(__file__))
_REPO_ROOT = os.path.dirname(_TESTS_DIR)
_SCRIPTS_DIR = os.path.join(_REPO_ROOT, "scripts")
if _SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, _SCRIPTS_DIR)

from append_run_provenance import validate_run_provenance  # noqa: E402


def _git(repo_dir, *args):
    result = subprocess.run(
        ["git", "-C", repo_dir] + list(args),
        capture_output=True,
        text=True,
        check=True,
    )
    return result.stdout.strip()


def _make_repo_with_commits(tmp_dir):
    """Create a real temp git repo with 3 commits and an intent.md, return
    (repo_dir, head_short_sha, intent_path)."""
    repo_dir = os.path.join(tmp_dir, "repo")
    os.makedirs(repo_dir)
    _git(tmp_dir, "init", "--quiet", repo_dir)
    _git(repo_dir, "-c", "user.email=test@example.com", "-c", "user.name=Test", "commit", "--allow-empty", "-m", "initial commit", "--quiet")

    intent_path = os.path.join(repo_dir, "intent.md")
    with open(intent_path, "w", encoding="utf-8") as fh:
        fh.write("# Build the provenance validator for append_run\n\nDetails here.\n")
    _git(repo_dir, "add", "intent.md")
    _git(repo_dir, "-c", "user.email=test@example.com", "-c", "user.name=Test", "commit", "-m", "add intent.md", "--quiet")

    with open(os.path.join(repo_dir, "note.txt"), "w", encoding="utf-8") as fh:
        fh.write("third commit\n")
    _git(repo_dir, "add", "note.txt")
    _git(repo_dir, "-c", "user.email=test@example.com", "-c", "user.name=Test", "commit", "-m", "third commit", "--quiet")

    head_short = _git(repo_dir, "rev-parse", "--short", "HEAD")
    return repo_dir, head_short, intent_path


def test_reachable_commit_passes():
    with tempfile.TemporaryDirectory() as tmp_dir:
        repo_dir, head_short, intent_path = _make_repo_with_commits(tmp_dir)
        result = validate_run_provenance(
            run_id="test-run-1",
            commit=head_short,
            goal="Build the provenance validator for append_run",
            repo_root=repo_dir,
            intent_path=intent_path,
        )
        assert result["ok"] is True, result
        block_findings = [f for f in result["findings"] if f["severity"] == "block"]
        assert block_findings == [], result
        assert result["derived_commit"] is not None


def test_unreachable_commit_blocks():
    with tempfile.TemporaryDirectory() as tmp_dir:
        repo_dir, _head_short, intent_path = _make_repo_with_commits(tmp_dir)
        # The exact wrong SHA cited in the E3 evidence — not in this repo's history.
        fabricated_sha = "6616b71"
        result = validate_run_provenance(
            run_id="bl-20260709T193157Z-codex-017210",
            commit=fabricated_sha,
            goal="Build the provenance validator for append_run",
            repo_root=repo_dir,
            intent_path=intent_path,
        )
        assert result["ok"] is False, result
        codes = [f["code"] for f in result["findings"]]
        assert "commit_unreachable" in codes, result
        block_finding = next(f for f in result["findings"] if f["code"] == "commit_unreachable")
        assert block_finding["severity"] == "block"


def test_goal_mismatch_warns_not_blocks():
    with tempfile.TemporaryDirectory() as tmp_dir:
        repo_dir, head_short, intent_path = _make_repo_with_commits(tmp_dir)
        result = validate_run_provenance(
            run_id="test-run-3",
            commit=head_short,
            goal="Close concrete open issues: directive/receipt durability, canonical framing",
            repo_root=repo_dir,
            intent_path=intent_path,
        )
        assert result["ok"] is True, result
        codes = [f["code"] for f in result["findings"]]
        assert "goal_mismatch" in codes, result
        warn_finding = next(f for f in result["findings"] if f["code"] == "goal_mismatch")
        assert warn_finding["severity"] == "warn"


def test_pending_commit_allowed():
    with tempfile.TemporaryDirectory() as tmp_dir:
        repo_dir, _head_short, intent_path = _make_repo_with_commits(tmp_dir)
        result = validate_run_provenance(
            run_id="test-run-4",
            commit="pending",
            goal="Build the provenance validator for append_run",
            repo_root=repo_dir,
            intent_path=intent_path,
        )
        assert result["ok"] is True, result
        block_findings = [f for f in result["findings"] if f["severity"] == "block"]
        assert block_findings == [], result


if __name__ == "__main__":
    tests = [
        test_reachable_commit_passes,
        test_unreachable_commit_blocks,
        test_goal_mismatch_warns_not_blocks,
        test_pending_commit_allowed,
    ]
    failures = 0
    for test in tests:
        try:
            test()
            print(f"PASS: {test.__name__}")
        except AssertionError as exc:
            failures += 1
            print(f"FAIL: {test.__name__}: {exc}")
    if failures:
        print(f"{failures} test(s) failed")
        sys.exit(1)
    print("All tests passed")
    sys.exit(0)
