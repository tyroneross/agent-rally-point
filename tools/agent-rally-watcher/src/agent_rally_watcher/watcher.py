# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
# build-loop@tyroneross:canary:agent-rally-watcher
# canary-end
"""Push-based tail of a Rally Point channel's ``changes.jsonl``.

Uses ``watchfiles`` (kqueue on macOS, inotify on Linux) for sub-second
file-modification notifications. On each notification, reads new lines
past every consumer's cursor, applies the filter, dispatches, and
persists the cursor.

Cursor invariants:
    - Stored as a byte offset (matches agent-rally-point's append-only model).
    - Never rewinds (load_cursor returns 0 on absent / corrupt cursor; advance ignores backward moves).
    - Persisted after every successful dispatch batch (per consumer).

Crash semantics: at-least-once delivery per consumer. The cursor advances
only after dispatch returns, so a crash mid-dispatch may re-deliver the
in-flight record on restart. File / notify sinks tolerate this (append
is idempotent at JSONL granularity in practice; notify deduplication is
the consumer's problem if they care).

ARP-007 quarantine semantics (2026-08 third-party security audit, GitHub
issue #52): a syntactically malformed complete line (bad JSON, or bytes
that don't decode as UTF-8) used to be silently skipped, with the cursor
advancing past it — the record was lost for that consumer forever, with no
trace. Now:

    1. The FIRST time a given malformed line is seen, its raw bytes are
       appended to that consumer's quarantine ledger
       (``cursor.quarantine_path``) and a WARNING is logged. This is a
       durable, inspectable record — never a silent drop.
    2. The cursor STOPS right before that line. It does not advance past
       unacknowledged corruption, and — deliberately — nothing AFTER the
       bad line dispatches either, for this consumer, until it is dealt
       with. This is intentional backpressure: a consumer that silently
       skipped ahead would look "healthy" while quietly losing data;
       stalling forces the condition to be noticed (the ledger + the
       warning log are exactly how it gets noticed).
    3. To avoid an UNBOUNDED stall once the operator has reviewed the
       ledger, ``cursor.save_quarantine_ack(consumer_id, offset)`` (or
       ``agent-rally-watcher ack-quarantine --consumer <id>``, cli.py)
       raises that consumer's acknowledgement watermark. Any malformed
       line whose start-offset is below the watermark is treated as
       already-reviewed: it is NOT re-quarantined (no duplicate ledger
       entries across the many sweeps that occur while stalled at the
       same offset) and the cursor advances past it, resuming forward
       progress — including any GOOD records that were queued up behind
       the bad line.

    Net effect: a bad line is quarantined exactly once per stall, reported
    non-silently, and the consumer can always be made to progress again —
    via an explicit, logged operator action, never automatically.
"""
from __future__ import annotations

import json
import logging
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Iterable

from .cursor import (
    Cursor,
    cursor_path,
    load_cursor,
    load_quarantine_ack,
    quarantine_path,
    save_cursor,
)
from .dispatch import dispatch
from .filter import Consumer, match

logger = logging.getLogger(__name__)

_CHANGES_FILENAME = "changes.jsonl"


@dataclass
class Watcher:
    """Configuration bundle for one channel watch.

    ``seek_to_end_on_first_start`` controls first-start backfill: when True
    (the v0.1.1 default) and a consumer's cursor file is absent, the cursor
    is pre-seeded to the current end-of-file before the first sweep, so only
    NEW events dispatch. When False, the cursor starts at byte 0 (full
    backfill — v0.1.0 behavior). Existing cursors are never touched.
    """

    channel_dir: Path
    consumers: list[Consumer]
    stop_event: threading.Event | None = None
    cursor_root: Path | None = None
    seek_to_end_on_first_start: bool = True

    @property
    def changes_path(self) -> Path:
        return self.channel_dir / _CHANGES_FILENAME


def _quarantine_line(
    consumer_id: str, cursor_root: Path | None, line_start: int, raw: bytes, exc: Exception
) -> None:
    """Append one malformed-line entry to ``consumer_id``'s quarantine ledger.

    Deduplicated by ``offset``: while a consumer is stalled at the same bad
    line (every sweep re-reads from the same cursor offset until acked),
    this must not append a fresh ledger entry each time.
    """
    qpath = quarantine_path(consumer_id, cursor_root)
    if qpath.exists():
        try:
            with open(qpath, "r", encoding="utf-8") as fh:
                for existing in fh:
                    try:
                        entry = json.loads(existing)
                    except ValueError:
                        continue
                    if entry.get("offset") == line_start:
                        return  # already recorded this sweep-stall
        except OSError:
            pass  # fall through and attempt to record anyway
    qpath.parent.mkdir(parents=True, exist_ok=True)
    entry = {
        "offset": line_start,
        "length": len(raw),
        "raw": raw.decode("utf-8", errors="replace"),
        "error": f"{type(exc).__name__}: {exc}",
        "quarantined_at": time.time(),
    }
    with open(qpath, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(entry) + "\n")


def _read_new_lines(
    path: Path, offset: int, *, consumer_id: str, cursor_root: Path | None
) -> tuple[list[dict[str, Any]], int]:
    """Read complete lines from ``offset`` to EOF.

    Returns ``(records, new_offset)``. Partial trailing line is left for
    the next call: ``new_offset`` only advances past lines terminated by
    ``\\n``. Mirrors agent-rally-point's ``read_changes_since`` semantics.

    ARP-007: a malformed complete line (bad JSON / bad UTF-8) is quarantined
    (see module docstring) and ``new_offset`` STOPS right before it, unless
    it is already below this consumer's quarantine-ack watermark (in which
    case it is skipped without re-quarantining and ``new_offset`` advances
    past it normally).
    """
    try:
        size = path.stat().st_size
    except (FileNotFoundError, OSError):
        return [], offset
    if offset < 0 or offset > size:
        # File rotated/truncated under us → restart from 0 to avoid skipping events
        offset = 0
    records: list[dict[str, Any]] = []
    new_offset = offset
    acked_through = load_quarantine_ack(consumer_id, cursor_root)
    try:
        with open(path, "rb") as fh:
            fh.seek(offset)
            while True:
                line_start = fh.tell()
                raw = fh.readline()
                if not raw or not raw.endswith(b"\n"):
                    break  # EOF, or a partial trailing line held for next sweep
                try:
                    records.append(json.loads(raw.decode("utf-8")))
                    new_offset = fh.tell()
                except (ValueError, UnicodeDecodeError) as exc:
                    if line_start < acked_through:
                        # Operator already reviewed + acknowledged this
                        # offset (or an offset covering it) — move on.
                        new_offset = fh.tell()
                        continue
                    _quarantine_line(consumer_id, cursor_root, line_start, raw, exc)
                    logger.warning(
                        "consumer=%s malformed line quarantined at byte offset %d (%s: %s) — "
                        "cursor stalled here until acknowledged: "
                        "cursor.save_quarantine_ack(%r, ...) or "
                        "`agent-rally-watcher ack-quarantine --consumer %s`",
                        consumer_id,
                        line_start,
                        type(exc).__name__,
                        exc,
                        consumer_id,
                        consumer_id,
                    )
                    break  # stall: new_offset does not include this line
    except OSError:
        return [], offset
    return records, new_offset


def _process_once(watcher: Watcher) -> dict[str, int]:
    """One read-filter-dispatch sweep across all consumers. Returns per-consumer delivered counts."""
    delivered: dict[str, int] = {}
    path = watcher.changes_path
    for consumer in watcher.consumers:
        cursor = load_cursor(consumer.id, watcher.cursor_root)
        records, new_offset = _read_new_lines(
            path, cursor.offset, consumer_id=consumer.id, cursor_root=watcher.cursor_root
        )
        if not records and new_offset == cursor.offset:
            continue
        n = 0
        for rec in records:
            if not match(rec, consumer.filter):
                continue
            result = dispatch(rec, consumer.sink)
            if result.delivered:
                n += 1
            else:
                logger.warning(
                    "consumer=%s sink=%s drop: %s",
                    consumer.id,
                    result.sink_type,
                    result.detail,
                )
        cursor.advance(new_offset)
        save_cursor(cursor, watcher.cursor_root)
        delivered[consumer.id] = n
    return delivered


def _seed_absent_cursors_to_end(watcher: Watcher) -> int:
    """Pre-seed cursors at current EOF for consumers with no cursor file yet.

    Returns the number of cursors seeded. Existing cursors are NOT touched —
    a restart always honors the persisted offset, regardless of this flag.
    Called once before the first sweep when ``seek_to_end_on_first_start`` is True.
    """
    try:
        size = watcher.changes_path.stat().st_size
    except (FileNotFoundError, OSError):
        size = 0
    seeded = 0
    for consumer in watcher.consumers:
        cp = cursor_path(consumer.id, watcher.cursor_root)
        if cp.exists():
            continue  # restart — preserve persisted offset
        cursor = Cursor(consumer_id=consumer.id, offset=size)
        save_cursor(cursor, watcher.cursor_root)
        seeded += 1
    return seeded


def run_watcher(
    watcher: Watcher,
    *,
    backend: Callable[[Watcher], Iterable[None]] | None = None,
) -> None:
    """Blocking loop. Returns when ``watcher.stop_event`` is set.

    The default backend uses ``watchfiles.watch`` on the channel dir.
    ``backend`` is injectable for testing (yield once per change to drive a sweep).
    """
    if watcher.stop_event is None:
        watcher.stop_event = threading.Event()

    # First-start seek-to-end (v0.1.1 default). Pre-seeds only absent cursors;
    # restarts with persisted cursors are unaffected.
    if watcher.seek_to_end_on_first_start:
        seeded = _seed_absent_cursors_to_end(watcher)
        if seeded:
            logger.info(
                "seeded %d cursor(s) to file-end (use --from-start to backfill)", seeded
            )

    # First sweep — catch any events already in the log past the last cursor.
    _process_once(watcher)

    if backend is None:
        backend = _watchfiles_backend  # default real backend

    for _ in backend(watcher):
        if watcher.stop_event.is_set():
            return
        _process_once(watcher)


def _watchfiles_backend(watcher: Watcher) -> Iterable[None]:
    """Yield one event per ``changes.jsonl`` modification.

    Imported lazily so unit tests can run without watchfiles installed
    (they inject their own backend).
    """
    from watchfiles import Change, watch  # type: ignore[import-not-found]

    target = str(watcher.changes_path)
    # Watch the parent dir (so we catch create as well as modify).
    for changes in watch(
        str(watcher.channel_dir),
        stop_event=watcher.stop_event,
        rust_timeout=1000,  # ms; bounded so stop_event is checked between batches
        yield_on_timeout=True,
    ):
        # changes is a set of (Change, path) tuples; filter to our file
        for change, path in changes:
            if path == target and change in (Change.added, Change.modified):
                yield None
                break
        else:
            # Timeout tick: yield anyway so the caller can re-check stop_event.
            yield None


# canary: agent-rally-watcher@tyroneross — canonical source: github.com/tyroneross/agent-rally-watcher
