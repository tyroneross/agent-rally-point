#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Repo identity normalization — worktree-stable, clone-stable, collision-resistant.

The slug (``channel_paths.app_slug``) is the *basename* of the canonical repo root.
Two unrelated repos that happen to share a basename (``build-loop`` in two
different forks; ``app`` cloned twice; ``frontend`` everywhere) collide on the
slug. Worktrees of the *same* repo correctly converge (slug is shared) — but
the slug alone is not a stable global identifier.

``repo_id`` extends the slug with a content hash derived from the **normalized
git remote URL** (host + owner + repo name, lowercased, ``.git`` stripped). The
resulting identifier is:

  - **Worktree-stable**: every worktree shares the same remote, so the same id.
  - **Clone-stable**: re-cloning the same upstream produces the same id.
  - **Collision-resistant**: same-basename repos with different remotes get
    different ids.
  - **Frozen as part of ``protocol_version 1.0``**: changing the normalization
    algorithm or hash scheme is a backward-incompatible change that requires a
    protocol bump (see ``coordination-version-control.md``).

When no git remote is configured (a fresh ``git init`` with no ``origin``), the
fallback is a sha256 of the absolute resolved repo-root path — still stable for
that filesystem layout, never collides with a remote-derived id (different
prefix in the slug fall-back path).
"""
from __future__ import annotations

import hashlib
import os
import re
import subprocess
from pathlib import Path
from urllib.parse import urlparse

try:
    from .channel_paths import app_slug
except ImportError:
    from channel_paths import app_slug  # type: ignore[no-redef]

# Hash length: 8 hex chars = 32 bits = ~4.3B values, collision probability for
# 1000 repos is ~1e-7. Plenty for a per-user channel root, short enough to be
# legible in path listings.
_REPO_ID_HASH_LEN = 8

_GIT_TIMEOUT_S = 1.0


def _git_remote_url(cwd: Path) -> str | None:
    """Return the ``origin`` fetch URL, or None if absent / on error."""
    try:
        r = subprocess.run(
            ["git", "-C", str(cwd), "config", "--get", "remote.origin.url"],
            capture_output=True, text=True, timeout=_GIT_TIMEOUT_S,
        )
        if r.returncode != 0:
            return None
        url = r.stdout.strip()
        return url or None
    except (subprocess.SubprocessError, OSError):
        return None


def _normalize_remote_url(url: str) -> str:
    """Reduce a remote URL to ``<host>/<owner>/<repo>`` lowercased, no ``.git``.

    Handles both common shapes:
      - HTTPS: ``https://github.com/owner/repo.git``
      - SSH:   ``git@github.com:owner/repo.git``
      - SSH-URL: ``ssh://git@github.com/owner/repo.git``

    Unknown shapes are lowercased and stripped of trailing ``.git`` but
    otherwise returned verbatim — the hash will still be deterministic for that
    string, which is the only invariant we need.
    """
    s = url.strip().lower()
    # git@host:owner/repo[.git]  ->  host/owner/repo
    m = re.match(r"^git@([^:]+):(.+?)(?:\.git)?$", s)
    if m:
        return f"{m.group(1)}/{m.group(2)}"
    # parse as URL (handles https://, ssh://, etc.)
    try:
        u = urlparse(s)
        if u.netloc and u.path:
            path = u.path.lstrip("/")
            if path.endswith(".git"):
                path = path[:-4]
            return f"{u.netloc}/{path}"
    except ValueError:
        pass
    # Fallback: strip trailing .git only.
    if s.endswith(".git"):
        s = s[:-4]
    return s


def _hash8(s: str) -> str:
    """Stable 8-hex-char content hash. SHA-256 truncated."""
    return hashlib.sha256(s.encode("utf-8")).hexdigest()[:_REPO_ID_HASH_LEN]


def _repo_root_from_cwd(cwd: Path) -> Path | None:
    """Return the canonical repo root for ``cwd``, or None if not a git repo."""
    try:
        r = subprocess.run(
            ["git", "-C", str(cwd), "rev-parse", "--git-common-dir"],
            capture_output=True, text=True, timeout=_GIT_TIMEOUT_S,
        )
        if r.returncode != 0:
            return None
        common = Path(r.stdout.strip())
        if not common.is_absolute():
            common = cwd / common
        return common.resolve().parent
    except (subprocess.SubprocessError, OSError):
        return None


def _slug_from_normalized_remote(normalized: str) -> str:
    """Derive a stable slug component from a normalized remote URL.

    Input shape: ``host/owner/repo`` (lowercased, no ``.git``). Returns the
    last path segment (the repo name) normalized through the same character
    rules ``channel_paths`` uses. This makes the slug component clone-stable —
    two clones of ``github.com/owner/myproj`` both produce ``myproj`` regardless
    of the local checkout directory name.
    """
    if "/" not in normalized:
        seg = normalized
    else:
        seg = normalized.rsplit("/", 1)[-1]
    seg = seg.lower()
    seg = re.sub(r"[^a-z0-9._-]", "-", seg)
    seg = re.sub(r"-{2,}", "-", seg).strip("-")
    return seg[:64] or "_unscoped"


def repo_id(cwd: Path | str | None = None) -> str:
    """Return the worktree-stable, clone-stable, collision-resistant repo id.

    Shape: ``<slug>-<8hex>`` where:
      - **When a git remote is present** — slug is derived from the *remote's*
        repo name (last segment of normalized URL, lowercased), 8hex is sha256
        of the normalized remote URL. Two clones of the same upstream in
        different local directories converge to the same id; two repos with
        the same local basename but different remotes get different ids.
      - **When no remote** (``git init`` only) — slug is the local basename
        from ``app_slug(cwd)``, 8hex hashes ``"path:" + resolved_repo_root`` so
        the id is reproducible per filesystem location but never collides with
        a remote-derived id.
      - **Not a git repo at all** — id is ``_unscoped-<8hex>`` where 8hex is
        sha256 of the resolved cwd path.

    Frozen as part of ``protocol_version 1.0``. Never raises.
    """
    cwd_path = Path(os.path.expanduser(str(cwd))) if cwd else Path.cwd()

    repo_root = _repo_root_from_cwd(cwd_path)
    if repo_root is not None:
        url = _git_remote_url(repo_root)
        if url:
            normalized = _normalize_remote_url(url)
            slug = _slug_from_normalized_remote(normalized)
            return f"{slug}-{_hash8(normalized)}"
        # No remote -> use local basename slug, hash the resolved repo-root path.
        slug = app_slug(cwd_path)
        return f"{slug}-{_hash8('path:' + str(repo_root.resolve()))}"

    # Not a git repo at all — hash the resolved cwd.
    slug = app_slug(cwd_path)  # returns "_unscoped"
    try:
        resolved = str(cwd_path.resolve())
    except (OSError, RuntimeError):
        resolved = str(cwd_path)
    return f"{slug}-{_hash8('cwd:' + resolved)}"
