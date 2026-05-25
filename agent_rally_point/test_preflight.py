# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Tests for agent_rally_point.preflight — host-neutral session-start check-in.

Acceptance criteria (γ1 X2):

  - AC-G1: detects pending requires_ack handoffs from inbox/<tool>.jsonl
  - AC-G2: detects pending broadcasts from inbox/all.jsonl
  - AC-G3: self-filter — entries where from == tool are NOT pending-for-self
  - AC-G4: during policy=migration, reads BOTH canonical and legacy inboxes,
           dedupes by id
  - AC-G5: routing — proceed_solo when idle, join_active when pending_acks>0
           OR active_peers>0
  - AC-G6: --start-ping writes a presence record under <channel>/rally/
  - AC-G7: graceful fallback when agent-rally-discover is unavailable
           (uses internal resolution via channel_paths/repo_id)

All tests monkeypatch the module-level CANONICAL_APPS_ROOT and
LEGACY_APPS_ROOT so the user's real ~/.agent-rally-point/ is never touched.
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
if str(_HERE.parent) not in sys.path:
    sys.path.insert(0, str(_HERE.parent))

from agent_rally_point import preflight  # noqa: E402


# ───────────────────────────────────────────────────────────────────
# Test helpers
# ───────────────────────────────────────────────────────────────────

def _init_repo(path: Path, remote: str | None = "https://github.com/owner/myproj.git") -> None:
    path.mkdir(parents=True, exist_ok=True)
    subprocess.run(["git", "init", "-q", str(path)], check=True)
    subprocess.run(["git", "-C", str(path), "config", "user.email", "t@t.test"], check=True)
    subprocess.run(["git", "-C", str(path), "config", "user.name", "t"], check=True)
    (path / "README.md").write_text("x\n")
    subprocess.run(["git", "-C", str(path), "add", "-A"], check=True)
    subprocess.run(["git", "-C", str(path), "commit", "-q", "-m", "init"], check=True)
    if remote:
        subprocess.run(["git", "-C", str(path), "remote", "add", "origin", remote], check=True)


def _make_channel(channel_dir: Path) -> None:
    """Create the minimal channel skeleton: changes.jsonl + inbox/ + rally/."""
    channel_dir.mkdir(parents=True, exist_ok=True)
    (channel_dir / "changes.jsonl").write_text("")
    (channel_dir / "inbox").mkdir(exist_ok=True)
    (channel_dir / "rally").mkdir(exist_ok=True)


def _write_inbox(inbox_dir: Path, fname: str, records: list[dict]) -> None:
    inbox_dir.mkdir(parents=True, exist_ok=True)
    path = inbox_dir / fname
    with path.open("a", encoding="utf-8") as fh:
        for rec in records:
            fh.write(json.dumps(rec) + "\n")


def _handoff(
    *,
    id_: str,
    from_: str,
    to: str | None,
    subject: str = "do the thing",
    requires_ack: bool = True,
    checkpoint_id: str | None = None,
    ts: float | None = None,
) -> dict:
    return {
        "id": id_,
        "kind": "handoff",
        "from": from_,
        "to": to,
        "requires_ack": requires_ack,
        "ts": ts if ts is not None else time.time(),
        "payload": {"subject": subject, "checkpoint_id": checkpoint_id} if checkpoint_id else {"subject": subject},
    }


def _ack(*, ref_id: str, from_: str = "codex", ts: float | None = None) -> dict:
    return {
        "id": f"ack-{ref_id}",
        "kind": "ack",
        "from": from_,
        "ts": ts if ts is not None else time.time(),
        "payload": {"ref_handoff_id": ref_id},
    }


@pytest.fixture
def isolated_roots(tmp_path, monkeypatch):
    """Redirect CANONICAL_APPS_ROOT and LEGACY_APPS_ROOT into tmp_path.

    Also monkeypatches the BUILD_LOOP_APPS_ROOT env var so the package's
    discover() returns a legacy_channel_dir under tmp_path/legacy_apps. The
    discover module is reimported fresh so it picks up the env override.
    """
    canonical = tmp_path / "canonical_apps"
    legacy = tmp_path / "legacy_apps"
    canonical.mkdir()
    legacy.mkdir()
    monkeypatch.setattr(preflight, "CANONICAL_APPS_ROOT", canonical)
    monkeypatch.setattr(preflight, "LEGACY_APPS_ROOT", legacy)
    # Point the package's channel_paths.apps_root at our canonical tmpdir so
    # discover()'s computed canonical_channel matches our test layout. (env
    # var lookup happens inside apps_root() on every call — no reload needed.)
    monkeypatch.setenv("BUILD_LOOP_APPS_ROOT", str(canonical))
    yield canonical, legacy


@pytest.fixture
def repo(tmp_path):
    repo_dir = tmp_path / "repo"
    _init_repo(repo_dir)
    return repo_dir


# ───────────────────────────────────────────────────────────────────
# AC-G1: detects pending requires_ack handoffs from inbox/<tool>.jsonl
# ───────────────────────────────────────────────────────────────────

def test_ac_g1_detects_pending_requires_ack_in_direct_inbox(isolated_roots, repo):
    canonical_root, _ = isolated_roots
    rid = preflight.compute_repo_id(repo)
    channel = canonical_root / rid
    _make_channel(channel)
    _write_inbox(
        channel / "inbox", "claude_code.jsonl",
        [_handoff(id_="h-001", from_="codex", to="claude_code", subject="please verify")],
    )

    env = preflight.build_envelope(repo, "claude_code", session_id="claude-test")

    assert env["coordination_status"] == "active"
    assert len(env["pending_acks_for_me"]) == 1
    assert env["pending_acks_for_me"][0]["id"] == "h-001"
    assert env["pending_acks_for_me"][0]["source"] == "direct"
    assert env["pending_acks_for_me"][0]["from"] == "codex"


def test_ac_g1_acked_handoff_is_not_pending(isolated_roots, repo):
    canonical_root, _ = isolated_roots
    rid = preflight.compute_repo_id(repo)
    channel = canonical_root / rid
    _make_channel(channel)
    _write_inbox(
        channel / "inbox", "claude_code.jsonl",
        [
            _handoff(id_="h-001", from_="codex", to="claude_code"),
            _ack(ref_id="h-001", from_="claude_code"),
        ],
    )

    env = preflight.build_envelope(repo, "claude_code", session_id="claude-test")
    assert env["pending_acks_for_me"] == []


def test_ac_g1_handoff_without_requires_ack_is_ignored(isolated_roots, repo):
    canonical_root, _ = isolated_roots
    rid = preflight.compute_repo_id(repo)
    channel = canonical_root / rid
    _make_channel(channel)
    _write_inbox(
        channel / "inbox", "claude_code.jsonl",
        [_handoff(id_="h-002", from_="codex", to="claude_code", requires_ack=False)],
    )

    env = preflight.build_envelope(repo, "claude_code", session_id="claude-test")
    assert env["pending_acks_for_me"] == []


# ───────────────────────────────────────────────────────────────────
# AC-G2: detects pending broadcasts from inbox/all.jsonl
# ───────────────────────────────────────────────────────────────────

def test_ac_g2_detects_broadcast_addressed_to_all(isolated_roots, repo):
    canonical_root, _ = isolated_roots
    rid = preflight.compute_repo_id(repo)
    channel = canonical_root / rid
    _make_channel(channel)
    _write_inbox(
        channel / "inbox", "all.jsonl",
        [_handoff(id_="b-001", from_="codex", to="all", subject="broadcast announce")],
    )

    env = preflight.build_envelope(repo, "claude_code", session_id="claude-test")
    assert len(env["pending_acks_for_me"]) == 1
    assert env["pending_acks_for_me"][0]["id"] == "b-001"
    assert env["pending_acks_for_me"][0]["source"] == "broadcast"


def test_ac_g2_broadcast_with_explicit_other_recipient_skipped(isolated_roots, repo):
    canonical_root, _ = isolated_roots
    rid = preflight.compute_repo_id(repo)
    channel = canonical_root / rid
    _make_channel(channel)
    _write_inbox(
        channel / "inbox", "all.jsonl",
        [_handoff(id_="b-002", from_="codex", to="cursor", subject="not for me")],
    )

    env = preflight.build_envelope(repo, "claude_code", session_id="claude-test")
    assert env["pending_acks_for_me"] == []


# ───────────────────────────────────────────────────────────────────
# AC-G3: self-filter — from == tool excluded
# ───────────────────────────────────────────────────────────────────

def test_ac_g3_self_posted_direct_handoff_is_not_pending_for_self(isolated_roots, repo):
    canonical_root, _ = isolated_roots
    rid = preflight.compute_repo_id(repo)
    channel = canonical_root / rid
    _make_channel(channel)
    _write_inbox(
        channel / "inbox", "claude_code.jsonl",
        [_handoff(id_="self-001", from_="claude_code", to="claude_code", subject="self-loop")],
    )

    env = preflight.build_envelope(repo, "claude_code", session_id="claude-test")
    assert env["pending_acks_for_me"] == []


def test_ac_g3_self_posted_broadcast_is_not_pending_for_self(isolated_roots, repo):
    canonical_root, _ = isolated_roots
    rid = preflight.compute_repo_id(repo)
    channel = canonical_root / rid
    _make_channel(channel)
    _write_inbox(
        channel / "inbox", "all.jsonl",
        [_handoff(id_="self-bcast", from_="claude_code", to="all", subject="my own broadcast")],
    )

    env = preflight.build_envelope(repo, "claude_code", session_id="claude-test")
    assert env["pending_acks_for_me"] == []


# ───────────────────────────────────────────────────────────────────
# AC-G4: migration mode — read canonical + legacy inboxes, dedupe by id
# ───────────────────────────────────────────────────────────────────

def test_ac_g4_migration_mode_reads_legacy_when_canonical_only_has_channel(isolated_roots, repo):
    """Canonical channel exists but legacy inbox has the handoff — still surfaced."""
    canonical_root, legacy_root = isolated_roots
    rid = preflight.compute_repo_id(repo)
    slug = preflight.derive_slug(repo)
    canonical_channel = canonical_root / rid
    _make_channel(canonical_channel)
    # Legacy inbox is the only one holding the handoff.
    _write_inbox(
        legacy_root / slug / "inbox", "claude_code.jsonl",
        [_handoff(id_="legacy-001", from_="codex", to="claude_code", subject="written by build-loop")],
    )

    env = preflight.build_envelope(repo, "claude_code", session_id="claude-test")
    ids = [a["id"] for a in env["pending_acks_for_me"]]
    assert "legacy-001" in ids


def test_ac_g4_migration_mode_dedupes_same_id_across_canonical_and_legacy(isolated_roots, repo):
    """Same id in both inboxes -> one entry."""
    canonical_root, legacy_root = isolated_roots
    rid = preflight.compute_repo_id(repo)
    slug = preflight.derive_slug(repo)
    canonical_channel = canonical_root / rid
    _make_channel(canonical_channel)
    # Write the SAME id to both canonical and legacy.
    _write_inbox(
        canonical_channel / "inbox", "claude_code.jsonl",
        [_handoff(id_="dup-001", from_="codex", to="claude_code", subject="dual-written")],
    )
    _write_inbox(
        legacy_root / slug / "inbox", "claude_code.jsonl",
        [_handoff(id_="dup-001", from_="codex", to="claude_code", subject="dual-written")],
    )

    env = preflight.build_envelope(repo, "claude_code", session_id="claude-test")
    matching = [a for a in env["pending_acks_for_me"] if a["id"] == "dup-001"]
    assert len(matching) == 1


def test_ac_g4_ack_in_canonical_acks_legacy_handoff(isolated_roots, repo):
    """An ACK in either inbox satisfies a handoff in either inbox."""
    canonical_root, legacy_root = isolated_roots
    rid = preflight.compute_repo_id(repo)
    slug = preflight.derive_slug(repo)
    canonical_channel = canonical_root / rid
    _make_channel(canonical_channel)
    # Handoff in legacy, ACK in canonical.
    _write_inbox(
        legacy_root / slug / "inbox", "claude_code.jsonl",
        [_handoff(id_="cross-001", from_="codex", to="claude_code")],
    )
    _write_inbox(
        canonical_channel / "inbox", "claude_code.jsonl",
        [_ack(ref_id="cross-001", from_="claude_code")],
    )

    env = preflight.build_envelope(repo, "claude_code", session_id="claude-test")
    ids = [a["id"] for a in env["pending_acks_for_me"]]
    assert "cross-001" not in ids


# ───────────────────────────────────────────────────────────────────
# AC-G5: routing decision
# ───────────────────────────────────────────────────────────────────

def test_ac_g5_routing_proceed_solo_when_idle(isolated_roots, repo):
    canonical_root, _ = isolated_roots
    rid = preflight.compute_repo_id(repo)
    channel = canonical_root / rid
    _make_channel(channel)

    env = preflight.build_envelope(repo, "claude_code", session_id="claude-test")
    assert env["coordination_status"] == "idle"
    assert env["routing"]["action"] == "proceed_solo"


def test_ac_g5_routing_join_active_when_pending_acks(isolated_roots, repo):
    canonical_root, _ = isolated_roots
    rid = preflight.compute_repo_id(repo)
    channel = canonical_root / rid
    _make_channel(channel)
    _write_inbox(
        channel / "inbox", "claude_code.jsonl",
        [_handoff(id_="h-r5", from_="codex", to="claude_code", subject="hi")],
    )

    env = preflight.build_envelope(repo, "claude_code", session_id="claude-test")
    assert env["routing"]["action"] == "join_active"
    assert "pending" in env["routing"]["reason"].lower()


def test_ac_g5_routing_join_active_when_peers_present(isolated_roots, repo):
    canonical_root, _ = isolated_roots
    rid = preflight.compute_repo_id(repo)
    channel = canonical_root / rid
    _make_channel(channel)
    # Write a fresh peer presence record.
    rally = channel / "rally"
    rally.mkdir(exist_ok=True)
    rec = {
        "session_id": "codex-peer-001",
        "tool": "codex",
        "phase": "phase-3-execute",
        "files_in_flight": ["src/a.py"],
        "ts": time.time(),
    }
    (rally / "presence-codex-peer-001.json").write_text(json.dumps(rec))

    env = preflight.build_envelope(repo, "claude_code", session_id="claude-test")
    assert env["routing"]["action"] == "join_active"
    assert "peer" in env["routing"]["reason"].lower()


# ───────────────────────────────────────────────────────────────────
# AC-G6: --start-ping writes a presence record
# ───────────────────────────────────────────────────────────────────

def test_ac_g6_start_ping_writes_presence_record(isolated_roots, repo):
    canonical_root, _ = isolated_roots
    rid = preflight.compute_repo_id(repo)
    channel = canonical_root / rid
    _make_channel(channel)

    session_id = "claude-ping-001"
    ok = preflight.write_ping(channel, session_id, "claude_code", "preflight", "hello")
    assert ok is True

    path = channel / "rally" / f"presence-{session_id}.json"
    assert path.is_file()
    rec = json.loads(path.read_text())
    assert rec["session_id"] == session_id
    assert rec["tool"] == "claude_code"
    assert rec["phase"] == "preflight"
    assert rec["ping_emitter"] == "agent-rally-preflight"
    assert "ts" in rec


def test_ac_g6_main_with_start_ping_flag_writes_record(isolated_roots, repo, capsys):
    canonical_root, _ = isolated_roots
    rid = preflight.compute_repo_id(repo)
    channel = canonical_root / rid
    _make_channel(channel)

    rc = preflight.main([
        "--workdir", str(repo),
        "--tool", "claude_code",
        "--session-id", "claude-via-main",
        "--start-ping",
    ])
    assert rc == 0
    out = capsys.readouterr().out
    env = json.loads(out)
    assert env["channel_dir"] == str(channel)

    path = channel / "rally" / "presence-claude-via-main.json"
    assert path.is_file()


# ───────────────────────────────────────────────────────────────────
# AC-G7: graceful fallback when agent-rally-discover unavailable
# ───────────────────────────────────────────────────────────────────

def test_ac_g7_fallback_to_find_channel_dir_when_discover_disabled(isolated_roots, repo, monkeypatch):
    """Force both discover paths to return None; preflight must still resolve."""
    canonical_root, _ = isolated_roots
    rid = preflight.compute_repo_id(repo)
    channel = canonical_root / rid
    _make_channel(channel)

    monkeypatch.setattr(preflight, "discover_via_package", lambda workdir: None)
    monkeypatch.setattr(preflight, "discover_via_subprocess", lambda workdir: None)

    env = preflight.build_envelope(repo, "claude_code", session_id="claude-test")
    assert env["channel_dir"] == str(channel)
    assert env["channel_source"] in ("canonical", "legacy")
    assert env["repo_id"] == rid


def test_ac_g7_legacy_only_fallback(isolated_roots, repo, monkeypatch):
    """No canonical channel; only legacy exists — find_channel_dir picks legacy."""
    _, legacy_root = isolated_roots
    slug = preflight.derive_slug(repo)
    legacy_channel = legacy_root / slug
    _make_channel(legacy_channel)
    # NOTE: do NOT create the canonical channel.

    monkeypatch.setattr(preflight, "discover_via_package", lambda workdir: None)
    monkeypatch.setattr(preflight, "discover_via_subprocess", lambda workdir: None)

    channel_dir, source = preflight.find_channel_dir(repo)
    assert channel_dir == legacy_channel
    assert source == "legacy"


def test_ac_g7_non_git_workdir_returns_none_channel_and_exits_1(tmp_path, isolated_roots, capsys, monkeypatch):
    """Workdir outside any git repo -> both resolvers degrade -> channel_dir=None -> exit 1."""
    non_git = tmp_path / "not_a_repo"
    non_git.mkdir()

    # Force the package discover off so we exercise the find_channel_dir fallback
    # which requires the directory to exist on disk. Without a git repo, the
    # fallback's compute_repo_id returns "_unscoped" and the canonical/legacy
    # directories won't exist.
    monkeypatch.setattr(preflight, "discover_via_package", lambda workdir: None)
    monkeypatch.setattr(preflight, "discover_via_subprocess", lambda workdir: None)

    rc = preflight.main([
        "--workdir", str(non_git),
        "--tool", "claude_code",
        "--session-id", "orphan-001",
    ])
    out = capsys.readouterr().out
    env = json.loads(out)
    assert env["channel_dir"] is None
    assert rc == 1


# ───────────────────────────────────────────────────────────────────
# Smoke: human output renders without crash
# ───────────────────────────────────────────────────────────────────

def test_human_output_renders(isolated_roots, repo, capsys):
    canonical_root, _ = isolated_roots
    rid = preflight.compute_repo_id(repo)
    channel = canonical_root / rid
    _make_channel(channel)

    rc = preflight.main([
        "--workdir", str(repo),
        "--tool", "claude_code",
        "--session-id", "claude-human",
        "--human",
    ])
    assert rc == 0
    out = capsys.readouterr().out
    assert "AGENT-RALLY-PREFLIGHT" in out
    assert "coordination_status" in out
    assert "routing" in out


# ───────────────────────────────────────────────────────────────────
# Workdir validation
# ───────────────────────────────────────────────────────────────────

# ───────────────────────────────────────────────────────────────────
# AC-G8..G11: build_loop_id / run_label rendering on peers + pending-ACKs
# (the preflight-render-build-loop-id slice)
# ───────────────────────────────────────────────────────────────────

def _write_presence(channel_dir: Path, *, session_id: str, tool: str = "codex",
                    phase: str = "phase-3-execute", build_loop_id: str | None = None,
                    build_loop_run_label: str | None = None,
                    ts: float | None = None) -> None:
    rally = channel_dir / "rally"
    rally.mkdir(exist_ok=True)
    rec: dict = {
        "session_id": session_id,
        "tool": tool,
        "phase": phase,
        "files_in_flight": [],
        "ts": ts if ts is not None else time.time(),
    }
    if build_loop_id is not None:
        rec["build_loop_id"] = build_loop_id
    if build_loop_run_label is not None:
        rec["build_loop_run_label"] = build_loop_run_label
    (rally / f"presence-{session_id}.json").write_text(json.dumps(rec))


def test_ac_g8_human_render_shows_run_label_on_active_peer(isolated_roots, repo, capsys):
    """AC-G1: presence with build_loop_run_label → 'run=codex#123456' in human output."""
    canonical_root, _ = isolated_roots
    rid = preflight.compute_repo_id(repo)
    channel = canonical_root / rid
    _make_channel(channel)
    _write_presence(
        channel,
        session_id="codex-peer-G8",
        tool="codex",
        build_loop_id="bl-abc123",
        build_loop_run_label="codex#482913",
    )

    rc = preflight.main([
        "--workdir", str(repo),
        "--tool", "claude_code",
        "--session-id", "claude-G8",
        "--human",
    ])
    assert rc == 0
    out = capsys.readouterr().out
    assert "ACTIVE PEERS" in out
    assert "run=codex#482913" in out


def test_ac_g9_human_render_shows_run_label_on_pending_ack(isolated_roots, repo, capsys):
    """AC-G2: handoff with build_loop_run_label → label rendered in pending-ACK line."""
    canonical_root, _ = isolated_roots
    rid = preflight.compute_repo_id(repo)
    channel = canonical_root / rid
    _make_channel(channel)
    handoff = _handoff(id_="h-G9", from_="codex", to="claude_code", subject="please verify")
    handoff["build_loop_id"] = "bl-xyz789"
    handoff["build_loop_run_label"] = "codex#482913"
    _write_inbox(channel / "inbox", "claude_code.jsonl", [handoff])

    rc = preflight.main([
        "--workdir", str(repo),
        "--tool", "claude_code",
        "--session-id", "claude-G9",
        "--human",
    ])
    assert rc == 0
    out = capsys.readouterr().out
    assert "PENDING ACKs" in out
    assert "run=codex#482913" in out


def test_ac_g10_json_envelope_carries_build_loop_id_and_run_label(isolated_roots, repo):
    """AC-G3: build_loop_id + run_label appear on active_peers[] + pending_acks_for_me[] entries."""
    canonical_root, _ = isolated_roots
    rid = preflight.compute_repo_id(repo)
    channel = canonical_root / rid
    _make_channel(channel)

    _write_presence(
        channel,
        session_id="codex-peer-G10",
        tool="codex",
        build_loop_id="bl-peer-id",
        build_loop_run_label="codex#100001",
    )
    handoff = _handoff(id_="h-G10", from_="codex", to="claude_code", subject="check")
    handoff["build_loop_id"] = "bl-ack-id"
    handoff["build_loop_run_label"] = "codex#200002"
    _write_inbox(channel / "inbox", "claude_code.jsonl", [handoff])

    env = preflight.build_envelope(repo, "claude_code", session_id="claude-G10")

    assert len(env["active_peers"]) == 1
    peer = env["active_peers"][0]
    assert peer["build_loop_id"] == "bl-peer-id"
    assert peer["run_label"] == "codex#100001"

    assert len(env["pending_acks_for_me"]) == 1
    ack = env["pending_acks_for_me"][0]
    assert ack["build_loop_id"] == "bl-ack-id"
    assert ack["run_label"] == "codex#200002"

    # AC routing — pending_acks precede peers, so target_run_label points at the ACK.
    assert env["routing"]["action"] == "join_active"
    assert env["routing"].get("target_run_label") == "codex#200002"


def test_ac_g11_backwards_compat_records_without_fields_still_render(isolated_roots, repo, capsys):
    """AC-G4: records lacking build_loop_id/run_label render existing output, no error."""
    canonical_root, _ = isolated_roots
    rid = preflight.compute_repo_id(repo)
    channel = canonical_root / rid
    _make_channel(channel)
    # Presence WITHOUT the new fields.
    _write_presence(channel, session_id="codex-peer-G11", tool="codex")
    # Handoff WITHOUT the new fields.
    _write_inbox(
        channel / "inbox", "claude_code.jsonl",
        [_handoff(id_="h-G11", from_="codex", to="claude_code", subject="legacy")],
    )

    rc = preflight.main([
        "--workdir", str(repo),
        "--tool", "claude_code",
        "--session-id", "claude-G11",
        "--human",
    ])
    assert rc == 0
    out = capsys.readouterr().out
    assert "ACTIVE PEERS" in out
    assert "PENDING ACKs" in out
    # No 'run=' tag should appear (no labels present anywhere).
    assert "run=" not in out

    # JSON path — entries carry None for the new keys.
    env = preflight.build_envelope(repo, "claude_code", session_id="claude-G11b")
    assert env["active_peers"][0]["build_loop_id"] is None
    assert env["active_peers"][0]["run_label"] is None
    assert env["pending_acks_for_me"][0]["build_loop_id"] is None
    assert env["pending_acks_for_me"][0]["run_label"] is None
    # Routing exists but has no target_run_label.
    assert "target_run_label" not in env["routing"]


def test_ac_g11_multiple_peers_with_distinct_labels_surface_as_list(isolated_roots, repo):
    """Two peers with different run_labels → routing.target_run_labels is the sorted list."""
    canonical_root, _ = isolated_roots
    rid = preflight.compute_repo_id(repo)
    channel = canonical_root / rid
    _make_channel(channel)
    _write_presence(channel, session_id="codex-A", tool="codex",
                    build_loop_run_label="codex#A1")
    _write_presence(channel, session_id="codex-B", tool="codex",
                    build_loop_run_label="codex#B2")

    env = preflight.build_envelope(repo, "claude_code", session_id="claude-multi")
    assert env["routing"]["action"] == "join_active"
    assert env["routing"].get("target_run_labels") == ["codex#A1", "codex#B2"]
    assert "target_run_label" not in env["routing"]


# ───────────────────────────────────────────────────────────────────


def test_workdir_not_a_directory_returns_2(tmp_path, capsys):
    fake = tmp_path / "does_not_exist"
    rc = preflight.main([
        "--workdir", str(fake),
        "--tool", "claude_code",
    ])
    assert rc == 2
    err = capsys.readouterr().err
    assert "workdir not a directory" in err
