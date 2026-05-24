#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Legacy → canonical channel migration tool (alpha-4 + alpha-5).

Walks ``~/.build-loop/apps/*`` and copies each per-repo channel into
``~/.agent-rally-point/apps/<repo_id>/`` keyed by the new ``repo_id``
identifier. Writes an append-only audit log at
``~/.agent-rally-point/migration.log`` (JSONL) and places an advisory
read-only marker file at the legacy channel root so future writers can
notice (best-effort; the marker is *advisory* per hard-rule #3 in
``coordination-substrate-canonical.md`` — old build-loop scripts won't
honor it, which is why ``verify-cutover`` independently scans legacy
mtimes for fresh writes).

Subcommands:

  ``scan``           — dry-run; list discoverable legacy channels and
                       the canonical destination each would map to.
  ``apply``          — copy + log + marker. Idempotent: re-running over
                       an already-migrated channel writes an
                       ``already-migrated`` log entry and no-ops the copy.
  ``verify-cutover`` — return the 4-condition can-promote verdict
                       (alpha-5 cutover gate).

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


def _sha256_manifest_of_dir(root: Path) -> str:
    """Compute a stable sha256-of-sha256s for every regular file under root.

    The manifest is deterministic across runs (files sorted by relative path)
    and survives filesystem reordering. Symlinks are followed; binary files
    are hashed verbatim. Returns "" for an empty/missing directory.

    The advisory ``.RALLY_LEGACY_READONLY`` marker is **excluded** from the
    manifest — it's metadata about the migration itself, not channel state,
    and would otherwise make every post-apply legacy manifest diverge from
    its pre-apply canonical counterpart.
    """
    if not root.exists() or not root.is_dir():
        return ""
    entries = []
    for p in sorted(root.rglob("*")):
        if not p.is_file():
            continue
        if p.name == _READONLY_MARKER:
            continue  # marker file is metadata; not part of channel state
        try:
            rel = p.relative_to(root)
        except ValueError:
            continue
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


def _repo_id_for_legacy_slug(slug: str, *, legacy_root: Path | None = None) -> str:
    """Compute the canonical repo_id for a legacy slug.

    Legacy channel paths are keyed by basename slug (no remote info), so we
    cannot recover the normalized-remote-URL form. Fall back to a
    path-derived id of the form ``<slug>-legacy-<8hex>`` where 8hex hashes
    the legacy channel path. This guarantees:
      - Idempotent: same slug always maps to same canonical dir.
      - Disjoint from remote-derived repo_ids (different suffix).
      - Stable across machines for the same slug + legacy root.
    """
    base = legacy_root if legacy_root is not None else _legacy_root()
    legacy_path = (base / slug).resolve()
    h = hashlib.sha256(("legacy:" + str(legacy_path)).encode("utf-8")).hexdigest()[:8]
    return f"{slug}-legacy-{h}"


def _copy_tree_idempotent(src: Path, dest: Path) -> tuple[int, list[str]]:
    """Copy src → dest. Skips files that already exist with identical content.

    Returns (files_copied, file_paths_list). file_paths_list is every regular
    file under src (relative paths), regardless of whether it was copied or
    skipped as already-identical.
    """
    if not src.exists():
        return 0, []
    dest.mkdir(parents=True, exist_ok=True)
    copied = 0
    relpaths = []
    for s in sorted(src.rglob("*")):
        if not s.is_file():
            continue
        try:
            rel = s.relative_to(src)
        except ValueError:
            continue
        d = dest / rel
        relpaths.append(rel.as_posix())
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


# ---------------------------------------------------------------------------
# Subcommand implementations
# ---------------------------------------------------------------------------


def discover_legacy_channels(legacy_root: Path | None = None) -> list[dict]:
    """Return a list of legacy channel descriptors.

    Each descriptor: {slug, legacy_path, canonical_repo_id, canonical_path}.
    Channels named ``_unscoped`` are included (they're real cleanup targets).
    Hidden dirs (start with ``.``) and non-directories are skipped.
    """
    base = legacy_root if legacy_root is not None else _legacy_root()
    if not base.exists():
        return []
    out = []
    for entry in sorted(base.iterdir()):
        if not entry.is_dir():
            continue
        if entry.name.startswith("."):
            continue
        slug = entry.name
        rid = _repo_id_for_legacy_slug(slug, legacy_root=base)
        out.append({
            "slug": slug,
            "legacy_path": str(entry),
            "canonical_repo_id": rid,
            "canonical_path": str(_canonical_root() / rid),
        })
    return out


def apply_migration(
    *,
    legacy_root: Path | None = None,
    canonical_root: Path | None = None,
    place_marker: bool = True,
    dry_run: bool = False,
) -> dict:
    """Migrate every legacy channel to the canonical layout.

    Idempotent. Re-running over an already-migrated channel writes an
    ``already-migrated`` log entry (when the dest sha256 already matches
    the source) and no-ops the copy. Returns a summary dict with
    per-channel outcomes.
    """
    base_l = legacy_root if legacy_root is not None else _legacy_root()
    base_c = canonical_root if canonical_root is not None else _canonical_root()
    channels = discover_legacy_channels(legacy_root=base_l)
    outcomes = []
    failures = 0
    for ch in channels:
        legacy_path = Path(ch["legacy_path"])
        dest_path = base_c / ch["canonical_repo_id"]
        operation = "migrate"
        try:
            # Compute pre-copy manifests for the integrity record.
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
                    dest_manifest_after = _sha256_manifest_of_dir(dest_path)
                    if src_manifest != dest_manifest_after:
                        operation = "integrity-mismatch"
                        failures += 1

            if not dry_run and operation != "integrity-mismatch" and place_marker:
                _place_readonly_marker(
                    legacy_path,
                    {
                        "canonical_repo_id": ch["canonical_repo_id"],
                        "canonical_path": str(dest_path),
                    },
                )

            log_rec = {
                "ts": _iso_now(),
                "operation": operation,
                "source_path": str(legacy_path),
                "dest_path": str(dest_path),
                "file_count": relpaths_count,
                "sha256_manifest": src_manifest,
                "canonical_repo_id": ch["canonical_repo_id"],
                "slug": ch["slug"],
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


def verify_cutover(
    *,
    legacy_root: Path | None = None,
    canonical_root: Path | None = None,
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
         supported_build_loop_range. The actual installed build-loop
         version cannot be probed from inside this package — that's the
         operator's responsibility — but the file's presence indicates
         the contract was published.

    Returns: {can_promote: bool, conditions: {...}, fresh_writes: [...]}.
    Never raises.
    """
    base_l = legacy_root if legacy_root is not None else _legacy_root()
    base_c = canonical_root if canonical_root is not None else _canonical_root()
    channels = discover_legacy_channels(legacy_root=base_l)

    fully_copied = True
    integrity_ok = True
    for ch in channels:
        legacy_path = Path(ch["legacy_path"])
        dest_path = base_c / ch["canonical_repo_id"]
        src = _sha256_manifest_of_dir(legacy_path)
        dst = _sha256_manifest_of_dir(dest_path)
        if not dst:
            fully_copied = False
        if src and dst and src != dst:
            integrity_ok = False
        if src and not dst:
            integrity_ok = False

    # Fresh-writes scan: any file under any legacy channel with mtime newer
    # than the cutoff is a fresh write.
    cutoff = time.time() - (ttl_minutes * 60)
    fresh_writes: list[dict] = []
    for ch in channels:
        legacy_path = Path(ch["legacy_path"])
        for p in legacy_path.rglob("*"):
            if not p.is_file():
                continue
            if p.name == _READONLY_MARKER:
                continue  # the marker itself is allowed to be recent
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

    p_apply = sub.add_parser("apply", help="Copy + log + place advisory marker")
    p_apply.add_argument(
        "--dry-run", action="store_true", help="Don't write anything"
    )
    p_apply.add_argument(
        "--no-marker", action="store_true",
        help="Skip placing the advisory read-only marker",
    )
    p_apply.add_argument("--json", action="store_true", help="JSON output")

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
    p_verify.add_argument("--json", action="store_true", help="JSON output")

    args = parser.parse_args(argv)

    if args.subcommand == "scan":
        out = discover_legacy_channels()
        if args.json:
            print(json.dumps(out, indent=2, sort_keys=True))
        else:
            if not out:
                print("(no legacy channels found at ~/.build-loop/apps/)")
            for ch in out:
                print(f"{ch['slug']:30s}  →  {ch['canonical_repo_id']}")
                print(f"  {ch['legacy_path']}")
                print(f"  {ch['canonical_path']}")
        return 0

    if args.subcommand == "apply":
        result = apply_migration(
            place_marker=not args.no_marker,
            dry_run=args.dry_run,
        )
        if args.json:
            print(json.dumps(result, indent=2, sort_keys=True))
        else:
            for o in result["outcomes"]:
                print(
                    f"[{o.get('operation','?')}] "
                    f"{o.get('slug','?'):30s}  files={o.get('file_count',0)}  "
                    f"sha256={(o.get('sha256_manifest') or '')[:12]}"
                )
            print(
                f"\nchannels={result['channels_total']} "
                f"failures={result['failures']} "
                f"dry_run={result['dry_run']}"
            )
        return 0 if result["failures"] == 0 else 1

    if args.subcommand == "verify-cutover":
        verdict = verify_cutover(
            ttl_minutes=args.ttl_minutes,
            require_downstream=not args.no_downstream_check,
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
