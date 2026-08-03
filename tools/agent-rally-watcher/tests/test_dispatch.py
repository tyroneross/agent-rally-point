# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Dispatch sinks: file appends JSONL; unknown type returns False; http stub.

ARP-007 adversarial coverage lives at the bottom of this file: sink-path
escape/symlink rejection (file sink) and AppleScript-injection neutralization
(notify sink). ``tests/conftest.py``'s autouse fixture points
``AGENT_RALLY_WATCHER_SINK_ROOT`` at each test's own ``tmp_path``, so the
plain happy-path tests above need no changes despite the new containment
check.
"""
from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

import pytest

from agent_rally_watcher.dispatch import dispatch

REC = {"kind": "feedback", "tool": "codex", "payload": {"verdict": "PASS"}, "run_id": "r1"}


def test_file_sink_appends_jsonl(tmp_path: Path) -> None:
    out = tmp_path / "stream.jsonl"
    result = dispatch(REC, {"type": "file", "path": str(out)})
    assert result.delivered
    assert result.sink_type == "file"
    line = out.read_text(encoding="utf-8").strip()
    assert json.loads(line) == REC


def test_file_sink_two_records_two_lines(tmp_path: Path) -> None:
    out = tmp_path / "stream.jsonl"
    dispatch(REC, {"type": "file", "path": str(out)})
    dispatch({**REC, "run_id": "r2"}, {"type": "file", "path": str(out)})
    lines = out.read_text(encoding="utf-8").strip().split("\n")
    assert len(lines) == 2


def test_file_sink_missing_path() -> None:
    result = dispatch(REC, {"type": "file"})
    assert not result.delivered
    assert "missing" in result.detail


def test_unknown_sink_type_returns_false() -> None:
    result = dispatch(REC, {"type": "carrier-pigeon"})
    assert not result.delivered
    assert "unknown" in result.detail


def test_http_sink_is_stubbed() -> None:
    # v0.1 stub returns delivered=False so callers see the gap, not a silent drop
    result = dispatch(REC, {"type": "http", "url": "https://example.test/hook"})
    assert not result.delivered
    assert "stubbed" in result.detail


def test_file_sink_creates_parent_dirs(tmp_path: Path) -> None:
    out = tmp_path / "deep" / "nested" / "stream.jsonl"
    result = dispatch(REC, {"type": "file", "path": str(out)})
    assert result.delivered
    assert out.exists()


# ===========================================================================
# ARP-007 adversarial controls
# ===========================================================================


def test_file_sink_rejects_path_outside_root(tmp_path: Path, tmp_path_factory) -> None:
    """A configured sink path outside AGENT_RALLY_WATCHER_SINK_ROOT is REJECTED."""
    outside_root = tmp_path_factory.mktemp("outside-sink-root")
    escape_target = outside_root / "hostile.jsonl"
    result = dispatch(REC, {"type": "file", "path": str(escape_target)})
    assert not result.delivered
    assert "escapes allowed root" in result.detail
    assert not escape_target.exists()


def test_file_sink_rejects_relative_path_traversal(tmp_path: Path, monkeypatch) -> None:
    """A relative path with '../' segments that would land outside the root is REJECTED.

    The traversal is anchored inside a tmpdir rather than written as a bare
    '../../etc/...'. That earlier form resolved against the CWD, so a run with the
    fix reverted — a mutation check, exactly what this test exists to survive —
    wrote a real file into the repo at etc/hostile.jsonl. A test that litters the
    working tree when it fails is a test you stop trusting.
    """
    monkeypatch.chdir(tmp_path)
    (tmp_path / "a" / "b").mkdir(parents=True)
    monkeypatch.chdir(tmp_path / "a" / "b")
    result = dispatch(REC, {"type": "file", "path": "../../escaped/hostile.jsonl"})
    assert not result.delivered
    assert "escapes allowed root" in result.detail
    assert not (tmp_path / "escaped" / "hostile.jsonl").exists()


def test_file_sink_rejects_symlink_leaf(tmp_path: Path, tmp_path_factory) -> None:
    """A configured sink path that is itself a symlink to outside the root is REJECTED,
    and the OUTSIDE target is never written to (no write-through-symlink)."""
    outside_root = tmp_path_factory.mktemp("outside-sink-root-2")
    outside_target = outside_root / "victim.jsonl"
    link_path = tmp_path / "looks-allowed.jsonl"
    link_path.symlink_to(outside_target)  # target need not exist yet

    result = dispatch(REC, {"type": "file", "path": str(link_path)})
    assert not result.delivered
    assert "symlink" in result.detail
    assert not outside_target.exists()


def test_notify_argv_separation_no_shell_injection(monkeypatch: pytest.MonkeyPatch) -> None:
    """The AppleScript injection payload is passed as argv DATA, never interpolated
    into the script source — verified at the subprocess-call boundary."""
    captured: dict[str, list[str]] = {}

    def fake_run(cmd, **kwargs):
        captured["cmd"] = cmd
        return subprocess.CompletedProcess(cmd, 0)

    monkeypatch.setattr(subprocess, "run", fake_run)

    injection_payload = '" & do shell script "touch /tmp/pwned_arp007_test" & "'
    rec = {"kind": "feedback", "run_id": "r1", "payload": {"note": injection_payload}}
    result = dispatch(rec, {"type": "notify", "title": 'evil"title\\', "body_field": "note"})

    assert result.delivered
    cmd = captured["cmd"]
    assert cmd[0] == "osascript"
    # The script SOURCE text (cmd[2]) must be the fixed template — argv items
    # (cmd[3], cmd[4]) carry the untrusted content, unmodified and un-escaped.
    assert "on run argv" in cmd[2]
    assert injection_payload not in cmd[2]
    assert cmd[3] == 'evil"title\\'
    assert cmd[4] == injection_payload


@pytest.mark.skipif(
    sys.platform != "darwin",
    reason="osascript is macOS-only; skip live-injection control off Darwin",
)
def test_notify_live_injection_attempt_does_not_execute_shell_command(tmp_path: Path) -> None:
    """End-to-end adversarial control: a real osascript invocation with a
    script-breakout payload must not execute the embedded shell command.
    Uses a harmless marker file under a fixed, unique tmp path — asserts it
    does NOT appear after dispatch runs."""
    marker = tmp_path / "pwned_marker_live"
    assert not marker.exists()
    injection_payload = f'" & do shell script "touch {marker}" & "'
    rec = {"kind": "feedback", "run_id": "r1", "payload": {"note": injection_payload}}

    result = dispatch(rec, {"type": "notify", "title": "Rally Watcher", "body_field": "note"})

    # Fire-and-forget: whether this specific sandbox can actually SHOW a
    # notification (headless CI / no active GUI session commonly makes
    # `display notification` itself hang until the 5s timeout, observed on
    # this very host) is not the property under test and is intentionally
    # NOT asserted — `result.delivered` may legitimately be False here via
    # a timeout, same as it would be on a real Mac with no display. The
    # security property is narrower and unconditional regardless of
    # delivery outcome: the embedded shell command must never run.
    assert not marker.exists()
