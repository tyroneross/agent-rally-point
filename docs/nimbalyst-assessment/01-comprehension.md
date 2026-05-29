# nimbalyst comprehension patterns

Nimbalyst makes its own codebase legible to agents and new developers through a layered documentation system: an area-gated reference table in `CLAUDE.md` routes an agent to the right `docs/*.md` before touching any subsystem, each doc carries its own architecture diagram or ASCII flow, and a living `agent-mistakes.md` turns past blunders into institutional memory. The result is a repo where comprehension of any one component does not require reading everything — you follow the pointer.

---

## What nimbalyst does well (with citations)

### 1. CLAUDE.md documentation reference table as a routing layer

`CLAUDE.md:134–174` contains a two-column table (`File` | `Read when…`) with 28 entries covering every subsystem. Each row names the exact `docs/` file and the trigger condition ("before editing X, read docs/Y.md"). The table is the single most load-bearing comprehension artifact: an agent consulting the table before editing IPC code will land in `IPC_GUIDE.md`; one touching editor state lands in `EDITOR_STATE.md`. This prevents cross-concern confusion without requiring the agent to know the full map upfront.

**Why it aids comprehension:** Reduces from "read everything" to "read exactly this one doc" per task.

### 2. Per-subsystem docs with embedded ASCII/Mermaid architecture flows

`docs/FILE_WATCHING_AND_CHANGE_TRACKING.md:7–51` opens with a five-layer ASCII stack (React UI → Jotai atoms → IPC listeners → Main process watchers → PGLite) before any prose, naming every class involved. `docs/TRANSCRIPT_ARCHITECTURE.md:16–44` uses a nine-step ASCII pipeline with arrows showing the exact data flow. `docs/FILE_WATCHER_DIFF_SYSTEM.md:18–72` draws a branching flow for the pre-edit tag lifecycle. Diagrams precede description, so a reader can orient in seconds.

**Why it aids comprehension:** A glance at the diagram gives the mental model; prose fills details for the part you need.

### 3. Database schema doc with ER diagram + file:line cross-references

`docs/DATABASE_SCHEMA.md:15–91` contains a fenced Mermaid `erDiagram` showing all six tables with FK relationships and column types, followed by per-table prose sections. Each table section ends with "Related Code: `packages/electron/src/main/services/<file>.ts`" (`DATABASE_SCHEMA.md:129`, `151`). A `session_files` table column includes a hard warning: "**Hierarchy rules — READ BEFORE WRITING CODE THAT CREATES SESSIONS:** See SESSION_HIERARCHY.md" (`DATABASE_SCHEMA.md:117–118`).

**Why it aids comprehension:** Schema + code pointers + forward-refs to invariant docs collapse a 3-hop investigation into one file.

### 4. session_files table + FilesEditedSidebar — agent-file provenance as a first-class entity

`docs/DATABASE_SCHEMA.md:133–153` defines `session_files` with `link_type CHECK ('edited','referenced','read')` and indexes on `session_id`, `file_path`, and composite `(session_id, file_path, link_type)`. `packages/electron/src/main/services/SessionFileTracker.ts:1–9` documents that the tracker ensures "files modified by agents are (1) tracked in session_files, (2) have file watchers attached, (3) have tracker items refreshed." The `FilesEditedSidebar` component (`packages/electron/src/renderer/components/AgentMode/FilesEditedSidebar.tsx:1–13`) surfaces this as "files touched by session, grouped by workstream."

**Why it aids comprehension:** Makes agent authorship of files queryable — a human or another agent can ask "what did this session touch?" and get a typed, indexed answer.

### 5. DocumentContextService doc as a decision-table spec

`docs/DOCUMENT_CONTEXT_SERVICE.md:27–76` uses two side-by-side tables — one for the transition state machine (opened/closed/switched/modified/none), one for content optimization by transition × provider — before any implementation detail. The tables are the spec; the code is the execution. The doc also names the deliberate tradeoff: "only hash is persisted (not full content)" to keep DB small, trading away diff computation on first post-restart message (`DOCUMENT_CONTEXT_SERVICE.md:123–127`).

**Why it aids comprehension:** A future contributor can read the tables, understand the contract, and write a test without reading the source.

### 6. SESSION_HIERARCHY.md as an invariant enforcer with failure mode description

`docs/SESSION_HIERARCHY.md:6–8` opens with "Sessions nest at most one level deep. **A session is either a root or a child of a root — never a grandchild.**" It enumerates the five legal role combinations in a table (`SESSION_HIERARCHY.md:19–31`), marks six illegal combinations with ❌, and closes with "Why this matters" — when the invariant breaks, `sessionListRootAtom` silently swallows sessions (`SESSION_HIERARCHY.md:84–90`). Each code path that enforces the rule is named with file:line.

**Why it aids comprehension:** Framing the invariant as "this is how sessions disappear silently" gives the reader a concrete failure to reason about, not an abstract rule.

### 7. agent-mistakes.md as institutional anti-pattern memory

`.claude/agent-mistakes.md:1–68` is an append-only log of past agent mistakes: `git stash` destroying a working tree, committing `nimbalyst-local/` plan files, announcing "fixed" before E2E verification. Each entry has `What happened` / `Fix` / `Lesson`. When a mistake recurs, it graduates to a `.claude/rules/*.md` file. The escalation path from mistakes → rules → CLAUDE.md is documented in `docs/THE_HARNESS.md:62–65`.

**Why it aids comprehension:** Converts "we learned this once" into persistent, searchable context that prevents the same blunder in a future session.

### 8. FEATURE_INVENTORY.md as a queryable product surface

`docs/FEATURE_INVENTORY.md:1–363` catalogs every product feature in named sections (Editors, AI Sessions, Workstreams, Git, Extensions, etc.) with one-line descriptions and cross-references to related slash commands or MCP tools. A new developer or agent can ask "does Nimbalyst have X?" and get a yes/no + where without opening any source file.

**Why it aids comprehension:** Prevents building features that already exist and orients cross-cutting work (e.g., "what already uses the meta-agent API?").

### 9. THE_HARNESS.md — a seven-layer map of everything built around the agent

`docs/THE_HARNESS.md:18–27` presents a table of seven harness layers (Instructional / Capability / Workflow / Observability / Verification / Coordination / Provenance & Tracking) with what each does and where it lives. This is meta-comprehension: not just documenting the product, but documenting the documentation + tooling scaffolding itself.

**Why it aids comprehension:** An agent entering the repo for the first time gets a map of what help exists before it needs to use any of it.

---

## Adoptable for agent-rally-point

- **Area-gated CLAUDE.md reference table** — Add a `docs/` routing table to `CLAUDE.md` mirroring `nimbalyst/CLAUDE.md:134–174`. Rows: `RALLY_ARCHITECTURE.md` (when touching room state or fact store), `ORCHESTRATION.md` (when touching coordination protocol), `dynamic-workflows/README.md` (when touching workstream routing). Currently `RALLY.md` and `RALLY_ARCHITECTURE.md` are separate and there is no pointer that tells an agent which to read before which subsystem. **[S]**

- **ASCII pipeline diagram in ORCHESTRATION.md** — Add a five-layer stack diagram (CLI `rally enter` → fact store read → room state projection → agent context delivery → boundary check) analogous to `FILE_WATCHING_AND_CHANGE_TRACKING.md:7–51`. The current `ORCHESTRATION.md` uses prose paragraphs with no visual anchor for the data flow. **[S]**

- **agent-mistakes.md** — Create `.claude/agent-mistakes.md` with the same append-only format. Seed with known failure patterns from audit notes (e.g., agents writing to wrong coordination slot, agents not checking boundary before acting). Promotes to `.claude/rules/` on recurrence. **[S]**

- **Mermaid ER diagram for the fact store schema** — `docs/schemas/` already exists; add a `DATABASE_SCHEMA.md` with a Mermaid `erDiagram` for the TOML/JSON coordination files, including which keys are agent-authored vs. CLI-written. The `session_files` table model in nimbalyst (`DATABASE_SCHEMA.md:133–153`) shows how much clarity a typed, indexed "what did this agent touch?" store adds. **[M]**

- **Subsystem-level invariant docs** — Rally has a room-state invariant (one active owner per boundary, facts are append-only, etc.). Document these like `SESSION_HIERARCHY.md` — enumerate the legal states, mark illegal ones, and name the failure mode (e.g., "when two agents claim the same boundary, X silently wins"). **[M]**

- **FEATURE_INVENTORY.md** — One-page catalog of every `rally` CLI subcommand, every dynamic-workflow hook, and every MCP tool exposed — analogous to `FEATURE_INVENTORY.md`. Prevents duplicate capability growth and helps a new contributor understand surface area without running `rally --help` and reading source. **[S]**

---

## Adoptable for agent-astronomer

- **Area-gated reference table in CLAUDE.md** — The current `CLAUDE.md` has a good "Where things live" section but no routing table. Add a two-column table (`File` | `Read when…`) covering at minimum: `lib/deps-graph.ts` (when adding dependency extraction), `lib/library.ts` (when changing how the catalog is assembled), `drizzle/` schema (when touching persistence), `app/api/` (when adding routes). **[S]**

- **ASCII data flow diagram for the skill scan pipeline** — The catalog build path (scan paths → `scanner.ts` / `agent-scanner.ts` / `claude-md-scanner.ts` → `library.ts` → `dedup.ts` → API response) is complex enough to warrant an architecture diagram, similar to `TRANSCRIPT_ARCHITECTURE.md:16–44`. Currently all of this is in prose comments spread across `CLAUDE.md:24–50` (the lib table). **[M]**

- **agent-mistakes.md** — Same as rally-point: seed with known failure modes specific to astronomer (e.g., calling dedup from usage/propagate endpoints — which `CLAUDE.md:38` already calls out — and the cloud v1 / Phase 0 amputated path confusing new agents). **[S]**

- **Invariant doc for deduplication and propagation** — The `dedup.ts` note ("Do NOT call dedup from usage or propagate endpoints — they need the non-deduped list", `CLAUDE.md:38`) is a load-bearing invariant buried in a table row. Pull it into a `docs/DEDUP_INVARIANTS.md` with the same structure as `SESSION_HIERARCHY.md`: legal call sites, illegal call sites ❌, failure mode. **[S]**

- **DataModelLM-style visual for the shadow-git structure** — The `.astronomer/shadow/<itemId>/` hierarchy (`CLAUDE.md:9–18`) maps well to an Excalidraw or Mermaid diagram showing how a real skill file maps to its shadow git repo and djb2 ID. TAG:INFERRED — astronomer has no UI to render `.excalidraw`, but Mermaid in a markdown doc is sufficient and free. **[S]**

---

## Not worth copying

- **Excalidraw MCP integration for architectural decisions** — `docs/ARCHITECTURE_DIAGRAMS.md` requires the Excalidraw MCP server and a running Nimbalyst instance to create live-rendered `.excalidraw` files. Both rally-point (CLI/protocol, no UI) and astronomer (Next.js, no Nimbalyst dependency) lack this infrastructure. Mermaid in markdown achieves 80% of the benefit with zero tooling cost.

- **Seven-layer harness meta-doc (THE_HARNESS.md)** — `docs/THE_HARNESS.md` catalogs the full agent scaffolding including MCP servers, per-user memory, skills, E2E test tooling, and collab observability. Rally-point and astronomer each have roughly two harness layers (instructional + a few MCP tools); a seven-section harness doc would be documentation debt, not comprehension value. Tag as useful once the tooling surface grows to match.

- **DataModelLM visual ER editor (`.datamodel` files)** — Nimbalyst's DataModelLM extension (`packages/extensions/datamodellm/src/index.tsx`) provides a custom GUI editor for Prisma schemas rendered as drag-and-drop ER canvases. Both target projects use either simple TOML/JSON fact files (rally-point) or Drizzle schemas (astronomer) that are better served by a static Mermaid diagram. The visual editor requires the full Nimbalyst extension SDK runtime; it is not extractable.
