# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""macOS launchd plist generator for boot-time autostart.

Writes ``~/Library/LaunchAgents/com.tyroneross.agent-rally-watcher.<slug>.plist``.
Use ``launchctl bootstrap gui/$UID <plist>`` to load.
"""
from __future__ import annotations

import os
import shutil
import sys
from pathlib import Path

PLIST_TEMPLATE = """<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{python}</string>
        <string>-m</string>
        <string>agent_rally_watcher.cli</string>
        <string>start</string>
        <string>--foreground</string>
        <string>--channel-dir</string>
        <string>{channel_dir}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{stdout_path}</string>
    <key>StandardErrorPath</key>
    <string>{stderr_path}</string>
    <key>ProcessType</key>
    <string>Background</string>
</dict>
</plist>
"""


def _launch_agents_dir() -> Path:
    return Path(os.path.expanduser("~/Library/LaunchAgents"))


def _log_dir() -> Path:
    return Path(os.path.expanduser("~/.agent-rally-watcher/logs"))


def render_plist(slug: str, channel_dir: Path) -> str:
    """Return the plist XML for ``slug`` + ``channel_dir``."""
    safe_slug = slug.replace("/", "_")
    label = f"com.tyroneross.agent-rally-watcher.{safe_slug}"
    log_dir = _log_dir()
    return PLIST_TEMPLATE.format(
        label=label,
        python=shutil.which("python3") or sys.executable,
        channel_dir=str(channel_dir),
        stdout_path=str(log_dir / f"launchd-{safe_slug}.out.log"),
        stderr_path=str(log_dir / f"launchd-{safe_slug}.err.log"),
    )


def plist_path_for(slug: str) -> Path:
    safe_slug = slug.replace("/", "_")
    return _launch_agents_dir() / f"com.tyroneross.agent-rally-watcher.{safe_slug}.plist"


def write_plist(slug: str, channel_dir: Path, *, dry_run: bool = False) -> Path | str:
    """Write the plist (or return its body text in ``dry_run``)."""
    body = render_plist(slug, channel_dir)
    if dry_run:
        return body
    target = plist_path_for(slug)
    target.parent.mkdir(parents=True, exist_ok=True)
    _log_dir().mkdir(parents=True, exist_ok=True)
    target.write_text(body, encoding="utf-8")
    return target
