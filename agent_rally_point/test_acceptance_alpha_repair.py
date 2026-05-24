# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Acceptance suite — fix/repo-id-split repair (AC14..AC18).

Repair plan B+A: migration tool produces the SAME canonical name discover()
resolves to. One ID-derivation function. Relink command for previously-
migrated state. Unmatched fallback for ambiguous/no-match cases.

Criteria mapped:
  AC14 canonical_channel_dir resolves to an existing non-empty dir after
       fresh migration (the bug that triggered this PR)
  AC15 relink command renames <slug>-legacy-<hex>/ → <repo_id>/
  AC16 relink is idempotent
  AC17 repo-id split regression — migration dest matches repo_id() for any
       slug whose repo is findable
  AC18 unmatched slug writes to <slug>-unmatched-<hex>/ with marker file
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
    monkeypatch.delenv("AGENT_RALLY_REPO_SEARCH_PATHS", raising=False)
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
    path.mkdir(parents=True, exist_ok=True)
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


def _make_legacy_channel(home: Path, slug: str, files: dict[str, str]) -> Path:
    ch = home / ".build-loop" / "apps" / slug
    ch.mkdir(parents=True)
    for rel, content in files.items():
        p = ch / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(content)
    return ch


# ---------------------------------------------------------------------------
# AC14 — canonical_channel_dir exists after fresh migration
# ---------------------------------------------------------------------------


def test_AC14_canonical_dir_exists_after_fresh_migration(
    isolated_home, tmp_path, monkeypatch
):
    """The bug: discover()'s canonical_channel_dir pointed to an empty path
    while migrated state lived elsewhere. Fix proof: after `apply`, the path
    discover() returns exists and has non-empty contents.
    """
    # Set up a real git repo at a known location.
    repo_root = tmp_path / "search_root" / "build-loop"
    _init_repo(repo_root, remote="https://github.com/tyroneross/build-loop.git")

    # Make a legacy channel for the same slug, populate it.
    legacy = _make_legacy_channel(isolated_home, "build-loop", {
        "revision": "12\n",
        "changes.jsonl": '{"kind":"phase","payload":{"phase":"alpha"}}\n',
    })

    # Point repo-search at the parent of the repo dir.
    monkeypatch.setenv(
        "AGENT_RALLY_REPO_SEARCH_PATHS", str(tmp_path / "search_root")
    )

    # Run migration.
    from agent_rally_point.migrate import apply_migration
    result = apply_migration()
    assert result["failures"] == 0
    outcome = result["outcomes"][0]
    assert outcome["match_status"] == "matched", (
        f"expected matched, got {outcome['match_status']}: {outcome}"
    )

    # Compute what discover() should return.
    from agent_rally_point.repo_id import repo_id
    expected_rid = repo_id(repo_root)
    assert outcome["canonical_repo_id"] == expected_rid, (
        f"migration dest {outcome['canonical_repo_id']} ≠ "
        f"runtime repo_id() {expected_rid}"
    )

    # discover() from the repo's worktree should return the SAME path.
    monkeypatch.setenv("AGENT_RALLY_POLICY", "canonical")
    from agent_rally_point.discover import discover
    info = discover(cwd=repo_root)
    assert info["channel_layout"] == "canonical"
    canonical = Path(info["channel_dir"])
    assert canonical.exists(), (
        f"discover().channel_dir does not exist: {canonical}"
    )
    # And it contains the migrated state.
    assert (canonical / "revision").read_text() == "12\n"
    assert (canonical / "changes.jsonl").exists()


# ---------------------------------------------------------------------------
# AC15 — relink renames legacy/unmatched → canonical
# ---------------------------------------------------------------------------


def test_AC15_relink_renames_legacy_to_canonical(isolated_home, tmp_path):
    """Simulate the on-disk state we're repairing: an existing
    <slug>-unmatched-<hex>/ dir holding state. relink renames it to
    <repo_id>/."""
    # First, run a migration where no repo is findable → unmatched.
    legacy = _make_legacy_channel(isolated_home, "build-loop", {
        "revision": "99\n",
    })
    from agent_rally_point.migrate import apply_migration, relink
    r1 = apply_migration()
    outcome = r1["outcomes"][0]
    assert outcome["match_status"] == "unmatched"
    unmatched_dir = Path(outcome["dest_path"])
    assert unmatched_dir.exists()
    assert (unmatched_dir / "MIGRATION_NEEDS_RELINK").exists()

    # Now create the repo and relink.
    repo_root = tmp_path / "build-loop"
    _init_repo(repo_root, remote="https://github.com/tyroneross/build-loop.git")
    from agent_rally_point.repo_id import repo_id
    expected_rid = repo_id(repo_root)

    result = relink(slug="build-loop", repo_path=repo_root)
    assert result["operation"] == "relink", f"got {result}"
    assert result["canonical_repo_id"] == expected_rid

    canonical_dir = isolated_home / ".agent-rally-point" / "apps" / expected_rid
    assert canonical_dir.exists()
    assert (canonical_dir / "revision").read_text() == "99\n"
    # Source unmatched dir is gone (renamed).
    assert not unmatched_dir.exists()
    # NEEDS_RELINK marker was cleared.
    assert not (canonical_dir / "MIGRATION_NEEDS_RELINK").exists()


# ---------------------------------------------------------------------------
# AC16 — relink is idempotent
# ---------------------------------------------------------------------------


def test_AC16_relink_idempotent(isolated_home, tmp_path):
    """Re-running relink when the canonical target already exists is a no-op."""
    _make_legacy_channel(isolated_home, "build-loop", {"revision": "5\n"})
    repo_root = tmp_path / "build-loop"
    _init_repo(repo_root, remote="https://github.com/tyroneross/build-loop.git")
    from agent_rally_point.migrate import apply_migration, relink

    apply_migration()
    r1 = relink(slug="build-loop", repo_path=repo_root)
    assert r1["operation"] == "relink"

    # Second invocation: target exists, no candidates remain.
    r2 = relink(slug="build-loop", repo_path=repo_root)
    assert r2["operation"] == "already-canonical", f"got {r2}"


# ---------------------------------------------------------------------------
# AC17 — repo-id split blocker regression
# ---------------------------------------------------------------------------


def test_AC17_repo_id_split_blocker_regression(
    isolated_home, tmp_path, monkeypatch
):
    """For every slug whose repo is findable, the migration destination MUST
    equal ``repo_id(repo_path)``. This is the contract the BLOCKER violated."""
    # Set up two real repos with distinct remotes.
    search_root = tmp_path / "search"
    _init_repo(
        search_root / "build-loop",
        remote="https://github.com/tyroneross/build-loop.git",
    )
    _init_repo(
        search_root / "atomize-ai",
        remote="https://github.com/tyroneross/atomize-ai.git",
    )
    # Matching legacy channels.
    _make_legacy_channel(isolated_home, "build-loop", {"revision": "1\n"})
    _make_legacy_channel(isolated_home, "atomize-ai", {"revision": "2\n"})

    monkeypatch.setenv("AGENT_RALLY_REPO_SEARCH_PATHS", str(search_root))

    from agent_rally_point.migrate import _migration_destination_name
    from agent_rally_point.repo_id import repo_id

    for slug, repo_path in [
        ("build-loop", search_root / "build-loop"),
        ("atomize-ai", search_root / "atomize-ai"),
    ]:
        dest_name, status, found_path = _migration_destination_name(slug)
        assert status == "matched", f"{slug}: expected matched, got {status}"
        expected = repo_id(repo_path)
        assert dest_name == expected, (
            f"slug={slug}: migration dest={dest_name} but "
            f"repo_id()={expected} — split bug regression!"
        )


# ---------------------------------------------------------------------------
# AC18 — unmatched slug writes to unmatched dir with marker
# ---------------------------------------------------------------------------


def test_AC18_unmatched_slug_writes_to_unmatched_dir(isolated_home, tmp_path):
    """A slug with no matching repo migrates to <slug>-unmatched-<hex>/ with
    the MIGRATION_NEEDS_RELINK marker. Operator runs relink to resolve."""
    _make_legacy_channel(isolated_home, "ghost-app", {
        "revision": "7\n",
        "inbox/codex.jsonl": '{"kind":"handoff"}\n',
    })

    from agent_rally_point.migrate import apply_migration
    result = apply_migration()
    outcome = result["outcomes"][0]

    assert outcome["match_status"] == "unmatched"
    assert outcome["repo_path"] is None
    assert outcome["canonical_repo_id"].startswith("ghost-app-unmatched-")

    dest = Path(outcome["dest_path"])
    assert dest.exists()
    assert (dest / "revision").read_text() == "7\n"
    marker = dest / "MIGRATION_NEEDS_RELINK"
    assert marker.exists(), f"marker not placed at {marker}"

    data = json.loads(marker.read_text())
    assert data["status"] == "needs_relink"
    assert data["details"]["slug"] == "ghost-app"
    assert data["details"]["match_status"] == "unmatched"


# ---------------------------------------------------------------------------
# Bonus — ambiguous (multiple repo matches) also falls into unmatched
# ---------------------------------------------------------------------------


def test_ambiguous_repo_match_falls_into_unmatched(
    isolated_home, tmp_path, monkeypatch
):
    """Two repos with the same slug at different paths → ambiguous → unmatched
    naming so operator must use relink with the correct repo-path."""
    sa = tmp_path / "search_a"; sb = tmp_path / "search_b"
    _init_repo(sa / "myapp", remote="https://github.com/owner-a/myapp.git")
    _init_repo(sb / "myapp", remote="https://github.com/owner-b/myapp.git")
    _make_legacy_channel(isolated_home, "myapp", {"revision": "1\n"})

    monkeypatch.setenv(
        "AGENT_RALLY_REPO_SEARCH_PATHS", f"{sa},{sb}"
    )

    from agent_rally_point.migrate import _migration_destination_name
    dest_name, status, found_path = _migration_destination_name("myapp")
    assert status == "ambiguous", f"expected ambiguous, got {status}"
    assert dest_name.startswith("myapp-unmatched-")
    assert found_path is None


# ---------------------------------------------------------------------------
# Bonus — relink rejects when target exists AND candidate exists (no force)
# ---------------------------------------------------------------------------


def test_relink_refuses_on_existing_target_without_force(
    isolated_home, tmp_path
):
    """Two channels both holding state — relink must refuse to overwrite
    without --force to prevent silent data loss."""
    # Make legacy channel + apply migration (creates unmatched dir).
    _make_legacy_channel(isolated_home, "build-loop", {"revision": "OLD\n"})
    from agent_rally_point.migrate import apply_migration, relink
    apply_migration()

    # Now manually create what would be the canonical target dir with
    # different content (simulates a prior partial relink + new state).
    repo_root = tmp_path / "build-loop"
    _init_repo(repo_root, remote="https://github.com/tyroneross/build-loop.git")
    from agent_rally_point.repo_id import repo_id
    rid = repo_id(repo_root)
    canonical = isolated_home / ".agent-rally-point" / "apps" / rid
    canonical.mkdir(parents=True)
    (canonical / "revision").write_text("NEW\n")

    # relink should refuse without --force.
    r = relink(slug="build-loop", repo_path=repo_root)
    assert r["operation"] == "error", f"expected error, got {r}"
    assert "already exists" in r["error"]

    # With force, it backs up the existing target.
    r2 = relink(slug="build-loop", repo_path=repo_root, force=True)
    assert r2["operation"] == "relink"
    # The original NEW content was backed up.
    backups = [
        p for p in canonical.parent.iterdir()
        if p.name.startswith(f"{rid}.backup-")
    ]
    assert len(backups) == 1
    assert (backups[0] / "revision").read_text() == "NEW\n"
    # Canonical now holds the relinked content.
    assert (canonical / "revision").read_text() == "OLD\n"
