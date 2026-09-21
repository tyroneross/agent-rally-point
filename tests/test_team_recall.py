#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Committed tests for scripts/team_recall_retest.py (R1 verbatim + R3 mutation)."""

from __future__ import annotations

import os
import sys
import tempfile

_TESTS_DIR = os.path.dirname(os.path.abspath(__file__))
_REPO_ROOT = os.path.dirname(_TESTS_DIR)
_SCRIPTS_DIR = os.path.join(_REPO_ROOT, "scripts")
if _SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, _SCRIPTS_DIR)

from team_recall_retest import check_recall  # noqa: E402

LEAD = [
    "TEAM-CTX-1",
    "1. ALPHA: Stabilize shared-memory write protocol",
    "2. BRAVO: Add TEAM_CHARTER.md with roles",
    "Checksum: ALPHA-BRAVO-1",
]


def test_verbatim_recall_passes():
    assert check_recall(LEAD, list(LEAD)) == []


def test_reordered_recall_fails():
    got = [LEAD[0], LEAD[2], LEAD[1], LEAD[3]]
    problems = check_recall(LEAD, got)
    assert any("line 2" in p for p in problems)


def test_dropped_item_fails():
    problems = check_recall(LEAD, LEAD[:-1])
    assert any("line count" in p for p in problems)


def test_mutated_stale_copy_detected_via_cli():
    import subprocess

    with tempfile.TemporaryDirectory() as tmp:
        lead_path = os.path.join(tmp, "lead.txt")
        stale_path = os.path.join(tmp, "stale.txt")
        with open(lead_path, "w", encoding="utf-8") as fh:
            fh.write("\n".join(LEAD) + "\n")
        with open(stale_path, "w", encoding="utf-8") as fh:
            fh.write("\n".join([LEAD[0], LEAD[2], LEAD[1], LEAD[3]]) + "\n")
        result = subprocess.run(
            [
                sys.executable,
                os.path.join(_SCRIPTS_DIR, "team_recall_retest.py"),
                "--list-file",
                lead_path,
                "--members",
                "3",
                "--stale-copy",
                stale_path,
            ],
            capture_output=True,
            text=True,
        )
        assert result.returncode == 0, result.stdout + result.stderr
        assert "stale copy detected" in result.stdout
