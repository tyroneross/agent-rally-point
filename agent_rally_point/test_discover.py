# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Tests for agent_rally_point/discover.py — discovery layer.

Coverage:
  - manifest absent → auto-creates, installed=true
  - global manifest only → discovery returns canonical layout
  - repo-level overlay → fields override global on per-field basis
  - legacy fallback → canonical absent + legacy exists → channel_layout="legacy"
  - active_peers populated → read from sessions/
  - --quiet exit codes (0 installed, 1 not)
  - --field bare value vs JSON
  - cli entrypoint smokes via python3 -m
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve().parent
if str(_HERE) not in sys.path:
    sys.path.insert(0, str(_HERE))


@pytest.fixture
def isolated_home(tmp_path, monkeypatch):
    """Point HOME at tmp_path so manifest auto-create doesn't touch real home."""
    home = tmp_path / "home"
    home.mkdir()
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.delenv("BUILD_LOOP_APPS_ROOT", raising=False)
    yield home


@pytest.fixture
def fresh_discover(isolated_home, monkeypatch):
    """Reload the discover module so it picks up the patched HOME."""
    # Force a fresh import each call so any module-level path caches re-init.
    if "agent_rally_point.discover" in sys.modules:
        del sys.modules["agent_rally_point.discover"]
    import agent_rally_point.discover as d
    return d


def _init_git_repo(path: Path) -> None:
    subprocess.run(["git", "init", "-q", str(path)], check=True)
    subprocess.run(
        ["git", "-C", str(path), "config", "user.email", "t@t.test"], check=True
    )
    subprocess.run(["git", "-C", str(path), "config", "user.name", "t"], check=True)
    (path / "README.md").write_text("test repo\n")
    subprocess.run(["git", "-C", str(path), "add", "-A"], check=True)
    subprocess.run(
        ["git", "-C", str(path), "commit", "-q", "-m", "init"], check=True
    )


def test_manifest_auto_creates_when_absent(fresh_discover, isolated_home, tmp_path):
    repo = tmp_path / "myproj"
    repo.mkdir()
    _init_git_repo(repo)

    manifest = isolated_home / ".agent-rally-point" / "manifest.toml"
    assert not manifest.exists()

    info = fresh_discover.discover(cwd=repo)

    assert manifest.exists(), "global manifest should auto-create on first discover"
    assert info["installed"] is True
    assert info["schema_version"] == "1.0"
    text = manifest.read_text()
    assert "schema_version" in text
    assert "agent-rally-point" in text


def test_discover_returns_canonical_layout_for_git_repo(fresh_discover, isolated_home, tmp_path):
    repo = tmp_path / "myproj"
    repo.mkdir()
    _init_git_repo(repo)

    info = fresh_discover.discover(cwd=repo)

    assert info["app_slug"] == "myproj"
    # Default policy is "migration" → channel_layout="canonical" (write target),
    # channel_dir under ~/.agent-rally-point/apps/<repo_id>/ (not /<slug>/).
    assert info["channel_layout"] == "canonical"
    assert info["repo_id"].startswith("myproj-")
    expected = isolated_home / ".agent-rally-point" / "apps" / info["repo_id"]
    assert Path(info["channel_dir"]) == expected
    assert info["active_revision"] == 0
    assert info["active_peers"] == []
    # Migration-mode extras present
    assert info["merged_view"] is True
    assert info["legacy_channel_dir"] is not None
    assert info["coordination_unavailable"] is False


def test_repo_overlay_overrides_slug(fresh_discover, isolated_home, tmp_path):
    repo = tmp_path / "myproj"
    repo.mkdir()
    _init_git_repo(repo)
    (repo / ".agent-rally.toml").write_text(
        'schema_version = "1.0"\n[channel]\nslug = "custom-slug"\n'
    )

    info = fresh_discover.discover(cwd=repo)
    assert info["app_slug"] == "custom-slug"
    assert info["sources"]["app_slug"] == "repo"


def test_repo_overlay_overrides_apps_root(fresh_discover, isolated_home, tmp_path):
    repo = tmp_path / "myproj"
    repo.mkdir()
    _init_git_repo(repo)
    alt_root = tmp_path / "alt-channels"
    (repo / ".agent-rally.toml").write_text(
        f'schema_version = "1.0"\n[paths]\napps_root = "{alt_root}"\n'
    )

    info = fresh_discover.discover(cwd=repo)
    assert info["apps_root"] == str(alt_root)
    assert info["sources"]["apps_root"] == "repo"


def test_migration_merges_revision_from_both_channels(fresh_discover, isolated_home, tmp_path):
    """Under default (migration) policy, active_revision is max(canonical, legacy)."""
    repo = tmp_path / "myproj"
    repo.mkdir()
    _init_git_repo(repo)
    # Pre-discover to learn repo_id
    info0 = fresh_discover.discover(cwd=repo)
    rid = info0["repo_id"]
    # Create legacy with high revision; canonical absent.
    legacy = isolated_home / ".build-loop" / "apps" / "myproj"
    legacy.mkdir(parents=True)
    (legacy / "revision").write_text("42\n")

    info = fresh_discover.discover(cwd=repo)
    # Migration mode: channel_layout stays "canonical" (write target), but
    # active_revision merges in legacy's value (42 > canonical's 0).
    assert info["channel_layout"] == "canonical"
    assert info["policy"] == "migration"
    assert info["active_revision"] == 42
    assert info["merged_view"] is True
    assert Path(info["legacy_channel_dir"]) == legacy


def test_canonical_revision_wins_when_higher_under_migration(
    fresh_discover, isolated_home, tmp_path
):
    repo = tmp_path / "myproj"
    repo.mkdir()
    _init_git_repo(repo)
    info0 = fresh_discover.discover(cwd=repo)
    rid = info0["repo_id"]
    canonical = isolated_home / ".agent-rally-point" / "apps" / rid
    canonical.mkdir(parents=True)
    (canonical / "revision").write_text("7\n")
    legacy = isolated_home / ".build-loop" / "apps" / "myproj"
    legacy.mkdir(parents=True)
    (legacy / "revision").write_text("3\n")

    info = fresh_discover.discover(cwd=repo)
    assert info["channel_layout"] == "canonical"
    assert info["active_revision"] == 7  # max(7, 3)


def test_active_peers_populated(fresh_discover, isolated_home, tmp_path):
    repo = tmp_path / "myproj"
    repo.mkdir()
    _init_git_repo(repo)
    # Pre-discover to learn repo_id.
    info0 = fresh_discover.discover(cwd=repo)
    rid = info0["repo_id"]
    channel = isolated_home / ".agent-rally-point" / "apps" / rid
    sessions = channel / "sessions"
    sessions.mkdir(parents=True)
    import time
    presence = {
        "session_id": "test-sess-1",
        "tool": "claude_code",
        "model": "claude-opus-4-7",
        "run_id": "test-run",
        "heartbeat_ts": time.time(),
        "branch_name": "main",
        "cwd": str(repo),
    }
    (sessions / "test-sess-1.json").write_text(json.dumps(presence))

    info = fresh_discover.discover(cwd=repo)
    assert len(info["active_peers"]) == 1
    assert info["active_peers"][0]["session_id"] == "test-sess-1"


def test_not_installed_outside_git_with_no_legacy(fresh_discover, isolated_home, tmp_path):
    # cwd not a git repo, no legacy, manifest will auto-create so installed=True.
    # To get installed=false, we'd need to disable manifest creation; the contract
    # says installed reflects "any layer resolved". Manifest auto-creation IS a layer.
    bare = tmp_path / "bare"
    bare.mkdir()
    info = fresh_discover.discover(cwd=bare)
    # Manifest auto-creates → installed=True even outside git.
    assert info["installed"] is True
    # Slug falls back to _unscoped (channel_paths.app_slug behavior).
    assert info["app_slug"] == "_unscoped"


def test_cli_quiet_returns_zero_when_installed(fresh_discover, isolated_home, tmp_path):
    repo = tmp_path / "myproj"
    repo.mkdir()
    _init_git_repo(repo)
    rc = subprocess.run(
        [sys.executable, "-m", "agent_rally_point.discover", "--quiet"],
        cwd=str(repo),
        env={**os.environ, "HOME": str(isolated_home)},
    ).returncode
    assert rc == 0


def test_cli_field_bare_value(fresh_discover, isolated_home, tmp_path):
    repo = tmp_path / "myproj"
    repo.mkdir()
    _init_git_repo(repo)
    out = subprocess.run(
        [
            sys.executable, "-m", "agent_rally_point.discover",
            "--field", "app_slug",
        ],
        cwd=str(repo),
        env={**os.environ, "HOME": str(isolated_home)},
        capture_output=True,
        text=True,
    )
    assert out.returncode == 0
    assert out.stdout.strip() == "myproj"


def test_cli_json_default(fresh_discover, isolated_home, tmp_path):
    repo = tmp_path / "myproj"
    repo.mkdir()
    _init_git_repo(repo)
    out = subprocess.run(
        [sys.executable, "-m", "agent_rally_point.discover"],
        cwd=str(repo),
        env={**os.environ, "HOME": str(isolated_home)},
        capture_output=True,
        text=True,
    )
    assert out.returncode == 0
    info = json.loads(out.stdout)
    assert info["installed"] is True
    assert info["app_slug"] == "myproj"
    assert info["channel_layout"] in ("canonical", "legacy")


def test_cli_field_unknown_exits_one(fresh_discover, isolated_home, tmp_path):
    repo = tmp_path / "myproj"
    repo.mkdir()
    _init_git_repo(repo)
    out = subprocess.run(
        [
            sys.executable, "-m", "agent_rally_point.discover",
            "--field", "nonexistent_field",
        ],
        cwd=str(repo),
        env={**os.environ, "HOME": str(isolated_home)},
        capture_output=True,
        text=True,
    )
    assert out.returncode == 1


# -- v0.3.0 (alpha-2): policy + protocol_version + last_resolved_at + compat table --


def test_manifest_includes_policy_section_with_migration_default(
    fresh_discover, isolated_home, tmp_path
):
    repo = tmp_path / "p"; repo.mkdir(); _init_git_repo(repo)
    fresh_discover.discover(cwd=repo)
    manifest = (isolated_home / ".agent-rally-point" / "manifest.toml").read_text()
    assert "[policy]" in manifest
    assert 'mode = "migration"' in manifest
    assert 'protocol_version = "1.0"' in manifest


def test_discover_envelope_has_new_v030_fields(fresh_discover, isolated_home, tmp_path):
    repo = tmp_path / "p"; repo.mkdir(); _init_git_repo(repo)
    info = fresh_discover.discover(cwd=repo)
    # v0.3.0 envelope additions
    assert info["protocol_version"] == "1.0"
    assert info["policy"] == "migration"  # default
    assert "last_resolved_at" in info
    # ISO8601 UTC shape: YYYY-MM-DDTHH:MM:SSZ
    import re
    assert re.match(
        r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$", info["last_resolved_at"]
    )
    # Manifest auto-creates with [policy] mode="migration" → resolves via global.
    assert info["sources"]["policy"] == "global"


def test_policy_env_override(fresh_discover, isolated_home, tmp_path, monkeypatch):
    repo = tmp_path / "p"; repo.mkdir(); _init_git_repo(repo)
    monkeypatch.setenv("AGENT_RALLY_POLICY", "canonical")
    info = fresh_discover.discover(cwd=repo)
    assert info["policy"] == "canonical"
    assert info["sources"]["policy"] == "env"


def test_policy_invalid_env_falls_through_to_default(
    fresh_discover, isolated_home, tmp_path, monkeypatch
):
    repo = tmp_path / "p"; repo.mkdir(); _init_git_repo(repo)
    monkeypatch.setenv("AGENT_RALLY_POLICY", "garbage-mode")
    info = fresh_discover.discover(cwd=repo)
    # Invalid env value silently falls through; manifest [policy] mode wins.
    assert info["policy"] == "migration"
    assert info["sources"]["policy"] == "global"


def test_policy_repo_overlay_overrides_global(
    fresh_discover, isolated_home, tmp_path
):
    repo = tmp_path / "p"; repo.mkdir(); _init_git_repo(repo)
    (repo / ".agent-rally.toml").write_text(
        'schema_version = "1.0"\n[policy]\nmode = "legacy-only"\n'
    )
    info = fresh_discover.discover(cwd=repo)
    assert info["policy"] == "legacy-only"
    assert info["sources"]["policy"] == "repo"


def test_compatibility_table_auto_materializes(
    fresh_discover, isolated_home, tmp_path
):
    repo = tmp_path / "p"; repo.mkdir(); _init_git_repo(repo)
    compat = isolated_home / ".agent-rally-point" / "compatibility.json"
    assert not compat.exists()
    fresh_discover.discover(cwd=repo)
    assert compat.exists()
    data = json.loads(compat.read_text())
    # Documented shape from coordination-version-control.md
    assert data["protocol_version"] == "1.0"
    assert data["supported_build_loop_range"].startswith(">=0.12.")
    assert isinstance(data["deprecation_notices"], list)
    assert data["agent_rally_point"]  # filled at write time


# -- alpha-3: policy-aware channel resolution + loud coordination_unavailable --


def test_canonical_policy_uses_repo_id_path(
    fresh_discover, isolated_home, tmp_path, monkeypatch
):
    repo = tmp_path / "myproj"; repo.mkdir(); _init_git_repo(repo)
    monkeypatch.setenv("AGENT_RALLY_POLICY", "canonical")
    info = fresh_discover.discover(cwd=repo)
    assert info["policy"] == "canonical"
    assert info["channel_layout"] == "canonical"
    # channel_dir is under canonical apps root keyed by repo_id, not slug.
    assert "/.agent-rally-point/apps/" in info["channel_dir"]
    assert info["repo_id"] in info["channel_dir"]
    # No migration extras under canonical policy.
    assert "merged_view" not in info or info.get("merged_view") is False
    # legacy_channel_dir NOT present (single channel under canonical).
    assert "legacy_channel_dir" not in info


def test_legacy_only_policy_resolves_legacy_path(
    fresh_discover, isolated_home, tmp_path, monkeypatch
):
    repo = tmp_path / "myproj"; repo.mkdir(); _init_git_repo(repo)
    monkeypatch.setenv("AGENT_RALLY_POLICY", "legacy-only")
    legacy = isolated_home / ".build-loop" / "apps" / "myproj"
    legacy.mkdir(parents=True)
    (legacy / "revision").write_text("11\n")
    info = fresh_discover.discover(cwd=repo)
    assert info["policy"] == "legacy-only"
    assert info["channel_layout"] == "legacy"
    assert Path(info["channel_dir"]) == legacy
    assert info["active_revision"] == 11
    assert info["sources"]["channel_dir"] == "legacy-only"


def test_canonical_policy_no_silent_legacy_fallback(
    fresh_discover, isolated_home, tmp_path, monkeypatch
):
    """Under canonical policy, legacy data MUST NOT be served as a fallback.

    This is the central hard rule from coordination-substrate-canonical.md:
    coordination_unavailable is loud-and-degraded, never silent.
    """
    repo = tmp_path / "myproj"; repo.mkdir(); _init_git_repo(repo)
    # Create a populated legacy channel — under canonical policy it must be
    # ignored entirely.
    legacy = isolated_home / ".build-loop" / "apps" / "myproj"
    legacy.mkdir(parents=True)
    (legacy / "revision").write_text("999\n")

    monkeypatch.setenv("AGENT_RALLY_POLICY", "canonical")
    info = fresh_discover.discover(cwd=repo)
    assert info["channel_layout"] == "canonical"
    # channel_dir is canonical (under .agent-rally-point), NOT legacy.
    assert ".agent-rally-point/apps/" in info["channel_dir"]
    assert ".build-loop/apps/" not in info["channel_dir"]
    # Legacy's revision (999) is NOT served.
    assert info["active_revision"] == 0


def test_canonical_unreadable_surfaces_coordination_unavailable(
    fresh_discover, isolated_home, tmp_path, monkeypatch
):
    """Canonical exists but is unreadable → coordination_unavailable=True, no fallback."""
    repo = tmp_path / "myproj"; repo.mkdir(); _init_git_repo(repo)
    # Manually create a canonical channel and make it unreadable.
    # (Pre-discover to learn the repo_id, then chmod the dir.)
    monkeypatch.setenv("AGENT_RALLY_POLICY", "canonical")
    info0 = fresh_discover.discover(cwd=repo)
    rid = info0["repo_id"]
    canonical = isolated_home / ".agent-rally-point" / "apps" / rid
    canonical.mkdir(parents=True, exist_ok=True)
    try:
        os.chmod(canonical, 0o000)
        info = fresh_discover.discover(cwd=repo)
        assert info["coordination_unavailable"] is True
        assert info["coordination_unavailable_reason"] == "canonical_unreadable"
        # Did NOT silently fall back to legacy.
        assert info["channel_layout"] == "canonical"
    finally:
        # Restore so pytest can clean up the tmp tree.
        os.chmod(canonical, 0o755)


def test_migration_unions_peers_from_both_channels(
    fresh_discover, isolated_home, tmp_path
):
    """Under migration policy, active_peers = union(canonical, legacy) deduped by session_id."""
    repo = tmp_path / "myproj"; repo.mkdir(); _init_git_repo(repo)
    info0 = fresh_discover.discover(cwd=repo)
    rid = info0["repo_id"]
    # canonical session
    canonical = isolated_home / ".agent-rally-point" / "apps" / rid / "sessions"
    canonical.mkdir(parents=True)
    import time as _t
    (canonical / "sess-canonical.json").write_text(json.dumps({
        "session_id": "sess-canonical", "tool": "claude_code", "model": "m",
        "run_id": "r", "heartbeat_ts": _t.time(),
    }))
    # legacy session
    legacy = isolated_home / ".build-loop" / "apps" / "myproj" / "sessions"
    legacy.mkdir(parents=True)
    (legacy / "sess-legacy.json").write_text(json.dumps({
        "session_id": "sess-legacy", "tool": "codex", "model": "m",
        "run_id": "r", "heartbeat_ts": _t.time(),
    }))

    info = fresh_discover.discover(cwd=repo)
    sids = {p["session_id"] for p in info["active_peers"]}
    assert sids == {"sess-canonical", "sess-legacy"}


def test_migration_envelope_has_both_paths_and_merged_view(
    fresh_discover, isolated_home, tmp_path
):
    repo = tmp_path / "myproj"; repo.mkdir(); _init_git_repo(repo)
    info = fresh_discover.discover(cwd=repo)
    assert info["policy"] == "migration"
    assert "canonical_channel_dir" in info
    assert "legacy_channel_dir" in info
    assert info["merged_view"] is True
    assert info["sources"]["channel_dir"] == "migration-dual"
    # Both paths point to the right places.
    assert ".agent-rally-point/apps/" in info["canonical_channel_dir"]
    assert ".build-loop/apps/" in info["legacy_channel_dir"]


def test_envelope_protocol_version_is_one_zero(
    fresh_discover, isolated_home, tmp_path
):
    repo = tmp_path / "p"; repo.mkdir(); _init_git_repo(repo)
    info = fresh_discover.discover(cwd=repo)
    assert info["protocol_version"] == "1.0"
    assert info["repo_id"]  # non-empty
