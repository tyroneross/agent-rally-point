# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Dispatch a matched record to a consumer's sink.

v0.1 sinks:
    type: file    + path: <path>                  — append one JSON line
    type: notify  + title + [body_field]          — macOS osascript notify
    type: http    + url                           — STUB: logs warning, no POST

All sinks are fire-and-forget. A sink failure is logged and the dispatch
loop continues to the next record. Returns True iff the record was
successfully delivered.
"""
from __future__ import annotations

import json
import logging
import os
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Any

logger = logging.getLogger(__name__)


@dataclass
class DispatchResult:
    delivered: bool
    sink_type: str
    detail: str = ""


def _dispatch_file(record: dict[str, Any], sink: dict[str, Any]) -> DispatchResult:
    raw_path = sink.get("path")
    if not raw_path:
        return DispatchResult(False, "file", "missing 'path'")
    p = Path(os.path.expanduser(str(raw_path)))
    try:
        p.parent.mkdir(parents=True, exist_ok=True)
        line = json.dumps(record, separators=(",", ":")) + "\n"
        with open(p, "a", encoding="utf-8") as fh:
            fh.write(line)
        return DispatchResult(True, "file", str(p))
    except OSError as e:
        return DispatchResult(False, "file", f"{type(e).__name__}: {e}")


def _dispatch_notify(record: dict[str, Any], sink: dict[str, Any]) -> DispatchResult:
    title = str(sink.get("title") or "Rally Watcher")
    body_field = sink.get("body_field")
    payload = record.get("payload") or {}
    if body_field and body_field in payload:
        body = str(payload[body_field])
    else:
        # Default body: kind + run_id, e.g. "feedback :: run-001"
        body = f"{record.get('kind', 'event')} :: {record.get('run_id', 'unknown')}"
    # Escape AppleScript double-quotes minimally
    safe_title = title.replace('"', "'")
    safe_body = body.replace('"', "'")
    script = f'display notification "{safe_body}" with title "{safe_title}"'
    try:
        subprocess.run(
            ["osascript", "-e", script],
            check=False,
            capture_output=True,
            timeout=5,
        )
        return DispatchResult(True, "notify", title)
    except (OSError, subprocess.SubprocessError) as e:
        return DispatchResult(False, "notify", f"{type(e).__name__}: {e}")


def _dispatch_http(record: dict[str, Any], sink: dict[str, Any]) -> DispatchResult:
    # v0.1 stub. Real implementation lands in v0.2 (urllib stdlib POST + retry).
    url = sink.get("url", "<unset>")
    logger.warning("http sink stubbed (v0.1): would POST to %s — dropped record", url)
    return DispatchResult(False, "http", f"stubbed: {url}")


_SINK_DISPATCHERS = {
    "file": _dispatch_file,
    "notify": _dispatch_notify,
    "http": _dispatch_http,
}


def dispatch(record: dict[str, Any], sink: dict[str, Any]) -> DispatchResult:
    """Route ``record`` to the configured sink. Never raises."""
    sink_type = str(sink.get("type") or "")
    fn = _SINK_DISPATCHERS.get(sink_type)
    if fn is None:
        return DispatchResult(False, sink_type or "unknown", "unknown sink type")
    try:
        return fn(record, sink)
    except Exception as e:  # noqa: BLE001 — fire-and-forget contract
        return DispatchResult(False, sink_type, f"unexpected {type(e).__name__}: {e}")


class Dispatcher:
    """Stateless helper bundling sink config; useful for testing seams."""

    def __init__(self, sink: dict[str, Any]) -> None:
        self.sink = sink

    def send(self, record: dict[str, Any]) -> DispatchResult:
        return dispatch(record, self.sink)
