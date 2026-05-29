# Handoff: pi-dynamic-workflows ↔ Agent Rally Point assessment

> **For:** a fresh Claude Code session picking up this evaluation.
> **Status:** assessment complete, no code written. Decision pending a gate (see §6).
> **Date:** 2026-05-28. **Verification:** doc/code review of both repos (no runtime test).

---

## 1. What you're picking up

A multi-turn evaluation of whether **pi-dynamic-workflows** (an external dynamic-workflow
orchestrator) should relate to **Agent Rally Point** (this repo), and if so how. The question
evolved across the session to its final form:

1. Are they **compatible**?
2. Do they **overlap**?
3. Should pi-dynamic-workflows functionality be **added to** rally-point?
4. **Benefits and drawbacks** of adding the coordination/observation layer that would let
   orchestrators like pi-dynamic plug into Rally.

Answers are settled (§5). The only open item is a build/no-build gate (§6).

---

## 2. The two artifacts + locations

### pi-dynamic-workflows (the external thing being assessed)
- **Repo:** https://github.com/Michaelliv/pi-dynamic-workflows  (MIT, ~33 stars, v1.0.0 dated 2026-05-28)
- **Not cloned locally** as of this handoff. Clone if you need source detail.
- **What it is:** a TypeScript/Node prototype that adds dynamic fan-out to **Pi** (an AI coding
  assistant). It is a near-clone of Claude Code's own Workflow tool: the model writes a small JS
  script, evaluated in a sandboxed VM, with globals `agent(prompt, opts)`, `parallel(thunks)`,
  `pipeline(items, ...stages)`, `phase(title)`, plus structured JSON-Schema output and Esc-abort.
  Determinism constraints: no `Date.now()`, no `Math.random()`, no `require`.
- **Core files (per its README):** `workflow.ts` (AST-validated parser + sandboxed runtime),
  `workflow-tool.ts` (Pi tool integration), `agent.ts` (in-memory subagent runner),
  `structured-output.ts` (terminating output tool), `display.ts` (progress).
- **Explicit non-features:** no persistence, no resumable runs, no scheduling, no retries, no
  distributed execution. Early prototype.
- **Classification:** it is an **orchestrator / in-process agent-runtime / workflow engine**.

### Agent Rally Point (this repo — the substrate)
- **Repo:** https://github.com/tyroneross/agent-rally-point.git
- **Primary checkout:** `/Users/tyroneross/dev/git-folder/agent-rally-point`
  - current branch at handoff: `codex/rally-global-discovery` @ `3d9e567`
- **Worktrees:** `-integration`, `-main`, `-rally2-primary` (same repo, sibling dirs under
  `~/dev/git-folder/`).
- **What it is:** a local-first, file-backed, **daemon-free coordination substrate** for coding
  agents sharing a repo. Rust product, CLI binary `rally`. Event-sourced: agent action → typed
  event → append to durable `changes.jsonl` → pure query projections (inbox/claims/blockers/
  diagnosis/context/next). All projections are disposable caches rebuildable from the log.
- **Crates:** `rally-core` (events, store, query, diagnose, preflight, context),
  `rally-trust` (identity, signing, six-state trust policy), `rally-protocol`/`rally-sync`
  (canonical JSON, export/import), `rally-cli` (thin renderer).
- **Direction ("attuned coordination"):** per-agent ranked, source-linked ContextBrief +
  judgment layer (`rally next` / `rally judge` / `rally hook`) recommending the highest-leverage
  next action.

---

## 3. The load-bearing boundary (read this before proposing anything)

> **Citation note:** several files cited below (`docs/RUST_GREENFIELD_ARCHITECTURE.md`,
> `docs/COORDINATION_TRACE.md`, `.bookmark/bookmark.context.md`) are from the original
> pi-dynamic-workflows repo and were **not ported into this repo**. They are preserved here
> as the source-of-record for the boundary rationale; treat the paths as external references,
> not as files present in this checkout.

Rally's charter **explicitly refuses** the execution half of pi-dynamic. Key citations in this repo:

- `docs/RUST_GREENFIELD_ARCHITECTURE.md:85-94` — Non-Goals include *"a long-running orchestrator
  that owns agent lifecycles"* and *"a general workflow engine with retries, scheduling, and task
  queues."*
- `docs/RUST_GREENFIELD_ARCHITECTURE.md:99-106` — Refuses *"agent runtime loops, model/tool
  orchestration, hosted workflow execution."* Protocol positioning: *"Rally is below orchestration."*
- `docs/RUST_GREENFIELD_ARCHITECTURE.md:504` — orchestrators (LangGraph/CrewAI/AutoGen) *"publish
  Rally events at handoff/claim/blocker/verdict boundaries."* ← the integration model.
- `README.md:31-33` / `:468-469` — *"Network transport is intentionally out of scope… Rally
  defines what the bytes mean."* (the bytes-vs-meaning seam)
- `.bookmark/bookmark.context.md:10-11, 46-49, 57` — standby/wake are **facts, not execution**;
  *"the LLM agent itself is not awake"*; *"scheduled execution belongs in an external runner"*
  (LaunchAgent/cron/systemd/Codex heartbeat/Build Loop). standby/wake is roadmapped but **not yet
  in the schema**.
- `docs/COORDINATION_TRACE.md:62` — derived state is non-authoritative, rebuildable from the log.
- Trust gate: any command that bridges event content into an agent/editor/shell must declare and
  enforce a minimum trust state (`COORDINATION_TRACE.md:65-68`, `SIGNED_EVENTS`).

**One-line rule:** Rally **records and derives**; it does **not execute**. pi-dynamic **executes**.

---

## 4. Architectural relationship

```
   pi-dynamic-workflows  (orchestrator: runs the fan-out, ephemeral, in-memory, TS-in-Pi)
            │  emits handoff/artifact/decision events at step boundaries
            ▼
   Agent Rally Point      (substrate: records facts, derives views, durable, Rust kernel)
```

They are **different layers, different runtimes** (TS VM vs Rust binary) — they never share a
process, only a log. This is the same relationship Rally already documents for LangGraph/CrewAI.
pi-dynamic is simply another orchestrator client, scoped to the Pi assistant.

---

## 5. Conclusions (settled)

| Question | Answer |
|---|---|
| Compatible? | **Yes**, complementary by design. Opt-in via an adapter; neither needs the other to run. |
| Overlap? | **Minimal.** Both touch "fan-out across agents," but pi-dynamic *executes* it while Rally would only *record + derive* (read-only DAG view). Real overlap appears **only** if Rally tries to run steps — which is the Non-Goal. |
| Add pi-dynamic functionality *into* Rally? | **No.** It is the workflow-engine + agent-runtime Non-Goal verbatim; wrong substrate (Node VM in a Rust kernel); duplicates Build Loop/LangGraph/host Workflow tool. |
| What *should* be added? | The **inverse**: a substrate-side observation seam so orchestrators publish *into* Rally. |

**The distinction that is the whole answer:**
> Don't move pi-dynamic's execution **down** into Rally. Add Rally's observation seam **up**
> toward orchestrators.

### The "coordination layer" worth adding (events + derived views + trust-gated triggers; execution excluded)

**Benefits**
1. Closes the judgment layer's **time gap** — `standby`/`wake` facts let `rally next`
   recommendations survive idle windows (the originating problem in `bookmark.context.md:49`).
2. **Durable trail over ephemeral fan-outs** — pi-dynamic runs vanish (no persistence); Rally
   gives a permanent, queryable, cross-session record.
3. **Laggard detection** — derived DAG view over causation edges shows which legs landed/stalled.
4. **Trust governance extends to cadence** — a fact-triggers-wake path inherits the six-state gate.
5. **One seam, many clients** — pi-dynamic, LangGraph, Build Loop, host Workflow tool all conform.
6. **Charter-pure** — recording + derivation only, stays inside the kernel boundary.

**Drawbacks**
1. **Scope-creep gravity (highest risk)** — once Rally records cadence intent, social pressure to
   also *fire* the runner; one feature-request from breaching the record-not-execute seam.
2. **Latent value / adoption risk** — payoff exists only if a real orchestrator emits events;
   Rally can build the seam and have zero clients.
3. **Schema + maintenance surface** — new event families + trust-gating code paths in a thin kernel.
4. **Partial-capability mismatch** — Rally adds observability + coordination but **not run-resume**
   (non-authoritative derived state). Don't let "durable log" imply "resumable workflow."
5. **Authority confusion** — two state sources (orchestrator run-state vs Rally derived view);
   the "Rally is non-authoritative" invariant is subtle.
6. **Time-model reconciliation** — pi-dynamic is deterministic/timeless by design; Rally events are
   time-ordered facts. Mapping one onto the other needs care.

---

## 6. Decision gate + recommended next action

Two questions decide whether to build now:

1. **Is there a real client today?** Is any orchestrator (pi-dynamic, or your own Build Loop /
   host Workflow runs) going to emit events on a Rally repo soon? If **no**, building the seam is
   YAGNI — value stays latent.
2. **Can the record-vs-execute line be held socially, not just technically?** Mitigate by writing
   the boundary into the design doc as a hard rule *before* any code.

**Recommended paths (pick by the gate):**
- **No near-term client →** docs-only note positioning pi-dynamic-workflows against Rally's
  Non-Goals (reinforces the boundary with a concrete artifact). Lowest risk.
- **Named first client →** spec a **generic orchestrator→Rally event-emission adapter contract**
  (not pi-specific): events `handoff`/`artifact`/`decision`/`standby`/`wake`, a derived fan-out
  DAG view, trust-gated triggers, **execution explicitly excluded**. pi-dynamic = reference client.

Per user workflow rules, any code/spec work here **routes through build-loop** (`/build-loop:run`),
not direct edits.

---

## 7. Where this assessment lives
- This handoff: `docs/pi-dynamic-assessment-handoff.md` (this file).
- Source grounding: the citations in §3 (all in this repo's `docs/` + `README.md` + `.bookmark/`).
- pi-dynamic source of truth: the GitHub repo in §2 (not cloned locally).
