#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Format team role + performance lines for Rally Watcher notify sinks.

Does not call osascript itself. Prints one title line and one body line
that a notify sink (or `agent-rally-watcher`) can pass as argv data.

Usage:
    python3 scripts/team_status_notify.py --snapshot <room.json>
    python3 scripts/team_status_notify.py --demo

Exit 0 on a well-formed snapshot; non-zero if required fields are missing.
"""

from __future__ import annotations

import argparse
import json
import sys
from typing import Any


def format_status(snapshot: dict[str, Any]) -> tuple[str, str]:
    """Return (title, body) from a rally room snapshot dict."""
    lead = snapshot.get("lead") or snapshot.get("lead_tool") or "unassigned"
    mode = snapshot.get("team_mode") or snapshot.get("mode") or "standard"
    context = snapshot.get("context_version") or snapshot.get("mission_seq") or "?"
    squads = snapshot.get("squads") or snapshot.get("peers") or []
    if not isinstance(squads, list):
        squads = []

    roles: list[str] = []
    acked = 0
    for peer in squads:
        if not isinstance(peer, dict):
            continue
        tool = str(peer.get("tool") or peer.get("id") or "unknown")
        role = str(peer.get("role") or ("lead" if tool == str(lead) else "worker"))
        acknowledged = bool(peer.get("acknowledged") or peer.get("acked"))
        if acknowledged:
            acked += 1
        roles.append(f"{tool}:{role}:{'ack' if acknowledged else 'no-ack'}")

    title = f"Rally team · mode={mode} · ctx={context}"
    body = f"lead={lead} members={len(squads)} acked={acked}"
    if roles:
        body = f"{body} | " + "; ".join(roles[:8])
    return title, body


def demo_snapshot() -> dict[str, Any]:
    return {
        "lead": "cursor:5be1cf8a-6687-468e-8f0e-e1e9e8ab3818",
        "team_mode": "standard",
        "context_version": "TEAM-CTX-1",
        "squads": [
            {
                "tool": "cursor:5be1cf8a-6687-468e-8f0e-e1e9e8ab3818",
                "role": "worker",
                "acknowledged": True,
            },
            {"tool": "claude_code:muse-lead", "role": "lead", "acknowledged": True},
            {"tool": "codex:reviewer", "role": "reviewer", "acknowledged": False},
        ],
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Format team status notify lines.")
    parser.add_argument("--snapshot", help="Path to rally room JSON.")
    parser.add_argument("--demo", action="store_true", help="Use a canned snapshot.")
    args = parser.parse_args(argv)

    if args.demo:
        snapshot = demo_snapshot()
    elif args.snapshot:
        with open(args.snapshot, encoding="utf-8") as fh:
            snapshot = json.load(fh)
        if not isinstance(snapshot, dict):
            print("ERROR: snapshot must be a JSON object", file=sys.stderr)
            return 1
    else:
        print("ERROR: pass --snapshot or --demo", file=sys.stderr)
        return 2

    title, body = format_status(snapshot)
    print(title)
    print(body)
    return 0


if __name__ == "__main__":
    sys.exit(main())
