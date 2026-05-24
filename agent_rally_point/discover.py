#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Agent Rally Point discovery — installed?, where's the channel?, who's live?

Sibling tools call ``discover()`` (Python) or ``agent-rally discover`` (CLI)
to resolve the active channel directory, the current revision, and the
list of live peer sessions without hardcoding paths or re-deriving the
slug. Resolution order:

    1. repo-level ``.agent-rally.toml`` overlay (opt-in, per-repo)
    2. global ``~/.agent-rally-point/manifest.toml`` (auto-created on first run)
    3. canonical default ``~/.agent-rally-point/apps/<slug>/``
    4. legacy ``~/.build-loop/apps/<slug>/`` (read-only fallback)

The discover layer is **read-only** (except for the idempotent
auto-creation of the global manifest on first read). It does not migrate
legacy channels to canonical. See ``docs/DISCOVERY.md`` for the full
contract.

Fire-and-forget tone matches the rest of the package: a discovery
failure must never crash a host action. Errors during overlay parsing,
manifest read, or peer presence read are swallowed and surface as
sensible defaults in the returned dict.
"""
from __future__ import annotations

import argparse
import datetime as _dt
import json
import os
import subprocess
import sys
import tomllib
from pathlib import Path
from typing import Any

from .channel_paths import DEFAULT_APPS_ROOT, app_slug
from .presence import read_active_presence
from .revision import read_revision


def _get_version() -> str:
    """Read package version lazily to avoid runpy circular-import warning
    when invoked as ``python3 -m agent_rally_point.discover``."""
    try:
        from . import __version__
        return __version__
    except ImportError:
        return "unknown"

_GLOBAL_MANIFEST_REL = ".agent-rally-point/manifest.toml"
_COMPAT_REL = ".agent-rally-point/compatibility.json"
_REPO_OVERLAY_NAME = ".agent-rally.toml"
_LEGACY_APPS_ROOT = "~/.build-loop/apps"
_CANONICAL_APPS_ROOT = "~/.agent-rally-point/apps"
_SCHEMA_DOC_URL = (
    "https://github.com/tyroneross/agent-rally-point/blob/main/docs/SCHEMA.md"
)
_SCHEMA_VERSION = "1.0"

# Protocol version — discovery-envelope contract version. Bumps when fields
# are added/renamed/removed in the discover() return shape. Frozen at "1.0"
# along with repo_id normalization and channel_layout policy semantics.
# See coordination-version-control.md for the three-version-field design.
_PROTOCOL_VERSION = "1.0"

# Default policy when the manifest has no [policy] section. "migration" is
# dual-aware: discover returns BOTH canonical and legacy paths, writes mirror
# to both, no silent fallback. Only after the cutover verifier passes do we
# promote to "canonical".
_DEFAULT_POLICY = "migration"
_VALID_POLICIES = ("canonical", "migration", "legacy-only")

# Compatibility-table content. Materialized at ~/.agent-rally-point/compatibility.json
# on first discover. Build-loop's bridge reads this to gate the protocol handshake.
_COMPAT_TABLE = {
    "agent_rally_point": None,  # filled in at write time from __version__
    "protocol_version": _PROTOCOL_VERSION,
    "supported_build_loop_range": ">=0.12.17,<0.14.0",
    "deprecation_notices": [],
}


def _global_manifest_path() -> Path:
    return Path(os.path.expanduser(f"~/{_GLOBAL_MANIFEST_REL}"))


def _compat_table_path() -> Path:
    return Path(os.path.expanduser(f"~/{_COMPAT_REL}"))


def _ensure_global_manifest() -> Path:
    """Create ``~/.agent-rally-point/manifest.toml`` if absent. Idempotent."""
    p = _global_manifest_path()
    if p.exists():
        return p
    try:
        p.parent.mkdir(parents=True, exist_ok=True)
        now = _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        content = (
            f'schema_version = "{_SCHEMA_VERSION}"\n'
            f'protocol_version = "{_PROTOCOL_VERSION}"\n'
            "\n"
            "[package]\n"
            'name = "agent-rally-point"\n'
            f'version = "{_get_version()}"\n'
            f'installed_at = "{now}"\n'
            "\n"
            "[paths]\n"
            f'apps_root = "{_CANONICAL_APPS_ROOT}"\n'
            f'legacy_apps_root = "{_LEGACY_APPS_ROOT}"\n'
            "\n"
            "[policy]\n"
            "# Coordination substrate policy. Values:\n"
            "#   canonical    — write only to ~/.agent-rally-point/apps/<repo_id>/\n"
            "#   migration    — DUAL-AWARE: write to both canonical and legacy,\n"
            "#                  discover returns both paths + merged_view.\n"
            "#   legacy-only  — write only to ~/.build-loop/apps/<slug>/\n"
            "# Default: migration. Promote to canonical only after\n"
            "# `agent-rally-point migrate verify-cutover` returns can_promote=true.\n"
            f'mode = "{_DEFAULT_POLICY}"\n'
            "\n"
            "[api]\n"
            'discover_module = "agent_rally_point.discover"\n'
            'cli_entry = "agent-rally"\n'
            f'schema_doc = "{_SCHEMA_DOC_URL}"\n'
            "\n"
            "[defaults]\n"
            "heartbeat_minutes = 15\n"
            "stale_session_seconds = 3600\n"
        )
        p.write_text(content)
    except OSError:
        return p
    return p


def _ensure_compatibility_table() -> Path:
    """Materialize ``~/.agent-rally-point/compatibility.json`` if absent.

    The table documents which build-loop versions this agent-rally-point
    expects to handshake with. Build-loop's discovery bridge reads this
    file at session start to gate the protocol version check. Fire-and-
    forget — never raises if the home dir is read-only.
    """
    p = _compat_table_path()
    if p.exists():
        return p
    try:
        p.parent.mkdir(parents=True, exist_ok=True)
        table = dict(_COMPAT_TABLE)
        table["agent_rally_point"] = _get_version()
        p.write_text(json.dumps(table, indent=2, sort_keys=True) + "\n")
    except OSError:
        return p
    return p


def _resolve_policy(global_manifest: dict, repo_overlay: dict) -> tuple[str, str]:
    """Return (policy_mode, source). Source ∈ {env, repo, global, default}.

    Env override: ``$AGENT_RALLY_POLICY`` ∈ {canonical, migration, legacy-only}.
    Invalid values silently fall through to the next layer.
    """
    env_override = os.environ.get("AGENT_RALLY_POLICY")
    if env_override in _VALID_POLICIES:
        return env_override, "env"
    repo_policy = repo_overlay.get("policy", {}).get("mode")
    if repo_policy in _VALID_POLICIES:
        return str(repo_policy), "repo"
    global_policy = global_manifest.get("policy", {}).get("mode")
    if global_policy in _VALID_POLICIES:
        return str(global_policy), "global"
    return _DEFAULT_POLICY, "default"


def _load_toml(path: Path) -> dict:
    try:
        with open(path, "rb") as fh:
            return tomllib.load(fh)
    except (FileNotFoundError, OSError, tomllib.TOMLDecodeError):
        return {}


def _repo_root(cwd: Path) -> Path | None:
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            cwd=str(cwd),
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
    except (subprocess.CalledProcessError, OSError):
        return None
    return Path(out) if out else None


def _resolve_apps_root(repo_overlay: dict, global_manifest: dict) -> tuple[Path, str]:
    """Return (apps_root_path, source) where source ∈ {repo, global, env, default}."""
    env_override = os.environ.get("BUILD_LOOP_APPS_ROOT")
    if env_override:
        return Path(os.path.expanduser(env_override)), "env"
    repo_root_path = repo_overlay.get("paths", {}).get("apps_root")
    if repo_root_path:
        return Path(os.path.expanduser(repo_root_path)), "repo"
    global_root = global_manifest.get("paths", {}).get("apps_root")
    if global_root:
        return Path(os.path.expanduser(global_root)), "global"
    return Path(os.path.expanduser(DEFAULT_APPS_ROOT)), "default"


def _resolve_slug(cwd: Path, repo_overlay: dict) -> tuple[str, str]:
    """Return (slug, source) where source ∈ {repo, derived}."""
    repo_slug = repo_overlay.get("channel", {}).get("slug")
    if repo_slug:
        return str(repo_slug), "repo"
    return app_slug(cwd), "derived"


def discover(cwd: Path | str | None = None) -> dict[str, Any]:
    """Resolve the active Rally Point layout for ``cwd``.

    Returns a dict with the full discovery envelope (see ``docs/DISCOVERY.md``).
    Never raises — a misconfigured overlay or missing manifest degrades to
    sensible defaults; ``installed`` reflects whether any layer resolved.
    """
    cwd_path = Path(os.path.expanduser(str(cwd))) if cwd else Path.cwd()

    # Layer 1: ensure + read global manifest (idempotent).
    manifest_path = _ensure_global_manifest()
    global_manifest = _load_toml(manifest_path)

    # Materialize the build-loop ↔ agent-rally-point compatibility table (idempotent).
    # Build-loop's discovery bridge reads this file to gate the protocol handshake.
    _ensure_compatibility_table()

    # Layer 2: read repo overlay (if present at canonical repo root).
    repo_root = _repo_root(cwd_path)
    repo_overlay: dict = {}
    overlay_path: Path | None = None
    if repo_root is not None:
        candidate = repo_root / _REPO_OVERLAY_NAME
        if candidate.exists():
            overlay_path = candidate
            repo_overlay = _load_toml(candidate)

    # Resolve apps_root + slug + policy with overlay precedence.
    apps_root_path, apps_root_source = _resolve_apps_root(repo_overlay, global_manifest)
    slug, slug_source = _resolve_slug(cwd_path, repo_overlay)
    policy_mode, policy_source = _resolve_policy(global_manifest, repo_overlay)

    # Canonical channel dir under the resolved apps_root.
    canonical_channel = apps_root_path / slug

    # Legacy fallback: only if canonical does NOT exist on disk.
    legacy_root = Path(os.path.expanduser(_LEGACY_APPS_ROOT))
    legacy_channel = legacy_root / slug
    channel_dir = canonical_channel
    channel_layout = "canonical"
    channel_source = "default"
    if not canonical_channel.exists() and legacy_channel.exists():
        channel_dir = legacy_channel
        channel_layout = "legacy"
        channel_source = "legacy-fallback"
    elif apps_root_source != "default":
        channel_source = apps_root_source

    # Live state — never blocks, swallows errors.
    try:
        active_revision = read_revision(channel_dir)
    except Exception:  # noqa: BLE001 — discovery must not crash a host
        active_revision = 0
    try:
        # exclude_session=""; discover() is called outside a session context.
        active_peers = read_active_presence(channel_dir, exclude_session="")
    except Exception:  # noqa: BLE001
        active_peers = []

    installed = (
        manifest_path.exists()
        or canonical_channel.exists()
        or legacy_channel.exists()
    )

    now_iso = _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    return {
        "installed": bool(installed),
        "version": _get_version(),
        "schema_version": _SCHEMA_VERSION,
        "protocol_version": _PROTOCOL_VERSION,
        "policy": policy_mode,
        "last_resolved_at": now_iso,
        "channel_dir": str(channel_dir),
        "channel_layout": channel_layout,
        "app_slug": slug,
        "apps_root": str(apps_root_path),
        "active_revision": int(active_revision),
        "active_peers": list(active_peers),
        "schema_doc_url": _SCHEMA_DOC_URL,
        "apis": {
            "post": "agent_rally_point.post:post",
            "checkpoint_read": "agent_rally_point.checkpoint:checkpoint_read",
            "discover": "agent_rally_point.discover:discover",
        },
        "sources": {
            "channel_dir": channel_source,
            "policy": policy_source,
            "apps_root": apps_root_source,
            "app_slug": slug_source,
            "manifest_global": str(manifest_path) if manifest_path.exists() else None,
            "overlay_repo": str(overlay_path) if overlay_path else None,
        },
    }


def _main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="agent-rally discover",
        description="Resolve the active Rally Point layout for the current cwd.",
    )
    parser.add_argument("--field", metavar="NAME", help="Print only this field.")
    parser.add_argument(
        "--quiet",
        action="store_true",
        help="Suppress output; exit 0 if installed, 1 if not.",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit JSON (default). Explicit flag is a no-op.",
    )
    args = parser.parse_args(argv)

    info = discover()

    if args.quiet:
        return 0 if info["installed"] else 1

    if args.field:
        if args.field not in info:
            print(f"unknown field: {args.field}", file=sys.stderr)
            return 1
        value = info[args.field]
        if isinstance(value, (dict, list)):
            print(json.dumps(value, indent=2, sort_keys=True))
        else:
            print(value)
        return 0 if info["installed"] else 1

    print(json.dumps(info, indent=2, sort_keys=True))
    return 0 if info["installed"] else 1


if __name__ == "__main__":
    sys.exit(_main())
