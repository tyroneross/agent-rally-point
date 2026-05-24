# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Tests for repo_id.py — frozen as part of protocol_version 1.0.

Coverage:
  - same repo via two worktrees → same repo_id
  - same repo with .git suffix on remote vs without → same repo_id
  - two repos with same basename but different remotes → different repo_id
  - no-remote repo → path-hash fallback
  - non-git cwd → _unscoped + cwd-hash fallback
  - HTTPS vs SSH form of same remote → same repo_id
"""
from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve().parent
if str(_HERE) not in sys.path:
    sys.path.insert(0, str(_HERE))


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


def _fresh_repo_id():
    if "agent_rally_point.repo_id" in sys.modules:
        del sys.modules["agent_rally_point.repo_id"]
    if "agent_rally_point.channel_paths" in sys.modules:
        del sys.modules["agent_rally_point.channel_paths"]
    import agent_rally_point.repo_id as r
    return r


def test_same_remote_produces_same_repo_id(tmp_path):
    r = _fresh_repo_id()
    a = tmp_path / "clone_a"
    b = tmp_path / "clone_b"
    a.mkdir(); b.mkdir()
    _init_repo(a, remote="https://github.com/owner/myproj.git")
    _init_repo(b, remote="https://github.com/owner/myproj.git")
    # Same remote -> same repo_id regardless of local path.
    assert r.repo_id(a) == r.repo_id(b)


def test_dot_git_suffix_does_not_change_id(tmp_path):
    r = _fresh_repo_id()
    a = tmp_path / "with_git"
    b = tmp_path / "without_git"
    a.mkdir(); b.mkdir()
    _init_repo(a, remote="https://github.com/owner/myproj.git")
    _init_repo(b, remote="https://github.com/owner/myproj")
    assert r.repo_id(a) == r.repo_id(b)


def test_https_and_ssh_form_match(tmp_path):
    r = _fresh_repo_id()
    a = tmp_path / "https_form"
    b = tmp_path / "ssh_form"
    a.mkdir(); b.mkdir()
    _init_repo(a, remote="https://github.com/owner/myproj.git")
    _init_repo(b, remote="git@github.com:owner/myproj.git")
    assert r.repo_id(a) == r.repo_id(b)


def test_different_remotes_same_basename_distinct_ids(tmp_path):
    r = _fresh_repo_id()
    a = tmp_path / "fork_a"
    b = tmp_path / "fork_b"
    a.mkdir(); b.mkdir()
    _init_repo(a, remote="https://github.com/alice/build-loop.git")
    _init_repo(b, remote="https://github.com/bob/build-loop.git")
    ida, idb = r.repo_id(a), r.repo_id(b)
    # Same slug ("build-loop") but different hashes -> distinct ids.
    assert ida.startswith("build-loop-")
    assert idb.startswith("build-loop-")
    assert ida != idb


def test_worktree_converges_to_same_id(tmp_path):
    r = _fresh_repo_id()
    main = tmp_path / "main"
    main.mkdir()
    _init_repo(main, remote="https://github.com/owner/myproj.git")
    # Make a worktree
    wt = tmp_path / "worktree"
    subprocess.run(
        ["git", "-C", str(main), "worktree", "add", "-q", "-b", "feat/x", str(wt)],
        check=True,
    )
    assert r.repo_id(main) == r.repo_id(wt)


def test_no_remote_falls_back_to_path_hash(tmp_path):
    r = _fresh_repo_id()
    a = tmp_path / "noremote"
    a.mkdir()
    _init_repo(a)  # no remote
    rid = r.repo_id(a)
    # Should be slug-<8hex>, and reproducible.
    assert rid.startswith("noremote-")
    assert len(rid.split("-")[-1]) == 8
    assert r.repo_id(a) == rid  # idempotent


def test_non_git_cwd_uses_unscoped_prefix(tmp_path):
    r = _fresh_repo_id()
    bare = tmp_path / "bare"
    bare.mkdir()
    rid = r.repo_id(bare)
    assert rid.startswith("_unscoped-")
    assert len(rid.split("-", 1)[-1]) == 8


def test_repo_id_is_deterministic(tmp_path):
    r = _fresh_repo_id()
    a = tmp_path / "rep"
    a.mkdir()
    _init_repo(a, remote="https://github.com/owner/repo.git")
    # Multiple calls -> identical.
    ids = {r.repo_id(a) for _ in range(5)}
    assert len(ids) == 1
