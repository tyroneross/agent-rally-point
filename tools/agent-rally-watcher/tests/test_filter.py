# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Filter rule matching: AND-combine, empty=match-all, payload_match."""
from __future__ import annotations

from pathlib import Path

import pytest

from agent_rally_watcher.filter import FilterRule, load_consumers, match

REC_FEEDBACK_PASS = {
    "kind": "feedback",
    "tool": "codex",
    "run_id": "run-001",
    "payload": {"verdict": "PASS"},
}
REC_FEEDBACK_BLOCKED = {
    "kind": "feedback",
    "tool": "codex",
    "run_id": "run-002",
    "payload": {"verdict": "BLOCKED"},
}
REC_HANDOFF = {
    "kind": "handoff",
    "tool": "claude_code",
    "run_id": "run-003",
    "payload": {"to": "codex"},
}


def test_empty_rule_matches_everything() -> None:
    rule = FilterRule()
    assert match(REC_FEEDBACK_PASS, rule)
    assert match(REC_HANDOFF, rule)


def test_kinds_inclusion() -> None:
    rule = FilterRule(kinds=["feedback"])
    assert match(REC_FEEDBACK_PASS, rule)
    assert not match(REC_HANDOFF, rule)


def test_kinds_not_exclusion() -> None:
    rule = FilterRule(kinds_not=["commit"])
    assert match(REC_FEEDBACK_PASS, rule)
    rule2 = FilterRule(kinds_not=["feedback"])
    assert not match(REC_FEEDBACK_PASS, rule2)


def test_tools_not_self_echo() -> None:
    rule = FilterRule(tools_not=["claude_code"])
    assert match(REC_FEEDBACK_PASS, rule)  # tool=codex, allowed
    assert not match(REC_HANDOFF, rule)  # tool=claude_code, blocked


def test_payload_match_exact() -> None:
    rule = FilterRule(payload_match={"verdict": "BLOCKED"})
    assert match(REC_FEEDBACK_BLOCKED, rule)
    assert not match(REC_FEEDBACK_PASS, rule)


def test_payload_match_missing_key_fails() -> None:
    rule = FilterRule(payload_match={"verdict": "PASS"})
    assert not match(REC_HANDOFF, rule)  # no 'verdict' key in payload


def test_and_combined_rules() -> None:
    rule = FilterRule(kinds=["feedback"], payload_match={"verdict": "BLOCKED"})
    assert match(REC_FEEDBACK_BLOCKED, rule)
    assert not match(REC_FEEDBACK_PASS, rule)


def test_load_consumers_parses_example(tmp_path: Path) -> None:
    cfg = tmp_path / "consumers.toml"
    cfg.write_text(
        """
[consumers.claude_code.filter]
kinds = ["feedback", "handoff"]
tools_not = ["claude_code"]

[consumers.claude_code.sink]
type = "file"
path = "/tmp/out.jsonl"

[consumers.codex.filter]
kinds = ["feedback"]

[consumers.codex.sink]
type = "notify"
title = "hi"
""".strip(),
        encoding="utf-8",
    )
    consumers = load_consumers(cfg)
    assert len(consumers) == 2
    cc = next(c for c in consumers if c.id == "claude_code")
    assert cc.filter.kinds == ["feedback", "handoff"]
    assert cc.filter.tools_not == ["claude_code"]
    assert cc.sink["type"] == "file"


def test_load_consumers_rejects_missing_sink(tmp_path: Path) -> None:
    cfg = tmp_path / "consumers.toml"
    cfg.write_text(
        '[consumers.x.filter]\nkinds = ["feedback"]\n',
        encoding="utf-8",
    )
    with pytest.raises(ValueError):
        load_consumers(cfg)


def test_load_consumers_parses_inline_payload_match(tmp_path: Path) -> None:
    """TOML inline-table for payload_match (the v0.1.0 YAML used a nested dict)."""
    cfg = tmp_path / "consumers.toml"
    cfg.write_text(
        """
[consumers.urgent.filter]
kinds = ["feedback"]
payload_match = { verdict = "BLOCKED" }

[consumers.urgent.sink]
type = "notify"
title = "blocker"
""".strip(),
        encoding="utf-8",
    )
    consumers = load_consumers(cfg)
    assert len(consumers) == 1
    assert consumers[0].filter.payload_match == {"verdict": "BLOCKED"}
