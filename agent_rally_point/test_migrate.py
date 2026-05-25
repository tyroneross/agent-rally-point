# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Tests for migrate.py — legacy→canonical channel migration + cutover verifier."""
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
    yield home


@pytest.fixture
def fresh_migrate(isolated_home, monkeypatch):
    # Default tests run with no repo-search-paths matches — every channel
    # falls into the "unmatched" path. Tests that exercise the matched
    # path explicitly set AGENT_RALLY_REPO_SEARCH_PATHS.
    monkeypatch.setenv("AGENT_RALLY_REPO_SEARCH_PATHS", str(isolated_home / "_no_such_dir"))
    for mod in (
        "agent_rally_point.migrate",
        "agent_rally_point.repo_id",
        "agent_rally_point.channel_paths",
    ):
        if mod in sys.modules:
            del sys.modules[mod]
    import agent_rally_point.migrate as m
    return m


def _make_legacy_channel(home: Path, slug: str, files: dict[str, str]) -> Path:
    """Materialize a fake legacy channel under home/.build-loop/apps/<slug>/."""
    ch = home / ".build-loop" / "apps" / slug
    ch.mkdir(parents=True)
    for rel, content in files.items():
        p = ch / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(content)
    return ch


def test_scan_lists_legacy_channels(fresh_migrate, isolated_home):
    _make_legacy_channel(isolated_home, "app-a", {"revision": "5\n"})
    _make_legacy_channel(isolated_home, "app-b", {
        "revision": "7\n", "changes.jsonl": '{"kind":"phase"}\n',
    })
    channels = fresh_migrate.discover_legacy_channels()
    slugs = {ch["slug"] for ch in channels}
    assert slugs == {"app-a", "app-b"}
    # No repo-search-paths match these fake slugs → unmatched naming.
    for ch in channels:
        assert ch["canonical_repo_id"].startswith(ch["slug"] + "-unmatched-")
        assert ch["match_status"] == "unmatched"
        assert ch["repo_path"] is None
        assert "/.agent-rally-point/apps/" in ch["canonical_path"]


def test_apply_migrates_files_and_writes_log(fresh_migrate, isolated_home):
    _make_legacy_channel(isolated_home, "app-a", {
        "revision": "5\n",
        "changes.jsonl": '{"kind":"phase","payload":{}}\n',
        "sessions/sess-1.json": '{"session_id":"sess-1"}',
    })
    result = fresh_migrate.apply_migration()
    assert result["failures"] == 0
    assert result["channels_total"] == 1
    outcome = result["outcomes"][0]
    assert outcome["operation"] == "migrate"
    assert outcome["file_count"] == 3

    # Files actually copied.
    canonical = Path(outcome["dest_path"])
    assert (canonical / "revision").read_text() == "5\n"
    assert (canonical / "changes.jsonl").exists()
    assert (canonical / "sessions/sess-1.json").exists()

    # Migration log materialized.
    log = isolated_home / ".agent-rally-point" / "migration.log"
    assert log.exists()
    entries = [json.loads(l) for l in log.read_text().splitlines() if l.strip()]
    assert any(e.get("operation") == "migrate" for e in entries)
    assert any(e.get("sha256_manifest") for e in entries)


def test_apply_is_idempotent(fresh_migrate, isolated_home):
    _make_legacy_channel(isolated_home, "app-x", {"revision": "1\n"})
    r1 = fresh_migrate.apply_migration()
    r2 = fresh_migrate.apply_migration()
    # Second run: every channel logs "already-migrated".
    assert r1["outcomes"][0]["operation"] == "migrate"
    assert r2["outcomes"][0]["operation"] == "already-migrated"
    assert r2["failures"] == 0


def test_apply_places_advisory_marker(fresh_migrate, isolated_home):
    ch = _make_legacy_channel(isolated_home, "app-m", {"revision": "1\n"})
    fresh_migrate.apply_migration()
    marker = ch / ".RALLY_LEGACY_READONLY"
    assert marker.exists()
    data = json.loads(marker.read_text())
    assert data["advisory"] is True
    assert data["policy_after_cutover"] == "canonical"


def test_apply_dry_run_writes_nothing(fresh_migrate, isolated_home):
    _make_legacy_channel(isolated_home, "app-d", {"revision": "1\n"})
    result = fresh_migrate.apply_migration(dry_run=True)
    canonical_root = isolated_home / ".agent-rally-point" / "apps"
    # No canonical channel materialized.
    assert not canonical_root.exists() or not any(canonical_root.iterdir())
    # No migration log written either.
    log = isolated_home / ".agent-rally-point" / "migration.log"
    assert not log.exists()
    assert result["dry_run"] is True


def _read_migration_log(home: Path) -> list[dict]:
    log = home / ".agent-rally-point" / "migration.log"
    if not log.exists():
        return []
    out = []
    for line in log.read_text().splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return out


def test_apply_changes_jsonl_dedup_merge_overlapping(
    fresh_migrate, isolated_home
):
    """AC-10-1 — overlapping source/dest changes.jsonl dedup-merge.

    Pre-populate canonical with lines A, B; source has A, B, C. Result:
    dest contains A, B, C — no duplicates of A/B.
    """
    line_a = '{"ts":1,"kind":"phase","tool":"x","model":"x","run_id":"r","app_slug":"s","payload":{"n":"a"},"revision":1}'
    line_b = '{"ts":2,"kind":"phase","tool":"x","model":"x","run_id":"r","app_slug":"s","payload":{"n":"b"},"revision":2}'
    line_c = '{"ts":3,"kind":"phase","tool":"x","model":"x","run_id":"r","app_slug":"s","payload":{"n":"c"},"revision":3}'

    _make_legacy_channel(isolated_home, "app-overlap", {
        "revision": "3\n",
        "changes.jsonl": f"{line_a}\n{line_b}\n{line_c}\n",
    })
    # Pre-seed canonical dest as if β1.2 dual-write had landed A and B.
    channels = fresh_migrate.discover_legacy_channels()
    dest = Path(channels[0]["canonical_path"])
    dest.mkdir(parents=True, exist_ok=True)
    (dest / "changes.jsonl").write_text(f"{line_a}\n{line_b}\n")
    (dest / "revision").write_text("2\n")

    result = fresh_migrate.apply_migration()
    assert result["failures"] == 0
    assert result["outcomes"][0]["operation"] == "migrate"

    merged = (dest / "changes.jsonl").read_text().splitlines()
    # All three lines present, no duplicates.
    assert merged.count(line_a) == 1
    assert merged.count(line_b) == 1
    assert merged.count(line_c) == 1
    assert len(merged) == 3
    # Revision bumped to max(src=3, dest_before=2) = 3.
    assert (dest / "revision").read_text().strip() == "3"


def test_apply_changes_jsonl_dedup_merge_non_overlapping(
    fresh_migrate, isolated_home
):
    """AC-10-2 — non-overlapping source/dest concatenates correctly.

    Pre-populate canonical with line A; source has B, C. Result:
    dest contains A, B, C in append order.
    """
    line_a = '{"ts":1,"kind":"phase","tool":"x","model":"x","run_id":"r","app_slug":"s","payload":{"n":"a"},"revision":1}'
    line_b = '{"ts":2,"kind":"phase","tool":"x","model":"x","run_id":"r","app_slug":"s","payload":{"n":"b"},"revision":2}'
    line_c = '{"ts":3,"kind":"phase","tool":"x","model":"x","run_id":"r","app_slug":"s","payload":{"n":"c"},"revision":3}'

    _make_legacy_channel(isolated_home, "app-disjoint", {
        "revision": "3\n",
        "changes.jsonl": f"{line_b}\n{line_c}\n",
    })
    channels = fresh_migrate.discover_legacy_channels()
    dest = Path(channels[0]["canonical_path"])
    dest.mkdir(parents=True, exist_ok=True)
    (dest / "changes.jsonl").write_text(f"{line_a}\n")
    (dest / "revision").write_text("1\n")

    result = fresh_migrate.apply_migration()
    assert result["failures"] == 0

    merged = (dest / "changes.jsonl").read_text().splitlines()
    assert merged == [line_a, line_b, line_c]
    assert (dest / "revision").read_text().strip() == "3"


def test_apply_changes_jsonl_merge_logs_event_level_merge(
    fresh_migrate, isolated_home
):
    """AC-10-3 — the migration log carries an event-level-merge record."""
    line_a = '{"ts":1,"kind":"phase","tool":"x","model":"x","run_id":"r","app_slug":"s","payload":{"n":"a"},"revision":1}'
    line_b = '{"ts":2,"kind":"phase","tool":"x","model":"x","run_id":"r","app_slug":"s","payload":{"n":"b"},"revision":2}'

    _make_legacy_channel(isolated_home, "app-log", {
        "revision": "2\n",
        "changes.jsonl": f"{line_a}\n{line_b}\n",
    })
    channels = fresh_migrate.discover_legacy_channels()
    dest = Path(channels[0]["canonical_path"])
    dest.mkdir(parents=True, exist_ok=True)
    (dest / "changes.jsonl").write_text(f"{line_a}\n")
    (dest / "revision").write_text("1\n")

    fresh_migrate.apply_migration()

    records = _read_migration_log(isolated_home)
    merge_records = [
        r for r in records if r.get("operation") == "event-level-merge"
    ]
    assert len(merge_records) == 1, (
        f"expected exactly one event-level-merge log row, got: {records}"
    )
    m = merge_records[0]
    assert m["changes_lines_in_src"] == 2
    assert m["changes_lines_in_dest_before"] == 1
    assert m["changes_lines_appended"] == 1
    assert m["changes_lines_skipped_dup"] == 1
    assert m["changes_lines_in_dest_after"] == 2
    assert m["src_revision"] == 2
    assert m["dest_revision_before"] == 1
    assert m["dest_revision_after"] == 2


def test_apply_changes_jsonl_idempotent_no_op_on_rerun(
    fresh_migrate, isolated_home
):
    """AC-10 follow-up — re-running apply after merge is a no-op."""
    line_a = '{"ts":1,"kind":"phase","tool":"x","model":"x","run_id":"r","app_slug":"s","payload":{"n":"a"},"revision":1}'
    line_b = '{"ts":2,"kind":"phase","tool":"x","model":"x","run_id":"r","app_slug":"s","payload":{"n":"b"},"revision":2}'

    _make_legacy_channel(isolated_home, "app-rerun", {
        "revision": "2\n",
        "changes.jsonl": f"{line_a}\n{line_b}\n",
    })
    r1 = fresh_migrate.apply_migration()
    r2 = fresh_migrate.apply_migration()
    assert r1["outcomes"][0]["operation"] == "migrate"
    # Second pass: src and dest are byte-equal manifests → already-migrated.
    assert r2["outcomes"][0]["operation"] == "already-migrated"


def test_verify_cutover_refuses_when_canonical_empty(fresh_migrate, isolated_home):
    _make_legacy_channel(isolated_home, "app-1", {"revision": "1\n"})
    v = fresh_migrate.verify_cutover(require_downstream=False)
    # Nothing copied → fully_copied=False, integrity=False.
    assert v["can_promote"] is False
    assert v["conditions"]["legacy_fully_copied"] is False


def test_verify_cutover_accepts_after_clean_migration(fresh_migrate, isolated_home):
    _make_legacy_channel(isolated_home, "app-a", {"revision": "1\n"})
    _make_legacy_channel(isolated_home, "app-b", {"revision": "2\n"})
    fresh_migrate.apply_migration()

    # Pretend we waited for the no-fresh-writes TTL by using a 0-minute TTL
    # (which makes "fresh" = "modified in the last 0 seconds", impossible to
    # satisfy with regular wall-clock — so we actually need to backdate the
    # legacy file mtimes OR use a negative TTL... actually, simplest:
    # use a -1-minute TTL so cutoff is in the FUTURE and nothing is "fresh").
    # The simpler approach is to set mtimes to long-ago.
    legacy_root = isolated_home / ".build-loop" / "apps"
    long_ago = time.time() - 3600
    for p in legacy_root.rglob("*"):
        if p.is_file():
            os.utime(p, (long_ago, long_ago))

    v = fresh_migrate.verify_cutover(
        ttl_minutes=15, require_downstream=False
    )
    assert v["can_promote"] is True, f"verdict: {v}"
    assert v["conditions"]["legacy_fully_copied"] is True
    assert v["conditions"]["integrity_verified"] is True
    assert v["conditions"]["no_fresh_writes_within_ttl"] is True
    assert v["fresh_writes"] == []


def test_verify_cutover_refuses_on_fresh_legacy_write(fresh_migrate, isolated_home):
    _make_legacy_channel(isolated_home, "app-a", {"revision": "1\n"})
    fresh_migrate.apply_migration()
    # Touch a legacy file NOW — this is a "fresh write" under any TTL > 0.
    legacy_file = isolated_home / ".build-loop" / "apps" / "app-a" / "revision"
    legacy_file.write_text("99\n")  # mtime is now()

    v = fresh_migrate.verify_cutover(
        ttl_minutes=15, require_downstream=False
    )
    # Even though everything else is fine, fresh-write detection refuses.
    assert v["can_promote"] is False
    assert v["conditions"]["no_fresh_writes_within_ttl"] is False
    assert len(v["fresh_writes"]) >= 1


def test_verify_cutover_ignores_watcher_log_fresh_writes(
    fresh_migrate, isolated_home
):
    """AC-9-1 — telemetry paths (watchers/*.log) MUST NOT gate cutover.

    Codex Item 9 (rev 219): watcher daemons append to ``watchers/*.log``
    every few seconds while any session is alive, so an
    rglob("*")-then-exclude-marker fresh-write scan would effectively
    never let the gate pass. The verifier now scans only the
    coordination-state whitelist.
    """
    _make_legacy_channel(isolated_home, "app-a", {"revision": "1\n"})
    fresh_migrate.apply_migration()
    # Backdate everything to long-ago so the scan starts clean.
    legacy_root = isolated_home / ".build-loop" / "apps"
    long_ago = time.time() - 3600
    for p in legacy_root.rglob("*"):
        if p.is_file():
            os.utime(p, (long_ago, long_ago))
    # Materialize the compat table so downstream_ready passes.
    compat = isolated_home / ".agent-rally-point" / "compatibility.json"
    compat.write_text(json.dumps({
        "agent_rally_point": "0.3.0", "protocol_version": "1.0",
        "supported_build_loop_range": ">=0.12.17,<0.14.0",
        "deprecation_notices": [],
    }))

    # NOW write a fresh watcher log file — this is the realistic case.
    watchers_dir = legacy_root / "app-a" / "watchers"
    watchers_dir.mkdir(parents=True, exist_ok=True)
    (watchers_dir / "claude-code-abcd1234.log").write_text(
        "watcher heartbeat\n"
    )  # mtime is now()

    v = fresh_migrate.verify_cutover(
        ttl_minutes=15, require_downstream=True
    )
    # Telemetry doesn't gate; cutover passes.
    assert v["conditions"]["no_fresh_writes_within_ttl"] is True, (
        f"watcher log was treated as a fresh state write: {v['fresh_writes']}"
    )
    assert v["can_promote"] is True, f"verdict: {v}"


def test_verify_cutover_catches_changes_jsonl_fresh_write(
    fresh_migrate, isolated_home
):
    """AC-9-2 — the legitimate case: a fresh changes.jsonl write DOES gate.

    Counterpart to AC-9-1; confirms the whitelist still catches the
    state files cutover is supposed to gate on.
    """
    _make_legacy_channel(isolated_home, "app-a", {
        "revision": "1\n",
        "changes.jsonl": '{"kind":"phase","payload":{},"revision":1}\n',
    })
    fresh_migrate.apply_migration()
    legacy_root = isolated_home / ".build-loop" / "apps"
    long_ago = time.time() - 3600
    for p in legacy_root.rglob("*"):
        if p.is_file():
            os.utime(p, (long_ago, long_ago))

    # Touch changes.jsonl NOW.
    changes = legacy_root / "app-a" / "changes.jsonl"
    changes.write_text(changes.read_text() + '{"kind":"phase","payload":{},"revision":2}\n')

    v = fresh_migrate.verify_cutover(
        ttl_minutes=15, require_downstream=False
    )
    assert v["conditions"]["no_fresh_writes_within_ttl"] is False
    assert any("changes.jsonl" in w["path"] for w in v["fresh_writes"]), (
        f"changes.jsonl fresh write not surfaced: {v['fresh_writes']}"
    )


def test_verify_cutover_catches_inbox_fresh_write(
    fresh_migrate, isolated_home
):
    """AC-9-bonus — positive coverage of the inbox/*.jsonl whitelist glob."""
    _make_legacy_channel(isolated_home, "app-a", {
        "revision": "1\n",
        "inbox/msg-1.jsonl": '{"to":"peer","payload":{}}\n',
    })
    fresh_migrate.apply_migration()
    legacy_root = isolated_home / ".build-loop" / "apps"
    long_ago = time.time() - 3600
    for p in legacy_root.rglob("*"):
        if p.is_file():
            os.utime(p, (long_ago, long_ago))

    # Touch an inbox jsonl NOW.
    (legacy_root / "app-a" / "inbox" / "msg-2.jsonl").write_text(
        '{"to":"peer","payload":{}}\n'
    )

    v = fresh_migrate.verify_cutover(
        ttl_minutes=15, require_downstream=False
    )
    assert v["conditions"]["no_fresh_writes_within_ttl"] is False
    assert any("inbox/msg-2.jsonl" in w["path"] for w in v["fresh_writes"]), (
        f"inbox fresh write not surfaced: {v['fresh_writes']}"
    )


def test_verify_cutover_refuses_on_integrity_mismatch(fresh_migrate, isolated_home):
    _make_legacy_channel(isolated_home, "app-a", {"revision": "1\n"})
    fresh_migrate.apply_migration()
    # Mutate the canonical copy so manifests no longer match.
    canonical = isolated_home / ".agent-rally-point" / "apps"
    rid_dirs = list(canonical.iterdir())
    assert rid_dirs
    (rid_dirs[0] / "revision").write_text("999\n")
    # Backdate legacy mtimes so fresh-write doesn't dominate the verdict.
    legacy_root = isolated_home / ".build-loop" / "apps"
    long_ago = time.time() - 3600
    for p in legacy_root.rglob("*"):
        if p.is_file():
            os.utime(p, (long_ago, long_ago))

    v = fresh_migrate.verify_cutover(
        ttl_minutes=15, require_downstream=False
    )
    assert v["can_promote"] is False
    assert v["conditions"]["integrity_verified"] is False


def test_verify_cutover_requires_compatibility_table_by_default(
    fresh_migrate, isolated_home
):
    _make_legacy_channel(isolated_home, "app-a", {"revision": "1\n"})
    fresh_migrate.apply_migration()
    legacy_root = isolated_home / ".build-loop" / "apps"
    long_ago = time.time() - 3600
    for p in legacy_root.rglob("*"):
        if p.is_file():
            os.utime(p, (long_ago, long_ago))

    # require_downstream=True (default) and no compatibility.json present.
    # apply_migration() did NOT materialize compatibility.json — that's
    # discover()'s job. So downstream_ready should be False here.
    compat = isolated_home / ".agent-rally-point" / "compatibility.json"
    assert not compat.exists()
    v = fresh_migrate.verify_cutover(ttl_minutes=15, require_downstream=True)
    assert v["conditions"]["downstream_ready"] is False
    assert v["can_promote"] is False

    # Materialize the compat table → cutover now passes.
    compat.write_text(json.dumps({
        "agent_rally_point": "0.3.0", "protocol_version": "1.0",
        "supported_build_loop_range": ">=0.12.17,<0.14.0",
        "deprecation_notices": [],
    }))
    v2 = fresh_migrate.verify_cutover(ttl_minutes=15, require_downstream=True)
    assert v2["conditions"]["downstream_ready"] is True
    assert v2["can_promote"] is True


def test_cli_scan_smoke(isolated_home):
    _make_legacy_channel(isolated_home, "app-cli", {"revision": "1\n"})
    out = subprocess.run(
        [sys.executable, "-m", "agent_rally_point.migrate", "scan", "--json"],
        env={**os.environ, "HOME": str(isolated_home)},
        capture_output=True, text=True,
    )
    assert out.returncode == 0
    data = json.loads(out.stdout)
    assert any(ch["slug"] == "app-cli" for ch in data)


def test_cli_apply_then_verify(isolated_home):
    _make_legacy_channel(isolated_home, "app-cli2", {"revision": "1\n"})
    # apply
    r1 = subprocess.run(
        [sys.executable, "-m", "agent_rally_point.migrate", "apply", "--json"],
        env={**os.environ, "HOME": str(isolated_home)},
        capture_output=True, text=True,
    )
    assert r1.returncode == 0, r1.stderr
    # verify-cutover — write compat table so downstream_ready passes,
    # backdate legacy mtimes so fresh-write check passes.
    compat = isolated_home / ".agent-rally-point" / "compatibility.json"
    compat.write_text(json.dumps({
        "agent_rally_point": "0.3.0", "protocol_version": "1.0",
        "supported_build_loop_range": ">=0.12.17,<0.14.0",
        "deprecation_notices": [],
    }))
    long_ago = time.time() - 3600
    for p in (isolated_home / ".build-loop" / "apps").rglob("*"):
        if p.is_file():
            os.utime(p, (long_ago, long_ago))
    r2 = subprocess.run(
        [
            sys.executable, "-m", "agent_rally_point.migrate",
            "verify-cutover", "--ttl-minutes", "15", "--json",
        ],
        env={**os.environ, "HOME": str(isolated_home)},
        capture_output=True, text=True,
    )
    assert r2.returncode == 0
    data = json.loads(r2.stdout)
    assert data["can_promote"] is True
