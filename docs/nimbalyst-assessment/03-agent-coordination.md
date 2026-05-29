# nimbalyst coordination vs rally

Nimbalyst is a **visual executor host**: it owns the full session lifecycle — spawning agent
subprocesses, persisting transcripts in PGLite, presenting kanban/history UIs, and enforcing
file-edit ownership — inside an Electron process. Rally is a **headless fact-store facilitator**:
it records claims, handoffs, artifacts, decisions, and blockers in a repo-local SQLite
(`.rally/facts.db`) but never starts or stops any agent. The two models are complementary:
Nimbalyst lacks cross-agent write-boundary enforcement; rally lacks UI and executor machinery.

---

## How nimbalyst coordinates (with citations)

### Sessions and persistence

Sessions are rows in `ai_sessions` (PGLite), created via
`packages/electron/src/main/ipc/SessionHandlers.ts:276–326`. Each row carries:
`provider`, `model`, `worktree_id`, `parent_session_id`, `agent_role` (`standard` |
`meta-agent`), `session_type` (`session` | `workstream` | `blitz`). Session lifecycle is
tracked in `SessionStateManager`; analytics emitted per-session via `AnalyticsService`.

**Resume / search.** Full-text search is available via
`sessions:search` IPC (`SessionHandlers.ts:766`). A session can be resumed at any time
(messages are stored in `ai_agent_messages`). Draft input is persisted via
`sessions:update-draft-input`, and a "last-read" timestamp is stored to show unread badges.

**File-to-session linkage.** `session_files` rows record every path a session edited,
with `link_type = 'edited'` and a timestamp. The query
`SessionHandlers.ts:102–158` joins uncommitted git paths against this table to surface
"this session has N uncommitted files" in the kanban. Cross-worktree path matching
(`sessions:get-by-file`, `SessionHandlers.ts:919–1003`) uses a `LIKE '%' || relative_path`
predicate against all sibling worktree paths so the UI shows sessions from main AND worktrees
that touched the same logical file.

### Workstreams (parent → child session hierarchy)

`docs/SESSION_HIERARCHY.md` defines the two-layer invariant: sessions nest at most one level
deep. Three structural roles:

| Role | `session_type` | `parent_session_id` | `worktree_id` |
|---|---|---|---|
| Standalone | `session` | NULL | NULL |
| Workstream parent | `workstream` | NULL | NULL |
| Workstream child | `session` | parent.id | NULL |
| Worktree-resident | `session` | NULL | worktree.id |
| Blitz parent | `blitz` | NULL | NULL |

`MetaAgentService.resolveOrCreateWorkstream` (`MetaAgentService.ts:556–621`) promotes a
standalone session into a workstream container (creating a `session_type='workstream'` row,
reparenting the original session) on first child spawn. Worktree-resident sessions are
never wrapped in a workstream row — the worktree IS the workstream
(`SESSION_HIERARCHY.md:34–52`).

Child sessions can be spawned via MCP from an agent using
`spawn_session` / `create_session` tools defined in `metaAgentServer.ts:251–338`. The
`spawn_session` tool carries a self-contained "handoff brief" prompt parameter; fire-and-forget
is the default (`notifyOnComplete=false`). There is no durable cross-agent handoff record —
the handoff is only a text prompt injected into the child session.

### Worktrees and git

`GitWorktreeService` (`main/services/GitWorktreeService.ts`) manages `git worktree add/remove`
via `simple-git`. Worktrees are created in `../{project}_worktrees/` with auto-generated
`worktree/{adjective-noun}` branches. The `worktrees` table stores id, name, path, branch,
`base_branch`, and `workspace_id`. Status polling (`worktree:get-status` IPC) returns
`commitsAhead`, `commitsBehind`, `hasUncommittedChanges`, `isMerged`, and
`uniqueCommitsAhead` (via `git cherry`).

`WorktreeHandlers.ts:74–91` archives sessions for a worktree on deletion by calling
`AISessionsRepository.updateMetadata(sessionId, { isArchived: true })` for each session.
Terminal instances are cleaned up via `getTerminalsByWorktreeId` / `deleteTerminalInstance`.

Git integration is event-driven: `GitRefWatcher` watches `.git/refs/heads/<branch>` and
`.git/index` via Chokidar, invalidates `GitStatusService` cache (5 s TTL), and emits
`git:status-changed` and `git:commit-detected` IPC events to all windows
(`docs/GIT_INTEGRATION.md`). No polling as of 2026-01-23.

AI commit: `git_commit_proposal_request` prompts are durable — stored in `ai_agent_messages`
and routed via `messages:respond-to-prompt` (`SessionHandlers.ts:1191`). The response channel
(`getGitCommitProposalResponseChannel`) resolves the MCP waiter immediately if still alive;
if the subprocess exited, a synthetic `item.completed` event is persisted to DB so transcript
replay is correct.

### Permissions model

Permission model is **per-project, per-tool-call-pattern**:
- Trust mode: `Ask for Approval` (default) | `Always Allow` | `Bypass All`
- Read-only bash commands and git status commands are auto-approved
- Write operations prompt on first use with scope: one-time / session / always-in-project
- Approved patterns are saved to `.claude/settings.local.json`
(`docs/AGENT_PERMISSIONS.md:1–80`)

Extension permissions use a brokered capability model (`extension-sdk/src/types/permissions.ts`):
`workspace-files`, `nimbalyst-database-read`, `nimbalyst-database-write`, `secrets-read`,
`mcp-server-register` — three risk tiers (`low` | `elevated` | `high`). Backend module grants
give ambient Node access; no in-process sandbox exists for raw `fs`/`net`/`child_process`.

### Kanban / room state projection

Sessions, workstreams, and worktrees are projected onto a 5-phase kanban board
(`sessionKanban.ts:26`): `backlog → planning → implementing → validating → complete`.
Phase is stored in `metadata.phase` on each session row. The kanban also surfaces
`ChildRunStateSummary` — `running`, `waiting`, `review`, `idle`, `done`, `total` child counts
per workstream/worktree card.

### Internal MCP surface

Four internal MCP servers run inside the Electron main process over HTTP/SSE (localhost):
1. **Shared MCP** — `applyDiff`, `streamContent`, `capture_editor_screenshot`
2. **Session Naming MCP** — extension build/install/reload, app restart, log retrieval
3. **Extension Dev Kit MCP** — extension-specific dev tools
4. **Settings Control MCP** — `workspace_create/open`, `ai_set_default_model`,
   `features_toggle`, `sync_set_for_project`; rate-limited to 30 writes/60 s; excluded from
   meta-agent profile (`docs/INTERNAL_MCP_SERVERS.md:1–90`)

The **Meta-Agent MCP** (`metaAgentServer.ts`) is the inter-session coordination bus: tools
`list_worktrees`, `create_session`, `spawn_session`, `get_session_status`, `get_session_result`,
`send_prompt`, `respond_to_prompt`, `list_spawned_sessions`.

---

## Where rally is stronger / where nimbalyst is stronger

| Dimension | Rally stronger | Nimbalyst stronger |
|---|---|---|
| Write-boundary enforcement | `rally check before-write` blocks a second agent from writing a path already claimed (MECE enforcement per workstream descriptor) | No cross-session boundary check; agents can silently collide on the same file |
| Durable facts | `artifact`, `decision`, `lesson`, `blocker` are first-class indexed records in `.rally/facts.db`, queryable across sessions | No equivalent — decisions/lessons live only in transcript text |
| Handoff semantics | `rally say handoff --target <tool>` is a typed, routed record with a summary and target; the target reads it via `rally next` | `spawn_session` injects a free-text handoff brief into the child as a plain prompt; no structured field for "target" or "evidence" |
| Headless / CI-friendly | Runs anywhere `rally` binary exists; no desktop required | Requires Electron host; no CLI-only mode for agent-to-agent coordination |
| Multi-host routing | Agnostic to host (Claude Code, Codex, Gemini CLI, `ci`); any agent joins the same room | Tied to Electron main process; Codex sessions require the Codex provider path |
| Cross-repo rooms | Rally rooms are per-repo but portable; facts are git-committed | Session DB is per-device PGLite; no cross-machine sharing without cloud sync |
| Session persistence / resume | — | Full transcript persistence in PGLite; draft input saved; full-text search; "unread" badges; session branching |
| Worktree lifecycle | Rally can record worktree artifact facts, but does not manage git worktrees | Creates, tracks, archives, and deletes git worktrees; links sessions to worktrees |
| Visual coordination | — | Kanban with 5 phases, workstream child-run-state badges, uncommitted-file counts per session |
| AI commit workflow | — | Durable `git_commit_proposal` prompts with MCP waiter + DB fallback; synthetic completion events for transcript replay |
| Permission UX | — | Per-project trust mode, pattern-scoped approvals, risk-tiered extension grants |
| Extension / plugin surface | — | Extension SDK, backend modules, 4 internal MCP servers, manifest-driven collab support |

---

## Adoptable for agent-rally-point

### 1. Session persistence with resume metadata `[M]`
Nimbalyst stores `draft_input`, `last_read_timestamp`, and `metadata.phase` in the session row.
Rally's `handoff` and `task` facts carry context but have no "resume point" field — an agent
picking up a task must re-derive state from the room. **Adopt:** add a `resume_hint` JSON blob
field to rally's `task` fact so a returning agent reads structured context (last file touched,
next sub-step, unresolved blocker) without re-parsing transcripts.

### 2. File-to-session ownership ledger `[S]`
`session_files` records `(session_id, file_path, link_type='edited', timestamp)` and powers
"uncommitted count" badges. Rally's `claim` primitive is similar but ephemeral (release on
handoff). **Adopt:** persist `claim` facts with a `file_path` field and a `link_type`
(`claim | edit | read-only`) so `rally room` can project "which files does this agent own and
has it touched". This would close the write-collision gap with real evidence.

### 3. Worktree-per-task as a first-class spawn option `[M]`
Nimbalyst's `spawn_session` accepts `useWorktree=true` to give the child its own git branch
and directory automatically. Rally workstream tasks have an `owns` field (MECE paths) but no
mechanism to provision a worktree. **Adopt:** add an optional `worktree: true` field to the
workstream descriptor task schema, and emit a `rally say artifact` that includes the worktree
path as the URI. The host skill (Claude/Codex) would provision the worktree before claiming
task ownership.

### 4. Kanban projection of rally room state `[L]`
Nimbalyst projects `metadata.phase` into 5-column kanban cards with child-run-state badges.
Rally's `rally room --json` already exposes task status (done/claimed/pending). **Adopt:**
map rally task states (`pending` → backlog, `claimed` → implementing, `done` → complete,
`blocked` → a separate "blocked" column) so a `rally room` render can feed a read-only kanban
view. Could be a static HTML export from `workstream-status.mjs` rather than a full UI.

### 5. Structured handoff brief (not free-text prompt) `[S]`
`spawn_session` requires a free-text `prompt` as the handoff brief. Rally's `say handoff`
already has `--target`, `--subject`, `--summary` structured fields. **Adopt** in the Claude
skill for dynamic-workflows: when a host spawns a sub-agent, emit a `rally say handoff` with
`subject` = task id, `summary` = the verification command and expected output, so the target
can confirm the scope before touching files.

### 6. Permission-scope-to-task coupling `[M]`
Nimbalyst's permission system approves patterns at three scopes (one-time / session /
always-in-project). Rally's `check before-write` is binary (allow/block). **Adopt:** add an
optional `allow_patterns` array to the workstream descriptor task schema (mirroring Nimbalyst's
bash command patterns). A pre-fan-out lint rule (`workstream-lint.mjs`) can warn when two tasks
declare overlapping allow-patterns, surfacing potential permission escalation.

---

## Integration surface

Could Nimbalyst serve as a rally HOST — an Electron process that rally adapters target?

**Positive evidence:**

- **Meta-Agent MCP** (`metaAgentServer.ts`) runs on localhost HTTP/SSE and is already consumed
  by Claude Code and Codex providers. A rally adapter could connect to this MCP as a third
  client and call `create_session` / `spawn_session` / `get_session_status`. Tools are defined
  (`metaAgentServer.ts:251–338`); auth is via `requireMcpAuth`.

- **Extension SDK** (`packages/extension-sdk/src/index.ts`) lets third-party code register an
  MCP server (`mcp-server-register` permission, `types/permissions.ts:52`). A rally-nimbalyst
  extension could expose `rally`-backed tools (claim, check, artifact) as new MCP tools to any
  running agent session. TAG:INFERRED — no existing rally extension; the `mcp-server-register`
  path would require a backend module contribution with user consent.

- **IPC preload API** (`preload/index.ts`) exposes `worktreeCreate`, `worktreeList`,
  `worktreeGetStatus` — the same primitives rally's worktree-per-task adoption would need.

- **`opencode-plugin`** (`packages/opencode-plugin/src/fileSnapshotPlugin.ts`) exposes
  `OpenCodePluginHooks` for file snapshot events. This is the hook surface for detecting
  file writes — what rally's `check before-write` would need to intercept. TAG:INFERRED —
  the hook contract is minimal; it does not yet emit path-ownership events that rally could
  consume passively.

**Gaps:**
- No outbound webhook or event bus from Nimbalyst to an external process; all IPC is
  Electron-internal (main ↔ renderer). A rally adapter would need to be a process that
  connects TO the Nimbalyst MCP server, not a listener that Nimbalyst pushes to.
- PGLite database is per-device and not exposed externally; rally's `.rally/facts.db` would
  be a second, parallel fact store rather than a replacement.
- Meta-Agent MCP is workspace-scoped (workspace path in query params); the adapter would
  need to know the current workspace path before connecting — no discovery endpoint.

**Verdict:** Nimbalyst is a viable rally HOST via the Meta-Agent MCP. The path is an
extension that registers MCP tools backed by `rally` binary calls, exposed alongside the
existing `create_session`/`spawn_session` tools. Effort: `[L]` (extension authoring +
`mcp-server-register` grant + auth integration).
