# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Tests for agent-rally-point CLI trace commands."""
from __future__ import annotations

import sys
from pathlib import Path

_HERE = Path(__file__).resolve().parent
if str(_HERE) not in sys.path:
    sys.path.insert(0, str(_HERE))

from agent_rally_point.cli import main  # noqa: E402
from agent_rally_point.coordination_trace import load_records  # noqa: E402


def test_handoff_inbox_ack_thread_report_smoke(tmp_path: Path, capsys):
    # intent: CLI handoff lifecycle writes trace events and clears inbox after ack.
    assert main([
        "handoff", "--channel-dir", str(tmp_path), "--to", "codex",
        "--from-tool", "pi", "--subject", "review schema", "--files", "docs/SCHEMA.md",
    ]) == 0
    out = capsys.readouterr().out
    handoff_id = out.split()[2]

    assert main(["inbox", "--channel-dir", str(tmp_path), "--tool", "codex"]) == 0
    assert handoff_id in capsys.readouterr().out

    assert main(["ack", "--channel-dir", str(tmp_path), "--tool", "codex", handoff_id, "--summary", "done"]) == 0
    assert "posted done ack" in capsys.readouterr().out

    assert main(["inbox", "--channel-dir", str(tmp_path), "--tool", "codex"]) == 0
    assert "Inbox empty" in capsys.readouterr().out

    assert main(["thread", "--channel-dir", str(tmp_path), handoff_id]) == 0
    thread_out = capsys.readouterr().out
    assert "handoff" in thread_out and "ack" in thread_out

    assert main(["report", "--channel-dir", str(tmp_path), "--since", "1d", "--ids"]) == 0
    assert "Pending handoffs: 0" in capsys.readouterr().out

    records = load_records(tmp_path)
    assert [r["kind"] for r in records] == ["handoff", "ack"]
