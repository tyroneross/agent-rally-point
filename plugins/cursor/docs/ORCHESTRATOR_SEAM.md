<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Orchestrator Seam — Event Vocabulary and Lineage Contract

> Current event-vocabulary and runner-boundary contract.

---

## 0. The Hard Rule (non-negotiable)

**Rally RECORDS and DERIVES; it NEVER EXECUTES.**

Every event below adds *facts* or *derived views*. The actual fan-out, model wake, step execution, and scheduling always happen in an **external runner** (LaunchAgent / cron / Build Loop / `rally watch`).

Litmus for every change: *"Does this make Rally start, resume, retry, or schedule work?"*
If **yes** → it is out of scope; move it to the runner.

`rally watch` is the reference runner. It polls `rally wake-due --json` and invokes the suggested commands. Rally emits the suggestion; the runner fires the work.

---

## 1. Event Vocabulary (5 families)

These are the canonical event kinds that form the orchestrator seam. Each maps to a `FactKind` in the Rally ledger.

| Family   | `kind`     | Who emits        | Semantics |
|----------|------------|------------------|-----------|
| Work     | `handoff`  | Any agent        | Delegates a unit of work to a peer or `--target all`. |
| Work     | `claim`    | Any agent        | Records intent to take on a scope. |
| Output   | `artifact` | Any agent        | Declares a completed, verifiable output. |
| Signal   | `decision` | Any agent        | Records an architectural or coordination choice. |
| Lifecycle| `standby`  | Any agent        | Agent going dormant; requests a future wake signal. |
| Lifecycle| `wake`     | Any agent        | Acknowledges a standby; records the agent re-activating. |

**Key invariant:** `artifact` closes a `claim` (projection drops the claim from `active_claims`). `wake` + `artifact` both close a `standby` from `wake-due` (the standby disappears from the projection once referenced by either kind).

---

## 2. Lineage Fields (run / step / parent-step)

These are **additive scope markers** stored in `Fact.scope[]`. They require no schema change to the `Fact` struct.

| Marker              | Value format | Purpose |
|---------------------|--------------|---------|
| `run:<id>`          | Any string   | Groups all facts belonging to one fan-out batch (run identifier). |
| `step:<id>`         | Any string   | Identifies this specific fact's step within the run. |
| `parent-step:<id>`  | Any string   | Causation link: names the step that triggered this step. |

### CLI usage

All three markers are optional flags on `rally say <kind>`:

```
rally say claim --tool <tool> --subject "..." \
  --run RUN-42 --step S1 --parent-step S0 \
  --json
```

### DAG reconstruction

`rally dag --run <id>` reads all facts carrying `run:<id>`, groups them by `step:`, and derives:

- **Nodes** — one per unique `step:<id>`, tagged `landed | in_flight | stalled`.
- **Edges** — from `parent-step:<parent>` markers (kind=`parent_step`) and `ref_id` links within the run (kind=`ref`).

Node status:
- `landed` — the step has an `artifact` fact.
- `stalled` — the step has a `standby` fact whose `wake_after` timestamp has passed with no subsequent `wake` or `artifact`.
- `in_flight` — otherwise.

---

## 3. Standby / Wake Encoding

### Standby

`rally say standby --tool <t> --reason <r> --wake-after <+30m|iso>`

Stored as:
- `kind: standby`
- `summary: "reason:<r> wake_after:<iso-8601>"` — both fields as space-separated tokens.
- `status: "pending"`
- `scope[]`: optional lineage markers (`run:`, `step:`, `parent-step:`).

The `--wake-after` argument accepts:
- Relative offsets: `+30m` (30 minutes), `+2h` (2 hours), `+1d` (1 day).
- Absolute ISO-8601 strings: `2026-05-30T14:00:00Z`.

### Wake

`rally say wake --tool <t> --ref-standby <standby-event-id>`

Stored as:
- `kind: wake`
- `ref_id`: set to the standby fact's `event_id`.
- `scope[]`: optional lineage markers.

---

## 4. Wake-Due Projection

`rally wake-due [--tool <tool>] [--json]`

Returns standby facts whose `wake_after` timestamp has passed **and** whose owning tool is a known room participant (trust gate). For each:

```json
{
  "standby_event_id": "fact_...",
  "owner": "claude_code:01",
  "reason": "idle",
  "wake_after": "2026-05-30T13:00:00Z",
  "suggested_command": "rally next --tool claude_code:01 --json"
}
```

**`suggested_command` is a plain string.** Rally never executes it. The runner (e.g., `rally watch --on-activity`) invokes it when it decides to.

Trust gate: standbys authored by tools not present in the room's squads are silently excluded. An untrusted agent cannot conscript a peer's heartbeat.

---

## 5. Reference Client (pi-dynamic)

pi-dynamic step boundaries map to `rally say` calls:

| pi-dynamic event      | Rally event                                                  |
|-----------------------|--------------------------------------------------------------|
| Step start            | `rally say claim --run <run-id> --step <step-id>`           |
| Blocked/waiting       | `rally say standby --wake-after <+Nm>`                       |
| Step complete         | `rally say artifact --run <run-id> --step <step-id> --ref <claim-event-id>` |
| Fan-out to peer       | `rally say handoff --target <peer> --run <run-id> --step <step-id>` |
| Fan-out child start   | `rally say claim --run <run-id> --step <child-step> --parent-step <parent-step>` |

The `run_id` is shared across all steps in one orchestrated batch. pi-dynamic generates a UUID at batch start and stamps every emitted fact with it.

---

## 6. Runner Split

```
Rally (records/derives)          External runner (fires/executes)
─────────────────────────        ────────────────────────────────
rally say standby …         →    rally watch polls wake-due
rally wake-due --json       →    runner reads suggested_command
                                 runner invokes: rally next --tool X --json
                                 agent acts on next result
                                 agent calls: rally say artifact / standby
```

`rally watch` is the reference runner. Its `--on-activity` hook receives `RALLY_ROOM`, `RALLY_FROM_SEQ`, `RALLY_TO_SEQ` as env vars and can invoke `rally next --tool <tool> --json` to surface actionable work.

---

## 7. Charter Reference

This spec implements `RUST_GREENFIELD_ARCHITECTURE.md §85–94, 105, 504`:

> Rally is a **ledger + projection engine**. It never starts, retries, or schedules work. External agents publish into Rally; Rally signals back through read-only projections. The runner fires on those signals.
