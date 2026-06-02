# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""CLI channel-dir resolution: canonical → legacy → default fallback."""
from __future__ import annotations

from pathlib import Path

import pytest

from agent_rally_watcher import cli


@pytest.fixture(autouse=True)
def _reset_legacy_warning():
    """Each test starts with the one-shot legacy warning latch reset."""
    cli._legacy_warning_emitted = False
    yield
    cli._legacy_warning_emitted = False


@pytest.fixture
def _isolate_home(tmp_path, monkeypatch):
    """Point HOME at tmp_path so ~/.agent-rally-point/ and ~/.build-loop/ are scoped."""
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.delenv("BUILD_LOOP_APPS_ROOT", raising=False)
    return tmp_path


def test_canonical_path_used_when_it_exists(_isolate_home):
    """Branch 1: ~/.agent-rally-point/apps/<slug>/ exists → use it."""
    home = _isolate_home
    canonical = home / ".agent-rally-point" / "apps" / "demo"
    canonical.mkdir(parents=True)
    legacy = home / ".build-loop" / "apps" / "demo"
    legacy.mkdir(parents=True)  # both exist; canonical wins

    result = cli._channel_dir_for("demo")
    assert result == canonical


def test_legacy_path_used_when_canonical_absent(_isolate_home, capsys):
    """Branch 2: only ~/.build-loop/apps/<slug>/ exists → use legacy + warn."""
    home = _isolate_home
    legacy = home / ".build-loop" / "apps" / "demo"
    legacy.mkdir(parents=True)
    # canonical does NOT exist

    result = cli._channel_dir_for("demo")
    assert result == legacy

    captured = capsys.readouterr()
    assert "legacy channel dir" in captured.err
    assert str(legacy) in captured.err


def test_default_canonical_when_neither_exists(_isolate_home):
    """Branch 3: neither path exists → return canonical (caller creates it)."""
    home = _isolate_home
    expected = home / ".agent-rally-point" / "apps" / "demo"

    result = cli._channel_dir_for("demo")
    assert result == expected
    # The function does NOT create the dir — that's the caller's job
    assert not result.exists()


def test_legacy_warning_emits_once(_isolate_home, capsys):
    """Two consecutive legacy-branch calls in one process → only one warning."""
    home = _isolate_home
    legacy = home / ".build-loop" / "apps" / "demo"
    legacy.mkdir(parents=True)

    cli._channel_dir_for("demo")
    cli._channel_dir_for("demo")

    captured = capsys.readouterr()
    assert captured.err.count("legacy channel dir") == 1


def test_env_override_short_circuits_fallback(_isolate_home, monkeypatch):
    """BUILD_LOOP_APPS_ROOT override bypasses the legacy-fallback heuristic."""
    home = _isolate_home
    override_root = home / "custom" / "apps"
    override = override_root / "demo"
    override.mkdir(parents=True)
    monkeypatch.setenv("BUILD_LOOP_APPS_ROOT", str(override_root))

    # Legacy exists too; override must win
    legacy = home / ".build-loop" / "apps" / "demo"
    legacy.mkdir(parents=True)

    result = cli._channel_dir_for("demo")
    assert result == override
