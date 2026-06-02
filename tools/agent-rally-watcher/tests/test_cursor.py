# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Cursor persistence: load returns 0 absent, advance never rewinds, round-trip."""
from __future__ import annotations

from pathlib import Path

import pytest

from agent_rally_watcher.cursor import Cursor, cursor_path, load_cursor, save_cursor


def test_load_returns_zero_when_absent(tmp_path: Path) -> None:
    c = load_cursor("claude_code", root=tmp_path)
    assert c.offset == 0
    assert c.consumer_id == "claude_code"


def test_save_then_load_round_trips(tmp_path: Path) -> None:
    save_cursor(Cursor(consumer_id="claude_code", offset=512), root=tmp_path)
    c = load_cursor("claude_code", root=tmp_path)
    assert c.offset == 512


def test_advance_never_rewinds() -> None:
    c = Cursor(consumer_id="x", offset=100)
    c.advance(50)
    assert c.offset == 100
    c.advance(200)
    assert c.offset == 200


def test_invalid_consumer_id_rejected(tmp_path: Path) -> None:
    with pytest.raises(ValueError):
        cursor_path("../escape", root=tmp_path)
    with pytest.raises(ValueError):
        cursor_path("", root=tmp_path)
    with pytest.raises(ValueError):
        cursor_path(".hidden", root=tmp_path)


def test_corrupt_cursor_file_returns_zero(tmp_path: Path) -> None:
    p = cursor_path("claude_code", root=tmp_path)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text("not-a-number\n", encoding="utf-8")
    c = load_cursor("claude_code", root=tmp_path)
    assert c.offset == 0
