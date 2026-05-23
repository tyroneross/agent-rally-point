<!-- build-loop@tyroneross:canary:agent-rally-point -->
<!-- canary-end -->
# Agent Rally Point

> **Local-first coordination point for coding agents working in the same repo: presence, handoffs, verifier gates, conflicts, and closeout without a server.**

Two or more AI agent CLIs (Claude Code, Codex, peer Claude sessions, CI verifiers) coordinating on a shared git repo with a human in the loop. No HTTP service. No broker. Just files + git for repo identity.

## Status

**v0.1.0 — alpha, skeleton release** (2026-05-20). Channel primitives extracted from [build-loop](https://github.com/tyroneross/build-loop) v0.12.8 verbatim. 55/55 tests passing in standalone repo. CLI + validators land in v0.2.0; full constitution + docs in v0.3.0.

## What it does

- **Presence**: agents heartbeat into `~/.agent-rally-point/apps/<slug>/sessions/`; peers see each other via `read_active_presence()`.
- **Channel**: append-only event log (`changes.jsonl`) with monotonic revision counter; readers compute deltas via `checkpoint_read()`.
- **Canonical post**: single `post()` helper that bumps revision + appends record atomically (prevents the silent-no-op bug).
- **Lifecycle hygiene**: explicit session reap on closeout; optional `changes.jsonl` rotation when log grows.
- **Worktree-aware identity**: `app_slug(cwd)` derives from `git rev-parse --git-common-dir` so main checkout + all worktrees + every clone of the same canonical repo share ONE channel.

Coordination tooling (status sensor, MECE-brief validator, release-surface verifier) lands in v0.2.

## Install

```bash
uv pip install agent-rally-point   # or: pip install agent-rally-point
```

PyPI publication pending. Until then, install from source:

```bash
git clone https://github.com/tyroneross/agent-rally-point.git
cd agent-rally-point
uv pip install -e .
```

## Quickstart (Python API)

```python
from pathlib import Path
from agent_rally_point import (
    app_slug, app_channel_dir, write_presence, post, checkpoint_read,
)

cwd = Path.cwd()
slug = app_slug(cwd)                            # worktree-independent repo identity
channel = app_channel_dir(slug)                 # ~/.agent-rally-point/apps/<slug>/

# Write own presence (heartbeat)
write_presence(
    channel_dir=channel,
    session_id="claude-feature-x-001",
    tool="claude_code",
    model="claude-opus-4-7",
    run_id="feature-x-001",
    app_slug=slug,
    phase="phase-1-assess",
    files_in_flight=["src/feature_x.py"],
    cwd=cwd,
)

# Post a verdict (uses the canonical helper — bumps revision + appends atomically)
new_rev = post(
    channel_dir=channel,
    kind="feedback",
    tool="claude_code",
    model="claude-opus-4-7",
    run_id="feature-x-001",
    app_slug=slug,
    payload={"step": "1", "verdict": "PASS", "evidence": {"commit": "abc1234"}},
)

# Peer reads what's new
envelope = checkpoint_read(
    channel_dir=channel,
    session_id="codex-verifier-001",
    my_files=["src/feature_x.py"],
)
print(envelope)
```

CLI surface (`agent-rally-point status`, `watch`, `post`, etc.) ships in v0.2.

## How this differs from A2A / MCP / LangGraph / CrewAI / Temporal

Most agent coordination today is **service-mediated** (A2A's HTTP+JSON-RPC), **in-process** (LangGraph state machines, CrewAI crews), or **broker-backed** (Temporal). Agent Rally Point is **filesystem-mediated** — for the case where two-or-more agent CLIs share a dev box (or shared FS) and need durable handoff signals without infrastructure.

Use Agent Rally Point when:
- You have 2+ agent CLIs (Claude Code, Codex, peer Claude) on the same repo
- You want zero-setup coordination (no service, no broker)
- You want the discipline patterns (gating verdicts, MECE handoffs, release-surface verification) as shipped tooling

Use A2A / MCP / LangGraph / CrewAI / Temporal when:
- Cross-vendor over the network → A2A
- Agent-to-tool wiring → MCP
- Single-runtime state machines → LangGraph
- Role-based agent teams in one Python process → CrewAI
- Long-running production workflows with retries → Temporal

These are complements, not competitors.

## Constitution

The binding rules of coordination (operating rule, MECE packets, post() canonical, release-surface verification, closeout hygiene) ship as in-package docs in v0.3. For now, see the extraction-source research packet at [`tyroneross/dev/research/topics/multi-peer-coordination-standalone-extraction/`](https://github.com/tyroneross) (private) or the build-loop predecessor at [`build-loop/references/coordination-rules.md`](https://github.com/tyroneross/build-loop/blob/main/references/coordination-rules.md).

## Trademark disclaimer

**Agent Rally Point is not affiliated with Rally Point IT, LLC** or any other entity holding marks containing "Rally Point." Agent Rally Point is an open-source developer tool for AI agent coordination; Rally Point IT, LLC is an IT services / infrastructure consultancy. The composite name uses the military-origin "rally point" metaphor (a known place where independent units regroup with status).

## License

Apache-2.0. See [LICENSE](./LICENSE).

## Provenance

Channel primitives copied verbatim from [build-loop](https://github.com/tyroneross/build-loop) `scripts/app_pulse/` at v0.12.8 (commit `10224c2`). Build-loop will become a downstream consumer of `agent-rally-point` once v1.0 ships; channel root migrates from `~/.build-loop/apps/` to `~/.agent-rally-point/apps/` at that cutover.

## License & Attribution

This project is licensed under the [Apache License 2.0](LICENSE).

- [`LICENSE`](LICENSE) — full license text.
- [`NOTICE`](NOTICE) — attribution notices that, per Apache 2.0 §4(d), must travel with any redistribution of this work.
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — contribution conventions: per-file SPDX headers (REUSE 3.3), AI co-author trailer, signed commits, conventional commits.

Per-file `SPDX-FileCopyrightText` and `SPDX-License-Identifier` headers are required on shipped source files. Files that cannot carry inline comments (JSON, generated assets) are annotated in [`REUSE.toml`](REUSE.toml). Validate compliance locally with `uvx reuse lint`.
