<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Spec — Rally coordination program (auto-start · presence · rollup · mini-loop · injection)

Status: approved design, 2026-05-29. Build in dependency order via build-loop. **Governing
constraint: keep everything simple** — no new dependencies; one fact kind / one hook / one
subcommand / one guidance skill; mini-loop is text, not code. Each component is the smallest thing
that works. This spec says WHAT + acceptance; build-loop Phase 2 owns the detailed HOW.

Grounded in the 2026-05-29 E2E dogfood (this session): the work-loop (claim → collision-checked
write → artifact → release → retrospective) and repo-isolation are **verified working** across 4
concurrent agents in 2 repos. The one gap that blocks everything below: `rally enter` writes **no
presence fact** and returns `channel:null`. Component A fixes exactly that.

## Components & build order

```
A. Presence (linchpin)  →  B. Auto-start hook  +  C. Central rollup  →  D. mini-loop
                                                                         E. Command injection (verify-and-fix, independent)
```

---

### A. Presence substrate (B11 / B12 / B16) — FIRST

**Problem:** `rally enter` creates the room/ledger but records no fact; `rally room` shows no
participants; `enter --json` returns `channel:null`. Agents are invisible until they claim.

**Change (minimal):**
1. `rally enter` emits exactly **one** fact, `kind: "presence"` (schema `agent-rally.fact.v1`,
   reusing the existing envelope) carrying `tool`, resolved `room` id, `ts`. First `enter` with no
   existing `role:lead` also self-asserts lead (one extra `decision` fact, as already specced in B11).
2. `rally room` projection gains `squads[]` (distinct `tool`s seen, each with `last_seen_seq`,
   `last_seen_ts`, `active|idle` derived from recency) and top-level `lead`.
3. `rally enter --json` and every mutating command return the **resolved room id** (non-null).

**Acceptance:**
- `enter` then `room --json` → `data.room.squads[]` contains the tool; `data.room.lead` set.
- `enter --json` → room id non-null.
- Round-trip test (B16): the presence fact written by `enter` is read back identically via `room`,
  `recent`, and survives a ledger replay.
- Existing work-loop tests stay green.

---

### B. Auto-start = lazy auto-enter in the CLI (NO hook)

**Decision (2026-05-30):** not a SessionStart hook (hooks kept rare per project doctrine). Instead,
**any tool-scoped rally command auto-registers presence if the calling `--tool` has not yet entered
this room's current engagement.** Reuses Component A's presence-write (+ first-enter-is-lead).
Navigate-to-repo is the CLI's existing repo-root resolution. Deterministic, zero hook, zero new
process, cross-host — any agent that touches `rally` at all is registered. **No persistent
background watcher** (YAGNI); revisit an MCP server only if a live watcher is later needed.

**Change:** extract A's presence-write into a reusable `ensure_presence(tool)` and call it at the
start of tool-scoped command handlers (`say`, `check`, `next`, `inject`, `run`, …) — write the
presence fact only when no presence fact for that tool exists in the current engagement. Explicit
`rally enter` is unchanged. Read-only commands without `--tool` do nothing new.

**Acceptance:**
- A fresh agent that runs `rally say claim …` (no explicit `enter`) appears in `rally room`
  `squads[]` (auto-entered) and `lead` is set if it was first.
- A second command by the same tool writes NO duplicate presence fact (idempotent per engagement).
- Existing explicit `enter` and all work-loop tests stay green.

---

### C. Central rollup — `rally status --global`

**Change:** one new read-only Rust subcommand. Walk `~/.agent-rally-point/rooms/` registry → for
each known repo room read its projection → emit aggregated high-level status JSON: per repo
`{ repo, room, lead, open_claims, last_activity_ts, alive_agents[] }`. Optionally write a
**regenerable** cache `~/.agent-rally-point/rooms/status.json`. **Never a write target for repo
facts** — derived only; deleting the cache loses nothing.

**Acceptance:**
- With ≥2 repos having rooms, `rally status --global --json` lists each with correct lead / open
  claims / last activity, derived live from per-repo ledgers.
- No repo fact is ever written by this command (verified: ledgers unchanged after running it).
- Cross-repo isolation preserved (repo A's claims never attributed to repo B).

---

### D. mini-loop (guidance-only, in `dynamic-workflows/`)

**Change:** add `dynamic-workflows/skills/mini-loop/SKILL.md` — an ultra-light loop the host LLM
runs per dynamic-workflows task to raise fan-out accuracy. Phases: **assess** (restate the task's
intent / owns / validation / output) → **plan** (1–3 steps) → **execute** → **mini-judge** (does the
result satisfy the task's own `validation` + `output` contract? `pass` | `flag` with reason). On
`flag`, one retry, then surface. **Strips everything** from build-loop except this core: no IBR,
NavGator, mockup-gallery, ui-design, architecture/debug scripts, no sub-agents, no extra files. The
Rally Flow per-task loop (SHARED protocol §4 → now `rally-workflows`) references it as the
recommended quality wrapper.

**Acceptance:**
- `mini-loop/SKILL.md` exists, ≤120 lines, references only the task contract — zero tool/script deps.
- `rally-workflows/SKILL.md` (the per-task loop) points to mini-loop as the optional quality wrapper.
- A demo task run through mini-loop shows the mini-judge catching an output that violates its
  `validation` contract.

---

### E. Command injection (verify-and-fix existing Tier-2)

**Existing machinery:** `rally run <agent> --backend tmux --name <s>` starts an idle managed
session; `rally say handoff … → <event-id>`; `rally inject <s> --handoff <event-id> --require-ack`
delivers instructions and waits for ack. Goal: **ensure the full path works**, fix only what's
broken — do not redesign.

**Verify (E2E, like the work-loop test):**
1. Lead starts an idle subagent session (`rally run`).
2. Lead injects a handoff with `--require-ack`; subagent receives it and emits an `ack`/`seen` fact.
3. The injected subagent can itself `rally inject` a further instruction (recursive lead→sub→sub
   instruction delivery).
4. Clean stop releases the session.

**Acceptance:** each step above produces the expected fact and ack; any failure gets a minimal fix
(no new abstraction). Tracked against B13's session-bound-ack note where they overlap.

---

## Cross-cutting simplicity guardrails

- No new runtime dependencies in any component.
- Presence = exactly one new fact kind; rollup = exactly one new subcommand; auto-start = exactly
  one hook; mini-loop = exactly one guidance skill (text).
- Central rollup stays **derived/read-only** — it does not revive the retired global write-store
  (B17) and respects repo-scope-is-truth (Lesson #9, B18).
- mini-loop is host-LLM guidance, never a script port (the "host agent is the LLM" rule).
- Every component ships with its acceptance test green before the next dependent one starts.
