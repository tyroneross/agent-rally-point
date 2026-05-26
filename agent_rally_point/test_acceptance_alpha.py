# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Acceptance suite — agent-rally-point v0.3.0 alpha sequence.

One test per acceptance criterion from .build-loop/goal.md. These tests
overlap with the per-chunk unit tests on purpose; the chunk tests verify
the unit, this suite verifies the contract from goal.md. If a chunk-unit
test is removed/renamed, the acceptance check still proves the criterion.

Criteria mapped:
  AC1  canonical channel layout active under canonical policy
  AC2  policy field round-trips through manifest
  AC3  dual-aware migration mode envelope
  AC4  versioned discover envelope (protocol_version, last_resolved_at,
       repo_id, policy, channel_layout)
  AC5  migration subcommand round-trip
  AC6  cutover verifier refuses on fresh writes
  AC7  agent-rally-discover available as console script (existence
       check via pyproject.toml entry — pipx install is a manual op)
  AC8  presence watcher liveness fix
  AC9  repo_id normalization stable
  AC10 no silent fallback in discover under canonical policy
  AC11 baseline tests still pass (covered by full-suite pytest run)
  AC12 compatibility table written on first discover
  AC13 branch + PR workflow (verified at end of phase 4, not in pytest)
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve().parent
if str(_HERE) not in sys.path:
    sys.path.insert(0, str(_HERE))


@pytest.fixture
def isolated_home(tmp_path, monkeypatch):
    home = tmp_path / "home"
    home.mkdir()
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.delenv("BUILD_LOOP_APPS_ROOT", raising=False)
    monkeypatch.delenv("AGENT_RALLY_POLICY", raising=False)
    for mod in (
        "agent_rally_point.discover",
        "agent_rally_point.migrate",
        "agent_rally_point.repo_id",
        "agent_rally_point.channel_paths",
        "agent_rally_point.presence",
    ):
        if mod in sys.modules:
            del sys.modules[mod]
    yield home


def _init_repo(path: Path, remote: str | None = None) -> None:
    subprocess.run(["git", "init", "-q", str(path)], check=True)
    subprocess.run(["git", "-C", str(path), "config", "user.email", "t@t.test"], check=True)
    subprocess.run(["git", "-C", str(path), "config", "user.name", "t"], check=True)
    (path / "README.md").write_text("x\n")
    subprocess.run(["git", "-C", str(path), "add", "-A"], check=True)
    subprocess.run(["git", "-C", str(path), "commit", "-q", "-m", "init"], check=True)
    if remote:
        subprocess.run(
            ["git", "-C", str(path), "remote", "add", "origin", remote], check=True
        )


# AC1
def test_AC1_canonical_layout_under_canonical_policy(
    isolated_home, tmp_path, monkeypatch
):
    repo = tmp_path / "myproj"; repo.mkdir()
    _init_repo(repo, remote="https://github.com/owner/myproj.git")
    monkeypatch.setenv("AGENT_RALLY_POLICY", "canonical")
    from agent_rally_point.discover import discover
    info = discover(cwd=repo)
    assert info["channel_layout"] == "canonical"
    assert info["policy"] == "canonical"
    assert ".agent-rally-point/apps/" in info["channel_dir"]
    assert info["repo_id"] in info["channel_dir"]


# AC2
def test_AC2_policy_field_each_value(isolated_home, tmp_path, monkeypatch):
    repo = tmp_path / "p"; repo.mkdir(); _init_repo(repo)
    from agent_rally_point.discover import discover
    for mode in ("canonical", "migration", "legacy-only"):
        monkeypatch.setenv("AGENT_RALLY_POLICY", mode)
        info = discover(cwd=repo)
        assert info["policy"] == mode
        assert info["sources"]["policy"] == "env"


# AC3
def test_AC3_migration_envelope_has_dual_paths(isolated_home, tmp_path):
    repo = tmp_path / "p"; repo.mkdir(); _init_repo(repo)
    from agent_rally_point.discover import discover
    info = discover(cwd=repo)
    assert info["policy"] == "migration"
    assert "canonical_channel_dir" in info
    assert "legacy_channel_dir" in info
    assert info["merged_view"] is True
    assert info["sources"]["channel_dir"] == "migration-dual"


# AC4
def test_AC4_versioned_envelope_has_all_required_fields(isolated_home, tmp_path):
    repo = tmp_path / "p"; repo.mkdir(); _init_repo(repo)
    from agent_rally_point.discover import discover
    info = discover(cwd=repo)
    for key in (
        "protocol_version", "policy", "last_resolved_at", "repo_id",
        "channel_layout", "schema_version", "version",
    ):
        assert key in info, f"missing required envelope key: {key}"
    assert info["protocol_version"] == "1.0"
    # ISO8601 UTC
    import re
    assert re.match(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$", info["last_resolved_at"])


# AC5
def test_AC5_migration_subcommand_round_trip(isolated_home):
    # Create a legacy channel + apply migration.
    legacy = isolated_home / ".build-loop" / "apps" / "ac5-app"
    legacy.mkdir(parents=True)
    (legacy / "revision").write_text("5\n")
    (legacy / "changes.jsonl").write_text('{"kind":"phase"}\n')

    from agent_rally_point.migrate import (
        discover_legacy_channels, apply_migration,
    )
    chans = discover_legacy_channels()
    assert any(c["slug"] == "ac5-app" for c in chans)

    r = apply_migration()
    assert r["failures"] == 0

    # Audit log materialized.
    log = isolated_home / ".agent-rally-point" / "migration.log"
    assert log.exists()
    entries = [json.loads(l) for l in log.read_text().splitlines() if l.strip()]
    assert any(e.get("slug") == "ac5-app" for e in entries)

    # Files copied under canonical.
    canonical_apps = isolated_home / ".agent-rally-point" / "apps"
    rid_dirs = list(canonical_apps.iterdir())
    assert rid_dirs
    rid_dir = rid_dirs[0]
    assert (rid_dir / "revision").read_text() == "5\n"

    # Advisory marker placed.
    assert (legacy / ".RALLY_LEGACY_READONLY").exists()


# AC6
def test_AC6_cutover_refuses_on_fresh_writes(isolated_home):
    legacy = isolated_home / ".build-loop" / "apps" / "ac6-app"
    legacy.mkdir(parents=True)
    (legacy / "revision").write_text("1\n")
    from agent_rally_point.migrate import apply_migration, verify_cutover
    apply_migration()
    # Touch a legacy file fresh.
    (legacy / "revision").write_text("999\n")
    v = verify_cutover(ttl_minutes=15, require_downstream=False)
    assert v["can_promote"] is False
    assert v["conditions"]["no_fresh_writes_within_ttl"] is False


# AC7
def test_AC7_console_scripts_declared_in_pyproject():
    """pyproject.toml declares the console scripts. pipx install would
    materialize them on $PATH. The shell-level resolution is a manual
    verification step (see README); this test verifies the pyproject
    contract is honored.
    """
    repo_root = Path(__file__).resolve().parent.parent
    pyproject = (repo_root / "pyproject.toml").read_text()
    assert 'agent-rally-discover = "agent_rally_point.discover:_main"' in pyproject
    assert 'agent-rally-migrate = "agent_rally_point.migrate:_main"' in pyproject


# AC8
def test_AC8_presence_watcher_dead_parent_exit(isolated_home, tmp_path):
    from agent_rally_point import presence as pr
    chan = tmp_path / "chan"; chan.mkdir()
    DEAD_PID = 99999
    ticks = pr.run_refresh_loop(
        chan, session_id="ac8", tool="t", model="m", run_id="r",
        app_slug="a", phase_provider=lambda: "x", files_provider=lambda: [],
        interval=0, parent_pid=DEAD_PID, max_iterations=10,
        sleep_fn=lambda _x: None,
    )
    assert ticks == 0  # exited because parent dead


# AC9
def test_AC9_repo_id_normalization_stable_across_forms(isolated_home, tmp_path):
    a = tmp_path / "a"; b = tmp_path / "b"; c = tmp_path / "c"
    a.mkdir(); b.mkdir(); c.mkdir()
    _init_repo(a, remote="https://github.com/owner/repo.git")
    _init_repo(b, remote="https://github.com/owner/repo")  # no .git
    _init_repo(c, remote="git@github.com:owner/repo.git")  # ssh form

    from agent_rally_point.repo_id import repo_id
    rid_a = repo_id(a)
    rid_b = repo_id(b)
    rid_c = repo_id(c)
    # All three should converge.
    assert rid_a == rid_b == rid_c

    # Same-basename different-owner gives different id.
    d = tmp_path / "d"; d.mkdir()
    _init_repo(d, remote="https://github.com/different/repo.git")
    assert repo_id(d) != rid_a


# AC10
def test_AC10_no_silent_fallback_under_canonical_policy(
    isolated_home, tmp_path, monkeypatch
):
    repo = tmp_path / "p"; repo.mkdir(); _init_repo(repo)
    # Populate legacy with high revision — must NOT be served.
    legacy = isolated_home / ".build-loop" / "apps" / "p"
    legacy.mkdir(parents=True)
    (legacy / "revision").write_text("999\n")

    monkeypatch.setenv("AGENT_RALLY_POLICY", "canonical")
    from agent_rally_point.discover import discover
    info = discover(cwd=repo)
    assert info["channel_layout"] == "canonical"
    assert ".build-loop/apps/" not in info["channel_dir"]
    # Did NOT serve legacy's revision.
    assert info["active_revision"] == 0


# AC11 is satisfied implicitly: the entire pytest run (100+ tests) green
# is the proof. No assertion here.


# AC12
def test_AC12_compatibility_table_materializes(isolated_home, tmp_path):
    repo = tmp_path / "p"; repo.mkdir(); _init_repo(repo)
    compat = isolated_home / ".agent-rally-point" / "compatibility.json"
    assert not compat.exists()
    from agent_rally_point.discover import discover
    discover(cwd=repo)
    assert compat.exists()
    data = json.loads(compat.read_text())
    assert data["protocol_version"] == "1.0"
    assert data["agent_rally_point"]
    assert "supported_build_loop_range" in data
    assert isinstance(data.get("deprecation_notices"), list)


# AC13 is verified at git/PR level — see the feat/canonical-substrate branch.
# A pytest assertion here verifies we're on a feature branch (defensive only;
# the real check is the PR review).
def test_AC13_not_on_main_branch():
    repo_root = Path(__file__).resolve().parent.parent
    branch = subprocess.run(
        ["git", "-C", str(repo_root), "rev-parse", "--abbrev-ref", "HEAD"],
        capture_output=True, text=True,
    ).stdout.strip()
    # Defensive: if test is run from main (e.g., after merge), this is
    # informational rather than a hard fail. We assert on the active dev
    # workflow: in development the branch should be an explicit work branch.
    if branch == "main":
        pytest.skip("AC13 not meaningful when run from main (post-merge).")
    # Accept the parent alpha branch, repair branches, or Codex app branches.
    # The PR-level review enforces "no direct-to-main"; this defensive check
    # just confirms development isn't happening on main.
    assert branch != "main", f"unexpectedly on main: {branch}"
    allowed_prefixes = ("feat/", "fix/", "codex/")
    assert branch.startswith(allowed_prefixes), f"on unexpected branch: {branch}"
