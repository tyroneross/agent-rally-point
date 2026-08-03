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

ARP-007 (2026-08 third-party security audit, GitHub issue #52) hardening:

  file sink — the configured ``path`` is consumer config (legitimate power
  to choose where events land), but a surprising or hostile config value
  must not be able to escape a bounded root or write through a symlink.
  ``AGENT_RALLY_WATCHER_SINK_ROOT`` (default ``~/.agent-rally-watcher``,
  mirrors ``cursor.py``'s ``AGENT_RALLY_WATCHER_CURSOR_ROOT``) is the
  allowed root — it is an environment/site setting, deliberately NOT read
  from the sink dict itself, so a config author cannot loosen their own
  containment. A relative ``path`` is resolved against the root (not the
  daemon's cwd). The resolved real path (symlinks in existing ancestors
  dereferenced) must stay under the root; the leaf itself is rejected
  outright if it is already a symlink (no-follow), and the actual write
  uses ``O_NOFOLLOW`` at the OS level as a TOCTOU backstop.

  notify sink — the notification body/title used to be interpolated
  directly into AppleScript source text (minimal quote-replacement only).
  Now passed as ``argv`` to an ``on run argv`` script, so the payload is
  always DATA to AppleScript, never source text — quotes, backslashes, and
  script-breakout attempts (``" & do shell script "..." & "``) are inert.
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

DEFAULT_SINK_ROOT = "~/.agent-rally-watcher"


def _sink_root() -> Path:
    """Resolve the allowed root for file-sink paths (env override, else default).

    Deliberately env-var only — never read from the sink config dict, so a
    hostile/misconfigured consumer entry cannot widen its own containment.
    """
    raw = os.environ.get("AGENT_RALLY_WATCHER_SINK_ROOT") or DEFAULT_SINK_ROOT
    return Path(os.path.expanduser(raw)).resolve()


def _resolve_sink_path(raw_path: str) -> tuple[Path | None, str]:
    """Validate + resolve a configured file-sink path against the allowed root.

    Returns ``(resolved_path, "")`` on success, or ``(None, reason)`` on
    rejection. Config power to pick any path UNDER the root is legitimate;
    this only stops escape (``..``, absolute paths outside root) and
    symlink redirection (the leaf itself must not be a symlink — checked
    here as defense-in-depth; ``_dispatch_file`` also opens with
    ``O_NOFOLLOW`` to close the TOCTOU window between this check and the
    write).
    """
    root = _sink_root()
    candidate = Path(os.path.expanduser(raw_path))
    if not candidate.is_absolute():
        # Relative paths are root-relative, NOT cwd-relative — the daemon's
        # cwd is not a security boundary and should never be part of this
        # decision.
        candidate = root / candidate
    if candidate.is_symlink():
        return None, f"refusing to write through a symlink: {candidate}"
    resolved = candidate.resolve(strict=False)
    try:
        resolved.relative_to(root)
    except ValueError:
        return None, f"sink path escapes allowed root {root}: {resolved}"
    return resolved, ""


@dataclass
class DispatchResult:
    delivered: bool
    sink_type: str
    detail: str = ""


def _dispatch_file(record: dict[str, Any], sink: dict[str, Any]) -> DispatchResult:
    raw_path = sink.get("path")
    if not raw_path:
        return DispatchResult(False, "file", "missing 'path'")
    resolved, reason = _resolve_sink_path(str(raw_path))
    if resolved is None:
        logger.warning("file sink rejected: %s", reason)
        return DispatchResult(False, "file", reason)
    try:
        resolved.parent.mkdir(parents=True, exist_ok=True)
        line = json.dumps(record, separators=(",", ":")) + "\n"
        data = line.encode("utf-8")
        flags = os.O_WRONLY | os.O_CREAT | os.O_APPEND
        # O_NOFOLLOW: TOCTOU backstop — if a symlink appears at the leaf
        # between _resolve_sink_path's check and this open, the OS refuses
        # (ELOOP) instead of writing through it. Not available on Windows;
        # this project targets macOS/Linux (see pyproject.toml classifiers).
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        fd = os.open(resolved, flags, 0o644)
        try:
            os.write(fd, data)
        finally:
            os.close(fd)
        return DispatchResult(True, "file", str(resolved))
    except OSError as e:
        return DispatchResult(False, "file", f"{type(e).__name__}: {e}")


_NOTIFY_APPLESCRIPT = """\
on run argv
  display notification (item 2 of argv) with title (item 1 of argv)
end run
"""


def _dispatch_notify(record: dict[str, Any], sink: dict[str, Any]) -> DispatchResult:
    title = str(sink.get("title") or "Rally Watcher")
    body_field = sink.get("body_field")
    payload = record.get("payload") or {}
    if body_field and body_field in payload:
        body = str(payload[body_field])
    else:
        # Default body: kind + run_id, e.g. "feedback :: run-001"
        body = f"{record.get('kind', 'event')} :: {record.get('run_id', 'unknown')}"
    # ARP-007: title/body are passed as `argv` to an `on run argv` script,
    # never interpolated into the script SOURCE. AppleScript treats argv
    # items as literal data — quotes, backslashes, and script-breakout
    # attempts (e.g. `" & do shell script "..." & "`) are inert. No
    # escaping needed or attempted; the separation is structural, not
    # string-sanitization-based.
    try:
        subprocess.run(
            ["osascript", "-e", _NOTIFY_APPLESCRIPT, title, body],
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
