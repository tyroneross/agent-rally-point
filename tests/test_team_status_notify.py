#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Tests for scripts/team_status_notify.py (CHARLIE: role/performance notify)."""

from __future__ import annotations

import os
import sys

_TESTS_DIR = os.path.dirname(os.path.abspath(__file__))
_REPO_ROOT = os.path.dirname(_TESTS_DIR)
_SCRIPTS_DIR = os.path.join(_REPO_ROOT, "scripts")
if _SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, _SCRIPTS_DIR)

from team_status_notify import demo_snapshot, format_status  # noqa: E402


def test_demo_snapshot_names_roles_and_ack_counts():
    title, body = format_status(demo_snapshot())
    assert "mode=standard" in title
    assert "ctx=TEAM-CTX-1" in title
    assert "lead=" in body
    assert "acked=2" in body
    assert "claude_code:muse-lead:lead:ack" in body
    assert "codex:reviewer:reviewer:no-ack" in body


def test_empty_room_still_formats():
    title, body = format_status({"lead": None, "squads": []})
    assert "mode=standard" in title
    assert "members=0" in body
    assert "acked=0" in body
