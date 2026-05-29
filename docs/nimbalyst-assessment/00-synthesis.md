# nimbalyst → agent-rally-point / agent-astronomer — adoption roadmap

Synthesis of three facet assessments of [nimbalyst](https://github.com/nimbalyst/nimbalyst)
(local clone `~/dev/git-folder/nimbalyst`), produced by a dynamic-workflows fan-out and
coordinated through rally. Sources:
[01-comprehension](01-comprehension.md) · [02-ui-ux](02-ui-ux.md) ·
[03-agent-coordination](03-agent-coordination.md).

The single strongest theme across all three facets: **nimbalyst turns implicit knowledge into
queryable, navigable artifacts** — a routing table instead of "read the code", a typed
`session_files` ledger instead of "grep the transcript", a semantic token layer instead of
hardcoded colors, a kanban projection instead of "ask each agent its status". Both of the
owner's projects currently hold this knowledge implicitly. That is the gap worth closing.

---

## Ranked roadmap (do top-down)

Ranked by value ÷ effort. `[S]` ≈ <1h, `[M]` ≈ a focused session, `[L]` ≈ multi-session.

### Tier 1 — quick wins, both repos (all `[S]`, no runtime risk)

| # | Change | Repo | From |
|---|--------|------|------|
| 1 | **Area-gated `CLAUDE.md` routing table** (`File \| Read when…`) pointing each subsystem to its doc | both | comp §1 |
| 2 | **`.claude/agent-mistakes.md`** append-only blunder log, escalates to `.claude/rules/` on recurrence | both | comp §7 |
| 3 | **`FEATURE_INVENTORY.md`** — flat catalog of every `rally` subcommand / MCP tool (rally) and every CLI command / API route (astronomer) | both | comp §8 |
| 4 | **Pull load-bearing invariants into their own docs** with explicit failure modes (rally: room-state/boundary rules; astronomer: `docs/DEDUP_INVARIANTS.md` from the `dedup.ts` "do not call from usage/propagate" rule) | both | comp §6 |
| 5 | **Semantic CSS variable layer** (`--aa-bg/secondary/tertiary`, status-as-text-color) via Tailwind v4 `@theme`; replace ad-hoc `gray-*`/`blue-*` | astronomer | ui §1 |
| 6 | **Three-tier background depth** on sidebar/panels/code blocks to kill the flat-grey look | astronomer | ui §2 |

Tier 1 is the highest-leverage work: six `[S]` items, mostly markdown, that make both repos
self-documenting and give astronomer a real token foundation. Items 1–4 are exactly the "help a
human/agent understand the components of a repo" capability the owner liked in nimbalyst —
nimbalyst achieves it almost entirely through **documentation discipline**, not tooling.

### Tier 2 — focused sessions (`[M]`)

| # | Change | Repo | From |
|---|--------|------|------|
| 7 | **ASCII/Mermaid pipeline diagrams** in the heaviest docs (rally: enter→fact-store→room→delivery→boundary; astronomer: scan→scanner→library→dedup→API) | both | comp §2 |
| 8 | **Persisted `claim` facts with `file_path` + `link_type`** (`claim\|edit\|read`) so `rally room` projects "who owns / has touched which file" — closes the write-collision gap with evidence | rally | coord §2 |
| 9 | **`resume_hint` JSON blob on `task` facts** (last file, next sub-step, open blocker) so a returning agent resumes without re-parsing transcripts | rally | coord §1 |
| 10 | **Collapsible section groups** (chevron + count, zero border chrome) for `/skills` `/plugins` namespaces | astronomer | ui §5 |
| 11 | **Centralized HelpTooltip registry** keyed by `data-testid` (`lib/help-content.ts`) for nav + actions | astronomer | ui §3 |
| 12 | **Per-session status indicator** (single priority-ordered icon) on `/history` rows | astronomer | ui §7 |
| 13 | **`Cmd+K` command palette** with Tab-to-upgrade content search across skills/plugins/library | astronomer | ui §9 |
| 14 | **`worktree: true` task field** in the workstream descriptor; host provisions a worktree and emits its path as the artifact URI | rally | coord §3 |

### Tier 3 — larger bets (`[L]`)

| # | Change | Repo | From |
|---|--------|------|------|
| 15 | **Kanban projection of room state** — `workstream-status.mjs` emits a static HTML kanban from `rally room --json` (pending→backlog, claimed→implementing, done→complete, blocked→blocked) | rally | coord §4 |
| 16 | **Declarative walkthrough/onboarding system** (typed `WalkthroughDefinition`, target by `data-testid`) for first-run `/skills` `/plugins` | astronomer | ui §4 |
| 17 | **nimbalyst-as-rally-host** — an extension registering `rally`-backed MCP tools (claim/check/artifact) alongside nimbalyst's Meta-Agent MCP, so nimbalyst sessions coordinate through a rally room | rally | coord §integration |

---

## Cross-cutting recommendation

If only Tier 1 ships, both projects get the thing the owner valued most about nimbalyst —
**repo legibility** — at near-zero cost and zero runtime risk. nimbalyst's comprehension edge is
~80% documentation pattern (routing table, per-subsystem invariant docs, mistakes log, feature
inventory) and only ~20% bespoke tooling (the DataModelLM visual editor, Excalidraw MCP), and
the tooling 20% is **not worth copying** for a CLI and a small Next.js app (comp §"Not worth
copying"; a static Mermaid diagram captures the value).

For rally specifically, items 8–9 (persisted file-scoped claims + resume hints) are the most
strategically valuable: they convert rally from an ephemeral coordination cache into a durable,
queryable ownership ledger — the one capability nimbalyst has (`session_files`) that rally's
write-boundary model is otherwise stronger than but cannot yet *prove* over time (coord
comparison table).

---

## Calm-Precision guardrails (astronomer UI items)

Per the owner's design ethos, while adopting ui items: keep accent colors as thin top borders
(not per-item borders around list rows — ui §8 conflict note), keep status as text/icon color
not filled badges, and keep walkthroughs/tooltips non-modal and dismissible. Drop the
Electron-only patterns entirely: IPC theme propagation, the Discord project rail shell, and
Virtuoso virtualization (lists are ≤200 items) — ui §"Not worth copying".

---

## Process finding from the dogfood (not from nimbalyst)

Running this assessment *through* rally surfaced a **doc-vs-binary drift in agent-rally-point
itself**: `RALLY.md` and `dynamic-workflows/skills/claude/SKILL.md` document a
`rally enter / say / next / room / check before-write` command surface, but the installed
binary (`~/.cargo/bin/rally`) exposes `rally start / claim / artifact / task / decision /
preflight / report / context`. Either the docs are aspirational or the installed binary is
stale relative to `crates/rally-cli`. **Recommend:** reconcile RALLY.md + the dynamic-workflows
skill against the actual CLI surface (or `cargo install --path crates/rally-cli` if the binary
is behind), and add a doc-vs-CLI consistency check. This is itself a Tier-1-style legibility fix
and would have been caught earlier by item 3 (FEATURE_INVENTORY of the actual subcommands).
