#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Small Herdr bridge for dogfooding Agent Rally Point in panes."""
from __future__ import annotations

import json
import subprocess
from dataclasses import dataclass
from typing import Callable

from .coordination_trace import PendingHandoff, event_label, find_record

Runner = Callable[[list[str]], subprocess.CompletedProcess]


@dataclass(frozen=True)
class HerdrAgent:
    """A detected Herdr agent pane."""

    agent: str
    pane_id: str
    status: str
    cwd: str | None = None


def _run(args: list[str]) -> subprocess.CompletedProcess:
    return subprocess.run(args, capture_output=True, text=True, check=False)


def list_agents(*, runner: Runner = _run) -> list[HerdrAgent]:
    """Return Herdr agents, or an empty list when Herdr is unavailable."""
    result = runner(["herdr", "agent", "list"])
    if result.returncode != 0:
        return []
    try:
        data = json.loads(result.stdout)
    except json.JSONDecodeError:
        return []
    out: list[HerdrAgent] = []
    for rec in (data.get("result") or {}).get("agents", []):
        agent = rec.get("agent")
        pane_id = rec.get("pane_id")
        if isinstance(agent, str) and isinstance(pane_id, str):
            out.append(HerdrAgent(
                agent=agent,
                pane_id=pane_id,
                status=str(rec.get("agent_status") or "unknown"),
                cwd=rec.get("cwd") if isinstance(rec.get("cwd"), str) else None,
            ))
    return out


def match_agent(agents: list[HerdrAgent], target_tool: str | None) -> HerdrAgent | None:
    """Find the Herdr pane most likely to own ``target_tool``."""
    if not target_tool:
        return None
    aliases = {
        "claude_code": {"claude", "claude_code"},
        "pi": {"pi"},
        "codex": {"codex"},
        "cursor": {"cursor"},
        "gemini": {"gemini"},
    }
    wanted = aliases.get(target_tool, {target_tool})
    for agent in agents:
        if agent.agent in wanted:
            return agent
    return None


def report_pending_status(
    handoffs: list[PendingHandoff],
    *,
    runner: Runner = _run,
) -> list[str]:
    """Report pending handoffs into matching Herdr panes as custom status.

    Returns human-readable action lines for CLI output.
    """
    agents = list_agents(runner=runner)
    lines: list[str] = []
    for item in handoffs:
        agent = match_agent(agents, item.to_tool)
        if agent is None:
            lines.append(f"no Herdr pane for {item.to_tool}: {item.event_id}")
            continue
        result = runner([
            "herdr", "pane", "report-agent", agent.pane_id,
            "--source", "custom:agent-rally",
            "--agent", agent.agent,
            "--state", "blocked",
            "--custom-status", "handoff",
        ])
        if result.returncode == 0:
            lines.append(f"reported {item.event_id} to {agent.agent} pane {agent.pane_id}")
        else:
            lines.append(f"failed reporting {item.event_id} to {agent.pane_id}: {result.stderr.strip()}")
    return lines


def handoff_prompt(record: dict) -> str:
    """Return a compact prompt to inject into a target agent pane."""
    payload = record.get("payload") or {}
    files = payload.get("ref_files") or payload.get("files") or []
    file_text = f"\nFiles: {', '.join(files)}" if isinstance(files, list) and files else ""
    notes = payload.get("notes")
    notes_text = f"\nNotes: {notes}" if isinstance(notes, str) and notes else ""
    return (
        "Agent Rally Point handoff:\n"
        f"ID: {record.get('id') or payload.get('id') or '(no id)'}\n"
        f"{event_label(record)}"
        f"{file_text}{notes_text}\n"
        "Please acknowledge with agent-rally ack/reject/needs-info when done."
    )


def inject_handoff(
    records: list[dict],
    identifier: str,
    *,
    runner: Runner = _run,
) -> str:
    """Inject a handoff prompt into the target Herdr agent pane."""
    record = find_record(records, identifier)
    if record is None or record.get("kind") != "handoff":
        raise ValueError(f"handoff {identifier!r} not found")
    payload = record.get("payload") or {}
    target = payload.get("to_tool") or payload.get("to")
    agent = match_agent(list_agents(runner=runner), target if isinstance(target, str) else None)
    if agent is None:
        raise RuntimeError(f"no Herdr pane found for target {target!r}")
    result = runner(["herdr", "pane", "run", agent.pane_id, handoff_prompt(record)])
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip() or "herdr pane run failed")
    return f"injected {identifier} into {agent.agent} pane {agent.pane_id}"
