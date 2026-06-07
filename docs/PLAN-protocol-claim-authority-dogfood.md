<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> | SPDX-License-Identifier: Apache-2.0 -->

# Build-loop Plan - Protocol Claim Authority And Dogfood

## Goal

Implement the next Rally protocol layer in a way that can be compiled, tested,
and dogfooded with at least one Claude Code terminal and one Codex terminal.

The target design is defined in
[`docs/PROTOCOL-NORTH-STAR.md`](PROTOCOL-NORTH-STAR.md):

- Session identity is distinct from tool identity.
- Liveness and transport receipts are brainstem-owned, not LLM-authored.
- Active claims are acquired through a transactional claim authority.
- Work and claims are separate objects.
- Resource scopes are structured and canonical.
- Claim leases expire dead ownership without durable renewal spam.
- Durable events carry causality, idempotency, and authorization context.

The dogfood harness should use Rally Flow, not ad hoc subagent spawning:

- [`skills/rally-workflows/SKILL.md`](../skills/rally-workflows/SKILL.md) for
  descriptor-driven fan-out, lineage, DAG, standby, and resume.
- [`skills/mini-loop/SKILL.md`](../skills/mini-loop/SKILL.md) inside each worker
  task as the lightweight assess -> plan -> execute -> mini-judge loop.
- [`dynamic-workflows/PROTOCOL.md`](../dynamic-workflows/PROTOCOL.md) for the
  workstream descriptor contract.

## Non-Negotiables

Rally remains a facilitator, not an executor.

- Rally may transactionally acquire, reject, expire, and release claims.
- Rally may warn, record conflicts, and project next actions.
- Rally must not edit files, run agent work, auto-merge, auto-push, or schedule
  hidden execution.
- `queue` is a recorded claim state, not a scheduler.
- Heartbeats, read receipts, lease renewals, and transport delivery are mutable
  registry/index state by default, not durable ledger spam.

## Build-loop Shape

Use build-loop phases, but keep each phase independently shippable:

```text
Phase 0: baseline + compatibility inventory
Phase 1: session identity registry
Phase 2: structured scopes + claim authority
Phase 3: claim leases + expiration
Phase 4: event envelope + causal/auth fields
Phase 5: handoff lifecycle + targeted ACK dogfood
Phase 6: operation/work result semantics
Phase 7: Rally Flow multi-worker dogfood and closeout
```

Each phase must compile, pass focused tests, post a Rally artifact, and include
a next-step decision if it fails.

## Owner Lanes

| Lane | Recommended Owner | Primary Files | Reason |
|---|---|---|---|
| Session identity | Claude Code | `crates/rally-cli/src/*session*`, managed backends | Claude can dogfood managed terminal identity directly. |
| Structured scopes | Codex | `crates/rally-cli/src/*claim*`, store/projection modules | High-value Rust modeling and deterministic tests. |
| Transactional claim authority | Codex | store/index/write path | Requires concurrency tests and careful state transitions. |
| Lease expiry | Codex + Claude review | claim authority + brainstem/registry seam | Crosses storage and liveness semantics. |
| Event envelope | Claude Code | fact schema, JSON output contracts, compatibility docs | Needs careful migration language and host-facing docs. |
| Handoff ACK dogfood | Claude + Codex together | managed session commands, docs, smoke scripts | Must prove "delivered" is not "acked." |
| Rally Flow coordinator | Lead agent | workstream descriptor, `rally dag`, checkpoint synthesis | Keeps spawned workers lightweight and observable. |
| Mini-loop workers | Spawned Claude/Codex workers | assigned task scope only | Each worker runs a lightweight per-task loop rather than full build-loop. |
| Final audit | Independent peer or fresh agent | docs, tests, dogfood transcript | Checks for scheduler creep and identity ambiguity. |

The split can change, but file ownership should be claimed explicitly before
editing. Do not let both agents edit the same Rust module without a Rally claim.

## Rally Flow Dogfood Workstream

Use a workstream descriptor for the implementation itself. The descriptor should
be linted before fan-out and every task should stamp one shared `run_id`.

Draft descriptor shape:

```json
{
  "workstream": "rally-protocol-claim-authority",
  "description": "Implement Rally protocol session identity, transactional claims, leases, causal event envelope, and Claude/Codex handoff dogfood. Rally facilitates; hosts execute.",
  "tasks": [
    {
      "id": "session-identity",
      "intent": "Add endpoint/session identity registry and from_session_id write context.",
      "owns": ["crates/rally-cli/src/session_identity.rs"],
      "validation": "cargo test --all",
      "output": "session registry distinguishes two Claude sessions and one Codex session"
    },
    {
      "id": "structured-scopes",
      "intent": "Add structured resource scopes, access modes, canonicalization, and conflict rules.",
      "owns": ["crates/rally-cli/src/resource_scope.rs"],
      "validation": "cargo test --all",
      "output": "canonical scope tests and conflict policy tests pass"
    },
    {
      "id": "claim-leases",
      "intent": "Add claim leases, mutable renewal, durable expiry, and rebuild from ledger.",
      "owns": ["crates/rally-cli/src/claim_authority.rs"],
      "depends_on": ["structured-scopes", "session-identity"],
      "validation": "cargo test --all",
      "output": "lease renewal is non-durable; expiry emits exactly one durable event"
    },
    {
      "id": "event-envelope-auth",
      "intent": "Add causal/idempotency fields and advisory authorization checks.",
      "owns": ["crates/rally-cli/src/event_envelope.rs", "docs/schemas/rally-protocol-events.md"],
      "validation": "cargo test --all",
      "output": "old logs replay and privileged events enforce role policy"
    },
    {
      "id": "integration-wiring",
      "intent": "Wire new protocol modules into CLI/store/output without changing their internal contracts.",
      "owns": [
        "crates/rally-cli/src/cli.rs",
        "crates/rally-cli/src/lib.rs",
        "crates/rally-cli/src/store.rs",
        "crates/rally-cli/src/output.rs"
      ],
      "depends_on": [
        "session-identity",
        "structured-scopes",
        "claim-leases",
        "event-envelope-auth"
      ],
      "validation": "cargo test --all",
      "output": "existing commands still replay old ledgers and new protocol commands work"
    },
    {
      "id": "handoff-dogfood",
      "intent": "Prove targeted delivery, ACK, accept/reject, and resolve with Claude and Codex sessions.",
      "owns": [
        "docs/PLAN-protocol-claim-authority-dogfood.md",
        "docs/PROTOCOL-NORTH-STAR.md",
        "scripts/protocol-dogfood-smoke.sh"
      ],
      "depends_on": ["integration-wiring"],
      "validation": "git diff --check",
      "output": "dogfood event ids show delivered is not acked and ack uses exact from_session_id/ref_event_id"
    },
    {
      "id": "integration-audit",
      "intent": "Audit scheduler boundary, state projections, docs, and validation evidence.",
      "owns": "read-only",
      "depends_on": [
        "session-identity",
        "structured-scopes",
        "claim-leases",
        "event-envelope-auth",
        "integration-wiring",
        "handoff-dogfood"
      ],
      "validation": "cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test --all && git diff --check",
      "output": "closeout report with pass/fail evidence and follow-ups"
    }
  ]
}
```

Before dispatch:

```bash
node dynamic-workflows/core/workstream-lint.mjs <descriptor>.json
```

During dispatch:

- Use Tier 1 host-native spawned workers for small read-only or file-disjoint
  tasks, capped at 4 parallel.
- Use Tier 2 `rally run` plus `rally inject` for the Claude/Codex terminal ACK
  dogfood because that test specifically needs independent terminal sessions.
- Each spawned worker runs `mini-loop`: assess task packet, plan 1-3 steps,
  execute inside `owns`, run validation, mini-judge, then post artifact or
  blocker.
- The coordinator aggregates with:
  ```bash
  rally dag --run <run_id> --json
  rally room --json
  ```

## Phase 0 - Baseline And Compatibility Inventory

Purpose: know the current command and data shape before changing semantics.

Actions:

1. Run:
   ```bash
   cargo fmt --all -- --check
   cargo clippy --all-targets -- -D warnings
   cargo test --all
   git diff --check
   ```
2. Capture current outputs for:
   ```bash
   rally whoami --tool codex --json
   rally sessions --json
   rally room --json
   rally say claim --tool codex --subject "phase 0 smoke" --path docs/PROTOCOL-NORTH-STAR.md --json
   ```
3. Record existing event fields and backwards-compatibility constraints.

Checkpoint:

| Outcome | Detect | Next Step |
|---|---|---|
| Pass | Baseline tests green and JSON shapes captured | Start Phase 1. |
| Fail: compile/test | Failing command and first root error captured | Fix only if required for baseline; otherwise document as pre-existing. |
| Fail: Rally command | Error from current CLI | Record compatibility gap before designing new shape. |

## Phase 1 - Session Identity Registry

Purpose: distinguish two Claude sessions, Codex sessions, managed panes, and
cloud workers beyond `tool_type`.

Deliverables:

- `endpoint_id` derivation for terminal, tmux, managed session, local process.
- Fresh `session_id` lease on every live runtime.
- `legible_name`, `tool_type`, `actor_id`, `principal_id`, `pid`, `host`, `cwd`,
  `branch`, `last_seen`, `expires_at`.
- `from_session_id` on new durable writes.
- Compatibility fallback for older events that only have `tool`.

Tests:

- Two managed Claude sessions get different `session_id`s and legible names.
- Same tmux pane restart gets a new `session_id` but stable endpoint lineage.
- `rally whoami --json` reports ambiguity when the runtime cannot be resolved.
- Durable write rejects missing `from_session_id` only after compatibility gate
  is explicitly enabled.

Checkpoint:

| Outcome | Detect | Next Step |
|---|---|---|
| Pass | Tests prove two Claudes are distinguishable | Move to structured scopes. |
| Fail: duplicate sessions | Same `session_id` appears for two endpoints | Fix endpoint derivation before proceeding. |
| Fail: old commands break | Existing `rally say` no longer works | Add compatibility shim and re-run baseline. |

## Phase 2 - Structured Scopes And Transactional Claim Authority

Purpose: prevent live collision before work begins.

Deliverables:

- Structured `ResourceScope` type with display-string parsing.
- Scope canonicalization for repo, file, directory, branch, port, process, task,
  service, and cross-repo scopes.
- Access modes: `exclusive`, `shared_read`, `advisory`, `namespace`.
- `claim.acquired`, `claim.released`, `claim.transferred`.
- Transactional active claim index with idempotent acquisition.
- Deterministic conflict policy:
  `reject`, `queue`, `allow_with_warning`, `request_handoff`.

Tests:

- Same file, two `exclusive` claims: exactly one acquires.
- Parent namespace and child file exclusive claims conflict.
- `shared_read` plus `exclusive` follows policy.
- `advisory` does not block but records warning.
- Concurrent writers cannot both acquire the same exclusive scope.
- Rebuild from ledger reconstructs the active claim index.

Checkpoint:

| Outcome | Detect | Next Step |
|---|---|---|
| Pass | Concurrent conflict test produces one owner | Start lease expiry. |
| Fail: double acquire | Two active exclusive claims exist | Stop. Fix transaction boundary before more features. |
| Fail: false conflict | Reviewer/shared-read blocks mutation unexpectedly | Adjust access-mode rules and add regression. |

## Phase 3 - Claim Leases And Expiration

Purpose: dead sessions cannot hold resources forever.

Deliverables:

- `lease_expires_at` on active claims.
- Lease renewal in mutable claim index only.
- `claim.expired` durable event emitted once when expiry is observed.
- Lead/operator release path for stale claims.

Tests:

- Lease renewal does not append a durable event.
- Expired claim emits one `claim.expired`.
- Expired claim leaves active ownership.
- A new session can acquire the resource after expiry.
- Expiry does not mark the linked work as failed or abandoned automatically.

Checkpoint:

| Outcome | Detect | Next Step |
|---|---|---|
| Pass | Expiry frees claim and records one durable event | Start event envelope. |
| Fail: ledger spam | Renewal increases durable event count | Move renewal to mutable index. |
| Fail: stranded claim | Expired claim remains active | Fix projection and active index reconciliation. |

## Phase 4 - Event Envelope, Causality, And Authorization

Purpose: make durable events replayable, debuggable, retry-safe, and policy-aware.

Deliverables:

- Standard envelope support for `causation_id`, `correlation_id`,
  `idempotency_key`, `work_id`, `run_id`, `attempt_id`, `claim_id`,
  `handoff_id`, `principal_id`, `actor_id`, and `auth_context`.
- Event-kind validation for required ids.
- Advisory authorization checks for privileged events:
  release other owner's claim, transfer claim, cancel work, supersede work,
  publish validation, operation intent/result.
- Compatibility reader for older events.

Tests:

- Duplicate `idempotency_key` does not create duplicate facts.
- ACK/resolve requires `ref_event_id` and `causation_id`.
- Observer role cannot release another session's claim.
- Lead role can transfer or expire a stale claim when policy allows.
- Old ledger segments still replay.

Checkpoint:

| Outcome | Detect | Next Step |
|---|---|---|
| Pass | New and old events replay together | Start handoff lifecycle dogfood. |
| Fail: replay break | Existing log fails to parse | Add tolerant reader before writing stricter events. |
| Fail: auth too strict | Normal agent cannot release its own claim | Fix role rules before dogfood. |

## Phase 5 - Handoff Lifecycle And Targeted ACK Dogfood

Purpose: prove delivery, ACK, acceptance, and resolve are separate.

Deliverables:

- Durable lifecycle:
  `handoff.requested -> handoff.acked -> handoff.accepted/rejected -> work.resolved/work.failed/work.superseded`.
- Delivery/read tracked in inbox/session indexes, not durable by default.
- `to_session_id` for targeted handoffs.
- `rally next` and `rally inject` route by live session, not broad `tool_type`,
  when a specific target is required.

Dogfood setup:

1. Launch or identify a managed Claude Code terminal:
   ```bash
   rally sessions --json
   rally run claude --name protocol-dogfood --dry-run --json
   rally run claude --name protocol-dogfood --json
   ```
2. Identify Codex's current session:
   ```bash
   rally whoami --tool codex --json
   ```
3. Codex posts a targeted handoff to the Claude session.
4. Brainstem records delivery/index state.
5. Claude must post `handoff.acked` with exact `from_session_id` and
   `ref_event_id`.
6. Claude posts `handoff.accepted` or `handoff.rejected`.
7. Claude resolves with evidence or fails with reason.

Checkpoint:

| Outcome | Detect | Next Step |
|---|---|---|
| Pass | Delivered is visible but not treated as ACK; Claude ACK cites exact ref | Move to operation/work result semantics. |
| Fail: wrong Claude | ACK `from_session_id` does not match target | Fix session targeting and legible-name display. |
| Fail: delivery counted as ACK | Message lifecycle skips ACK | Fix lifecycle projection before continuing. |
| Fail: no ACK | Delivery succeeded but Claude did not act | Capture session, inspect prompt state, retry injection or report host issue. |

## Phase 6 - Work And Operation Result Semantics

Purpose: prevent false success states.

Deliverables:

- `work.failed`, `work.cancelled`, `work.abandoned`, `work.superseded`.
- Replace risky operation completion with `operation.result`.
- Operation statuses:
  `succeeded`, `failed`, `cancelled`, `denied`, `rolled_back`.
- Validation result tied to artifact or resolve.

Tests:

- Failed work is retryable and distinct from abandoned work.
- Cancelled work is not suggested by `next`.
- Superseded work points to replacement.
- Failed operation does not project as landed.
- Rolled-back operation keeps evidence trail.

Checkpoint:

| Outcome | Detect | Next Step |
|---|---|---|
| Pass | `room`/`next` distinguish failed, cancelled, superseded, resolved | Start full dogfood. |
| Fail: false complete | Failed operation appears landed | Fix projection before closeout. |
| Fail: retry confusion | Abandoned and failed work collapse | Split states and add next-action tests. |

## Phase 7 - Rally Flow Multi-worker Dogfood

Purpose: use Rally Flow to coordinate the Rally implementation with lightweight
workers, while reserving full build-loop for the coordinator and risky
integration decisions.

Dogfood scenario:

1. Coordinator writes and lints the workstream descriptor.
2. Coordinator chooses one `run_id` and stamps every fact with `--run`.
3. Codex claims structured-scope and claim-authority implementation tasks with
   `exclusive` access.
4. Claude claims session/handoff docs and targeted ACK test files.
5. Spawned workers run `mini-loop` for their task packet only.
6. Cross-host Claude/Codex terminal task uses `rally run`/`rally inject`, not
   in-process subagents.
7. Codex requests Claude review through a targeted handoff.
8. Brainstem delivers to the Claude session.
9. Claude ACKs, accepts, reviews, and publishes validation.
10. Codex resolves implementation with artifact and validation evidence.
11. A fresh auditor tries to acquire a conflicting claim and confirms
    deterministic conflict behavior.
12. One intentionally expired scratch claim proves lease expiry without affecting
    active work.
13. Coordinator confirms `rally dag --run <run_id>` shows landed/in-flight/stalled
    accurately.

Success evidence:

```text
session registry: two unique sessions with legible names
claim authority: conflicting exclusive claim rejected or queued by policy
handoff lifecycle: requested -> acked -> accepted -> resolved
ledger: no durable heartbeat or lease-renewal spam
validation: cargo fmt/clippy/test green
room projection: no false complete states
dag projection: every task landed or intentionally blocked/superseded
```

Failure diagnostics:

| Symptom | Likely Cause | Immediate Next Step |
|---|---|---|
| Targeted handoff reaches wrong terminal | Session id or endpoint derivation wrong | Stop dogfood; inspect session registry and endpoint inputs. |
| Claude sees prompt but no ACK | Transport delivered, semantic action absent | Capture pane, check whether prompt submitted, retry wake route. |
| Both agents acquire same file | Active claim index not transactional | Stop implementation; fix claim transaction first. |
| Ledger grows rapidly while idle | Heartbeat or lease renewals durable | Move state to registry and add regression test. |
| Old logs fail replay | Strict schema broke compatibility | Add tolerant old-event parser. |
| `next` suggests cancelled/superseded work | Work projection bug | Fix state derivation before new features. |
| Operation failure appears complete | Result status collapsed | Fix `operation.result` projection. |
| Worker posts artifact without validation | Mini-loop skipped or task packet weak | Reject artifact, require mini-judge evidence, update descriptor. |
| `rally dag` misses a task | Missing `--run`/`--step` lineage | Patch worker loop and re-run from checkpoint. |

## Recommended Claude Instructions

Use this when launching the Claude dogfood lane:

```text
You are the Claude Code dogfood lane for Rally protocol identity and handoff work.
Read docs/PROTOCOL-NORTH-STAR.md and docs/PLAN-protocol-claim-authority-dogfood.md first.
Use skills/rally-workflows/SKILL.md for fan-out and skills/mini-loop/SKILL.md for each spawned task.
Own the session identity, handoff lifecycle, docs, and dogfood verification lane unless Rally says otherwise.
Before editing, claim exact files with Rally. Do not edit Codex-owned claim authority files without handoff.
Your required proof: targeted handoff ACK uses your exact from_session_id and ref_event_id; delivered is not treated as ACK.
If injection reaches the wrong terminal or no ACK occurs, stop and report the session registry evidence instead of continuing.
Post checkpoints as Rally artifacts with commands and results.
```

Claude checkpoints:

- C1: `rally whoami` shows unambiguous `session_id` and legible name.
- C2: Claude receives targeted handoff and posts `handoff.acked`.
- C3: Claude posts `handoff.accepted` or `handoff.rejected`.
- C4: Claude publishes validation artifact for Codex's claim-authority lane.
- C5: Claude confirms no durable delivery/read events unless strict audit mode is enabled.

## Recommended Codex Instructions

Use this for the Codex implementation lane:

```text
You are the Codex implementation lane for Rally protocol claim authority.
Read docs/PROTOCOL-NORTH-STAR.md and docs/PLAN-protocol-claim-authority-dogfood.md first.
Use skills/rally-workflows/SKILL.md for fan-out and skills/mini-loop/SKILL.md for each spawned task.
Own structured scopes, claim acquisition, lease expiry, and concurrency tests unless Rally says otherwise.
Before editing, claim exact files with Rally using exclusive access.
Do not broaden into session identity or handoff docs unless Claude releases or hands off the scope.
Your required proof: two concurrent exclusive claims for the same canonical scope cannot both acquire.
If old ledgers fail replay, stop and add compatibility before new schema writes.
Post checkpoints as Rally artifacts with commands and results.
```

Codex checkpoints:

- X1: Structured scope parser/canonicalizer passes file/dir/repo/port tests.
- X2: Concurrent exclusive claim test gives exactly one owner.
- X3: Lease renewal does not append durable facts.
- X4: Claim expiry emits one durable event and frees ownership.
- X5: Rebuild from ledger reconstructs active claim index.
- X6: `cargo fmt`, `cargo clippy`, and `cargo test --all` pass.

## Global Validation Gate

Before merge or closeout:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all
git diff --check
rally room --json
rally sessions --json
```

Closeout report must include:

- Canonical branch and HEAD.
- Changed files by phase.
- Passing commands.
- Dogfood transcript or event ids for handoff/ACK/claim conflict.
- Any skipped phase and why.
- Known follow-up triggers: crypto, HLC, federated transport, strict audit mode.
