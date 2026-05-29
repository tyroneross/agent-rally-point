# AGENTS.md

This file is read by coding agents on entry. The rally pointer below tells agents where to coordinate and which deeper docs to load.

<!-- rally:start -->
## Agent Rally Point

This repo coordinates parallel coding agents via **agent-rally-point** (per-repo, no external service).

- **Enter:** `rally enter --tool <host-llm-role-number>` (e.g. `claude_code:01`, `codex:01`)
- **What to do next:** `rally next --tool <you> --json`
- **Current state:** `rally room --json`
- **History (durable, per-engagement):** `.rally/log/`
- **Self-description (machine-readable pointers):** `.rally/manifest.json`

### Deeper docs

- **Guide (60-second):** [RALLY.md](RALLY.md)
- **Doctrine (Rally Flow):** [dynamic-workflows/COORDINATION.md](dynamic-workflows/COORDINATION.md)
- **Wire protocol:** [dynamic-workflows/PROTOCOL.md](dynamic-workflows/PROTOCOL.md)
- **Board / current lanes:** [docs/ORCHESTRATION.md](docs/ORCHESTRATION.md)
<!-- rally:end -->
