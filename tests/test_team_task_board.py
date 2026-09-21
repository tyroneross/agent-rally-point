#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Team task board: every task names a session company."""

from __future__ import annotations

import json
import os
import sys

_TESTS = os.path.dirname(os.path.abspath(__file__))
_ROOT = os.path.dirname(_TESTS)
_SCRIPTS = os.path.join(_ROOT, "scripts")
_BOARD = os.path.join(_ROOT, "config", "team-task-board.json")
if _SCRIPTS not in sys.path:
    sys.path.insert(0, _SCRIPTS)

from team_task_board import rows, validate_board  # noqa: E402


def test_committed_board_is_valid_and_assigns_sessions():
    board = json.loads(open(_BOARD, encoding="utf-8").read())
    assert validate_board(board) == []
    rendered = rows(board)
    assert rendered, "board has no tasks"
    assert all(r["session"] and r["session"] != "(unassigned)" for r in rendered)
    ids = [r["id"] for r in rendered]
    assert ids == ["ALPHA", "BRAVO", "CHARLIE", "DELTA"]
    charlie = next(r for r in rendered if r["id"] == "CHARLIE")
    assert "cursor:" in charlie["session"]
    assert charlie["callsign"] == "Native Co"
    assert charlie["reports_to"] == "claude_code:muse-lead"
    lead = next(r for r in rendered if r["id"] == "ALPHA")
    assert lead["reports_to"] == "user:operator"


def test_unknown_session_is_invalid():
    board = {
        "schema": "agent-rally.team-task-board.v1",
        "context_version": "TEAM-CTX-1",
        "commander_intent": "x",
        "higher_echelon": {"id": "user:operator"},
        "companies": [
            {
                "session": "codex:a",
                "callsign": "A",
                "host": "codex",
                "reports_to": "user:operator",
            }
        ],
        "tasks": [
            {
                "id": "T1",
                "intent": "do the thing",
                "session": "claude_code:ghost",
                "status": "open",
            }
        ],
    }
    problems = validate_board(board)
    assert any("not a company" in p for p in problems)


def test_missing_reports_to_fails():
    board = {
        "schema": "agent-rally.team-task-board.v1",
        "context_version": "TEAM-CTX-1",
        "commander_intent": "x",
        "higher_echelon": {"id": "user:operator"},
        "companies": [{"session": "codex:a", "callsign": "A", "host": "codex"}],
        "tasks": [
            {"id": "T1", "intent": "do the thing", "session": "codex:a", "status": "open"}
        ],
    }
    problems = validate_board(board)
    assert any("reports_to" in p for p in problems)


def test_self_report_fails():
    board = {
        "schema": "agent-rally.team-task-board.v1",
        "context_version": "TEAM-CTX-1",
        "commander_intent": "x",
        "higher_echelon": {"id": "user:operator"},
        "companies": [
            {
                "session": "codex:a",
                "callsign": "A",
                "host": "codex",
                "reports_to": "codex:a",
            }
        ],
        "tasks": [
            {"id": "T1", "intent": "do the thing", "session": "codex:a", "status": "open"}
        ],
    }
    problems = validate_board(board)
    assert any("cannot report to itself" in p for p in problems)
