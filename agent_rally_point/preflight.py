#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""agent-rally-preflight — host-neutral session-start coordination check-in.

The preflight is the single executable line every AI coding agent (Claude Code,
Codex, Cursor, Gemini, CI verifiers) runs at session start to:

1. Identify the canonical channel for the current repo (via :mod:`discover`).
2. Surface active coordination state (live peers, pending ACKs, recent activity).
3. Load shared context — north-star intent/goal, memory locations, guardrails.
4. Decide routing: ``join_active`` if peers or pending ACKs exist, else ``proceed_solo``.
5. Emit a structured envelope on stdout that the host LLM consumes.
6. Optionally write a presence record so peers see this session immediately.

Stdlib-only for the operational paths so the script works even in degraded
environments where parts of the package fail to import. When
:mod:`agent_rally_point.discover` is importable, it is preferred over the local
substrate scan — the discovery envelope already implements the canonical /
migration / legacy-only policy and worktree/clone-stable repo_id.

Usage
-----

::

    agent-rally-preflight                       # JSON envelope on stdout
    agent-rally-preflight --human               # human-readable summary
    agent-rally-preflight --workdir PATH        # override cwd
    agent-rally-preflight --tool TOOL           # claude_code | codex | cursor | ci
    agent-rally-preflight --session-id ID       # explicit (else random)
    agent-rally-preflight --start-ping          # write a presence record

Exit codes:

    ``0``  preflight complete, envelope on stdout (idle or coordinated)
    ``1``  no channel resolved (repo not under git, or substrate absent)
    ``2``  workdir not a directory
"""
from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
import re
import subprocess
import sys
import time
import uuid
from pathlib import Path
from typing import Any

SCHEMA_VERSION = "0.1"
PREFLIGHT_VERSION = "0.1.0"

CANONICAL_APPS_ROOT = Path("~/.agent-rally-point/apps").expanduser()
LEGACY_APPS_ROOT = Path("~/.build-loop/apps").expanduser()
BUILDLOOP_MEMORY_GLOBAL = Path("~/.build-loop/memory").expanduser()
CLAUDE_MEMORY_GLOBAL = Path("~/.claude/projects/-Users-tyroneross/memory/MEMORY.md").expanduser()


# ───────────────────────────────────────────────────────────────────
# Repo identification — prefer the package modules, fall back to stdlib
# ───────────────────────────────────────────────────────────────────

def _derive_slug_fallback(workdir: Path) -> str:
    """Stdlib-only basename slug, used when ``channel_paths`` is unavailable."""
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--git-common-dir"],
            cwd=str(workdir), check=True, capture_output=True, text=True,
        ).stdout.strip()
    except (subprocess.CalledProcessError, OSError):
        return "_unscoped"
    if not out:
        return "_unscoped"
    common = Path(out)
    if not common.is_absolute():
        common = workdir / common
    try:
        repo_root = common.resolve().parent
    except (OSError, RuntimeError):
        return "_unscoped"
    base = re.sub(r"[^a-z0-9._-]", "-", repo_root.name.lower())
    return re.sub(r"-{2,}", "-", base).strip("-")[:64] or "_unscoped"


def derive_slug(workdir: Path) -> str:
    """Return the worktree-independent repo slug for ``workdir``.

    Delegates to :func:`agent_rally_point.channel_paths.app_slug` when available
    (one source of truth across the package); falls back to a stdlib-only
    basename derivation otherwise. Never raises.
    """
    try:
        from .channel_paths import app_slug  # type: ignore[import-not-found]
        return app_slug(workdir)
    except Exception:  # noqa: BLE001 — fall back is the whole point
        return _derive_slug_fallback(workdir)


def _compute_repo_id_fallback(workdir: Path) -> str:
    """Stdlib-only repo-id, used when ``repo_id`` module is unavailable.

    Mirrors :func:`agent_rally_point.repo_id.repo_id`: slug + 8-hex content
    hash of the normalized git remote URL when present, otherwise hash of the
    resolved repo-root path.
    """
    slug = _derive_slug_fallback(workdir)
    if slug == "_unscoped":
        return "_unscoped"
    try:
        remote = subprocess.run(
            ["git", "remote", "get-url", "origin"],
            cwd=str(workdir), check=True, capture_output=True, text=True,
        ).stdout.strip()
    except (subprocess.CalledProcessError, OSError):
        remote = ""
    if remote:
        norm = remote.lower().rstrip("/")
        if norm.endswith(".git"):
            norm = norm[:-4]
        seed = norm
    else:
        try:
            seed = str(Path(workdir).resolve())
        except OSError:
            seed = str(workdir)
    h = hashlib.sha256(seed.encode("utf-8")).hexdigest()[:8]
    return f"{slug}-{h}"


def compute_repo_id(workdir: Path) -> str:
    """Return the worktree-stable, clone-stable repo id.

    Delegates to :func:`agent_rally_point.repo_id.repo_id` when available so the
    preflight, discover, and migrate tools all agree on identity. Falls back to
    a stdlib-only computation when the package module is unimportable.
    """
    try:
        from .repo_id import repo_id  # type: ignore[import-not-found]
        return repo_id(workdir)
    except Exception:  # noqa: BLE001
        return _compute_repo_id_fallback(workdir)


def discover_via_package(workdir: Path) -> dict | None:
    """Prefer the in-process :func:`discover` call when importable.

    Returns the discovery envelope or ``None`` on any failure. Importing
    succeeds whenever the package is installed (entry-point CLI is by
    definition installed), so this is the normal path.
    """
    try:
        from .discover import discover  # type: ignore[import-not-found]
        return discover(workdir)
    except Exception:  # noqa: BLE001
        return None


def discover_via_subprocess(workdir: Path) -> dict | None:
    """Fallback when the in-process call fails — shell out to the CLI.

    Used in degraded environments (broken venv, partial install). Returns
    ``None`` on any failure rather than raising.
    """
    try:
        result = subprocess.run(
            ["agent-rally-discover", "--json"],
            cwd=str(workdir), capture_output=True, text=True, timeout=3,
        )
        if result.returncode != 0:
            return None
        return json.loads(result.stdout)
    except (subprocess.SubprocessError, OSError, json.JSONDecodeError):
        return None


# ───────────────────────────────────────────────────────────────────
# Channel + coordination state
# ───────────────────────────────────────────────────────────────────

def find_channel_dir(workdir: Path) -> tuple[Path | None, str]:
    """Find the active channel directory without the discover envelope.

    Used only when both :func:`discover_via_package` and
    :func:`discover_via_subprocess` fail. Prefers canonical
    ``~/.agent-rally-point/apps/<repo_id>/`` then falls back to legacy
    ``~/.build-loop/apps/<slug>/``.
    """
    repo_id = compute_repo_id(workdir)
    slug = derive_slug(workdir)
    canonical = CANONICAL_APPS_ROOT / repo_id
    legacy = LEGACY_APPS_ROOT / slug

    if canonical.is_dir() and (canonical / "changes.jsonl").is_file():
        return canonical, "canonical"
    if legacy.is_dir() and (legacy / "changes.jsonl").is_file():
        return legacy, "legacy"
    return None, "none"


def read_recent_changes(channel_dir: Path, last_n: int = 10) -> list[dict]:
    changes_path = channel_dir / "changes.jsonl"
    if not changes_path.is_file():
        return []
    try:
        lines = changes_path.read_text(encoding="utf-8").splitlines()
    except OSError:
        return []
    out = []
    for line in lines[-last_n:]:
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return out


def read_active_peers(
    channel_dir: Path,
    exclude_session: str | None = None,
    ttl_min: float = 15,
) -> list[dict]:
    """Return live presence records for the channel.

    Stdlib path so it works even when :mod:`presence` cannot import. TTL
    filter mirrors :func:`agent_rally_point.presence.read_active_presence`
    (heartbeat default 15 min).
    """
    rally_dir = channel_dir / "rally"
    if not rally_dir.is_dir():
        return []
    cutoff = time.time() - ttl_min * 60
    peers = []
    try:
        for path in rally_dir.glob("presence-*.json"):
            try:
                rec = json.loads(path.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                continue
            if rec.get("ts", 0) < cutoff:
                continue
            if exclude_session and rec.get("session_id") == exclude_session:
                continue
            peers.append({
                "session_id": rec.get("session_id"),
                "tool": rec.get("tool"),
                "phase": rec.get("phase"),
                "files_in_flight": rec.get("files_in_flight", []),
                "age_seconds": int(time.time() - rec.get("ts", 0)),
            })
    except OSError:
        pass
    return peers


def _read_jsonl(path: Path) -> list[dict]:
    if not path.is_file():
        return []
    out = []
    try:
        for line in path.read_text(encoding="utf-8").splitlines():
            if not line.strip():
                continue
            try:
                out.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    except OSError:
        pass
    return out


def _candidate_inbox_dirs(channel_dir: Path) -> list[Path]:
    """Return every inbox directory the preflight should scan.

    During ``policy: migration`` both canonical and legacy inboxes can hold
    live messages. The caller dedupes by record ``id`` (latest write wins).
    """
    candidates: list[Path] = []
    if channel_dir and (channel_dir / "inbox").is_dir():
        candidates.append(channel_dir / "inbox")
    canonical_root = CANONICAL_APPS_ROOT
    legacy_root = LEGACY_APPS_ROOT
    if channel_dir and canonical_root in channel_dir.parents:
        # When inspecting a canonical channel, also probe legacy inboxes (the
        # cutover-verifier path) — build-loop's pre-cutover writers may still
        # be writing to ~/.build-loop/apps/<slug>/inbox/.
        for app_dir in (legacy_root.iterdir() if legacy_root.is_dir() else []):
            inbox = app_dir / "inbox"
            if inbox.is_dir():
                candidates.append(inbox)
    return candidates


def read_pending_acks(channel_dir: Path, tool: str) -> list[dict]:
    """Return ``requires_ack`` messages addressed to ``tool`` still unacked.

    A message is *pending* when its ``id`` (or its ``revision``) does not
    appear as ``ref_handoff_id`` / ``ack_for_revision`` in any later
    ``kind=ack`` record across any candidate inbox.

    Implements the self-filter (``rec["from"] == tool`` is excluded) and the
    dual-inbox dedupe (canonical + legacy merged, latest write wins per
    ``id``).
    """
    inbox_dirs = _candidate_inbox_dirs(channel_dir)
    direct_by_id: dict[str, dict] = {}
    broadcast_by_id: dict[str, dict] = {}
    outbound_by_id: dict[str, dict] = {}
    for inbox_dir in inbox_dirs:
        for rec in _read_jsonl(inbox_dir / f"{tool}.jsonl"):
            rid = rec.get("id")
            if rid:
                direct_by_id[rid] = rec
        for rec in _read_jsonl(inbox_dir / "all.jsonl"):
            rid = rec.get("id")
            if rid:
                broadcast_by_id[rid] = rec
        for f in inbox_dir.glob("*.jsonl"):
            if f.name in (f"{tool}.jsonl", "all.jsonl"):
                continue
            for rec in _read_jsonl(f):
                rid = rec.get("id")
                if rid:
                    outbound_by_id[rid] = rec
    direct_in = list(direct_by_id.values())
    broadcast_in = list(broadcast_by_id.values())
    outbound = list(outbound_by_id.values())

    acked_ids: set[str] = set()
    acked_revs: set[int] = set()
    for rec in direct_in + broadcast_in + outbound:
        if rec.get("kind") != "ack":
            continue
        payload = rec.get("payload") or {}
        ref_id = payload.get("ref_handoff_id")
        if isinstance(ref_id, str):
            acked_ids.add(ref_id)
        ref_rev = payload.get("ack_for_revision")
        if isinstance(ref_rev, (int, str)):
            try:
                acked_revs.add(int(ref_rev))
            except (ValueError, TypeError):
                pass

    pending = []
    for source_name, recs in (("direct", direct_in), ("broadcast", broadcast_in)):
        for rec in recs:
            if rec.get("kind") not in ("handoff", "escalation"):
                continue
            if not rec.get("requires_ack"):
                continue
            # SELF-FILTER: a tool never ACKs itself.
            if rec.get("from") == tool:
                continue
            to = rec.get("to")
            if source_name == "direct" and to and to != tool:
                continue
            if source_name == "broadcast" and to and to not in ("all", tool):
                continue
            rec_id = rec.get("id")
            if rec_id in acked_ids:
                continue
            payload = rec.get("payload") or {}
            rev = payload.get("revision") or payload.get("ref_revision")
            if isinstance(rev, (int, str)):
                try:
                    if int(rev) in acked_revs:
                        continue
                except (ValueError, TypeError):
                    pass
            subject = payload.get("subject")
            if not isinstance(subject, str):
                subject = "(no subject)"
            pending.append({
                "id": rec_id,
                "source": source_name,
                "kind": rec.get("kind"),
                "from": rec.get("from"),
                "to": rec.get("to"),
                "subject": subject[:80],
                "checkpoint_id": payload.get("checkpoint_id"),
                "age_seconds": int(time.time() - rec.get("ts", 0)),
            })
    return pending


# ───────────────────────────────────────────────────────────────────
# Shared context loading
# ───────────────────────────────────────────────────────────────────

def load_north_star(workdir: Path) -> dict:
    """Load ``intent.md`` and ``goal.md`` from ``.build-loop/`` if present."""
    bl = workdir / ".build-loop"
    out: dict[str, Any] = {}
    if not bl.is_dir():
        return out
    for fname, key in (("intent.md", "intent"), ("goal.md", "goal")):
        p = bl / fname
        if p.is_file():
            try:
                content = p.read_text(encoding="utf-8")
                out[key] = content[:2000]
                out[f"{key}_path"] = str(p)
            except OSError:
                pass
    return out


def memory_locations(workdir: Path) -> dict:
    """Return shared + project-scoped memory locations that exist on disk."""
    slug = derive_slug(workdir)
    out = {
        "global_buildloop": str(BUILDLOOP_MEMORY_GLOBAL),
        "project_buildloop": str(BUILDLOOP_MEMORY_GLOBAL / "projects" / slug),
        "claude_global_index": str(CLAUDE_MEMORY_GLOBAL),
        "project_intent_dir": str(workdir / ".build-loop") if (workdir / ".build-loop").is_dir() else None,
    }
    return {k: v for k, v in out.items() if v and Path(v).exists()}


def load_guardrails(workdir: Path) -> list[str]:
    """List guardrail document paths to remind the host LLM about."""
    guards = []
    slug = derive_slug(workdir)
    lanes = BUILDLOOP_MEMORY_GLOBAL / "projects" / slug / "coordination-lanes-and-fallbacks.md"
    if lanes.is_file():
        guards.append(f"Lane policy: {lanes}")
    claude_md = workdir / "CLAUDE.md"
    if claude_md.is_file():
        guards.append(f"Project rules: {claude_md}")
    global_claude_md = Path("~/.claude/CLAUDE.md").expanduser()
    if global_claude_md.is_file():
        guards.append(f"Global rules: {global_claude_md}")
    return guards


# ───────────────────────────────────────────────────────────────────
# Cross-repo summary
# ───────────────────────────────────────────────────────────────────

def all_repos_active() -> list[dict]:
    """Return a recency-sorted summary of every channel the user has used."""
    summaries: dict[str, dict] = {}
    for root in (CANONICAL_APPS_ROOT, LEGACY_APPS_ROOT):
        if not root.is_dir():
            continue
        for app_dir in root.iterdir():
            if not app_dir.is_dir():
                continue
            changes = app_dir / "changes.jsonl"
            if not changes.is_file():
                continue
            try:
                mtime = changes.stat().st_mtime
            except OSError:
                continue
            age_min = (time.time() - mtime) / 60
            key = app_dir.name
            existing = summaries.get(key)
            if existing is None or mtime > existing["_mtime"]:
                summaries[key] = {
                    "channel": app_dir.name,
                    "channel_dir": str(app_dir),
                    "layout": "canonical" if root == CANONICAL_APPS_ROOT else "legacy",
                    "last_event_min_ago": round(age_min, 1),
                    "_mtime": mtime,
                }
    out = sorted(summaries.values(), key=lambda x: x["_mtime"], reverse=True)
    for entry in out:
        entry.pop("_mtime", None)
    return out


# ───────────────────────────────────────────────────────────────────
# Routing decision
# ───────────────────────────────────────────────────────────────────

def routing_decision(
    active_peers: list[dict],
    pending_acks: list[dict],
    all_repos: list[dict],
) -> dict:
    """Decide whether to join active coordination or proceed solo.

    Order of precedence: pending ACKs (handle the inbox first) > active peers
    (coordinate before parallel work) > proceed solo with periodic pings.
    """
    if pending_acks:
        return {
            "action": "join_active",
            "reason": f"{len(pending_acks)} pending ACK(s) addressed to this tool — handle before new work",
            "join_target": pending_acks[0],
        }
    if active_peers:
        return {
            "action": "join_active",
            "reason": f"{len(active_peers)} peer(s) actively working in this channel — coordinate before parallel work",
            "join_target": active_peers,
        }
    return {
        "action": "proceed_solo",
        "reason": "No active peers and no pending ACKs — proceed with assigned task, log ping check-ins to substrate",
        "all_repos_glance": all_repos[:5],
    }


# ───────────────────────────────────────────────────────────────────
# Ping check-in
# ───────────────────────────────────────────────────────────────────

def write_ping(
    channel_dir: Path,
    session_id: str,
    tool: str,
    phase: str,
    message: str,
) -> bool:
    """Write a presence record under ``<channel_dir>/rally/`` atomically.

    Returns ``True`` on success, ``False`` on any I/O error (fire-and-forget).
    """
    rally = channel_dir / "rally"
    try:
        rally.mkdir(parents=True, exist_ok=True)
    except OSError:
        return False
    rec = {
        "schema_version": "1.0",
        "session_id": session_id,
        "tool": tool,
        "phase": phase,
        "message": message,
        "ts": time.time(),
        "ping_emitter": "agent-rally-preflight",
    }
    path = rally / f"presence-{session_id}.json"
    tmp = path.with_suffix(".tmp")
    try:
        tmp.write_text(json.dumps(rec, indent=2), encoding="utf-8")
        os.replace(tmp, path)
        return True
    except OSError:
        return False


# ───────────────────────────────────────────────────────────────────
# Main envelope
# ───────────────────────────────────────────────────────────────────

def build_envelope(workdir: Path, tool: str, session_id: str) -> dict:
    """Assemble the full preflight envelope for ``(workdir, tool, session)``."""
    discovered = discover_via_package(workdir)
    channel_source = "agent-rally-point.discover"
    if discovered is None:
        discovered = discover_via_subprocess(workdir)
        channel_source = "agent-rally-discover-cli" if discovered else "none"

    channel_dir: Path | None = None
    canonical_channel_dir: str | None = None
    repo_id: str | None = None

    if discovered:
        ch = discovered.get("channel_dir")
        channel_dir = Path(ch) if ch else None
        canonical_channel_dir = discovered.get("canonical_channel_dir")
        repo_id = discovered.get("repo_id")
    else:
        channel_dir, channel_source = find_channel_dir(workdir)
        repo_id = compute_repo_id(workdir)

    active_peers: list[dict] = []
    pending_acks: list[dict] = []
    recent_changes: list[dict] = []
    if channel_dir and channel_dir.is_dir():
        active_peers = read_active_peers(channel_dir, exclude_session=session_id)
        pending_acks = read_pending_acks(channel_dir, tool=tool)
        recent_changes_full = read_recent_changes(channel_dir, last_n=5)
        recent_changes = [
            {
                "revision": r.get("revision"),
                "kind": r.get("kind"),
                "from": r.get("author_tool") or r.get("from"),
                "subject": (r.get("payload") or {}).get("subject", "")[:60],
            }
            for r in recent_changes_full
        ]

    north_star = load_north_star(workdir)
    memory = memory_locations(workdir)
    guardrails = load_guardrails(workdir)
    all_repos = all_repos_active()
    routing = routing_decision(active_peers, pending_acks, all_repos)

    return {
        "schema_version": SCHEMA_VERSION,
        "preflight_version": PREFLIGHT_VERSION,
        "ts": datetime.datetime.now(datetime.UTC).isoformat().replace("+00:00", "Z"),
        "session_id": session_id,
        "tool": tool,
        "workdir": str(workdir),
        "repo_id": repo_id,
        "channel_dir": str(channel_dir) if channel_dir else None,
        "canonical_channel_dir": canonical_channel_dir,
        "channel_source": channel_source,
        "coordination_status": "active" if (active_peers or pending_acks) else "idle",
        "active_peers": active_peers,
        "pending_acks_for_me": pending_acks,
        "recent_changes": recent_changes,
        "north_star": north_star,
        "memory_locations": memory,
        "guardrails": guardrails,
        "all_repos_active": all_repos,
        "routing": routing,
        "log_target": str(channel_dir / "inbox" / f"{tool}.jsonl") if channel_dir else None,
    }


# ───────────────────────────────────────────────────────────────────
# Human-readable output
# ───────────────────────────────────────────────────────────────────

def render_human(env: dict) -> str:
    L = []
    L.append("=" * 70)
    L.append(f" AGENT-RALLY-PREFLIGHT v{env['preflight_version']}  {env['ts']}")
    L.append("=" * 70)
    L.append(f" tool:       {env['tool']}")
    L.append(f" session_id: {env['session_id']}")
    L.append(f" workdir:    {env['workdir']}")
    L.append(f" repo_id:    {env['repo_id']}")
    L.append(f" channel:    {env['channel_dir']}  [via {env['channel_source']}]")
    L.append("")
    L.append(f" coordination_status: {env['coordination_status'].upper()}")
    routing = env['routing']
    L.append(f" routing:    {routing['action']} - {routing['reason']}")
    L.append("")
    if env['pending_acks_for_me']:
        L.append(f" PENDING ACKs for {env['tool']}  ({len(env['pending_acks_for_me'])} total):")
        for a in env['pending_acks_for_me']:
            cp = f"checkpoint_id={a.get('checkpoint_id')}" if a.get('checkpoint_id') else ""
            from_str = str(a.get('from') or '?')
            L.append(f"   [{a['source']:9}] {a['kind']:9} from={from_str:8} age={a['age_seconds']:5}s  {a['subject']}  {cp}")
        L.append("")
    if env['active_peers']:
        L.append(" ACTIVE PEERS:")
        for p in env['active_peers']:
            sid = (p.get('session_id') or '?')[:32]
            L.append(f"   {sid} ({p.get('tool')}) phase={p.get('phase')}  age={p.get('age_seconds')}s")
        L.append("")
    if env['north_star']:
        L.append(" NORTH STAR:")
        ns = env['north_star']
        if ns.get('intent_path'):
            L.append(f"   intent.md:  {ns['intent_path']}")
        if ns.get('goal_path'):
            L.append(f"   goal.md:    {ns['goal_path']}")
        L.append("")
    if env['memory_locations']:
        L.append(" MEMORY:")
        for k, v in env['memory_locations'].items():
            L.append(f"   {k}: {v}")
        L.append("")
    if env['guardrails']:
        L.append(" GUARDRAILS:")
        for g in env['guardrails']:
            L.append(f"   - {g}")
        L.append("")
    if env['recent_changes']:
        L.append(" RECENT CHANGES:")
        for r in env['recent_changes']:
            L.append(f"   rev {r['revision']} {r['kind']} from={r['from']}: {r['subject']}")
        L.append("")
    L.append(" all_repos_active:")
    for repo in env['all_repos_active'][:8]:
        L.append(f"   {repo['channel']}  [{repo['layout']}]  last_event={repo['last_event_min_ago']}min ago")
    L.append("=" * 70)
    return "\n".join(L)


# ───────────────────────────────────────────────────────────────────
# Entry point
# ───────────────────────────────────────────────────────────────────

def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(
        prog="agent-rally-preflight",
        description="Host-neutral session-start coordination check-in.",
    )
    p.add_argument("--workdir", default=".")
    p.add_argument("--tool", default=os.environ.get("AGENT_RALLY_TOOL", "claude_code"))
    p.add_argument("--session-id", default=None)
    p.add_argument("--human", action="store_true",
                   help="Emit a human-readable summary instead of JSON.")
    p.add_argument("--start-ping", action="store_true",
                   help="Write a single presence record under <channel>/rally/.")
    p.add_argument("--ping-message", default="session-start preflight")
    p.add_argument("--ping-phase", default="preflight")
    args = p.parse_args(argv)

    workdir = Path(args.workdir).expanduser().resolve()
    if not workdir.is_dir():
        print(f"error: workdir not a directory: {workdir}", file=sys.stderr)
        return 2

    session_id = args.session_id or f"{args.tool}-{uuid.uuid4().hex[:16]}-{int(time.time())}"

    env = build_envelope(workdir, args.tool, session_id)

    if args.start_ping and env['channel_dir']:
        write_ping(
            Path(env['channel_dir']), session_id,
            args.tool, args.ping_phase, args.ping_message,
        )

    if args.human:
        print(render_human(env))
    else:
        print(json.dumps(env, indent=2))

    if env['channel_dir'] is None:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
