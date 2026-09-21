#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""R1 alignment: fixture, charter example, and simulator stay on one list."""

from __future__ import annotations

import os
import sys

_TESTS_DIR = os.path.dirname(os.path.abspath(__file__))
_REPO_ROOT = os.path.dirname(_TESTS_DIR)
_SCRIPTS_DIR = os.path.join(_REPO_ROOT, "scripts")
_FIXTURE = os.path.join(_TESTS_DIR, "team-ctx-1.txt")
_RETESTS = os.path.join(_REPO_ROOT, "docs", "TEAM-RECALL-RETESTS.md")
if _SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, _SCRIPTS_DIR)

from team_recall_retest import check_recall  # noqa: E402

CANON = [
    "TEAM-CTX-1",
    "1. ALPHA: Stabilize shared-memory write protocol",
    "2. BRAVO: Add TEAM_CHARTER.md with roles",
    "3. CHARLIE: Wire rally native event notifications",
    "Checksum: ALPHA-BRAVO-CHARLIE-1",
]


def test_fixture_matches_canon():
    with open(_FIXTURE, encoding="utf-8") as fh:
        got = [line.rstrip("\n") for line in fh if line.strip()]
    assert check_recall(CANON, got) == []


def test_retest_doc_embeds_canon_lines():
    text = open(_RETESTS, encoding="utf-8").read()
    for line in CANON:
        assert line in text, f"missing from TEAM-RECALL-RETESTS.md: {line}"
