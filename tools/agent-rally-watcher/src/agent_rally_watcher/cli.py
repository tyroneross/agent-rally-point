# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""argparse-based CLI: ``start | stop | status | reload | install-launchd | ack-quarantine``.

Channel resolution: by default, runs ``git rev-parse --git-common-dir``
against ``cwd`` and resolves the canonical Rally Point channel under
``~/.agent-rally-point/apps/<slug>/``. ``--channel-dir PATH`` overrides.

Channel-dir fallback (when not overridden):
    1. ``~/.agent-rally-point/apps/<slug>/`` if it exists (canonical).
    2. ``~/.build-loop/apps/<slug>/`` if it exists (legacy — rally-point shipped
       inside build-loop before becoming standalone). Logs a one-shot warning.
    3. ``~/.agent-rally-point/apps/<slug>/`` (creates the canonical layout).
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path

from . import __version__
from .cursor import quarantine_ack_path, quarantine_path, save_quarantine_ack
from .daemon import (
    DaemonPaths,
    daemon_status,
    reload_daemon,
    start_daemon,
    stop_daemon,
)
from .installers import launchd

DEFAULT_RALLY_POINT_APPS_ROOT = "~/.agent-rally-point/apps"
LEGACY_BUILD_LOOP_APPS_ROOT = "~/.build-loop/apps"

_legacy_warning_emitted = False


def _normalize_base(name: str) -> str:
    base = name.lower()
    base = re.sub(r"[^a-z0-9._-]", "-", base)
    base = re.sub(r"-{2,}", "-", base).strip("-")
    return base[:64]


def derive_channel_slug(cwd: Path) -> str:
    """Mirror of agent-rally-point's ``app_slug``: worktree-independent.

    Resolves ``git rev-parse --git-common-dir``; parent of common-dir is
    the canonical repo root. Outside git → ``_unscoped``.
    """
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--git-common-dir"],
            cwd=str(cwd),
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
    except (subprocess.CalledProcessError, OSError):
        return "_unscoped"
    if not out:
        return "_unscoped"
    common = Path(out)
    if not common.is_absolute():
        common = cwd / common
    try:
        repo_root = common.resolve().parent
    except (OSError, RuntimeError):
        return "_unscoped"
    return _normalize_base(repo_root.name) or "_unscoped"


def _channel_dir_for(slug: str) -> Path:
    """Resolve the channel dir for ``slug`` with canonical/legacy fallback.

    1. ``$BUILD_LOOP_APPS_ROOT`` override or ``~/.agent-rally-point/apps/<slug>/`` if it exists.
    2. ``~/.build-loop/apps/<slug>/`` if it exists (legacy compat — emits a one-shot warning).
    3. ``~/.agent-rally-point/apps/<slug>/`` (default; will be created by the caller).
    """
    global _legacy_warning_emitted
    override = os.environ.get("BUILD_LOOP_APPS_ROOT")
    canonical = Path(os.path.expanduser(override or DEFAULT_RALLY_POINT_APPS_ROOT)) / slug
    if canonical.exists():
        return canonical
    legacy = Path(os.path.expanduser(LEGACY_BUILD_LOOP_APPS_ROOT)) / slug
    if legacy.exists():
        if not _legacy_warning_emitted:
            print(
                f"agent-rally-watcher: using legacy channel dir {legacy} "
                f"(canonical is ~/.agent-rally-point/apps/{slug}/). "
                "Migrate by moving the directory; existing data formats match.",
                file=sys.stderr,
            )
            _legacy_warning_emitted = True
        return legacy
    return canonical


def _resolve_channel(args: argparse.Namespace) -> tuple[str, Path]:
    """Return ``(slug, channel_dir)`` from ``--channel-dir`` or cwd-derived."""
    if getattr(args, "channel_dir", None):
        p = Path(os.path.expanduser(args.channel_dir))
        slug = p.name or "_unscoped"
        return slug, p
    slug = derive_channel_slug(Path.cwd())
    return slug, _channel_dir_for(slug)


def cmd_start(args: argparse.Namespace) -> int:
    slug, channel_dir = _resolve_channel(args)
    paths = DaemonPaths.for_slug(slug)
    if not channel_dir.exists():
        print(
            f"agent-rally-watcher: channel dir {channel_dir} does not exist yet "
            "(it will be created when the first Rally Point event posts)",
            file=sys.stderr,
        )
        channel_dir.mkdir(parents=True, exist_ok=True)
    # --from-now / --from-start are a single boolean; argparse stores --from-start
    # as `from_start=True`. Default (neither flag) → from_start=False → seek-to-end.
    seek_to_end = not args.from_start
    return start_daemon(
        paths,
        channel_dir,
        foreground=args.foreground,
        seek_to_end_on_first_start=seek_to_end,
    )


def cmd_stop(args: argparse.Namespace) -> int:
    slug, _ = _resolve_channel(args)
    return stop_daemon(DaemonPaths.for_slug(slug))


def cmd_status(args: argparse.Namespace) -> int:
    slug, channel_dir = _resolve_channel(args)
    paths = DaemonPaths.for_slug(slug)
    state, pid = daemon_status(paths)
    print(f"slug:    {slug}")
    print(f"channel: {channel_dir}")
    print(f"state:   {state}")
    if pid is not None:
        print(f"pid:     {pid}")
    print(f"log:     {paths.log_file}")
    print(f"config:  {paths.consumers_config}")
    return 0 if state in ("running", "stopped") else 1


def cmd_reload(args: argparse.Namespace) -> int:
    slug, _ = _resolve_channel(args)
    return reload_daemon(DaemonPaths.for_slug(slug))


def cmd_ack_quarantine(args: argparse.Namespace) -> int:
    """ARP-007: raise a consumer's quarantine-ack watermark so its cursor
    can advance past malformed lines it stalled on. See watcher.py's module
    docstring for the full semantics — this is the documented, explicit
    "acknowledge and move on" path; there is no silent equivalent."""
    qpath = quarantine_path(args.consumer)
    through = args.through
    if through is None:
        # Default: ack everything currently on the ledger for this consumer.
        through = 0
        if qpath.exists():
            try:
                with open(qpath, "r", encoding="utf-8") as fh:
                    for line in fh:
                        try:
                            entry = json.loads(line)
                        except ValueError:
                            continue
                        offset = entry.get("offset")
                        length = entry.get("length")
                        if isinstance(offset, int) and isinstance(length, int):
                            through = max(through, offset + length)
            except OSError as e:
                print(f"agent-rally-watcher: could not read {qpath}: {e}", file=sys.stderr)
                return 1
        if through == 0:
            print(
                f"agent-rally-watcher: no quarantined lines found for consumer "
                f"'{args.consumer}' at {qpath} — nothing to acknowledge",
                file=sys.stderr,
            )
            return 1
    save_quarantine_ack(args.consumer, through)
    print(
        f"agent-rally-watcher: acknowledged quarantine for '{args.consumer}' "
        f"through byte offset {through} ({quarantine_ack_path(args.consumer)})"
    )
    print("  next sweep will skip malformed line(s) at/below this offset and resume forward progress")
    return 0


def cmd_install_launchd(args: argparse.Namespace) -> int:
    slug, channel_dir = _resolve_channel(args)
    plist_path = launchd.write_plist(slug=slug, channel_dir=channel_dir, dry_run=args.dry_run)
    if args.dry_run:
        print(plist_path)  # in dry-run, write_plist returns the plist body text
    else:
        print(f"wrote: {plist_path}")
        print(f"load:  launchctl bootstrap gui/$UID {plist_path}")
    return 0


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="agent-rally-watcher", description=__doc__)
    p.add_argument("--version", action="version", version=f"agent-rally-watcher {__version__}")
    sub = p.add_subparsers(dest="cmd", required=True)

    def _add_channel_arg(sp: argparse.ArgumentParser) -> None:
        sp.add_argument("--channel-dir", help="Override the auto-derived Rally Point channel dir")

    sp_start = sub.add_parser("start", help="Start the watcher daemon")
    _add_channel_arg(sp_start)
    sp_start.add_argument("--foreground", action="store_true", help="Run in the current process (no fork)")
    backfill = sp_start.add_mutually_exclusive_group()
    backfill.add_argument(
        "--from-now",
        dest="from_start",
        action="store_false",
        help="Seek absent cursors to EOF on first start so only new events dispatch (default)",
    )
    backfill.add_argument(
        "--from-start",
        dest="from_start",
        action="store_true",
        help="Backfill from byte 0 on first start (v0.1.0 behavior)",
    )
    sp_start.set_defaults(func=cmd_start, from_start=False)

    sp_stop = sub.add_parser("stop", help="Stop the watcher daemon")
    _add_channel_arg(sp_stop)
    sp_stop.set_defaults(func=cmd_stop)

    sp_status = sub.add_parser("status", help="Report daemon state")
    _add_channel_arg(sp_status)
    sp_status.set_defaults(func=cmd_status)

    sp_reload = sub.add_parser("reload", help="Reload consumer config (v0.1: stop + start)")
    _add_channel_arg(sp_reload)
    sp_reload.set_defaults(func=cmd_reload)

    sp_install = sub.add_parser("install-launchd", help="Write a macOS launchd plist for autostart")
    _add_channel_arg(sp_install)
    sp_install.add_argument("--dry-run", action="store_true", help="Print plist instead of writing")
    sp_install.set_defaults(func=cmd_install_launchd)

    sp_ack = sub.add_parser(
        "ack-quarantine",
        help="Acknowledge quarantined malformed lines so a stalled consumer's cursor can advance (ARP-007)",
    )
    sp_ack.add_argument("--consumer", required=True, help="Consumer id (consumers.toml table key / cursor filename)")
    sp_ack.add_argument(
        "--through",
        type=int,
        default=None,
        help="Byte offset to acknowledge through (default: everything currently on the quarantine ledger)",
    )
    sp_ack.set_defaults(func=cmd_ack_quarantine)

    return p


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
