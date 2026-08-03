# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Per-consumer cursor persistence.

Each consumer has a cursor file at ``~/.agent-rally-watcher/consumers/<id>.cursor``
storing the byte offset into ``changes.jsonl`` it has already consumed. Restart-safe.

Format: single line, ASCII integer + newline. Atomic write via temp-file + rename.

ARP-007 (2026-08 third-party security audit, GitHub issue #52): also holds
the quarantine ledger + acknowledgement watermark for malformed lines —
``<id>.quarantine.jsonl`` and ``<id>.quarantine-ack``. See ``watcher.py``'s
module docstring for the full semantics; in short, a malformed line stalls
the consumer's cursor (does not advance past it silently) until
``save_quarantine_ack`` raises the watermark past it — the sanctioned,
documented way to move on. Never hand-edit ``.cursor`` to skip corruption;
that discards the durable record this module preserves.
"""
from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path

DEFAULT_CURSOR_ROOT = "~/.agent-rally-watcher/consumers"


def _cursor_root() -> Path:
    raw = os.environ.get("AGENT_RALLY_WATCHER_CURSOR_ROOT") or DEFAULT_CURSOR_ROOT
    return Path(os.path.expanduser(raw))


def cursor_path(consumer_id: str, root: Path | None = None) -> Path:
    """Return the cursor path for ``consumer_id`` (does not create)."""
    if not consumer_id or "/" in consumer_id or consumer_id.startswith("."):
        raise ValueError(f"invalid consumer_id: {consumer_id!r}")
    base = root if root is not None else _cursor_root()
    return base / f"{consumer_id}.cursor"


@dataclass
class Cursor:
    """In-memory cursor state for one consumer."""

    consumer_id: str
    offset: int = 0

    def advance(self, new_offset: int) -> None:
        if new_offset < self.offset:
            return  # never rewind
        self.offset = int(new_offset)


def load_cursor(consumer_id: str, root: Path | None = None) -> Cursor:
    """Read the cursor for ``consumer_id``; 0 if absent or malformed."""
    p = cursor_path(consumer_id, root)
    try:
        raw = p.read_text(encoding="utf-8").strip()
        return Cursor(consumer_id=consumer_id, offset=int(raw))
    except (FileNotFoundError, ValueError, OSError):
        return Cursor(consumer_id=consumer_id, offset=0)


def save_cursor(cursor: Cursor, root: Path | None = None) -> None:
    """Atomically persist ``cursor`` (temp-file + rename)."""
    p = cursor_path(cursor.consumer_id, root)
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_suffix(p.suffix + ".tmp")
    tmp.write_text(f"{cursor.offset}\n", encoding="utf-8")
    os.replace(tmp, p)


def quarantine_path(consumer_id: str, root: Path | None = None) -> Path:
    """Return the quarantine ledger path for ``consumer_id`` (does not create).

    One JSON object per line: ``{"offset": int, "length": int, "raw": str,
    "error": str, "quarantined_at": float}``. ``offset``/``length`` are the
    byte range of the malformed line within ``changes.jsonl`` — authoritative
    for computing an ack watermark. ``raw`` is a best-effort text repr
    (``errors="replace"`` on decode) for human inspection only; it may not
    byte-round-trip for lines that failed to decode as UTF-8 at all, which
    is exactly why ``length`` is stored separately rather than derived from it.
    """
    if not consumer_id or "/" in consumer_id or consumer_id.startswith("."):
        raise ValueError(f"invalid consumer_id: {consumer_id!r}")
    base = root if root is not None else _cursor_root()
    return base / f"{consumer_id}.quarantine.jsonl"


def quarantine_ack_path(consumer_id: str, root: Path | None = None) -> Path:
    """Return the quarantine-ack watermark path for ``consumer_id`` (does not create)."""
    if not consumer_id or "/" in consumer_id or consumer_id.startswith("."):
        raise ValueError(f"invalid consumer_id: {consumer_id!r}")
    base = root if root is not None else _cursor_root()
    return base / f"{consumer_id}.quarantine-ack"


def load_quarantine_ack(consumer_id: str, root: Path | None = None) -> int:
    """Read the acknowledged-through byte offset; 0 if absent or malformed.

    A malformed line whose start-offset is < this watermark is treated as
    already reviewed by the operator: it is not re-quarantined (no duplicate
    ledger entries across repeated sweeps of a stalled offset) and the
    cursor is allowed to advance past it.
    """
    p = quarantine_ack_path(consumer_id, root)
    try:
        raw = p.read_text(encoding="utf-8").strip()
        return int(raw)
    except (FileNotFoundError, ValueError, OSError):
        return 0


def save_quarantine_ack(consumer_id: str, upto_offset: int, root: Path | None = None) -> None:
    """Atomically persist the acknowledged-through offset (temp-file + rename).

    THE sanctioned way to un-stall a consumer whose cursor has stopped at an
    unacknowledged quarantined line (see ``watcher.py``). Also reachable via
    ``agent-rally-watcher ack-quarantine --consumer <id>`` (cli.py), which
    defaults ``upto_offset`` to "everything currently on the ledger".
    """
    if upto_offset < 0:
        raise ValueError(f"upto_offset must be >= 0, got {upto_offset}")
    p = quarantine_ack_path(consumer_id, root)
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_suffix(p.suffix + ".tmp")
    tmp.write_text(f"{upto_offset}\n", encoding="utf-8")
    os.replace(tmp, p)
