# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""launchd plist rendering: well-formed XML with expected keys."""
from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest

from agent_rally_watcher.installers.launchd import render_plist, write_plist


def test_render_contains_expected_keys() -> None:
    body = render_plist("demo-repo", Path("/tmp/agent-rally-point/apps/demo-repo"))
    assert "<key>Label</key>" in body
    assert "com.tyroneross.agent-rally-watcher.demo-repo" in body
    assert "<key>RunAtLoad</key>" in body
    assert "<key>KeepAlive</key>" in body
    assert "/tmp/agent-rally-point/apps/demo-repo" in body


def test_slug_with_slash_is_sanitized() -> None:
    body = render_plist("repo/workers", Path("/tmp/x"))
    assert "com.tyroneross.agent-rally-watcher.repo_workers" in body


def test_write_plist_dry_run_returns_body(tmp_path: Path) -> None:
    body = write_plist("demo", Path("/tmp/x"), dry_run=True)
    assert isinstance(body, str)
    assert "<?xml" in body


@pytest.mark.skipif(shutil.which("plutil") is None, reason="plutil only on macOS")
def test_plutil_lint_clean(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """``plutil -lint`` accepts the generated plist."""
    monkeypatch.setenv("HOME", str(tmp_path))  # contain LaunchAgents write
    target = write_plist("demo-repo", Path("/tmp/x"))
    assert isinstance(target, Path)
    result = subprocess.run(
        ["plutil", "-lint", str(target)],
        check=False,
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, result.stderr
