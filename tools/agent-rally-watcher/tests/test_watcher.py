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

from agent_rally_watcher.cursor import (
    load_cursor,
    load_quarantine_ack,
    quarantine_path,
    save_quarantine_ack,
)
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


# ===========================================================================
# ARP-007 adversarial controls — quarantine semantics
# ===========================================================================


def test_malformed_line_is_quarantined_and_stalls_cursor(tmp_path: Path) -> None:
    """A corrupt complete line is NOT silently skipped: it lands in the
    quarantine ledger, is surfaced via a warning log, and the cursor does
    NOT advance past it (nor past anything after it) until acknowledged."""
    channel_dir = tmp_path / "channel"
    cursor_root = tmp_path / "cursors"
    changes = _seed_channel(channel_dir)  # 5 well-formed records
    pre_corrupt_size = changes.stat().st_size

    corrupt_line = "{not valid json at all"
    good_line_after = json.dumps({"kind": "feedback", "run_id": "after-corrupt", "payload": {}})
    with open(changes, "a", encoding="utf-8") as fh:
        fh.write(corrupt_line + "\n")
        fh.write(good_line_after + "\n")

    rule = FilterRule()  # match everything
    consumer = _consumer(tmp_path, "quarantine_test", rule)
    watcher = Watcher(channel_dir=channel_dir, consumers=[consumer], cursor_root=cursor_root)

    delivered = _process_once(watcher)

    # The 5 pre-existing (good) fixture records dispatch normally — this is
    # the FIRST-ever sweep for this consumer, so they're read in the SAME
    # sweep that then hits the corrupt line. The good record AFTER the
    # corrupt line must NOT have dispatched — the consumer stalls at the
    # corrupt line, so nothing queued behind it gets silently lost OR
    # silently delivered out of a broken sequence.
    assert delivered.get("quarantine_test", 0) == 5
    out_path = tmp_path / "quarantine_test.out.jsonl"
    out_text = out_path.read_text(encoding="utf-8")
    assert len(out_text.strip().split("\n")) == 5
    assert "after-corrupt" not in out_text

    # Cursor sits exactly at the byte offset where the corrupt line starts —
    # NOT advanced past it (this is the direct fix for the audit finding:
    # the old behavior advanced the cursor past corrupt lines silently).
    cursor = load_cursor("quarantine_test", root=cursor_root)
    assert cursor.offset == pre_corrupt_size

    # The raw corrupt line landed in the quarantine ledger — durable,
    # inspectable record; NOT a silent drop.
    qpath = quarantine_path("quarantine_test", cursor_root)
    assert qpath.exists()
    entries = [json.loads(line) for line in qpath.read_text(encoding="utf-8").splitlines()]
    assert len(entries) == 1
    assert entries[0]["offset"] == pre_corrupt_size
    assert entries[0]["raw"].rstrip("\n") == corrupt_line
    assert "error" in entries[0] and entries[0]["error"]


def test_quarantine_does_not_duplicate_across_repeated_stalled_sweeps(tmp_path: Path) -> None:
    """Re-sweeping while stalled at the same corrupt line must not bloat the
    quarantine ledger with duplicate entries."""
    channel_dir = tmp_path / "channel"
    cursor_root = tmp_path / "cursors"
    changes = _seed_channel(channel_dir)
    with open(changes, "a", encoding="utf-8") as fh:
        fh.write("{still not valid json\n")

    rule = FilterRule()
    consumer = _consumer(tmp_path, "repeat_stall", rule)
    watcher = Watcher(channel_dir=channel_dir, consumers=[consumer], cursor_root=cursor_root)

    _process_once(watcher)
    _process_once(watcher)
    _process_once(watcher)

    qpath = quarantine_path("repeat_stall", cursor_root)
    entries = qpath.read_text(encoding="utf-8").splitlines()
    assert len(entries) == 1


def test_quarantine_ack_unstalls_the_consumer(tmp_path: Path) -> None:
    """After the operator acknowledges the quarantined offset, the consumer
    resumes forward progress — including the good record queued behind the
    bad line — without re-quarantining."""
    channel_dir = tmp_path / "channel"
    cursor_root = tmp_path / "cursors"
    changes = _seed_channel(channel_dir)
    pre_corrupt_size = changes.stat().st_size
    corrupt_line = "{not valid json at all"
    with open(changes, "a", encoding="utf-8") as fh:
        fh.write(corrupt_line + "\n")
        fh.write(json.dumps({"kind": "feedback", "run_id": "after-corrupt", "payload": {}}) + "\n")

    rule = FilterRule()
    consumer = _consumer(tmp_path, "ack_test", rule)
    watcher = Watcher(channel_dir=channel_dir, consumers=[consumer], cursor_root=cursor_root)

    _process_once(watcher)  # first-ever sweep: delivers the 5 fixture records, then stalls at the corrupt line
    cursor_before = load_cursor("ack_test", root=cursor_root)
    assert cursor_before.offset == pre_corrupt_size

    # Operator acknowledges everything currently quarantined for this consumer.
    corrupt_line_len = len((corrupt_line + "\n").encode("utf-8"))
    save_quarantine_ack("ack_test", pre_corrupt_size + corrupt_line_len, cursor_root)
    assert load_quarantine_ack("ack_test", cursor_root) > 0

    delivered = _process_once(watcher)
    # This sweep's OWN delivered count (not cumulative across sweeps) is
    # just the one record that was stalled behind the corrupt line — the 5
    # fixture records already delivered on the warm-up sweep above.
    assert delivered.get("ack_test", 0) == 1
    out_lines = (tmp_path / "ack_test.out.jsonl").read_text(encoding="utf-8").strip().split("\n")
    assert len(out_lines) == 6  # 5 from warm-up + 1 unstalled by the ack
    assert json.loads(out_lines[-1])["run_id"] == "after-corrupt"

    # Cursor advanced all the way to EOF — past the acked corrupt line AND
    # the good record after it.
    cursor_after = load_cursor("ack_test", root=cursor_root)
    assert cursor_after.offset == changes.stat().st_size

    # No duplicate ledger entry was written on the ack'd sweep.
    qpath = quarantine_path("ack_test", cursor_root)
    entries = qpath.read_text(encoding="utf-8").splitlines()
    assert len(entries) == 1


def test_valid_jsonl_still_processes_unaffected_by_quarantine_logic(tmp_path: Path) -> None:
    """Positive control: a channel with no malformed lines is completely
    unaffected by the quarantine machinery."""
    channel_dir = tmp_path / "channel"
    cursor_root = tmp_path / "cursors"
    _seed_channel(channel_dir)
    rule = FilterRule()
    consumer = _consumer(tmp_path, "clean", rule)
    watcher = Watcher(channel_dir=channel_dir, consumers=[consumer], cursor_root=cursor_root)

    delivered = _process_once(watcher)
    assert delivered["clean"] == 5
    assert not quarantine_path("clean", cursor_root).exists()


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
