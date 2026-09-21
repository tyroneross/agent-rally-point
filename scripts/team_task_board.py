#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Render the team task board: task × owning session (company).

Usage:
    python3 scripts/team_task_board.py --board config/team-task-board.json
    python3 scripts/team_task_board.py --board config/team-task-board.json --json
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

VALID_STATUS = {"open", "planned", "in_progress", "blocked", "done", "standby"}


def load_board(path: Path) -> dict[str, Any]:
    data = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(data, dict):
        raise ValueError("board must be a JSON object")
    return data


def validate_board(board: dict[str, Any]) -> list[str]:
    problems: list[str] = []
    if board.get("schema") != "agent-rally.team-task-board.v1":
        problems.append("schema must be agent-rally.team-task-board.v1")
    if not board.get("context_version"):
        problems.append("missing context_version")
    if not board.get("commander_intent"):
        problems.append("missing commander_intent")

    hhq = board.get("higher_echelon")
    hhq_id = ""
    if not isinstance(hhq, dict) or not hhq.get("id"):
        problems.append("missing higher_echelon.id (HHQ)")
    else:
        hhq_id = str(hhq["id"])

    companies = board.get("companies") or []
    sessions = set()
    reports: dict[str, str] = {}
    if not isinstance(companies, list):
        problems.append("companies must be a list")
        companies = []
    for i, co in enumerate(companies):
        if not isinstance(co, dict) or not co.get("session"):
            problems.append(f"companies[{i}] missing session")
            continue
        sid = str(co["session"])
        sessions.add(sid)
        parent = str(co.get("reports_to") or "")
        if not parent:
            problems.append(f"company {sid} missing reports_to (higher echelon)")
            continue
        if parent == sid:
            problems.append(f"company {sid} cannot report to itself")
        reports[sid] = parent

    allowed_parents = set(sessions)
    if hhq_id:
        allowed_parents.add(hhq_id)
    for sid, parent in reports.items():
        if parent not in allowed_parents:
            problems.append(f"company {sid} reports_to {parent} is not HHQ or a company")
        # one-hop cycle only; deeper cycles are still a coordination bug
        if reports.get(parent) == sid:
            problems.append(f"company {sid} and {parent} report to each other")

    tasks = board.get("tasks") or []
    if not isinstance(tasks, list) or not tasks:
        problems.append("tasks must be a non-empty list")
        return problems
    ids: set[str] = set()
    for i, task in enumerate(tasks):
        if not isinstance(task, dict):
            problems.append(f"tasks[{i}] not an object")
            continue
        tid = str(task.get("id") or "")
        if not tid:
            problems.append(f"tasks[{i}] missing id")
        elif tid in ids:
            problems.append(f"duplicate task id {tid}")
        else:
            ids.add(tid)
        if not task.get("intent"):
            problems.append(f"task {tid or i} missing intent")
        status = str(task.get("status") or "open")
        if status not in VALID_STATUS:
            problems.append(f"task {tid} bad status {status!r}")
        session = task.get("session")
        if session and sessions and str(session) not in sessions:
            problems.append(f"task {tid} session {session} is not a company")
        for dep in task.get("depends_on") or []:
            if dep not in ids and dep not in {str(t.get("id")) for t in tasks if isinstance(t, dict)}:
                # checked after loop for forward refs
                pass
    known = {str(t.get("id")) for t in tasks if isinstance(t, dict)}
    for task in tasks:
        if not isinstance(task, dict):
            continue
        for dep in task.get("depends_on") or []:
            if dep not in known:
                problems.append(f"task {task.get('id')} depends on unknown {dep}")
    return problems


def rows(board: dict[str, Any]) -> list[dict[str, str]]:
    companies = {
        str(c.get("session")): c
        for c in (board.get("companies") or [])
        if isinstance(c, dict) and c.get("session")
    }
    out: list[dict[str, str]] = []
    for task in board.get("tasks") or []:
        if not isinstance(task, dict):
            continue
        session = str(task.get("session") or "")
        co = companies.get(session, {})
        out.append(
            {
                "id": str(task.get("id") or ""),
                "intent": str(task.get("intent") or ""),
                "status": str(task.get("status") or "open"),
                "session": session or "(unassigned)",
                "callsign": str(co.get("callsign") or ""),
                "host": str(co.get("host") or ""),
                "reports_to": str(task.get("reports_to") or co.get("reports_to") or ""),
                "resources": ", ".join(task.get("resources") or co.get("resources") or []),
            }
        )
    return out


def render_table(board: dict[str, Any]) -> str:
    lines = [
        f"CTX {board.get('context_version')} — {board.get('commander_intent')}",
        "",
        f"{'ID':<8} {'STATUS':<12} {'SESSION / COMPANY':<36} {'REPORTS TO':<28} INTENT",
        "-" * 108,
    ]
    hhq = board.get("higher_echelon") or {}
    if isinstance(hhq, dict) and hhq.get("id"):
        lines.insert(
            1,
            f"HHQ {hhq.get('callsign') or 'higher'} ({hhq.get('id')}) — intent down, status up",
        )
    for row in rows(board):
        owner = row["session"]
        if row["callsign"]:
            owner = f"{row['callsign']} ({row['session']})"
        lines.append(
            f"{row['id']:<8} {row['status']:<12} {owner:<36} {row['reports_to']:<28} {row['intent']}"
        )
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Render team task board.")
    parser.add_argument("--board", required=True, help="Path to team-task-board.json")
    parser.add_argument("--json", action="store_true", dest="as_json")
    args = parser.parse_args(argv)

    board = load_board(Path(args.board))
    problems = validate_board(board)
    if problems:
        print("ERROR: " + "; ".join(problems), file=sys.stderr)
        return 1
    if args.as_json:
        print(json.dumps({"context_version": board.get("context_version"), "rows": rows(board)}, indent=2))
    else:
        print(render_table(board))
    return 0


if __name__ == "__main__":
    sys.exit(main())
