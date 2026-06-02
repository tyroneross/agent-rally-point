# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Dispatch sinks: file appends JSONL; unknown type returns False; http stub."""
from __future__ import annotations

import json
from pathlib import Path

from agent_rally_watcher.dispatch import dispatch

REC = {"kind": "feedback", "tool": "codex", "payload": {"verdict": "PASS"}, "run_id": "r1"}


def test_file_sink_appends_jsonl(tmp_path: Path) -> None:
    out = tmp_path / "stream.jsonl"
    result = dispatch(REC, {"type": "file", "path": str(out)})
    assert result.delivered
    assert result.sink_type == "file"
    line = out.read_text(encoding="utf-8").strip()
    assert json.loads(line) == REC


def test_file_sink_two_records_two_lines(tmp_path: Path) -> None:
    out = tmp_path / "stream.jsonl"
    dispatch(REC, {"type": "file", "path": str(out)})
    dispatch({**REC, "run_id": "r2"}, {"type": "file", "path": str(out)})
    lines = out.read_text(encoding="utf-8").strip().split("\n")
    assert len(lines) == 2


def test_file_sink_missing_path() -> None:
    result = dispatch(REC, {"type": "file"})
    assert not result.delivered
    assert "missing" in result.detail


def test_unknown_sink_type_returns_false() -> None:
    result = dispatch(REC, {"type": "carrier-pigeon"})
    assert not result.delivered
    assert "unknown" in result.detail


def test_http_sink_is_stubbed() -> None:
    # v0.1 stub returns delivered=False so callers see the gap, not a silent drop
    result = dispatch(REC, {"type": "http", "url": "https://example.test/hook"})
    assert not result.delivered
    assert "stubbed" in result.detail


def test_file_sink_creates_parent_dirs(tmp_path: Path) -> None:
    out = tmp_path / "deep" / "nested" / "stream.jsonl"
    result = dispatch(REC, {"type": "file", "path": str(out)})
    assert result.delivered
    assert out.exists()
