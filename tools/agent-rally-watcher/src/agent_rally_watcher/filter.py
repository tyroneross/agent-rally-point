# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Data-driven per-consumer filter rules.

Filter rule fields (AND-combined; missing field = match-all):

    kinds:           [str]            — record.kind in this list
    kinds_not:       [str]            — record.kind NOT in this list
    tools:           [str]            — record.tool in this list
    tools_not:       [str]            — record.tool NOT in this list
    senders:         [str]            — record.run_id in this list
    payload_match:   {key: value}     — record.payload[key] == value (string-equal)

``Consumer`` bundles a filter with a sink config. ``load_consumers`` parses TOML
(stdlib ``tomllib``, Python ≥3.11). Schema mirrors the prior YAML layout:

    [consumers.<id>.filter]
    kinds = ["feedback", "handoff"]
    tools_not = ["claude_code"]

    [consumers.<id>.sink]
    type = "file"
    path = "~/.agent-rally-watcher/streams/claude_code.jsonl"
"""
from __future__ import annotations

import tomllib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterable


@dataclass
class FilterRule:
    """A single consumer's filter rules."""

    kinds: list[str] = field(default_factory=list)
    kinds_not: list[str] = field(default_factory=list)
    tools: list[str] = field(default_factory=list)
    tools_not: list[str] = field(default_factory=list)
    senders: list[str] = field(default_factory=list)
    payload_match: dict[str, Any] = field(default_factory=dict)


@dataclass
class Consumer:
    """One consumer: ID + filter + sink config."""

    id: str
    filter: FilterRule
    sink: dict[str, Any]


def _as_list(v: Any) -> list[str]:
    if v is None:
        return []
    if isinstance(v, str):
        return [v]
    if isinstance(v, list):
        return [str(x) for x in v]
    raise ValueError(f"expected list or string, got {type(v).__name__}")


def _parse_filter(raw: dict[str, Any] | None) -> FilterRule:
    raw = raw or {}
    return FilterRule(
        kinds=_as_list(raw.get("kinds")),
        kinds_not=_as_list(raw.get("kinds_not")),
        tools=_as_list(raw.get("tools")),
        tools_not=_as_list(raw.get("tools_not")),
        senders=_as_list(raw.get("senders")),
        payload_match=dict(raw.get("payload_match") or {}),
    )


def load_consumers(config_path: Path | str) -> list[Consumer]:
    """Parse a consumers.toml file → list[Consumer]. Raises on shape errors."""
    p = Path(config_path).expanduser()
    data = tomllib.loads(p.read_text(encoding="utf-8"))
    raw_consumers = data.get("consumers")
    if not isinstance(raw_consumers, dict):
        raise ValueError(f"{p}: top-level 'consumers' must be a table")
    out: list[Consumer] = []
    for cid, entry in raw_consumers.items():
        if not isinstance(entry, dict):
            raise ValueError(f"{p}: consumer {cid!r} must be a table")
        sink = entry.get("sink")
        if not isinstance(sink, dict) or "type" not in sink:
            raise ValueError(f"{p}: consumer {cid!r} missing sink.type")
        out.append(Consumer(id=str(cid), filter=_parse_filter(entry.get("filter")), sink=sink))
    return out


def match(record: dict[str, Any], rule: FilterRule) -> bool:
    """Return True iff ``record`` satisfies all rule fields (AND-combined)."""
    kind = record.get("kind")
    tool = record.get("tool")
    run_id = record.get("run_id")
    payload = record.get("payload") or {}

    if rule.kinds and kind not in rule.kinds:
        return False
    if rule.kinds_not and kind in rule.kinds_not:
        return False
    if rule.tools and tool not in rule.tools:
        return False
    if rule.tools_not and tool in rule.tools_not:
        return False
    if rule.senders and run_id not in rule.senders:
        return False
    for k, expected in rule.payload_match.items():
        if str(payload.get(k)) != str(expected):
            return False
    return True


def iter_matches(records: Iterable[dict[str, Any]], rule: FilterRule) -> Iterable[dict[str, Any]]:
    """Yield records that pass ``rule``."""
    for r in records:
        if match(r, rule):
            yield r
