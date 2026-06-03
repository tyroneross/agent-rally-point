<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Full context: "take best from PR46" + "add pi-dynamic dynamic workflow"

> **For:** a fresh Claude Code **or** Codex session picking up two pieces of work on
> agent-rally-point — (A) porting PR46's best features onto the canonical line, and
> (B) adding a dynamic-workflow observation seam per `pi-dynamic-assessment-handoff.md`.
> **Status:** architecture scanned, context settled, **one decision pending** (§6) before
> the two build-loop plans are executable.
> **Date:** 2026-05-28. **Verification:** ✅ live git (branches, worktrees, commit dates,
> merge-base) + ✅ read of PR46 diff and the rally-core kernel + ✅ charter docs quoted verbatim.

---

## 1. Headline (read this first)

agent-rally-point has **two structurally divergent architecture lines**, and `main` has
**deliberately deleted the one your working copy is checked out on.**

| | **LEAN line ("rally2", now `rally`)** | **ATTUNED line (legacy multi-crate)** |
|---|---|---|
| Where | `main` (HEAD 62c8dbf, 05-28), `integration` (fc08d85, 05-28 — freshest), PR46's base | `codex/rally-global-discovery` ← **your current checkout** (3d9e567, **05-26**), `codex/rally2-primary-path`, `codex/rally2-standby-wake-contract` |
| Crates | **1**: `rally-cli` only (`check.rs` / `next.rs` / `store.rs` / `discovery.rs` / `backends.rs`) | **4–5**: `rally-core` + `rally-trust` + `rally-protocol` + `rally-cli` (+`rally2-cli`) |
| Event model | one flat `Fact` struct, `RoomSnapshot` of `Vec<Fact>` buckets | typed `EventKind` + per-kind `EventPayload` enum, `TraceProjection` over `Vec<Value>` |
| Features | predictive contracts + receipts (PR46), wake-intent facts, room discovery, managed sessions | roles/specialization, trust six-state policy, judgment layer (`judge`/`hook`), adapters (cmux/herdr — historical; herdr removed in Plan F), ranked `next`, ContextBrief |
| Status | **won** — `main` is now the product | **abandoned by main** — `0d5024b refactor: remove legacy rally implementation` |

**Timeline (verified):** the two lines share merge-base `96b057f` (05-26 22:23). Your attuned
branch added 1 commit and stopped (05-26 23:40). `main` then moved 27 commits: made rally2
primary (05-27), **removed the legacy multi-crate implementation** (05-28 07:25), renamed
rally2→rally (05-28 07:30), merged PR#44. **Your checkout is a stale branch on the deleted line.**

**Consequence for the request.** "Take best from PR46 onto *current version in my local*" reads two ways:
- If *current version* = the actual product (`main`/lean), PR46 already targets it → "take best" is mostly **merge PR46 + carry forward any attuned-line gems the lean line lacks**.
- If *current version* = the branch literally checked out (`codex/rally-global-discovery`/attuned), then PR46's features must be **re-implemented** against the typed kernel (real work; gaps in §4).

These are ~10× apart in effort. §6 is the decision that picks one. Everything below is true regardless.

---

## 2. Branch & worktree map (live)

| Worktree path | Branch | Line | HEAD / last activity | Adds |
|---|---|---|---|---|
| `agent-rally-point` (primary) | `codex/rally-global-discovery` | ATTUNED | 3d9e567 · 05-26 | global discovery, agent-visible obligations, ranked next, judgment hooks + CI gate, doctor/setup |
| `agent-rally-point-main` | `main` | LEAN | 62c8dbf · 05-28 | renamed-rally2 product; `persist --summary`, managed sessions |
| `agent-rally-point-integration` | `integration` | LEAN | fc08d85 · **05-28 (freshest)** | wake-intent facts + room discovery; backend-agnostic doorbell `rally_wake.py` |
| `agent-rally-point-rally2-primary` | `codex/rally2-primary-path` | ATTUNED (5-crate) | 4cd4d30 · 05-27 | rally2 act-on-next contract, next scheduler, clean-room coordinator |
| `/private/tmp/agent-rally-assess.91OiHe` | `codex/rally2-standby-wake-contract` | ATTUNED (5-crate) | 8c439df | standby/wake contract |
| `/private/tmp/agent-rally-pr46.DdcXt7` | detached 6e30af1 | LEAN | — | PR46 code (read-only source for the port) |
| `/private/tmp/...pr40-ci*` | pr40 ci tests | — | — | scratch, ignorable |

Dirty in primary checkout: `Cargo.lock`, `RALLY.md` (uncommitted, active codex session — do not clobber).

PR46: **OPEN, unmerged**, base `main`, head `codex/rally2-act-on-next-contract`. Refs `origin/pr/46`
and `origin/pr/46-merge` both exist remotely.

---

## 3. PR46 — exactly what "the best" is

Two features, both **pure projections over the fact ledger** (no new subsystem, no migration). Full
review in the prior assessment; mechanics distilled:

### 3a. Predictive contract claims (stale-base detection)
- A claim declares `--produces` / `--depends` contract tokens, optionally pinned `name@hash`.
- **Algorithm** (`next.rs:140-188`): for each consumer claim's `depends` token, match its bare name
  against every **cross-agent** producer claim's `produces` token (a tool changing its own base is not
  a collision; `event_id` self-match skipped). Any name match → a finding.
- **`confirmed`** iff *both* sides pinned *and* hashes differ (`Some(d),Some(p) if d!=p`). Else informational.
- **Soft gate** (`next.rs:341-345`): `coordination_required = any(confirmed)`; surfaced in `next.stale_bases[]`
  and in `enter` attention (`reason:"stale_base"`, every loop until resolved, even if producer predates the
  cursor). **Advisory — never forces `requires_human`.**
- **`suggested_command`**: `rally say blocker --tool <you> --ref <producer> --subject "stale base: <c>" --severity <high|medium> --json`.
- **Boundary validation** (`lib.rs:143-162`): token = `name` or `name@pin`; reject empty name or >1 `@`.

### 3b. Handoff receipts (self-reported lifecycle)
- `room` derives a receipt per handoff by walking its ref chain (`store.rs:503-561`).
- **States, precedence high→low:** `completed` (a chain artifact exists) > `blocked` (chain blocker not in
  `resolve_refs`) > `acknowledged` (handoff id ∈ `resolve_refs`) > `acted` (chain claim exists) > `delivered`.
- **`resolve_refs`** comes from `kind==resolve` only — a `release` (retire claim scope) does **not** acknowledge.
- `evidence` = latest (`max seq`) chain artifact's evidence. `self_reported` is **always true** (Rally renders,
  doesn't verify).
- **Known CodeRabbit Major:** the chain walk is **one-hop / direct-refs only** — a fact that refs a fact that
  refs the handoff is missed; multi-hop delegation silently renders `delivered`. Carry-or-fix decision in the plan.

### 3c. CI gate + nudge
- `check ci --strict` (`check.rs:189-231`): `confirmed-stale-base` + `active-blocker` = `stop`; `open-handoff` = `warn`.
- Exit code (`check.rs:70-72`): `exit 4` iff `strict && any stop`. CI workflow switched
  `check before-complete` → `check ci --tool ci --strict --json`.
- `before-write` always appends `declare-contracts` (severity `info`, never blocks) + same nudge in the
  managed-session prompt.
- **Doc bug to fix on port:** RALLY.md says receipt evidence is "the producer's own claim"; it's actually
  pulled from the artifact fact.

---

## 4. Cross-architecture porting gaps (only bite if the ATTUNED line is chosen)

PR46 assumes the **flat `Fact`**. The attuned kernel (`rally-core`) is shaped differently. To re-implement there:

1. **Model shape.** No flat `Fact`; typed `EventPayload` per kind. Decide: add `produces`/`depends` to
   `ClaimPayload` (`event.rs:205-256`) **or** build a `Fact`-like adapter view over `EventPayload`. Both
   stale-base and receipt code assume the flat shape.
2. **Refs.** PR46 uses a generic `ref_id`. Attuned uses typed `ref_handoff_id` (Ack), `ref_claim_id`
   (ClaimRelease), `ref_blocker_id` (BlockerResolved), `ref_task_id` (Artifact). The one-hop walk must be
   re-expressed over these.
3. **Acknowledge semantics.** PR46 derives `acknowledged`/blocked-cleared from `kind==resolve`. Attuned has a
   first-class **`Ack`/`AckPayload`** (handoff) + `BlockerResolved` (blocker) + `ClaimRelease` (claim). Re-map
   the precedence ladder onto those kinds.
4. **Projection home.** No `RoomSnapshot`. Attuned has `TraceProjection` with accessors returning typed
   structs (`PendingHandoff`/`ActiveClaim`/`ActiveBlocker`, `query.rs:14-127`). Add `StaleBase` + `Receipt`
   projections + accessors there. **`ClaimConflict` already exists and is conceptually adjacent to StaleBase.**
5. **scope vs resource.** PR46 path logic uses `Fact.scope: Vec<String>`. Attuned `ClaimPayload.resource` is a
   single `String`. Decide single-resource vs multi-scope.
6. **Command surface.** Attuned has **no `check` subcommand** — it has `judge` / `hook <phase>` /
   `execute_ci_gate` + `setup enforcement off|warn|strict`. PR46's `check before-write|before-complete|ci`
   (exit 4) must merge into those, or add a `check` alongside. Subcommand wiring: arm in `dispatch.rs::run`,
   flags in `args.rs`, `Command` + `parse_*` in `args/commands.rs`, handler in `*_commands.rs` → `WriteOutput`.
7. **Schema/validator chain** (per the schema-migration-full-chain rule): `event.rs` `schema_name()`/`event_type()`
   is the single kind→schema source. New fields + derived schemas (next `stale_bases`/`coordination_required`,
   room `receipts`) must thread through there **and** `docs/schemas/*.json` **and** the golden-contract test.
8. **Trust interaction.** Attuned query structs carry `origin` + `trust_status` (rally-trust). Decide whether
   receipt evidence/state should expose `trust_status` — PR46's `self_reported=true` ignores per-event trust.

If the **LEAN line** is chosen instead, items 1–8 mostly evaporate: PR46 already fits, so the work is
*merge/rebase PR46 + fix its known nits (one-hop receipts, evidence doc)* and optionally backport the
attuned gems (roles / judgment / trust) — but those backports then inherit gaps 1–8 in reverse.

---

## 5. pi-dynamic dynamic-workflow — settled conclusion + the charter boundary

Source: `docs/pi-dynamic-assessment-handoff.md` (prior session, no code). pi-dynamic-workflows
(github.com/Michaelliv/pi-dynamic-workflows, MIT, v1.0.0) is a **TS/Node dynamic-workflow orchestrator**
for the Pi assistant — a near-clone of Claude Code's Workflow tool (`agent()`/`parallel()`/`pipeline()`/
`phase()`, sandboxed VM, no persistence/resume/scheduling).

**The whole answer, settled:**
> Do **not** move pi-dynamic's *execution* **down** into Rally. Add Rally's *observation seam* **up**
> toward orchestrators. Rally **records and derives; it does not execute.**

This is forced by the charter (quoted verbatim, `RUST_GREENFIELD_ARCHITECTURE.md`):
- **Non-Goals (85-94):** "Rally should not become: a long-running orchestrator that owns agent lifecycles …
  a general workflow engine with retries, scheduling, and task queues …"
- **Refuses (105):** "Agent runtime loops, model/tool orchestration, editor transport, **hosted workflow
  execution**, broker semantics, global federation service."
- **Integration model (504):** "Orchestrated workflows can **publish Rally events at handoff, claim, blocker,
  and verdict boundaries. Rally stays below orchestration.**" (same for OpenAI Agents SDK :503, Temporal :505)
- **standby/wake (`bookmark.context.md:10,49`):** standby/wake are **facts, not execution**; "the LLM agent
  itself is not awake"; "real wakeups delegated to a runner" (LaunchAgent/cron/Build Loop/Codex heartbeat).
- Note: `README.md:31-33` is the *network-transport-out-of-scope* line, **not** "below orchestration" — that
  positioning is only at architecture-doc :504. Cite correctly.

**What to build (the inverse seam):** a **generic orchestrator→Rally event-emission adapter contract** (not
pi-specific) — events `handoff`/`artifact`/`decision`/`standby`/`wake`, a **derived fan-out DAG view** over
causation edges (laggard detection), **trust-gated triggers**, **execution explicitly excluded**. pi-dynamic
= reference client.

**Benefits:** closes the judgment-layer time gap (standby/wake survive idle windows); durable trail over
ephemeral fan-outs; laggard detection; trust governance extends to cadence; one seam many clients; charter-pure.
**Drawbacks:** scope-creep gravity (#1 risk — once cadence is recorded, pressure to *fire* it); latent value
if no client emits; schema/maintenance surface; "durable log" ≠ "resumable workflow"; two-state-source
authority confusion; deterministic-timeless vs time-ordered reconciliation.

**Decision gate (from the handoff §6):** (1) Is there a real client *today* that will emit events on a Rally
repo? If no → docs-only boundary note (lowest risk, avoids YAGNI). (2) Can the record-vs-execute line be held
*socially*, not just technically? → write the boundary as a hard rule in the design doc **before** any code.

---

## 6. THE DECISION (gates both plans)

**Which line is canonical for this work — and what does "current version in my local" mean?**

- **Option L — Lean/`main` is canonical.** Treat `codex/rally-global-discovery` as stale. Take-best = land
  PR46 on `main`/`integration` + (optionally) backport attuned gems. pi-dynamic seam built on the lean kernel.
  *Lowest effort, matches where the product actually moved.* Cost: attuned features (roles/trust/judgment/
  adapters) are not in the lean line — anything you want from them is a separate backport.
- **Option A — Attuned/current-checkout is canonical.** You intend to revive the multi-crate line. Take-best =
  full re-port of PR46 (gaps §4). pi-dynamic seam built on the typed kernel. *Highest effort.* Cost: you'd be
  building on a line `main` deleted; needs a story for reconciling with `main`.
- **Option U — Unify.** Decide a single target (likely lean `main`), then deliberately carry forward the best
  of *both* (PR46 features **and** the attuned roles/trust/judgment) into it. *Most product value, most work,
  needs sequencing.*

Until this is answered, the two build-loop plans below are drafted **against Option L** as the default
(it matches the product's actual direction and is lowest-risk), and flagged where Option A/U would diverge.

---

## 7. The two build-loop plans (multi-host: Claude + Codex)

- **Plan A — "take best from PR46":** `docs/PLAN-take-best-pr46.md` *(written after §6 is confirmed)*
- **Plan B — "pi-dynamic observation seam":** `docs/PLAN-pi-dynamic-seam.md` *(written after §6 is confirmed)*

Both will: route through build-loop (`/build-loop:run`), split MECE work across Claude + Codex with the rally
channel as the coordination substrate (dogfooding — declare `--produces`/`--depends`, use receipts to track
handoffs), pin owned-files per agent, and run `cargo fmt --check` + `clippy -D warnings` + `cargo test` +
golden-contract schema validation as exit gates.
