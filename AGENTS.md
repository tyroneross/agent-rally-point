# AGENTS.md

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

- **Guide (60-second):** [RALLY.md](RALLY.md)
- **Doctrine (Rally Flow):** [dynamic-workflows/COORDINATION.md](dynamic-workflows/COORDINATION.md)
- **Wire protocol:** [dynamic-workflows/PROTOCOL.md](dynamic-workflows/PROTOCOL.md)
- **Board / current lanes:** [docs/ORCHESTRATION.md](docs/ORCHESTRATION.md)
- **Any-agent onboarding contract:** [docs/ANY-AGENT-ONBOARDING.md](docs/ANY-AGENT-ONBOARDING.md)
- **Handoffs & managed agents:** [docs/HANDOFFS-AND-LAUNCHING-AGENTS.md](docs/HANDOFFS-AND-LAUNCHING-AGENTS.md)
<!-- rally:end -->
