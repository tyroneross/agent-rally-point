# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
# build-loop@tyroneross:canary:agent-rally-watcher
# canary-end
"""Agent Rally Watcher — push-based daemon companion to agent-rally-point.

Tails ``~/.agent-rally-point/apps/<slug>/changes.jsonl`` via watchfiles, applies
per-consumer filters, dispatches matched records to per-consumer sinks (file,
macOS notify, HTTP stubbed). Restart-safe via per-consumer cursors.
"""
from __future__ import annotations

__version__ = "0.1.1"

from .cursor import Cursor, load_cursor, save_cursor
from .filter import FilterRule, load_consumers, match
from .dispatch import Dispatcher, dispatch
from .watcher import Watcher, run_watcher

__all__ = [
    "__version__",
    "Cursor",
    "load_cursor",
    "save_cursor",
    "FilterRule",
    "load_consumers",
    "match",
    "Dispatcher",
    "dispatch",
    "Watcher",
    "run_watcher",
]
