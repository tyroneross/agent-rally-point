<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Feature — Dynamic Workflows (durable, observable, resumable multi-agent fan-out on Rally)

> **One-line:** turn a goal into a linted MECE workstream, fan it out across any agents on any
> hosts, and have every step durably **recorded**, rendered as a **DAG**, and **resumed** through
> trust-gated wake signals — while Rally itself never executes anything.
>
> **Read this if** you are picking up dynamic workflows cold and need the whole arc — what it is, the
> pieces, how they compose, and the boundary that keeps it safe.

---

## 1. What it is

Dynamic Workflows is Agent Rally Point's take on multi-agent fan-out. You decompose a goal into a set
of disjoint tasks, prove they can run in parallel without colliding, dispatch one agent per task, and
coordinate the whole batch through the `rally` ledger.

Three properties distinguish it from in-memory fan-out frameworks (e.g. pi-dynamic-workflows, whose
routing primitives this module adapts):

- **Durable** — every task's start (`claim`) and finish (`artifact`) is a fact in Rally, not a row in
  one parent process's RAM. The driver process can die, the session can end, hours can pass, the work
  can span machines — a fresh agent reconstructs exactly what's done and re-dispatches only the rest.
- **Observable** — `run` / `step` / `parent-step` lineage markers on every fact let `rally dag` rebuild
  the causation graph and tag each step `landed | in_flight | stalled` (laggard detection).
- **Resumable** — an idle or blocked agent goes **dormant** (`standby`) instead of busy-waiting or
  handing back. Once its `wake_after` passes, `rally wake-due` surfaces it with a *suggested* resume
  command, and an external runner fires it.

**Host-neutral.** The skill names no specific agent. Claude (`Agent`/`Task`), Codex (delegation), Pi
(child agents), tmux/SSH sessions across machines — each host supplies its own `<TOOL>` id and its own
fan-out mechanism. Rally's contract is identical for all of them. The same `.workstream.json`
descriptor and the same per-task loop run anywhere.

---

## 2. The pieces and how they compose

Four cooperating layers, from authoring a plan down to firing a resume:

```
  skills/rally-workflows/SKILL.md   ── decompose → lint → fan out → per-task rally loop
            │ (calls)                  (the human/agent-facing entry point)
            ▼
  dynamic-workflows/  (Node module)  ── lint · route · limiter · status
            │ (descriptor + lineage)    (zero-dependency host-side scaffolding)
            ▼
  the observation seam               ── rally say {claim|artifact|standby|wake|handoff|decision}
   (docs/ORCHESTRATOR_SEAM.md)          + --run / --step / --parent-step lineage
            │ (facts in the ledger)     ──► rally dag  (causation graph, status tags)
            ▼                            ──► rally wake-due  (trust-gated resume signals)
  rally watch  (the runner)          ── polls wake-due / activity, INVOKES suggested_command
            │                            (the ONLY layer that executes — swappable for cron / LaunchAgent / Build Loop)
            ▼
  mini-loop  (per-task quality)      ── assess → plan → execute → mini-judge, wrapped around each task's work
```

### 2.1 `skills/rally-workflows/SKILL.md` — the orchestration entry point

The host-neutral skill that runs the whole arc. Four moves:

1. **Decompose** — author a JSON workstream descriptor: `workstream` (objective), `description`
   (drop-in context), and a non-empty `tasks[]` where each task carries `id`, `intent`, `owns`,
   `validation`, `output` (+ optional `depends_on`, `tier`, `commands`).
2. **Lint** — run `workstream-lint.mjs`; do not dispatch until it exits 0.
3. **Fan out** — Tier 1 (default) host-native subagents, width from `resolveFanout()` (default 10,
   hard ceiling 12); Tier 2 cross-host via `rally run` + `rally inject` when work spans
   hosts/terminals/machines.
4. **Per-task rally loop** — each agent: `enter → claim → check before-write → do work → artifact →
   release → next`, stamping `--run`/`--step`/`--parent-step` on every fact so the batch stays
   observable. Idle/blocked → `standby` (dormant), not `blocker` (hard stop).

The skill resolves three host knobs at runtime (`<TOOL>` id, fan-out authorization, flag
conventions) and hard-codes no agent identity.

### 2.2 `dynamic-workflows/` — the Node module (zero runtime deps)

Host-side scaffolding the skill leans on. Drop-in, no `npm install`.

| File | Role |
|------|------|
| `core/workstream-lint.mjs` | **Lint** — source of truth for descriptor validity. Enforces four rules: structural completeness, **MECE** write boundaries (no two write-tasks `own` overlapping paths, prefix-aware), **determinism** (rejects `Date.now()` / `Math.random()` / `new Date()` in declared commands), and dependency integrity (`depends_on` resolves, no cycles). Exit `0` valid · `1` violations · `2` parse error. |
| `core/route.mjs` | **Route** — host-neutral `parallel()` / `pipeline()` + budget accounting with `onError`/abort failure-visibility. Owns concurrency/ordering only; the host supplies the actual agent-spawn thunks. Adapted from pi-dynamic-workflows (MIT). |
| `core/limiter.mjs` | **Limiter** — `createLimiter(n)`: bounded-concurrency helper so a host caps its own Tier-1 fan-out without a concurrency library. Imported only from `./limiter.mjs` (one canonical path). |
| `core/fanout.mjs` | **Fan-out resolver** — `resolveFanout()`: returns `effective_max` as the min of named caps (`requested_or_config`, `hard_ceiling`, host-supplied `host`, `ready_tasks`, `room_headroom`) plus the `limiting_factors` that produced it. Replaces the former hardcoded ≤4. Default 10, hard ceiling 12; the ceiling guards coordination overhead, not write safety — write safety is the linter's disjoint-`owns` proof, which holds at any N. `liveAgentsFromRoom()` derives `room_headroom` from a `rally room --json` snapshot (squads that are `fresh` AND `active`, minus your own tool ids) — the one sizing input a host cannot see for itself. |
| `core/workstream-status.mjs` | **Status / resume** — the durable counterpart to pi's in-memory `RuntimeState`. Given a descriptor + a `rally room --json` snapshot, classifies each task `done | claimed | pending` and computes the `to_dispatch` set (pending tasks whose deps are all done). Exit `0` complete · `3` work remains · `2` usage/parse error. Convention: a task is *done* when an `artifact`'s subject names its id, *claimed* when an active claim names the id or overlaps its `owns`. **Reads and derives only.** |

`PROTOCOL.md` is the canonical wire spec; `COORDINATION.md` covers multi-autonomous-agent doctrine;
`MODEL-TIERS.md` the frontier/executing/fast taxonomy.

### 2.3 The observation seam — facts in, projections out

The contract that makes fan-out durable + observable. Defined in `docs/ORCHESTRATOR_SEAM.md`.

- **Event vocabulary (6 kinds):** `handoff`, `claim`, `artifact`, `decision`, `standby`, `wake`. Each
  maps to a `FactKind` in the ledger. Invariant: `artifact` closes a `claim`; `wake` *or* `artifact`
  closes a `standby`.
- **Lineage fields (additive scope markers, no schema change):** `run:<id>` groups one fan-out batch,
  `step:<id>` identifies a step, `parent-step:<id>` is the causation link. Stamped via
  `rally say <kind> --run … --step … --parent-step …`.
- **`say standby` / `say wake`** encode the dormancy lifecycle: `standby --reason <r> --wake-after
  <+30m|iso>` arms a future wake; `wake --ref-standby <id>` acknowledges it.
- **`rally dag --run <id>`** reads every fact carrying `run:<id>`, groups by `step:`, and derives nodes
  (`landed` = has artifact; `stalled` = standby past `wake_after` with no wake/artifact; `in_flight` =
  otherwise) + edges (from `parent-step` markers and `ref_id` links).
- **`rally wake-due`** projects standby facts whose `wake_after` has passed **and** whose owner is a
  trusted room participant, each carrying a `suggested_command` **string Rally never runs**.

### 2.4 `rally watch` — the runner (the only executor)

The reference runner. Polls `rally wake-due --json` / new activity and **invokes** the suggested
command (e.g. `rally next --tool <TOOL> --json`). Swap in a LaunchAgent, cron, or Build Loop here —
Rally's contract is unchanged. This is the one layer that fires work.

### 2.5 `mini-loop` — per-task quality wrapper

`skills/mini-loop/SKILL.md`: a zero-dependency assess → plan → execute → mini-judge loop wrapped
around each task's *do the work* step. It checks the result against that task's own `validation` and
`output` contract **before** the agent posts an artifact, catching a wrong-but-plausible result at the
task instead of at integration.

---

## 3. Worked example — a 3-task workstream, end to end

Goal: harden error handling across three disjoint modules of the rally CLI. (Descriptor:
`dynamic-workflows/examples/audit-repo.workstream.json`.) Tasks: `store-errors` (writes
`store.rs`), `check-errors` (writes `check.rs`), and `review` (read-only, `depends_on` both).

### 3.1 Lint, then fan out with lineage

```bash
# 0 = valid → safe to dispatch
node dynamic-workflows/core/workstream-lint.mjs dynamic-workflows/examples/audit-repo.workstream.json

# Pick one run_id for the whole batch (stable string; minted at fan-out time, not a descriptor field).
RUN=ws_audit_rally_errors

# Two write-tasks fan out in parallel (disjoint owns → MECE-safe). Each stamps run + step:
rally say claim --tool claude:audit:1 --subject "Add context to fact-store IO" \
  --path crates/rally-cli/src/store.rs --run $RUN --step store-errors --json
rally say claim --tool codex:audit:1  --subject "Add context to boundary-check failures" \
  --path crates/rally-cli/src/check.rs --run $RUN --step check-errors --json

# … each agent runs its work through mini-loop, then posts an artifact closing its claim:
rally say artifact --tool claude:audit:1 --subject "store-errors: contextual Results" \
  --uri crates/rally-cli/src/store.rs --evidence "cargo test -p rally-cli store:: → ok" \
  --run $RUN --step store-errors --json

# The review task depends on both — it carries TWO parent-step markers (one per depends_on entry):
rally say claim --tool claude:audit:1 --subject "Confirm consistent error style" \
  --run $RUN --step review --parent-step store-errors --parent-step check-errors --json
```

### 3.2 Observe — `rally dag --run $RUN --json`

After both writers land and `review` is claimed (not yet artifacted):

```json
{
  "run_id": "ws_audit_rally_errors",
  "nodes": [
    { "step_id": "store-errors", "run_id": "ws_audit_rally_errors",
      "status": "landed",
      "tool": "claude:audit:1",
      "event_ids": ["fact_claim_se", "fact_art_se"],
      "subjects": ["Add context to fact-store IO", "store-errors: contextual Results"] },
    { "step_id": "check-errors", "run_id": "ws_audit_rally_errors",
      "status": "landed",
      "tool": "codex:audit:1",
      "event_ids": ["fact_claim_ce", "fact_art_ce"],
      "subjects": ["Add context to boundary-check failures", "check-errors: actionable messages"] },
    { "step_id": "review", "run_id": "ws_audit_rally_errors",
      "status": "in_flight",
      "tool": "claude:audit:1",
      "event_ids": ["fact_claim_rev"],
      "subjects": ["Confirm consistent error style"] }
  ],
  "edges": [
    { "from_step": "store-errors", "to_step": "review", "kind": "parent_step" },
    { "from_step": "check-errors", "to_step": "review", "kind": "parent_step" }
  ],
  "facts_scanned": 5
}
```

Both writers `landed`; `review` is `in_flight` waiting on nothing now its deps are done. Two
`parent_step` edges fan in to `review`. (Field names are the literal serialized `DagNode` /
`DagEdge` / `DagOutput` shapes from `crates/rally-cli/src/dag.rs`.)

### 3.3 Idle → standby → resume

Suppose `review` could not start yet because one dependency was still open. The review agent goes
dormant rather than busy-waiting:

```bash
rally say standby --tool claude:audit:1 --reason "waiting on check-errors" \
  --wake-after +30m --run $RUN --step review --json
```

In `rally dag`, the `review` node now reads `stalled` once `+30m` passes with no `wake`/`artifact`.
Thirty minutes on, the standby surfaces in the resume projection:

```bash
rally wake-due --json
```

```json
[
  {
    "standby_event_id": "fact_standby_rev",
    "owner": "claude:audit:1",
    "reason": "waiting on check-errors",
    "wake_after": "2026-05-30T13:00:00Z",
    "suggested_command": "rally next --tool claude:audit:1 --json"
  }
]
```

The runner — not Rally — fires it:

```bash
# rally watch is the runner; it reads suggested_command and INVOKES it.
rally watch --on-activity 'rally next --tool claude:audit:1 --json'
# → agent acts on the next result, then: rally say wake --tool claude:audit:1 --ref-standby fact_standby_rev
```

`wake` closes the standby; the node returns to `in_flight`, the agent finishes, posts its `artifact`,
and `rally dag` shows all three `landed`. A fresh agent picking this up from cold runs
`workstream-status.mjs` against a `rally room --json` snapshot and is told exactly which of the three
remain — durable resume with no in-memory state.

---

## 4. The non-negotiable charter

> **Rally RECORDS and DERIVES; it NEVER EXECUTES.**
> (restated in `docs/ORCHESTRATOR_SEAM.md §0`.)

Every piece above either adds *facts* (`rally say …`) or returns *read-only projections* (`rally dag`,
`rally wake-due`, `workstream-status.mjs`). None of them start, resume, retry, or schedule work.
`wake-due` emits a `suggested_command` **string** and stops there. The **runner** (`rally watch`, or a
cron / LaunchAgent / Build Loop substituted for it) is the single layer that fires.

**Litmus for any change to this feature:** *"Does this make Rally start, resume, retry, or schedule
work?"* If yes → it belongs in the runner, not in Rally. A trust gate reinforces the boundary: a
standby authored by a tool not present in the room's squads is excluded from `wake-due`, so a fact
cannot conscript a peer's compute without trust.

---

## See also

- `docs/ORCHESTRATOR_SEAM.md` — event vocabulary, lineage encoding, standby/wake, and the runner-split contract (the seam's source of truth).
- `skills/rally-workflows/SKILL.md` — the host-neutral orchestration skill (decompose → lint → fan out → per-task loop → observe & resume).
- `dynamic-workflows/PROTOCOL.md` · `COORDINATION.md` · `MODEL-TIERS.md` — descriptor wire spec, multi-agent doctrine, model tiers.
- `skills/mini-loop/SKILL.md` — the per-task quality wrapper.
