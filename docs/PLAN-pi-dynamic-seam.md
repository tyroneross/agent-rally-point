<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Build-loop plan B — pi-dynamic dynamic-workflow **observation seam** + idle heartbeat

> **Source decision:** `docs/pi-dynamic-assessment-handoff.md` §5–6. Settled: **do not** import
> pi-dynamic's execution into Rally; build the **inverse seam** — orchestrators publish *into* Rally.
> **User scope (2026-05-28):** *"Plan now — full end-to-end execution with gates and dogfooding when
> launched. Claude and Codex should also check every 30 min if idle and ask what's next with a temp heartbeat."*
> **Target line:** `main` (LEAN). It already has `rally_wake.py` doorbell + a standby/wake backend
> contract + room discovery (merged via integration) — this plan completes that arc.
> **Run via:** `/build-loop:run`. **Hosts:** Claude + Codex via the rally channel (dogfood). ≤4 agents/host/phase.
> **Branch hygiene:** chunk branches collapse onto `main` at close; no leftover branches/worktrees.

---

## 0. The hard rule (non-negotiable, encoded before any code)

**Rally RECORDS and DERIVES; it does NOT EXECUTE.** (charter `RUST_GREENFIELD_ARCHITECTURE.md:85-94, 105, 504`.)
Every chunk below adds *facts*, *derived views*, or *trust-gated eligibility signals*. The actual fan-out,
the actual model wake, the actual step execution always happen in an **external runner** (LaunchAgent / cron /
Codex heartbeat / Build Loop / host Workflow tool). If a chunk would make Rally *fire* anything, it is
out of scope — that is the Non-Goal verbatim and the #1 scope-creep risk in the assessment.

> Litmus for every PR in this plan: *"Does this make Rally start, resume, retry, or schedule work?"*
> If yes → reject; move it to the runner.

---

## 1. Goal / Deliverables / Decision gate

**Goal.** A **generic orchestrator→Rally event-emission seam** (pi-dynamic = reference client) so any
orchestrator's fan-out is durably recorded, rendered as a DAG, and can *signal* (not fire) trust-gated wakeups —
plus a **temp 30-min idle heartbeat** that makes Claude + Codex self-poll `rally next` ("what's next?") while idle.

**Deliverables.**
1. **standby / wake event families** — first-class coordination facts (roadmapped at `bookmark.context.md:49`,
   partially present via the integration merge's wake-intent facts). `standby{reason, wake_after, owner}`,
   `wake{ref_standby, by}`.
2. **Generic adapter contract** — `docs/ORCHESTRATOR_SEAM.md`: the event vocabulary
   (`handoff`/`artifact`/`decision`/`standby`/`wake`) + a `run_id`/`step_id`/`parent_step` lineage an
   orchestrator stamps so Rally can reconstruct the fan-out. Not pi-specific.
3. **Derived fan-out DAG view** — `rally dag --run <id> --json`: nodes = steps, edges = causation
   (`parent_step` / `ref_*`), each node tagged `landed | in-flight | stalled` (laggard detection).
4. **Trust-gated wake eligibility** — `rally next` / a `rally wake-due` projection surfaces standby facts whose
   `wake_after` has passed **and** whose trigger event meets the six-state trust minimum. Emits a
   *suggested wake command for the runner*; never runs it.
5. **pi-dynamic reference emitter** — a thin TS/Node shim (in `examples/`, or a `--from pi-dynamic` note) that
   maps pi-dynamic step boundaries → `rally say` events. Proves the seam with one real client.
6. **Temp idle heartbeat** — Claude + Codex each get a 30-min idle self-poll (details §3).

**Decision gate (handoff §6) — answered:** "real client today?" → **yes** (Build Loop + host Workflow + the
heartbeat itself are the first clients; pi-dynamic is the reference). So we build, with B0 as the social-line gate.

---

## 2. MECE work split

| Owner | Chunk | Owned files | `--produces` |
|---|---|---|---|
| **build-loop (B0)** | boundary rule + seam contract spec | `docs/ORCHESTRATOR_SEAM.md`, `RUST_GREENFIELD_ARCHITECTURE.md` (boundary note) | `spec.seam-contract` |
| **Codex** | B1 standby/wake facts | `crates/rally-cli/src/store.rs` (event kinds + payloads), writer in `cli.rs`/`lib.rs` | `facts.standby-wake` |
| **Codex** | B4 trust-gated wake eligibility | `crates/rally-cli/src/next.rs` (wake-due projection), trust check | `feature.wake-due` |
| **Claude** | B2 fan-out DAG view | `crates/rally-cli/src/{dag.rs(new),cli.rs(subcommand)}` | `feature.dag-view` |
| **Claude** | B3 pi-dynamic reference emitter | `examples/pi-dynamic-emitter/*`, `docs/ORCHESTRATOR_SEAM.md` (client section) | `client.pi-dynamic-ref` |
| **Claude** | B5 schema + tests | `docs/schemas/*.json`, `crates/rally-cli/tests/*` | `schema.seam`, `tests.seam` |
| **either** | B6 heartbeat runner | `scripts/rally_heartbeat.*`, host config | `runner.idle-heartbeat` |

**Shared edge:** B1's `standby`/`wake` kinds are read by B4 (wake-due) and B2 (DAG nodes). Codex lands
`facts.standby-wake` first → handoff to Claude (B2) and to its own B4. Dogfood via receipts.

---

## 3. The 30-min idle heartbeat (charter-pure)

Rally records *facts*; the **runner** does the 30-min check. Mechanism:

1. When an agent finishes a loop with no actionable `next`, it writes `rally say standby --reason idle
   --wake-after +30m --tool <self>`. (Fact, not execution.)
2. A **temp heartbeat runner** (the "temp heartbeat" you asked for — a stopgap, not permanent infra) wakes every
   ~30 min and runs `rally next --tool <self> --json`:
   - **Claude Code:** a session `/loop 30m "rally next; if actionable, act; else restate standby"` **or** a
     `ScheduleWakeup`-driven self-poll. Cache-aware: 30 min > 5-min cache TTL, so this is a deliberate cold poll.
   - **Codex:** a Codex heartbeat entry / cron invoking `rally next --tool codex --json`.
3. If `rally next` returns actionable work (a handoff, a `wake-due`, a fresh claim), the agent asks/acts
   ("what's next"); else it re-arms standby. Rally never starts the model — the runner does.
4. **Trust gate applies:** a wake triggered by a peer's fact must pass the six-state minimum before the runner
   surfaces it (B4), so an untrusted agent can't conscript a peer's heartbeat.

"Temp" = ship the heartbeat as a removable script + host-config snippet (open a `[CLEANUP] temp idle heartbeat`
task naming the disable command); promote to durable infra only after the dogfood proves cadence.

---

## 4. Chunks (change → verify → commit)

- **B0** boundary rule + `ORCHESTRATOR_SEAM.md` (event vocab, lineage fields, the litmus). F: doc states the
  hard rule + the 5-event contract + the "Rally signals, runner fires" split. **Gate: B1–B6 reference B0.**
- **B1** `standby`/`wake` kinds + payloads + `say` writers + validation. F: round-trips through `changes.jsonl`;
  schema validates; `room` buckets them.
- **B2** `rally dag --run <id>`: build the causation DAG, tag `landed|in-flight|stalled`. F: a fan-out of 1
  handoff → 3 child claims renders 3 nodes; a child with no artifact after its `wake_after` tags `stalled`.
- **B3** pi-dynamic emitter example: at step boundaries emit `rally say {handoff|artifact|decision}`. F: running
  the example against a scratch repo produces a DAG that `rally dag` reconstructs end-to-end.
- **B4** `wake-due` projection in `next.rs`: standby past `wake_after` + trust-min → emit *suggested* wake command.
  F: trust-OK standby past due surfaces; untrusted or not-yet-due does not; **no execution occurs** (asserted).
- **B5** schemas (`standby`/`wake`/`dag`/`wake-due`) + golden contracts + tests. F: golden suite green.
- **B6** temp heartbeat runner for both hosts. F: a simulated idle loop writes `standby`, the runner's 30-min
  poll (fast-forwarded in test) runs `rally next` and surfaces the re-arm; `[CLEANUP]` task opened.

## 5. Exit gates
```
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all
# charter assertion test: grep the diff — no new code path calls a runner/exec/spawn/schedule API
cargo run -p rally-cli -- dag --run <fixture> --json     # reconstructs the dogfood fan-out
```
Plus build-loop: plan-verify + plan-critic; **security-reviewer** (new trust-gated trigger = risk-surface change,
OWASP-Agentic ASI — a fact must not be able to conscript a peer's compute without trust); independent-auditor on
final diff; code-simplifier pre-commit. **At close: merge onto `main`, delete temp branches.**

## 6. Dogfood when launched
This very build runs through the seam: Claude + Codex emit `handoff`/`artifact`/`decision` per chunk, the DAG
view (B2) tracks who's landed vs stalled, and the temp heartbeat (B6) wakes each host's `rally next` every 30 min
so neither idles silently waiting on the peer. The seam's first real client is its own construction.

## 7. Sequencing vs Plan A
Plan A (contract claims + receipts) should land **first** — B2's DAG and B4's wake-due both lean on the
`produces`/`depends` lineage and the receipt lifecycle Plan A adds. Run A → then B on the same `main` line.
