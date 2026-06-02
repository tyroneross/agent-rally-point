# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Per-consumer cursor persistence.

Each consumer has a cursor file at ``~/.agent-rally-watcher/consumers/<id>.cursor``
storing the byte offset into ``changes.jsonl`` it has already consumed. Restart-safe.

Format: single line, ASCII integer + newline. Atomic write via temp-file + rename.
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
