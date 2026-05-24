# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Tests for scripts/app_pulse/presence.py — presence, reaper, cursor.

  - presence schema + overwrite-in-place
  - reap_stale drops files older than heartbeat_minutes (default 15,
    config-overridable via apps/<slug>/config.json — OQ2)
  - read_active_presence excludes self + reaped
  - cursor get/set round-trips
  - graceful absence
"""
from __future__ import annotations

import json
import subprocess
import sys
import time
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve().parent
if str(_HERE) not in sys.path:
    sys.path.insert(0, str(_HERE))

import presence as pr  # noqa: E402


@pytest.fixture()
def chan(tmp_path: Path) -> Path:
    d = tmp_path / "chan"
    d.mkdir()
    return d


def test_write_and_schema(chan: Path):
    pr.write_presence(
        chan, session_id="s1", tool="claude", model="opus", run_id="r1",
        app_slug="a", phase="execute", files_in_flight=["x.py"],
    )
    f = chan / "sessions" / "s1.json"
    rec = json.loads(f.read_text())
    assert set(rec) >= {
        "session_id", "tool", "model", "run_id", "app_slug", "phase",
        "files_in_flight", "heartbeat_ts", "cursor",
    }
    assert rec["cursor"] == {"revision": 0, "changes_offset": 0}
    # overwrite-in-place
    pr.write_presence(chan, session_id="s1", tool="claude", model="opus",
                      run_id="r1", app_slug="a", phase="review",
                      files_in_flight=[])
    assert json.loads(f.read_text())["phase"] == "review"
    assert len(list((chan / "sessions").glob("*.json"))) == 1


def test_read_active_excludes_self(chan: Path):
    pr.write_presence(chan, session_id="s1", tool="t", model="m",
                      run_id="r1", app_slug="a", phase="p")
    pr.write_presence(chan, session_id="s2", tool="t", model="m",
                      run_id="r2", app_slug="a", phase="p")
    peers = pr.read_active_presence(chan, exclude_session="s1")
    assert [p["session_id"] for p in peers] == ["s2"]


def test_reap_stale(chan: Path):
    pr.write_presence(chan, session_id="old", tool="t", model="m",
                      run_id="r", app_slug="a", phase="p")
    f = chan / "sessions" / "old.json"
    rec = json.loads(f.read_text())
    rec["heartbeat_ts"] = time.time() - 16 * 60  # 16 min ago > default 15
    f.write_text(json.dumps(rec))
    pr.write_presence(chan, session_id="fresh", tool="t", model="m",
                      run_id="r2", app_slug="a", phase="p")
    reaped = pr.reap_stale(chan)
    assert "old" in reaped and not f.exists()
    assert [p["session_id"]
            for p in pr.read_active_presence(chan, exclude_session="x")] \
        == ["fresh"]


def test_reap_respects_config_override(chan: Path):
    (chan / "config.json").write_text(json.dumps({"heartbeat_minutes": 1}))
    pr.write_presence(chan, session_id="s", tool="t", model="m",
                      run_id="r", app_slug="a", phase="p")
    f = chan / "sessions" / "s.json"
    rec = json.loads(f.read_text())
    rec["heartbeat_ts"] = time.time() - 2 * 60  # 2 min ago > 1 min override
    f.write_text(json.dumps(rec))
    assert "s" in pr.reap_stale(chan)


def test_cursor_round_trip(chan: Path):
    pr.write_presence(chan, session_id="s", tool="t", model="m",
                      run_id="r", app_slug="a", phase="p")
    assert pr.get_cursor(chan, "s") == {"revision": 0, "changes_offset": 0}
    pr.set_cursor(chan, "s", revision=7, changes_offset=128)
    assert pr.get_cursor(chan, "s") == {"revision": 7, "changes_offset": 128}
    # other presence fields preserved across cursor write
    assert json.loads((chan / "sessions" / "s.json").read_text())["phase"] \
        == "p"


def test_graceful_absence(chan: Path):
    assert pr.read_active_presence(chan / "nope", exclude_session="x") == []
    assert pr.reap_stale(chan / "nope") == []
    assert pr.get_cursor(chan / "nope", "s") == {
        "revision": 0, "changes_offset": 0,
    }


# ---------------------------------------------------------------------------
# Branch merge-status fields (2026-05-19 — peer-merged gate)
# ---------------------------------------------------------------------------


def _git(cwd: Path, *args: str) -> None:
    """Run git with a hardcoded committer identity; raise on failure."""
    env = {
        "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@x",
        "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@x",
        "PATH": "/usr/bin:/bin:/usr/local/bin",
    }
    subprocess.run(
        ["git", "-C", str(cwd), *args],
        capture_output=True, text=True, timeout=5, env=env, check=True,
    )


def _make_repo(root: Path) -> Path:
    root.mkdir(parents=True, exist_ok=True)
    _git(root, "init", "-q", "-b", "main")
    (root / "a.txt").write_text("a")
    _git(root, "add", ".")
    _git(root, "commit", "-q", "-m", "init")
    return root


def test_branch_merge_status_merged(chan: Path, tmp_path: Path):
    """HEAD is an ancestor of main -> 'merged'."""
    repo = _make_repo(tmp_path / "repo")
    pr.write_presence(
        chan, session_id="s", tool="t", model="m", run_id="r",
        app_slug="a", phase="p", cwd=repo,
    )
    rec = json.loads((chan / "sessions" / "s.json").read_text())
    assert rec["branch_merge_status"] == "merged"
    assert rec["branch_name"] == "main"
    assert rec["branch_head_sha"] != "unknown"
    assert isinstance(rec["branch_merge_status_checked_ts"], (int, float))


def test_branch_merge_status_unmerged(chan: Path, tmp_path: Path):
    """A feature branch ahead of main -> 'unmerged'."""
    repo = _make_repo(tmp_path / "repo")
    _git(repo, "checkout", "-q", "-b", "feat")
    (repo / "b.txt").write_text("b")
    _git(repo, "add", ".")
    _git(repo, "commit", "-q", "-m", "wip")
    pr.write_presence(
        chan, session_id="s", tool="t", model="m", run_id="r",
        app_slug="a", phase="p", cwd=repo,
    )
    rec = json.loads((chan / "sessions" / "s.json").read_text())
    assert rec["branch_merge_status"] == "unmerged"
    assert rec["branch_name"] == "feat"


def test_branch_merge_status_unknown_non_git(chan: Path, tmp_path: Path):
    """Non-git directory -> all 'unknown', no raise."""
    plain = tmp_path / "plain"
    plain.mkdir()
    pr.write_presence(
        chan, session_id="s", tool="t", model="m", run_id="r",
        app_slug="a", phase="p", cwd=plain,
    )
    rec = json.loads((chan / "sessions" / "s.json").read_text())
    assert rec["branch_merge_status"] == "unknown"
    assert rec["branch_name"] == "unknown"
    assert rec["branch_head_sha"] == "unknown"


# -- alpha-7: run_refresh_loop ---------------------------------------------


def test_refresh_loop_full_envelope_per_tick(chan: Path):
    """Every tick writes the FULL envelope, not just heartbeat_ts."""
    phases = iter(["execute", "review", "iterate"])
    files = iter([["a.py"], ["b.py"], ["c.py"]])

    pr.run_refresh_loop(
        chan,
        session_id="loop-1",
        tool="claude_code", model="opus", run_id="r", app_slug="myapp",
        phase_provider=lambda: next(phases),
        files_provider=lambda: next(files),
        interval=0,  # no sleep
        max_iterations=3,
        sleep_fn=lambda _x: None,
    )

    # After 3 ticks, the LAST envelope wrote phase="iterate" + ["c.py"]
    rec = json.loads((chan / "sessions" / "loop-1.json").read_text())
    assert rec["phase"] == "iterate"
    assert rec["files_in_flight"] == ["c.py"]
    assert rec["session_id"] == "loop-1"
    assert rec["tool"] == "claude_code"


def test_refresh_loop_exits_when_parent_pid_is_dead(chan: Path):
    """parent_pid pointing at a dead process stops the loop cleanly."""
    # PID 99999 is overwhelmingly likely to be dead/non-existent.
    DEAD_PID = 99999
    ticks = pr.run_refresh_loop(
        chan,
        session_id="dead-parent", tool="t", model="m", run_id="r",
        app_slug="a",
        phase_provider=lambda: "x",
        files_provider=lambda: [],
        interval=0,
        parent_pid=DEAD_PID,
        max_iterations=10,
        sleep_fn=lambda _x: None,
    )
    # Exits before any tick because parent is checked first.
    assert ticks == 0
    # No presence file written.
    assert not (chan / "sessions" / "dead-parent.json").exists()


def test_refresh_loop_continues_while_parent_alive(chan: Path):
    """Real, alive parent (this test process) → loop ticks normally."""
    import os as _os
    own_pid = _os.getpid()
    ticks = pr.run_refresh_loop(
        chan,
        session_id="alive-parent", tool="t", model="m", run_id="r",
        app_slug="a",
        phase_provider=lambda: "x",
        files_provider=lambda: [],
        interval=0,
        parent_pid=own_pid,
        max_iterations=3,
        sleep_fn=lambda _x: None,
    )
    assert ticks == 3
    assert (chan / "sessions" / "alive-parent.json").exists()


def test_refresh_loop_swallows_provider_exceptions(chan: Path):
    """A misbehaving provider doesn't crash the loop."""
    def bad_phase():
        raise RuntimeError("boom")
    ticks = pr.run_refresh_loop(
        chan,
        session_id="bad-provider", tool="t", model="m", run_id="r",
        app_slug="a",
        phase_provider=bad_phase,  # raises every tick
        files_provider=lambda: [],
        interval=0,
        max_iterations=2,
        sleep_fn=lambda _x: None,
    )
    assert ticks == 2  # exception was swallowed, defaults used
    rec = json.loads((chan / "sessions" / "bad-provider.json").read_text())
    assert rec["phase"] == "running"  # the fallback default
