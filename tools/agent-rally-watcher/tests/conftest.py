# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Shared test fixtures.

ARP-007 hardening (dispatch.py's file sink) constrains sink paths to
``AGENT_RALLY_WATCHER_SINK_ROOT`` (default ``~/.agent-rally-watcher``) —
deliberately NOT the daemon's cwd, so nothing in the test process should
write there. This autouse fixture points the allowed root at each test's own
``tmp_path`` instead, so existing tests that build sink paths under
``tmp_path`` (e.g. ``tests/test_watcher.py``'s ``_consumer`` helper) keep
working unmodified — pytest guarantees ``tmp_path`` is the same value
whether requested here or by the test function itself. Tests that need to
assert REJECTION of an out-of-root path build one outside ``tmp_path``
explicitly (e.g. a sibling tmp dir).
"""
from __future__ import annotations

import pytest


@pytest.fixture(autouse=True)
def _sink_root_env(tmp_path, monkeypatch):
    monkeypatch.setenv("AGENT_RALLY_WATCHER_SINK_ROOT", str(tmp_path))
