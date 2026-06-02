# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Watcher end-to-end: tail + filter + dispatch + cursor persistence.

Uses an injected synchronous backend (no watchfiles dependency in the test
process), so test runs in milliseconds and is hermetic.
"""
from __future__ import annotations

import json
import shutil
import threading
from pathlib import Path
from typing import Iterable

import pytest

from agent_rally_watcher.cursor import load_cursor
from agent_rally_watcher.filter import Consumer, FilterRule
from agent_rally_watcher.watcher import Watcher, _process_once, run_watcher

FIXTURE = Path(__file__).parent / "fixtures" / "sample_changes.jsonl"


def _seed_channel(channel_dir: Path) -> Path:
    """Copy the fixture into channel_dir/changes.jsonl."""
    channel_dir.mkdir(parents=True, exist_ok=True)
    target = channel_dir / "changes.jsonl"
    shutil.copy(FIXTURE, target)
    return target


def _consumer(tmp_path: Path, cid: str, rule: FilterRule) -> Consumer:
    return Consumer(
        id=cid,
        filter=rule,
        sink={"type": "file", "path": str(tmp_path / f"{cid}.out.jsonl")},
    )


def test_process_once_delivers_matched_records(tmp_path: Path) -> None:
    channel_dir = tmp_path / "channel"
    cursor_root = tmp_path / "cursors"
    _seed_channel(channel_dir)
    c_feedback = _consumer(tmp_path, "fb_only", FilterRule(kinds=["feedback"]))
    watcher = Watcher(channel_dir=channel_dir, consumers=[c_feedback], cursor_root=cursor_root)

    delivered = _process_once(watcher)
    assert delivered["fb_only"] == 2  # two feedback records in fixture

    out = (tmp_path / "fb_only.out.jsonl").read_text(encoding="utf-8").strip().split("\n")
    kinds = [json.loads(line)["kind"] for line in out]
    assert kinds == ["feedback", "feedback"]


def test_cursor_advances_and_resumes(tmp_path: Path) -> None:
    channel_dir = tmp_path / "channel"
    cursor_root = tmp_path / "cursors"
    changes = _seed_channel(channel_dir)
    rule = FilterRule()  # match everything
    consumer = _consumer(tmp_path, "all", rule)
    watcher = Watcher(channel_dir=channel_dir, consumers=[consumer], cursor_root=cursor_root)

    # First sweep — consume all 5 records
    _process_once(watcher)
    cursor1 = load_cursor("all", root=cursor_root)
    assert cursor1.offset == changes.stat().st_size

    # Append a 6th record
    with open(changes, "a", encoding="utf-8") as fh:
        fh.write(
            json.dumps(
                {
                    "ts": 1716480005.0,
                    "kind": "phase",
                    "tool": "claude_code",
                    "model": "claude-opus-4-7",
                    "run_id": "run-004",
                    "app_slug": "demo",
                    "payload": {"phase": "review"},
                    "revision": 6,
                }
            )
            + "\n"
        )

    # Second sweep — only the new record
    _process_once(watcher)
    out = (tmp_path / "all.out.jsonl").read_text(encoding="utf-8").strip().split("\n")
    assert len(out) == 6  # 5 from first sweep + 1 from second

    cursor2 = load_cursor("all", root=cursor_root)
    assert cursor2.offset == changes.stat().st_size
    assert cursor2.offset > cursor1.offset


def test_no_duplicate_on_restart(tmp_path: Path) -> None:
    """Simulate restart: rebuild watcher from cursor, expect zero re-delivery."""
    channel_dir = tmp_path / "channel"
    cursor_root = tmp_path / "cursors"
    _seed_channel(channel_dir)
    rule = FilterRule(kinds=["feedback"])
    c1 = _consumer(tmp_path, "fb", rule)
    w1 = Watcher(channel_dir=channel_dir, consumers=[c1], cursor_root=cursor_root)
    _process_once(w1)
    initial_lines = (tmp_path / "fb.out.jsonl").read_text(encoding="utf-8").count("\n")

    # "Restart" — fresh Watcher, same cursor_root
    c2 = _consumer(tmp_path, "fb", rule)
    w2 = Watcher(channel_dir=channel_dir, consumers=[c2], cursor_root=cursor_root)
    delivered = _process_once(w2)
    assert delivered.get("fb", 0) == 0

    final_lines = (tmp_path / "fb.out.jsonl").read_text(encoding="utf-8").count("\n")
    assert final_lines == initial_lines


def test_partial_trailing_line_held_for_next_sweep(tmp_path: Path) -> None:
    """A line without a trailing newline must not advance the cursor past it."""
    channel_dir = tmp_path / "channel"
    cursor_root = tmp_path / "cursors"
    changes = _seed_channel(channel_dir)
    # Append a partial line (no trailing newline)
    partial = json.dumps({"ts": 1.0, "kind": "feedback", "tool": "codex", "model": "x", "run_id": "r9", "app_slug": "demo", "payload": {"verdict": "PASS"}, "revision": 99})
    with open(changes, "a", encoding="utf-8") as fh:
        fh.write(partial)

    rule = FilterRule()
    consumer = _consumer(tmp_path, "all", rule)
    watcher = Watcher(channel_dir=channel_dir, consumers=[consumer], cursor_root=cursor_root)
    _process_once(watcher)

    # Cursor sits at the byte before the partial line
    cursor = load_cursor("all", root=cursor_root)
    expected = changes.stat().st_size - len(partial.encode("utf-8"))
    assert cursor.offset == expected

    # Complete the line — next sweep picks it up
    with open(changes, "a", encoding="utf-8") as fh:
        fh.write("\n")
    _process_once(watcher)
    cursor2 = load_cursor("all", root=cursor_root)
    assert cursor2.offset == changes.stat().st_size


def test_from_now_skips_existing_records_on_first_start(tmp_path: Path) -> None:
    """Default (seek_to_end_on_first_start=True) → existing records NOT dispatched."""
    channel_dir = tmp_path / "channel"
    cursor_root = tmp_path / "cursors"
    changes = _seed_channel(channel_dir)  # 5 records already present
    rule = FilterRule()  # match everything
    consumer = _consumer(tmp_path, "fromnow", rule)
    stop = threading.Event()
    watcher = Watcher(
        channel_dir=channel_dir,
        consumers=[consumer],
        stop_event=stop,
        cursor_root=cursor_root,
        seek_to_end_on_first_start=True,
    )

    def backend(_w: Watcher) -> Iterable[None]:
        stop.set()
        yield None

    run_watcher(watcher, backend=backend)

    # No output file should exist (no records dispatched)
    out_path = tmp_path / "fromnow.out.jsonl"
    assert not out_path.exists() or out_path.read_text(encoding="utf-8") == ""

    # Cursor should be at file-end
    cursor = load_cursor("fromnow", root=cursor_root)
    assert cursor.offset == changes.stat().st_size


def test_from_now_picks_up_new_events_after_first_start(tmp_path: Path) -> None:
    """After seek-to-end, events appended POST-start are dispatched normally."""
    channel_dir = tmp_path / "channel"
    cursor_root = tmp_path / "cursors"
    changes = _seed_channel(channel_dir)
    rule = FilterRule()
    consumer = _consumer(tmp_path, "fromnow2", rule)
    watcher = Watcher(
        channel_dir=channel_dir,
        consumers=[consumer],
        cursor_root=cursor_root,
        seek_to_end_on_first_start=True,
    )

    # Simulate first-start seek + initial sweep (no new events → no output)
    from agent_rally_watcher.watcher import _seed_absent_cursors_to_end, _process_once

    _seed_absent_cursors_to_end(watcher)
    _process_once(watcher)
    assert not (tmp_path / "fromnow2.out.jsonl").exists()

    # Append a NEW event
    with open(changes, "a", encoding="utf-8") as fh:
        fh.write(json.dumps({"kind": "feedback", "tool": "codex", "run_id": "post", "payload": {}}) + "\n")

    _process_once(watcher)
    out = (tmp_path / "fromnow2.out.jsonl").read_text(encoding="utf-8").strip()
    assert json.loads(out)["run_id"] == "post"


def test_from_start_backfills_all_records(tmp_path: Path) -> None:
    """seek_to_end_on_first_start=False → all 5 records dispatched (v0.1.0 behavior)."""
    channel_dir = tmp_path / "channel"
    cursor_root = tmp_path / "cursors"
    _seed_channel(channel_dir)
    rule = FilterRule()
    consumer = _consumer(tmp_path, "fromstart", rule)
    stop = threading.Event()
    watcher = Watcher(
        channel_dir=channel_dir,
        consumers=[consumer],
        stop_event=stop,
        cursor_root=cursor_root,
        seek_to_end_on_first_start=False,
    )

    def backend(_w: Watcher) -> Iterable[None]:
        stop.set()
        yield None

    run_watcher(watcher, backend=backend)

    lines = (tmp_path / "fromstart.out.jsonl").read_text(encoding="utf-8").strip().split("\n")
    assert len(lines) == 5  # all fixture records dispatched


def test_seek_to_end_does_not_clobber_existing_cursor(tmp_path: Path) -> None:
    """A persisted cursor (restart scenario) is NEVER seeked to EOF, even when flag is True."""
    from agent_rally_watcher.cursor import Cursor, save_cursor
    channel_dir = tmp_path / "channel"
    cursor_root = tmp_path / "cursors"
    _seed_channel(channel_dir)
    rule = FilterRule()
    consumer = _consumer(tmp_path, "restart", rule)

    # Pre-existing cursor at offset 0 (simulates a restart with v0.1.0-style state)
    save_cursor(Cursor(consumer_id="restart", offset=0), cursor_root)

    stop = threading.Event()
    watcher = Watcher(
        channel_dir=channel_dir,
        consumers=[consumer],
        stop_event=stop,
        cursor_root=cursor_root,
        seek_to_end_on_first_start=True,  # would seek if cursor were absent
    )

    def backend(_w: Watcher) -> Iterable[None]:
        stop.set()
        yield None

    run_watcher(watcher, backend=backend)

    # All 5 records dispatched because cursor was at 0 and was not clobbered
    lines = (tmp_path / "restart.out.jsonl").read_text(encoding="utf-8").strip().split("\n")
    assert len(lines) == 5


def test_run_watcher_drives_via_injected_backend(tmp_path: Path) -> None:
    """``run_watcher`` invokes _process_once for each backend yield."""
    channel_dir = tmp_path / "channel"
    cursor_root = tmp_path / "cursors"
    _seed_channel(channel_dir)
    rule = FilterRule(kinds=["feedback"])
    consumer = _consumer(tmp_path, "fb", rule)
    stop = threading.Event()
    watcher = Watcher(
        channel_dir=channel_dir,
        consumers=[consumer],
        stop_event=stop,
        cursor_root=cursor_root,
        seek_to_end_on_first_start=False,  # backfill semantics under test
    )

    def backend(w: Watcher) -> Iterable[None]:
        yield None  # one tick, then stop
        stop.set()
        yield None

    run_watcher(watcher, backend=backend)

    # All 2 feedback records delivered (initial sweep + one backend tick = still 2 total)
    lines = (tmp_path / "fb.out.jsonl").read_text(encoding="utf-8").strip().split("\n")
    assert len(lines) == 2
