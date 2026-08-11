# CLAUDE.md

This file is read by coding agents on entry. The rally pointer below tells agents where to coordinate and which deeper docs to load.

<!-- rally:start -->
## Agent Rally Point

This repo coordinates parallel coding agents via **agent-rally-point** (per-repo, no external service).

- **Self-locate FIRST:** `rally whoami --tool <you> --json` — host runtime, room, lead, mission, ack status. If `host_runtime.ambiguous` is true, STOP and resolve which host before acting (never guess).
- **Enter + acknowledge:** `rally enter --tool <host-llm-role-number> --json` (e.g. `claude_code:01`), then `rally ack --tool <you>` to confirm you ingested the rules/guardrails/lead/mission.
- **Resolve targets from live state:** Treat lead/tool ids as runtime data, not constants. Use `whoami`, `lead show`, `room`, `next`, and explicit handoff targets; do not copy ids from examples, old logs, or another repo.
- **What to do next:** `rally next --tool <you> --json`
- **Current state:** `rally room --json`
- **History (durable, per-engagement):** `.rally/log/`
- **Self-description (machine-readable pointers):** `.rally/manifest.json`

### Deeper docs

- **North Star (durable vision + invariants):** [NORTH_STAR.md](NORTH_STAR.md)
- **Guide (60-second):** [RALLY.md](RALLY.md)
- **Doctrine (Rally Flow):** [dynamic-workflows/COORDINATION.md](dynamic-workflows/COORDINATION.md)
- **Wire protocol:** [dynamic-workflows/PROTOCOL.md](dynamic-workflows/PROTOCOL.md)
- **Board / current lanes:** [docs/ORCHESTRATION.md](docs/ORCHESTRATION.md)
- **Handoffs & launching agents (Claude+Codex):** [docs/HANDOFFS-AND-LAUNCHING-AGENTS.md](docs/HANDOFFS-AND-LAUNCHING-AGENTS.md)
<!-- rally:end -->

## Performance claims (working agreement, set 2026-08-11)

Absolute latency and token thresholds — 20 ms `before-write`, 150 ms `start`, 60 tokens per prompt — are **reference points, not pass conditions**. Round numbers invite optimizing the number instead of the product.

If you claim a cost improvement, record three things: a **before** on the same build id from a disposable fixture, a **component attribution** for the delta (spawn count, ledger work, render, interpreter), and an **invariant re-check**. A headline win that raises spawn count, unattributed time, or output bytes is not a win. Full rule: `.build-loop/goal.md` criteria 12-14.

Known measured mechanism, so you do not re-derive it: hook cost scales with ledger size and each hook fire writes ~14 ledger lines back into that ledger (O39). Node is ~5-10 % of hook cost, not the bottleneck (O36).

## Status heartbeats (working agreement)

During any operation expected to exceed ~10 minutes (long implementations, renders, big test runs, orchestration waits), post a brief status to the room every ~10 minutes:

    rally say artifact --tool <you> --subject "STATUS: <task> — <progress marker>" --json

Silence longer than ~15 minutes while working is a coordination bug: peers and the director cannot tell "working" from "hung". Always post a final status when the operation completes or blocks.
