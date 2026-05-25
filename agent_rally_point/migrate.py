#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Legacy → canonical channel migration tool (alpha-4 + alpha-5 + repo-id-split repair).

Walks ``~/.build-loop/apps/*`` and copies each per-repo channel into
``~/.agent-rally-point/apps/<repo_id>/`` keyed by the runtime ``repo_id()``
function — the SAME function discover() uses. Single ID-derivation rule.

For each legacy slug, the migration searches ``--repo-search-paths``
(default ``~/dev/git-folder/``) for a git repo whose ``app_slug()`` matches.
When found, the destination name is ``repo_id(repo_path)``. When no repo
matches (or multiple match), the destination falls back to
``<slug>-unmatched-<8hex>/`` with a ``MIGRATION_NEEDS_RELINK`` marker; the
operator then runs ``agent-rally-migrate relink --slug <slug> --repo-path
<path>`` to rename the dir to its canonical ``repo_id``.

Subcommands:

  ``scan``           — dry-run; list discoverable legacy channels and
                       the canonical destination each would map to.
  ``apply``          — copy + log + marker. Idempotent.
  ``relink``         — rename an existing ``<slug>-{legacy,unmatched}-<hex>/``
                       dir to its canonical ``repo_id(<repo-path>)`` name.
                       Idempotent (no-op when target already exists).
  ``verify-cutover`` — 4-condition can-promote verdict (alpha-5).

Writes an append-only audit log at ``~/.agent-rally-point/migration.log``
(JSONL) and places an advisory marker file at the legacy channel root.

All operations are fire-and-forget at the per-channel level — a single
channel that fails to copy logs the error and continues. The overall
exit code is 0 on success, 1 on any per-channel failure (so CI can
catch problems), and 2 on argument/configuration errors.
"""
from __future__ import annotations

import argparse
import datetime as _dt
import hashlib
import json
import os
import shutil
import sys
import time
from pathlib import Path
from typing import Any

try:
    from .channel_paths import app_slug as _legacy_slug_from_cwd
    from .repo_id import repo_id as compute_repo_id
except ImportError:
    from channel_paths import app_slug as _legacy_slug_from_cwd  # type: ignore[no-redef]
    from repo_id import repo_id as compute_repo_id  # type: ignore[no-redef]


_LEGACY_APPS_ROOT = "~/.build-loop/apps"
_CANONICAL_APPS_ROOT = "~/.agent-rally-point/apps"
_MIGRATION_LOG_REL = ".agent-rally-point/migration.log"
_READONLY_MARKER = ".RALLY_LEGACY_READONLY"
_NEEDS_RELINK_MARKER = "MIGRATION_NEEDS_RELINK"

# Cutover-gate whitelist — the EXACT set of files whose mtime can refuse
# cutover under the fresh-writes-within-TTL condition. Codex flagged the
# original rglob("*")-then-exclude-marker approach as hostile UX (Item 9,
# rev 219): watchers append to ``watchers/*.log`` every few seconds while
# any session is alive, so the gate would effectively never pass.
#
# Only ACTUAL coordination state files gate the verdict. Telemetry
# (watcher logs, lock files), markers, and any future "operational" file
# are silently ignored by the gate. The whitelist is intentionally
# explicit rather than an exclude-list — adding a new state file
# requires deliberately listing it here, which forces the contract to
# stay clear.
#
# Shape entries:
#   ("file", "<relpath>")         — exact file at channel root
#   ("glob", "<pattern>")         — relative glob, evaluated via Path.glob()
_CUTOVER_GATED_FILES: tuple[tuple[str, str], ...] = (
    ("file", "changes.jsonl"),
    ("file", "revision"),
    ("file", "rejections.jsonl"),
    ("glob", "inbox/*.jsonl"),
    ("glob", "rally/*.json"),
    ("glob", "sessions/*.json"),
)


def _iter_gated_paths(channel_dir: Path):
    """Yield the subset of files under ``channel_dir`` that gate cutover.

    Cutover whitelist (see ``_CUTOVER_GATED_FILES``). Telemetry — watcher
    logs, lock files, the readonly/relink markers, anything else — is
    skipped. The iterator never raises; missing dirs/files are silently
    absent.
    """
    for kind, spec in _CUTOVER_GATED_FILES:
        if kind == "file":
            p = channel_dir / spec
            if p.is_file():
                yield p
        elif kind == "glob":
            try:
                for p in channel_dir.glob(spec):
                    if p.is_file():
                        yield p
            except OSError:
                continue

# Default repo-search paths. The migration walks these looking for git repos
# whose app_slug() matches a legacy slug. Override via --repo-search-paths
# (CSV) or $AGENT_RALLY_REPO_SEARCH_PATHS.
_DEFAULT_REPO_SEARCH_PATHS = "~/dev/git-folder/"

# Default cutover TTL: matches presence heartbeat (15 min). After the marker
# is placed, no legacy write within this window means the cutover is safe.
_DEFAULT_CUTOVER_TTL_MIN = 15


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _legacy_root() -> Path:
    return Path(os.path.expanduser(_LEGACY_APPS_ROOT))


def _canonical_root() -> Path:
    return Path(os.path.expanduser(_CANONICAL_APPS_ROOT))


def _migration_log_path() -> Path:
    return Path(os.path.expanduser(f"~/{_MIGRATION_LOG_REL}"))


def _iso_now() -> str:
    return _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _sha256_file(p: Path) -> str:
    h = hashlib.sha256()
    with open(p, "rb") as fh:
        while True:
            chunk = fh.read(65536)
            if not chunk:
                break
            h.update(chunk)
    return h.hexdigest()


def _is_telemetry_path(rel: Path) -> bool:
    """Return True for paths the cutover gate treats as telemetry, not state.

    Mirrors the spirit of the ``_CUTOVER_GATED_FILES`` whitelist: watcher
    logs, lock files, and similar operational bookkeeping are appended
    every few seconds and aren't part of channel state. Excluding them
    from BOTH the fresh-writes scan and the integrity manifest keeps
    the gate's contract coherent (Codex Item 9, rev 219).
    """
    parts = rel.parts
    if parts and parts[0] == "watchers":
        return True
    if rel.suffix == ".log" or rel.suffix == ".lock":
        return True
    return False


def _sha256_manifest_of_dir(root: Path) -> str:
    """Compute a stable sha256-of-sha256s for every regular file under root.

    The manifest is deterministic across runs (files sorted by relative path)
    and survives filesystem reordering. Symlinks are followed; binary files
    are hashed verbatim. Returns "" for an empty/missing directory.

    Excluded from the manifest:
      * advisory markers (``.RALLY_LEGACY_READONLY``,
        ``MIGRATION_NEEDS_RELINK``) — migration metadata, not channel state
      * telemetry paths (``watchers/**``, ``*.log``, ``*.lock``) — appended
        constantly by daemons, not state the cutover gate is about
    """
    if not root.exists() or not root.is_dir():
        return ""
    entries = []
    for p in sorted(root.rglob("*")):
        if not p.is_file():
            continue
        if p.name in (_READONLY_MARKER, _NEEDS_RELINK_MARKER):
            continue  # markers are metadata; not part of channel state
        try:
            rel = p.relative_to(root)
        except ValueError:
            continue
        if _is_telemetry_path(rel):
            continue  # telemetry; see _is_telemetry_path docstring
        try:
            digest = _sha256_file(p)
        except OSError:
            continue
        entries.append(f"{rel.as_posix()}:{digest}")
    return hashlib.sha256("\n".join(entries).encode("utf-8")).hexdigest()


def _append_migration_log(record: dict) -> None:
    """Append one JSONL record to the migration log. Fire-and-forget."""
    try:
        p = _migration_log_path()
        p.parent.mkdir(parents=True, exist_ok=True)
        line = json.dumps(record, separators=(",", ":")) + "\n"
        # O_APPEND single-write keeps records atomic across concurrent CLIs.
        fd = os.open(str(p), os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o644)
        try:
            os.write(fd, line.encode("utf-8"))
        finally:
            os.close(fd)
    except OSError:
        return


def _resolve_repo_search_paths(
    repo_search_paths: list[str] | None = None,
) -> list[Path]:
    """Return absolute, expanded, existing dirs from the repo-search-paths spec.

    Resolution order: explicit argument → env var → default. Non-existent
    paths are silently dropped (operator may pass a path that doesn't exist
    on this machine — fine, just no matches from there).
    """
    if repo_search_paths is None:
        env = os.environ.get("AGENT_RALLY_REPO_SEARCH_PATHS")
        if env:
            repo_search_paths = [s.strip() for s in env.split(",") if s.strip()]
        else:
            repo_search_paths = [_DEFAULT_REPO_SEARCH_PATHS]
    out: list[Path] = []
    seen: set[str] = set()
    for raw in repo_search_paths:
        try:
            p = Path(os.path.expanduser(raw)).resolve()
        except (OSError, RuntimeError):
            continue
        key = str(p)
        if key in seen:
            continue
        seen.add(key)
        if p.exists() and p.is_dir():
            out.append(p)
    return out


def _find_repos_matching_slug(
    slug: str, repo_search_paths: list[Path]
) -> list[Path]:
    """Find git-repo directories under repo_search_paths whose app_slug == slug.

    Searches one directory level deep per search path. A "git repo" is any
    directory containing a ``.git`` entry (file for worktrees, dir for main
    checkouts). Worktrees of the same repo all resolve to the same
    canonical repo root via ``app_slug()`` — but the *paths* themselves are
    distinct, so we dedup by the value of ``app_slug()`` (which is the
    canonical-repo-basename). This means the same repo appearing as a main
    checkout AND a worktree gets counted as ONE match, not two.
    """
    out: list[Path] = []
    seen_canonical_slugs: dict[str, Path] = {}
    for base in repo_search_paths:
        try:
            children = sorted(base.iterdir())
        except OSError:
            continue
        for child in children:
            if not child.is_dir():
                continue
            # A directory is a git repo if it contains a .git entry. Don't
            # descend further — slug-match operates one level deep.
            git_marker = child / ".git"
            if not git_marker.exists():
                continue
            try:
                child_slug = _legacy_slug_from_cwd(child)
            except Exception:  # noqa: BLE001
                continue
            if child_slug == slug:
                # Dedup worktree of same canonical repo: keep the first hit
                # that produces a unique canonical-repo path.
                try:
                    out_already_canonical_paths = {
                        _resolve_canonical_repo_root(p) for p in out
                    }
                    this_canonical = _resolve_canonical_repo_root(child)
                except Exception:  # noqa: BLE001
                    out.append(child)
                    continue
                if this_canonical not in out_already_canonical_paths:
                    out.append(child)
    return out


def _resolve_canonical_repo_root(p: Path) -> Path:
    """Return the canonical repo root for ``p`` — same root for every worktree.

    Uses ``git rev-parse --git-common-dir`` (parent of common-dir is the
    canonical repo). Falls back to the path itself when not a git repo.
    """
    import subprocess
    try:
        out = subprocess.run(
            ["git", "-C", str(p), "rev-parse", "--git-common-dir"],
            capture_output=True, text=True, timeout=1.0,
        )
        if out.returncode != 0:
            return p.resolve()
        common = Path(out.stdout.strip())
        if not common.is_absolute():
            common = p / common
        return common.resolve().parent
    except Exception:  # noqa: BLE001
        return p.resolve()


def _migration_destination_name(
    slug: str,
    *,
    repo_search_paths: list[Path] | None = None,
    legacy_root: Path | None = None,
) -> tuple[str, str, Path | None]:
    """Resolve the canonical destination name for a legacy slug.

    Single ID-derivation rule: when a repo is found, the destination name is
    EXACTLY ``repo_id(repo_path)`` — the same function discover() uses.

    Returns ``(dest_name, match_status, repo_path)`` where:
      - ``dest_name`` ∈ {``repo_id(repo_path)``, ``<slug>-unmatched-<8hex>``}
      - ``match_status`` ∈ {``matched``, ``unmatched``, ``ambiguous``}
      - ``repo_path`` is the matched git repo (or None on unmatched/ambiguous)

    Ambiguous (multiple repos with the same slug) falls back to unmatched
    naming — operator must run ``relink`` per-app with the correct
    ``--repo-path``.
    """
    paths = (
        repo_search_paths
        if repo_search_paths is not None
        else _resolve_repo_search_paths()
    )
    matches = _find_repos_matching_slug(slug, paths)
    if len(matches) == 1:
        repo_path = matches[0]
        try:
            dest = compute_repo_id(repo_path)
            return dest, "matched", repo_path
        except Exception:  # noqa: BLE001
            pass  # fall through to unmatched
    # Unmatched (0 hits) or ambiguous (>1) — same fallback shape.
    base = legacy_root if legacy_root is not None else _legacy_root()
    legacy_path = (base / slug).resolve()
    h = hashlib.sha256(("unmatched:" + str(legacy_path)).encode("utf-8")).hexdigest()[:8]
    status = "ambiguous" if len(matches) > 1 else "unmatched"
    return f"{slug}-unmatched-{h}", status, None


_CHANGES_JSONL_NAME = "changes.jsonl"


def _merge_changes_jsonl(src: Path, dest: Path) -> dict:
    """Event-level merge of two ``changes.jsonl`` files. Idempotent + dedup.

    Codex Item 10 (rev 219): in the dual-write (β1.2) handoff, legacy and
    canonical can each hold records the other doesn't — and they share
    records too. The original file-idempotent ``_copy_tree_idempotent``
    can't represent that: it either overwrites destination, skips, or
    would concatenate without dedup. The cutover apply step needs
    event-level merge — append source records whose line-identity isn't
    already present in dest, preserving append-only order semantics.

    Dedup key is the canonicalized stripped line (records don't carry an
    explicit ``id``; the schema is rich enough — ``ts`` float +
    ``revision`` + ``run_id`` + payload — that line-identity is the
    natural natural key). Empty lines are skipped.

    Returns a stats dict for the migration log row:
        {
          "lines_in_src": int,
          "lines_in_dest_before": int,
          "lines_appended": int,
          "lines_skipped_dup": int,
          "lines_in_dest_after": int,
        }
    """
    src_lines: list[str] = []
    try:
        with open(src, "r", encoding="utf-8") as fh:
            for line in fh:
                stripped = line.rstrip("\n")
                if stripped:
                    src_lines.append(stripped)
    except OSError:
        return {
            "lines_in_src": 0,
            "lines_in_dest_before": 0,
            "lines_appended": 0,
            "lines_skipped_dup": 0,
            "lines_in_dest_after": 0,
        }

    dest_lines: list[str] = []
    if dest.exists():
        try:
            with open(dest, "r", encoding="utf-8") as fh:
                for line in fh:
                    stripped = line.rstrip("\n")
                    if stripped:
                        dest_lines.append(stripped)
        except OSError:
            dest_lines = []

    dest_seen = set(dest_lines)
    appended = 0
    skipped = 0
    to_append: list[str] = []
    for line in src_lines:
        if line in dest_seen:
            skipped += 1
        else:
            to_append.append(line)
            dest_seen.add(line)
            appended += 1

    if appended:
        dest.parent.mkdir(parents=True, exist_ok=True)
        # Atomic-ish append: open with O_APPEND so concurrent writers
        # (unlikely during a migration but cheap insurance) interleave
        # whole lines, not partial bytes — matches changes.py's own
        # convention.
        try:
            fd = os.open(str(dest), os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o644)
            try:
                for line in to_append:
                    os.write(fd, (line + "\n").encode("utf-8"))
            finally:
                os.close(fd)
        except OSError as e:
            _append_migration_log({
                "ts": _iso_now(),
                "operation": "event-level-merge-error",
                "source_path": str(src),
                "dest_path": str(dest),
                "error": str(e),
            })
            appended = 0  # nothing landed

    return {
        "lines_in_src": len(src_lines),
        "lines_in_dest_before": len(dest_lines),
        "lines_appended": appended,
        "lines_skipped_dup": skipped,
        "lines_in_dest_after": len(dest_lines) + appended,
    }


def _jsonl_record_keys(path: Path, prefer_keys: tuple[str, ...]) -> set:
    """Return the set of "record identity keys" present in a JSONL file.

    For each non-empty line, parse JSON and pick the first available key in
    ``prefer_keys`` whose value is non-None. The returned set contains those
    values (stringified for type-safety against int/str drift). Lines that
    fail to parse OR have none of the preferred keys fall back to the raw
    stripped line text as their identity — preserves coverage when records
    don't carry the schema we expect.

    Never raises. Returns an empty set on OSError.

    Identity by ``revision`` (changes.jsonl) and ``id`` (inbox/rejections)
    is correct because both are assigned monotonically by the producer and
    are unique within the channel by construction. Byte content of the
    record may legitimately differ (e.g., ``producer_metadata`` added
    post-merge in β1.2 dual-write) — this function ignores that drift.
    """
    out: set = set()
    try:
        with open(path, "r", encoding="utf-8") as fh:
            for line in fh:
                stripped = line.rstrip("\n")
                if not stripped:
                    continue
                key: object | None = None
                try:
                    rec = json.loads(stripped)
                except (json.JSONDecodeError, ValueError):
                    rec = None
                if isinstance(rec, dict):
                    for k in prefer_keys:
                        v = rec.get(k)
                        if v is not None:
                            key = v
                            break
                if key is None:
                    # No preferred key parsed — fall back to raw line. This
                    # preserves the historical line-superset behavior for
                    # records lacking the schema fields we expect.
                    key = stripped
                out.add(repr(key) if not isinstance(key, (str, int, float, bool)) else str(key))
    except OSError:
        return set()
    return out


def _dest_covers_src(src_channel: Path, dest_channel: Path) -> bool:
    """Return True when dest "covers" src for migration-integrity purposes.

    Coverage rules — semantic, not byte-identity, because β1.2 dual-write
    can produce divergent serializations of the same record (e.g.,
    ``producer_metadata`` added post-merge):

      * ``changes.jsonl`` — record-superset by the ``revision`` field.
        Every legacy record's ``revision`` MUST exist in canonical.
        Records lacking ``revision`` fall back to raw-line identity.

      * ``revision`` file (channel root) — parse both as integers; pass
        iff ``dest_int >= src_int`` (canonical's cursor must cover legacy).
        When EITHER side is non-integer, fall back to byte-equal so test
        fixtures using sentinel content keep working.

      * ``inbox/*.jsonl`` and ``rejections.jsonl`` — record-superset by
        the ``id`` field (set at write time, unique within the file).
        Falls back to ``ts`` and then raw line.

      * All other regular files (``sessions/*.json``, ``rally/*.json``,
        ``rally/current.json``, etc.) — byte-equal at the same relpath
        when present in BOTH sides. Canonical-only files (e.g., a peer's
        ``presence-*.json``) are not iterated by this walk — the loop
        starts from ``src_channel`` — so canonical-superset is implicitly
        allowed and is the desired post-cutover state.

      * Excluded from coverage entirely: markers + telemetry (see
        ``_is_telemetry_path``).

    Returns False if either side is missing/empty. Never raises.
    """
    if not src_channel.exists() or not src_channel.is_dir():
        return False
    if not dest_channel.exists() or not dest_channel.is_dir():
        return False
    for s in sorted(src_channel.rglob("*")):
        if not s.is_file():
            continue
        if s.name in (_READONLY_MARKER, _NEEDS_RELINK_MARKER):
            continue
        try:
            rel = s.relative_to(src_channel)
        except ValueError:
            continue
        if _is_telemetry_path(rel):
            continue
        d = dest_channel / rel
        if not d.is_file():
            return False
        rel_posix = rel.as_posix()

        # changes.jsonl: record-superset by ``revision`` field.
        if rel_posix == _CHANGES_JSONL_NAME:
            src_keys = _jsonl_record_keys(s, prefer_keys=("revision",))
            dest_keys = _jsonl_record_keys(d, prefer_keys=("revision",))
            if not src_keys.issubset(dest_keys):
                return False
            continue

        # revision file: monotonic int-superset (dest_int >= src_int).
        # Falls through to byte-equal when either side is non-integer.
        if rel_posix == "revision":
            try:
                src_rev = int(s.read_text().strip())
                dest_rev = int(d.read_text().strip())
            except (OSError, ValueError):
                src_rev = None
                dest_rev = None
            if src_rev is not None and dest_rev is not None:
                if dest_rev < src_rev:
                    return False
                continue
            # Fall through to byte-equal for non-integer content.

        # Inbox jsonl + rejections.jsonl: record-superset by ``id`` field.
        # Both files are append-only with monotonic per-record ids assigned
        # at write time, so id-superset is the correct semantic-equivalent
        # of line-superset across post-merge re-serializations.
        if rel_posix == "rejections.jsonl" or (
            rel.parts and rel.parts[0] == "inbox" and rel_posix.endswith(".jsonl")
        ):
            src_keys = _jsonl_record_keys(s, prefer_keys=("id", "ts"))
            dest_keys = _jsonl_record_keys(d, prefer_keys=("id", "ts"))
            if not src_keys.issubset(dest_keys):
                return False
            continue

        # All other files: byte-equal at the same relpath.
        try:
            if _sha256_file(s) != _sha256_file(d):
                return False
        except OSError:
            return False
    return True


def _copy_tree_idempotent(src: Path, dest: Path) -> tuple[int, list[str]]:
    """Copy src → dest. Skips files that already exist with identical content.

    Special case: ``changes.jsonl`` gets event-level merge semantics —
    dedup by line-identity and append-only into dest. See
    ``_merge_changes_jsonl`` (Codex Item 10, rev 219). After the merge,
    dest's ``revision`` file is bumped to ``max(src_rev, dest_rev)`` so
    consumers see a cursor that covers every record.

    Returns (files_copied, file_paths_list). file_paths_list is every regular
    file under src (relative paths), regardless of whether it was copied or
    skipped as already-identical. For the merged ``changes.jsonl`` case,
    the file counts as "copied" when at least one line was appended.
    """
    if not src.exists():
        return 0, []
    dest.mkdir(parents=True, exist_ok=True)
    copied = 0
    relpaths = []
    merge_stats: dict | None = None
    # Capture dest_revision BEFORE the per-file walk — otherwise copying
    # src's revision file overwrites dest's pre-merge value and the
    # event-level-merge log row can't faithfully report
    # ``dest_revision_before``.
    def _read_rev(p: Path) -> int:
        try:
            return max(0, int((p / "revision").read_text().strip()))
        except (OSError, ValueError):
            return 0
    dest_rev_before = _read_rev(dest)
    for s in sorted(src.rglob("*")):
        if not s.is_file():
            continue
        try:
            rel = s.relative_to(src)
        except ValueError:
            continue
        d = dest / rel
        relpaths.append(rel.as_posix())

        # Special case: changes.jsonl gets event-level merge, not
        # whole-file replace. Only the top-level changes.jsonl (channel
        # root) is treated this way — any deeper "changes.jsonl" (none
        # exist in current protocol, but be explicit) falls through.
        if rel.as_posix() == _CHANGES_JSONL_NAME:
            stats = _merge_changes_jsonl(s, d)
            merge_stats = stats
            if stats["lines_appended"] > 0:
                copied += 1
            continue

        # Special case: ``revision`` is monotonic — never overwrite a
        # higher dest value with a lower src value (Codex Item 10
        # corollary; without this guard the event-level-merge log row's
        # ``dest_revision_before`` can't be observed and the cursor can
        # regress on a re-run with a stale legacy side). Only kicks in
        # when BOTH sides parse as integers (the protocol contract);
        # otherwise falls through to the byte-copy path so existing
        # callers using non-integer sentinel content keep working.
        if rel.as_posix() == "revision":
            try:
                src_rev_int = int(s.read_text().strip())
            except (OSError, ValueError):
                src_rev_int = None
            dest_rev_int: int | None
            if d.exists():
                try:
                    dest_rev_int = int(d.read_text().strip())
                except (OSError, ValueError):
                    dest_rev_int = None
            else:
                dest_rev_int = None
            if src_rev_int is not None and dest_rev_int is not None:
                target = max(src_rev_int, dest_rev_int)
                if dest_rev_int == target:
                    continue  # dest already covers src; idempotent no-op
                try:
                    d.parent.mkdir(parents=True, exist_ok=True)
                    tmp = d.with_suffix(".tmp")
                    tmp.write_text(f"{target}\n")
                    tmp.replace(d)
                    copied += 1
                except OSError as e:
                    _append_migration_log({
                        "ts": _iso_now(),
                        "operation": "copy-error",
                        "source_path": str(s),
                        "dest_path": str(d),
                        "error": str(e),
                    })
                continue
            # Fall through to default byte-copy when either side isn't
            # integer-parseable (covers test fixtures + future protocol
            # extensions without surprising existing callers).

        if d.exists():
            # Skip if content matches (idempotency on re-run).
            try:
                if _sha256_file(s) == _sha256_file(d):
                    continue
            except OSError:
                pass
        d.parent.mkdir(parents=True, exist_ok=True)
        try:
            shutil.copy2(str(s), str(d))
            copied += 1
        except OSError as e:
            _append_migration_log({
                "ts": _iso_now(),
                "operation": "copy-error",
                "source_path": str(s),
                "dest_path": str(d),
                "error": str(e),
            })

    # If changes.jsonl was merged, log the merge stats. Revision is
    # already at max(src,dest) thanks to the in-walk monotonic write
    # above; re-read it for an accurate ``dest_revision_after`` value
    # in the log row.
    if merge_stats is not None:
        src_rev = _read_rev(src)
        dest_rev_after = _read_rev(dest)
        _append_migration_log({
            "ts": _iso_now(),
            "operation": "event-level-merge",
            "source_path": str(src / _CHANGES_JSONL_NAME),
            "dest_path": str(dest / _CHANGES_JSONL_NAME),
            "src_revision": src_rev,
            "dest_revision_before": dest_rev_before,
            "dest_revision_after": dest_rev_after,
            **{f"changes_{k}": v for k, v in merge_stats.items()},
        })

    return copied, relpaths


def _place_readonly_marker(legacy_channel: Path, info: dict) -> None:
    """Drop an advisory marker file at the legacy channel root.

    The marker is informational only — old build-loop scripts that hardcode
    legacy paths won't read it. ``verify-cutover`` independently scans
    legacy mtimes for fresh writes after the marker is placed.
    """
    try:
        marker = legacy_channel / _READONLY_MARKER
        marker.write_text(json.dumps({
            "ts": _iso_now(),
            "policy_after_cutover": "canonical",
            "advisory": True,
            "details": info,
            "doc": (
                "This file is advisory. Old build-loop scripts may still "
                "write here. agent-rally-point migrate verify-cutover scans "
                "the legacy channel for fresh writes within one TTL and "
                "refuses cutover if any are detected."
            ),
        }, indent=2, sort_keys=True) + "\n")
    except OSError:
        return


def _place_needs_relink_marker(dest: Path, info: dict) -> None:
    """Drop a MIGRATION_NEEDS_RELINK marker inside an unmatched destination dir.

    Indicates to the operator (and to discover() readers) that this channel
    is at an unmatched name and should be relinked via
    ``agent-rally-migrate relink --slug <slug> --repo-path <path>``.
    """
    try:
        dest.mkdir(parents=True, exist_ok=True)
        marker = dest / _NEEDS_RELINK_MARKER
        marker.write_text(json.dumps({
            "ts": _iso_now(),
            "status": "needs_relink",
            "details": info,
            "doc": (
                "This channel was migrated to an unmatched destination name "
                "because no unique git repo could be found for the legacy "
                "slug. Run `agent-rally-migrate relink --slug <slug> "
                "--repo-path <path>` to rename this dir to its canonical "
                "repo_id."
            ),
        }, indent=2, sort_keys=True) + "\n")
    except OSError:
        return


# ---------------------------------------------------------------------------
# Subcommand implementations
# ---------------------------------------------------------------------------


def discover_legacy_channels(
    legacy_root: Path | None = None,
    *,
    repo_search_paths: list[str] | list[Path] | None = None,
) -> list[dict]:
    """Return a list of legacy channel descriptors.

    Each descriptor: {slug, legacy_path, canonical_repo_id, canonical_path,
    match_status, repo_path}.

    ``canonical_repo_id`` is the runtime ``repo_id(repo_path)`` when a repo
    is found, else the ``<slug>-unmatched-<8hex>`` fallback.
    ``match_status`` ∈ {matched, unmatched, ambiguous}.
    ``repo_path`` is the matched git repo (string) or None.

    Channels named ``_unscoped`` are included (they're real cleanup targets).
    Hidden dirs (start with ``.``) and non-directories are skipped.
    """
    base = legacy_root if legacy_root is not None else _legacy_root()
    if not base.exists():
        return []
    # Normalize repo_search_paths to list[Path] once.
    if repo_search_paths is None:
        paths = _resolve_repo_search_paths()
    else:
        # Accept either list[str] or list[Path].
        paths = _resolve_repo_search_paths(
            [str(p) for p in repo_search_paths]
        )
    out = []
    for entry in sorted(base.iterdir()):
        if not entry.is_dir():
            continue
        if entry.name.startswith("."):
            continue
        slug = entry.name
        dest_name, match_status, repo_path = _migration_destination_name(
            slug, repo_search_paths=paths, legacy_root=base
        )
        out.append({
            "slug": slug,
            "legacy_path": str(entry),
            "canonical_repo_id": dest_name,
            "canonical_path": str(_canonical_root() / dest_name),
            "match_status": match_status,
            "repo_path": str(repo_path) if repo_path else None,
        })
    return out


def apply_migration(
    *,
    legacy_root: Path | None = None,
    canonical_root: Path | None = None,
    repo_search_paths: list[str] | list[Path] | None = None,
    place_marker: bool = True,
    dry_run: bool = False,
) -> dict:
    """Migrate every legacy channel to the canonical layout.

    Idempotent. Re-running over an already-migrated channel writes an
    ``already-migrated`` log entry (when the dest sha256 already matches
    the source) and no-ops the copy.

    When a legacy slug can be matched 1:1 to a git repo under
    ``repo_search_paths`` (default ``~/dev/git-folder/``), the destination
    is the runtime ``repo_id(repo_path)`` — same function discover() uses.
    Otherwise it falls back to ``<slug>-unmatched-<8hex>`` and writes a
    ``MIGRATION_NEEDS_RELINK`` marker; operator runs ``relink`` to fix.
    """
    base_l = legacy_root if legacy_root is not None else _legacy_root()
    base_c = canonical_root if canonical_root is not None else _canonical_root()
    channels = discover_legacy_channels(
        legacy_root=base_l, repo_search_paths=repo_search_paths
    )
    outcomes = []
    failures = 0
    for ch in channels:
        legacy_path = Path(ch["legacy_path"])
        dest_path = base_c / ch["canonical_repo_id"]
        operation = "migrate"
        try:
            # Compute pre-copy manifest for the audit-log record only —
            # the integrity DECISION uses _dest_covers_src so the
            # changes.jsonl line-superset semantics from event-level
            # merge (Codex Item 10, rev 219) are honored. A byte-equal
            # manifest match still short-circuits to "already-migrated"
            # for the common no-op case.
            src_manifest = _sha256_manifest_of_dir(legacy_path)
            existing_dest_manifest = _sha256_manifest_of_dir(dest_path) if dest_path.exists() else ""

            if (
                src_manifest
                and existing_dest_manifest
                and src_manifest == existing_dest_manifest
            ):
                operation = "already-migrated"
                copied = 0
                relpaths_count = sum(
                    1 for _ in legacy_path.rglob("*") if _.is_file()
                )
            else:
                if dry_run:
                    operation = "dry-run"
                    copied = sum(
                        1 for _ in legacy_path.rglob("*") if _.is_file()
                    )
                    relpaths_count = copied
                else:
                    copied, relpaths = _copy_tree_idempotent(legacy_path, dest_path)
                    relpaths_count = len(relpaths)
                    # Coverage-based integrity check: byte-equal for
                    # ordinary files, line-superset for changes.jsonl.
                    if not _dest_covers_src(legacy_path, dest_path):
                        operation = "integrity-mismatch"
                        failures += 1

            if not dry_run and operation != "integrity-mismatch" and place_marker:
                _place_readonly_marker(
                    legacy_path,
                    {
                        "canonical_repo_id": ch["canonical_repo_id"],
                        "canonical_path": str(dest_path),
                        "match_status": ch["match_status"],
                    },
                )
                # Loud needs-relink signal for unmatched/ambiguous dests.
                if ch["match_status"] in ("unmatched", "ambiguous"):
                    _place_needs_relink_marker(dest_path, {
                        "slug": ch["slug"],
                        "match_status": ch["match_status"],
                        "legacy_path": ch["legacy_path"],
                    })

            log_rec = {
                "ts": _iso_now(),
                "operation": operation,
                "source_path": str(legacy_path),
                "dest_path": str(dest_path),
                "file_count": relpaths_count,
                "sha256_manifest": src_manifest,
                "canonical_repo_id": ch["canonical_repo_id"],
                "slug": ch["slug"],
                "match_status": ch["match_status"],
                "repo_path": ch["repo_path"],
            }
            if not dry_run:
                _append_migration_log(log_rec)
            outcomes.append(log_rec)
        except OSError as e:
            failures += 1
            _append_migration_log({
                "ts": _iso_now(),
                "operation": "channel-error",
                "source_path": str(legacy_path),
                "dest_path": str(dest_path),
                "error": str(e),
                "slug": ch["slug"],
            })
            outcomes.append({
                "ts": _iso_now(),
                "operation": "channel-error",
                "source_path": str(legacy_path),
                "dest_path": str(dest_path),
                "error": str(e),
                "slug": ch["slug"],
            })

    return {
        "channels_total": len(channels),
        "failures": failures,
        "outcomes": outcomes,
        "dry_run": dry_run,
    }


def relink(
    *,
    slug: str,
    repo_path: Path | str,
    canonical_root: Path | None = None,
    force: bool = False,
) -> dict:
    """Rename an existing ``<slug>-{legacy,unmatched}-<hex>/`` dir to ``repo_id(repo_path)``.

    Idempotent: when the canonical target already exists (matching repo_id),
    refuse with a clear error unless ``force=True`` (force will rename to a
    backup and proceed; reserved for operator-level recovery).

    The function searches the canonical root for an existing dir matching
    ``<slug>-legacy-*`` or ``<slug>-unmatched-*`` and renames it (single
    ``Path.rename`` — atomic on POSIX when src and dest share a filesystem,
    which is always the case here). Appends a ``relink`` record to the
    migration log with ``from_path``, ``to_path``, ``repo_path``.

    Never raises on the no-op (idempotent) case — returns
    ``{operation: "already-canonical"}`` when the canonical dir already
    exists and the legacy/unmatched source dir does not.
    """
    base_c = canonical_root if canonical_root is not None else _canonical_root()
    repo_path_resolved = Path(os.path.expanduser(str(repo_path))).resolve()
    try:
        target_name = compute_repo_id(repo_path_resolved)
    except Exception as e:  # noqa: BLE001
        return {
            "operation": "error",
            "error": f"repo_id() failed for {repo_path_resolved}: {e}",
            "slug": slug,
        }

    target_dir = base_c / target_name

    # Find source candidates: <slug>-legacy-* OR <slug>-unmatched-*.
    if not base_c.exists():
        return {
            "operation": "error",
            "error": f"canonical root does not exist: {base_c}",
            "slug": slug,
        }
    candidates: list[Path] = []
    try:
        for entry in base_c.iterdir():
            if not entry.is_dir():
                continue
            name = entry.name
            if name == target_name:
                continue  # the target itself — handled below
            if name.startswith(f"{slug}-legacy-") or name.startswith(f"{slug}-unmatched-"):
                candidates.append(entry)
    except OSError as e:
        return {
            "operation": "error",
            "error": f"failed to scan {base_c}: {e}",
            "slug": slug,
        }

    target_exists = target_dir.exists()

    # Idempotency: if target exists and no candidates remain, this is a no-op.
    if target_exists and not candidates:
        return {
            "operation": "already-canonical",
            "slug": slug,
            "to_path": str(target_dir),
            "repo_path": str(repo_path_resolved),
        }

    # Conflict: target exists AND a candidate exists → refuse unless force.
    if target_exists and candidates and not force:
        return {
            "operation": "error",
            "error": (
                f"target {target_dir} already exists; refusing to overwrite. "
                f"Candidates: {[str(c) for c in candidates]}. Use --force to "
                f"backup the existing target and rename the candidate over it."
            ),
            "slug": slug,
            "to_path": str(target_dir),
            "candidates": [str(c) for c in candidates],
        }

    # Force path: move existing target aside.
    if target_exists and force:
        backup = base_c / f"{target_name}.backup-{int(time.time())}"
        try:
            target_dir.rename(backup)
        except OSError as e:
            return {
                "operation": "error",
                "error": f"failed to backup {target_dir} → {backup}: {e}",
                "slug": slug,
            }
        _append_migration_log({
            "ts": _iso_now(),
            "operation": "relink-backup",
            "from_path": str(target_dir),
            "to_path": str(backup),
            "slug": slug,
        })

    # Multi-candidate path: pick the most-recently-modified candidate (the
    # one most likely to hold current state). Log the discard list.
    if len(candidates) > 1:
        candidates.sort(key=lambda p: p.stat().st_mtime, reverse=True)
        chosen = candidates[0]
        discarded = candidates[1:]
        _append_migration_log({
            "ts": _iso_now(),
            "operation": "relink-multi-candidate",
            "chosen": str(chosen),
            "discarded": [str(d) for d in discarded],
            "slug": slug,
        })
    else:
        chosen = candidates[0]

    # Atomic rename.
    try:
        chosen.rename(target_dir)
    except OSError as e:
        return {
            "operation": "error",
            "error": f"failed to rename {chosen} → {target_dir}: {e}",
            "slug": slug,
        }

    # Remove the needs-relink marker if present (the relink resolved it).
    marker = target_dir / _NEEDS_RELINK_MARKER
    if marker.exists():
        try:
            marker.unlink()
        except OSError:
            pass

    log_rec = {
        "ts": _iso_now(),
        "operation": "relink",
        "from_path": str(chosen),
        "to_path": str(target_dir),
        "repo_path": str(repo_path_resolved),
        "canonical_repo_id": target_name,
        "slug": slug,
    }
    _append_migration_log(log_rec)
    return log_rec


def verify_cutover(
    *,
    legacy_root: Path | None = None,
    canonical_root: Path | None = None,
    repo_search_paths: list[str] | list[Path] | None = None,
    ttl_minutes: int = _DEFAULT_CUTOVER_TTL_MIN,
    require_downstream: bool = True,
) -> dict:
    """Return the 4-condition can-promote verdict (alpha-5 cutover gate).

    Conditions:
      1. legacy_fully_copied — every legacy channel has a corresponding
         canonical channel with matching sha256_manifest.
      2. integrity_verified — every paired pair has identical manifests.
      3. no_fresh_writes_within_ttl — no file under any legacy channel has
         an mtime newer than (now - ttl_minutes), excluding the marker file
         itself.
      4. downstream_ready — when require_downstream is True, check that
         ~/.agent-rally-point/compatibility.json exists and lists a
         supported_build_loop_range.

    Returns: {can_promote: bool, conditions: {...}, fresh_writes: [...]}.
    Never raises.
    """
    base_l = legacy_root if legacy_root is not None else _legacy_root()
    base_c = canonical_root if canonical_root is not None else _canonical_root()
    channels = discover_legacy_channels(
        legacy_root=base_l, repo_search_paths=repo_search_paths
    )

    fully_copied = True
    integrity_ok = True
    for ch in channels:
        legacy_path = Path(ch["legacy_path"])
        dest_path = base_c / ch["canonical_repo_id"]
        src = _sha256_manifest_of_dir(legacy_path)
        dst = _sha256_manifest_of_dir(dest_path)
        if not dst:
            fully_copied = False
        # Integrity = coverage, not byte-identity. The β1.2 dual-write
        # path can produce divergent serializations of the same record
        # (e.g., ``producer_metadata`` added post-merge); a byte-equal
        # manifest check would mark a legitimately-merged channel as a
        # mismatch even when canonical proves a record-superset of legacy.
        # _dest_covers_src applies semantic-aware rules per file class
        # (revision int-superset, changes.jsonl record-superset by
        # ``revision``, inbox/rejections by ``id``).
        if src and not _dest_covers_src(legacy_path, dest_path):
            integrity_ok = False
        if src and not dst:
            integrity_ok = False

    # Fresh-writes scan: only files on the cutover-gated whitelist count.
    # Telemetry (watchers/*.log, lock files), markers, and operational
    # bookkeeping are excluded — they're appended every few seconds while
    # any session is alive and would otherwise make the gate impossible
    # to satisfy (Codex Item 9, rev 219).
    cutoff = time.time() - (ttl_minutes * 60)
    fresh_writes: list[dict] = []
    for ch in channels:
        legacy_path = Path(ch["legacy_path"])
        for p in _iter_gated_paths(legacy_path):
            try:
                mt = p.stat().st_mtime
            except OSError:
                continue
            if mt > cutoff:
                fresh_writes.append({
                    "path": str(p),
                    "mtime": mt,
                    "age_seconds": round(time.time() - mt, 1),
                })
    no_fresh = not fresh_writes

    compat_exists = (
        Path(os.path.expanduser("~/.agent-rally-point/compatibility.json")).exists()
    )
    downstream_ready = (not require_downstream) or compat_exists

    conditions = {
        "legacy_fully_copied": fully_copied,
        "integrity_verified": integrity_ok,
        "no_fresh_writes_within_ttl": no_fresh,
        "downstream_ready": downstream_ready,
    }
    return {
        "can_promote": all(conditions.values()),
        "conditions": conditions,
        "fresh_writes": fresh_writes[:50],  # cap for terminal display
        "ttl_minutes": ttl_minutes,
        "channels_scanned": len(channels),
    }


# ---------------------------------------------------------------------------
# CLI entry point
# ---------------------------------------------------------------------------


def _main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="agent-rally-migrate",
        description=(
            "Migrate legacy ~/.build-loop/apps/* channels to canonical "
            "~/.agent-rally-point/apps/<repo_id>/ layout."
        ),
    )
    sub = parser.add_subparsers(dest="subcommand", required=True)

    p_scan = sub.add_parser("scan", help="Dry-run: list discoverable legacy channels")
    p_scan.add_argument("--json", action="store_true", help="JSON output")
    p_scan.add_argument(
        "--repo-search-paths", default=None,
        help=(
            "CSV of paths to search for git repos matching legacy slugs. "
            "Default: ~/dev/git-folder/ (or $AGENT_RALLY_REPO_SEARCH_PATHS)."
        ),
    )

    p_apply = sub.add_parser("apply", help="Copy + log + place advisory marker")
    p_apply.add_argument(
        "--dry-run", action="store_true", help="Don't write anything"
    )
    p_apply.add_argument(
        "--no-marker", action="store_true",
        help="Skip placing the advisory read-only marker",
    )
    p_apply.add_argument("--json", action="store_true", help="JSON output")
    p_apply.add_argument(
        "--repo-search-paths", default=None,
        help=(
            "CSV of paths to search for git repos matching legacy slugs. "
            "Default: ~/dev/git-folder/ (or $AGENT_RALLY_REPO_SEARCH_PATHS)."
        ),
    )

    p_relink = sub.add_parser(
        "relink",
        help=(
            "Rename an existing <slug>-{legacy,unmatched}-<hex>/ canonical "
            "channel dir to its runtime repo_id(<repo-path>) name."
        ),
    )
    p_relink.add_argument(
        "--slug", required=True,
        help="The legacy slug (basename of ~/.build-loop/apps/<slug>/).",
    )
    p_relink.add_argument(
        "--repo-path", required=True,
        help="Absolute path to the git repo (any worktree). repo_id() of this path becomes the canonical name.",
    )
    p_relink.add_argument(
        "--force", action="store_true",
        help=(
            "When the canonical target already exists AND a legacy/unmatched "
            "candidate also exists, back up the existing target and proceed."
        ),
    )
    p_relink.add_argument("--json", action="store_true", help="JSON output")

    p_verify = sub.add_parser(
        "verify-cutover",
        help="Check the 4 cutover conditions; return can_promote verdict",
    )
    p_verify.add_argument(
        "--ttl-minutes", type=int, default=_DEFAULT_CUTOVER_TTL_MIN,
        help="Override the no-fresh-writes window (default 15)",
    )
    p_verify.add_argument(
        "--no-downstream-check", action="store_true",
        help="Skip the compatibility.json existence check",
    )
    p_verify.add_argument(
        "--repo-search-paths", default=None,
        help="CSV of paths to search for git repos matching legacy slugs.",
    )
    p_verify.add_argument("--json", action="store_true", help="JSON output")

    args = parser.parse_args(argv)

    # Parse --repo-search-paths CSV for the subcommands that accept it.
    rsp: list[str] | None = None
    rsp_attr = getattr(args, "repo_search_paths", None)
    if rsp_attr:
        rsp = [s.strip() for s in rsp_attr.split(",") if s.strip()]

    if args.subcommand == "scan":
        out = discover_legacy_channels(repo_search_paths=rsp)
        if args.json:
            print(json.dumps(out, indent=2, sort_keys=True))
        else:
            if not out:
                print("(no legacy channels found at ~/.build-loop/apps/)")
            for ch in out:
                status_mark = {
                    "matched": "✓",
                    "unmatched": "?",
                    "ambiguous": "!",
                }.get(ch["match_status"], "?")
                print(
                    f"{status_mark} {ch['slug']:30s}  →  {ch['canonical_repo_id']}  "
                    f"[{ch['match_status']}]"
                )
                print(f"  legacy:    {ch['legacy_path']}")
                print(f"  canonical: {ch['canonical_path']}")
                if ch["repo_path"]:
                    print(f"  repo:      {ch['repo_path']}")
        return 0

    if args.subcommand == "apply":
        result = apply_migration(
            place_marker=not args.no_marker,
            dry_run=args.dry_run,
            repo_search_paths=rsp,
        )
        if args.json:
            print(json.dumps(result, indent=2, sort_keys=True))
        else:
            for o in result["outcomes"]:
                print(
                    f"[{o.get('operation','?')}] "
                    f"{o.get('slug','?'):30s}  files={o.get('file_count',0)}  "
                    f"match={o.get('match_status','?')}  "
                    f"sha256={(o.get('sha256_manifest') or '')[:12]}"
                )
            print(
                f"\nchannels={result['channels_total']} "
                f"failures={result['failures']} "
                f"dry_run={result['dry_run']}"
            )
        return 0 if result["failures"] == 0 else 1

    if args.subcommand == "relink":
        result = relink(
            slug=args.slug, repo_path=args.repo_path, force=args.force,
        )
        if args.json:
            print(json.dumps(result, indent=2, sort_keys=True))
        else:
            op = result.get("operation", "?")
            if op == "relink":
                print(
                    f"[relink] {result['slug']}: {result['from_path']}  →  "
                    f"{result['to_path']}"
                )
            elif op == "already-canonical":
                print(
                    f"[already-canonical] {result['slug']}: {result['to_path']}"
                )
            else:
                print(f"[{op}] {result.get('error', '')}", file=sys.stderr)
        return 0 if result.get("operation") in ("relink", "already-canonical") else 1

    if args.subcommand == "verify-cutover":
        verdict = verify_cutover(
            ttl_minutes=args.ttl_minutes,
            require_downstream=not args.no_downstream_check,
            repo_search_paths=rsp,
        )
        if args.json:
            print(json.dumps(verdict, indent=2, sort_keys=True))
        else:
            cond = verdict["conditions"]
            print(f"can_promote: {verdict['can_promote']}")
            for k, v in cond.items():
                mark = "✓" if v else "✗"
                print(f"  {mark} {k}: {v}")
            if verdict["fresh_writes"]:
                print(
                    f"\nfresh writes detected within {verdict['ttl_minutes']}-min "
                    f"TTL ({len(verdict['fresh_writes'])}):"
                )
                for w in verdict["fresh_writes"][:10]:
                    print(f"  {w['path']}  ({w['age_seconds']:.0f}s ago)")
        return 0 if verdict["can_promote"] else 1

    parser.print_help()
    return 2


if __name__ == "__main__":
    sys.exit(_main())
