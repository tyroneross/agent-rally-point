# Plan amendment — engagement truth after the PersonalLLMWiki audit

## Governing correction

The accepted liveness plan is necessary but not sufficient. S1-S3 can make claim expiry
safe, but S3 explicitly leaves activation to an operator and therefore does not drain expired
claims by default (`docs/plans/2026-08-05-observed-liveness-and-durable-renewal.md:115-125`).
S4-S6 bound and align the room projection, but they do not distinguish a collaborator from a
historical or presence-only identity. The remediation needs three separate contracts:

1. **Lifecycle truth:** safe reaping must run without a manual command.
2. **Engagement truth:** the scoped view used for a task and its hooks must show who contributed,
   not everyone who ever appeared in the repository.
3. **Verification truth:** Rally records coordination evidence; it does not independently prove
   a test result.

This amendment adds S8-S10. It does not reopen the completed design of S1-S7 or the
preservation-sensitive PersonalLLMWiki structural proposal.

## Approach Lenses

**Clean-sheet best approach:** make engagement the primary query boundary, attach stable sessions
to it, keep the repository-wide collision index internal, and run lease cleanup in a bounded
background owner rather than a user command.

**Current-constraints approach:** reuse the existing engagement segments, stable session identity,
run markers on coordination facts, room query, managed handoff wait, and enter-triggered reaper.
Add scope and truth to those contracts without replacing the append-only ledger or changing the
shared delivery `Directive`/`Receipt` structs; the local store-daemon query wire receives a
versioned scoped-read operation.

**Bridge/backcast:** first activate the already-built observer/reaper safely (S8), then expose the
engagement/run boundary already stamped in the ledger (S9), then derive collaboration status from
positive managed delivery, ACK, and in-scope output (S10).

**Recommendation:** take the bridge. A ledger replatform would not improve the audited remediation;
it would delay the three measurable corrections and widen migration risk.

## Audit assessment

The independent audit measured 519 displayed claims, 503 expired claims, one meaningful Codex
lane, eleven presence-only/read-only Codex identities, and no in-scope handoff, decision,
blocker, or cross-agent artifact exchange. A live read on 2026-08-05 reproduced the defect
shape: the latest `rally room --json` read reported 501 active claims and 272 squads, of which 271
were idle; `rally doctor --reap-stale` independently classified 500 claims as reclaimable in
dry-run with zero attempted writes.
The moving counts do not weaken the finding: the default room is dominated by expired work and
historical presence.

| Audit finding | Assessment | Current-plan coverage |
|---|---|---|
| Expired claims dominate the room | real | partial: S1-S3 make reaping safe, but the default remains off |
| Presence-only identities look like collaborators | real | not covered: S4 preserves squads as a never-cut bucket; S6 preserves enforcement parity |
| Only one meaningful lane produced in-scope work | real for this run, not itself a defect | not covered: a solo run is valid, but the view must label it honestly |
| No in-scope handoff or artifact exchange occurred | real for this run | not covered: Rally must not infer collaboration from presence or ACK alone |
| Unique task identity is missing | real symptom, wrong proposed mechanism | engagement/run scope should identify the task; stable session identity should identify the actor |
| Managed handoffs did not contribute | real adoption/effectiveness gap | partial: delivery and ACK exist, but engagement contribution does not depend on them |
| Rally did not independently prove tests | true boundary, not a claim defect | validation text must forbid self-reported Rally evidence from closing a test gate |
| Path filtering exists but the engagement view is still repository-wide | real | partial: fact buckets filter; squads and system health explicitly do not (`crates/rally-cli/src/store.rs:791-840`) |

## Depends-on (reads-from)

| Dependency | Consumer | State | Evidence |
|---|---|---|---|
| Durable renewal and observed liveness | S8 | delivered by S1-S3 before S8 may activate | base plan `:86-125` |
| Nonzero auto-reap interval | S8 | absent | default is zero at `crates/rally-cli/src/reaper.rs:155-183` |
| Engagement label on `enter` and append segments | S9 | present but not queryable from `room` | `crates/rally-cli/src/cli.rs:141-154`; `store.rs:1257-1275` |
| Current-engagement resolution | S9-S10 | process env is safe; the persisted repo-wide fallback can race concurrent tasks | `store.rs:4138-4183` |
| Selected-segment snapshot | S9 | absent; routed request engagement rebinds appends but snapshot still reads the repo-wide DB | `rallyd_core.rs:588-591,689-695`; `store.rs:2426-2453,2565-2572` |
| Path-filtered facts | S9 | present | `RoomQuery` at `crates/rally-cli/src/store.rs:844-925` |
| Scoped squads/system health | S9 | absent | room filtering preserves both repository-wide at `store.rs:815-823` |
| Positive managed-session handoff wait | S10 | present, but its target response is not yet engagement/run-qualified | inject envelope at `crates/rally-cli/src/lib.rs:6428-6468` |
| Merge-time presence, ACK, and path-claim gate | S10 | present but repo-wide | `coordination_offenders` at `lib.rs:4465-4498` |
| Handoff task scope | S10 | engagement segment and `run:<id>` fact marker exist; inject does not require or echo them | `crates/rally-cli/src/cli.rs:1309-1390`; `lib.rs:2300-2360` |
| Target-authored handoff ACK | S10 | `rally say resolve --ref ... --run ...`; distinct from coordination `rally ack` and delivery receipt | `crates/rally-cli/src/lib.rs:7695-7745,13637-13666` |
| Shared daemon protocol source compatibility | S10 | public `Directive` Rust literals exist in sibling `ptyd` | `crates/rally-protocol/src/lib.rs:6-18`; `../ptyd/Cargo.toml:32-35` |

## Capability Gap Map

| Capability | Current source of truth | Target behavior | Gap | Build action | Validation |
|---|---|---|---|---|---|
| Expired-claim lifecycle | reaper eligibility + disabled interval | observed-dead claims drain automatically | safe machinery is not activated; enter contention is unbounded | S8 engine + S10 activation | concurrent-enter and quiet-live controls |
| Task-scoped room | engagement segments + fact-only `RoomQuery` | engagement/run/path view derives its own contributors and health | squads/health remain repository-wide | S9 | audited one-plus-eleven replay |
| Collaboration state | scoped handoff/resolve/artifact facts and repo-wide coordination gate | collaboration requires a target-authored scoped resolve plus in-scope output | presence/general ACK can overstate contribution | S10 | handoff, resolve, and artifact state-transition journey |

## Activation Map

- S8 auto-reap — trigger: existing `command_enter` call site at `crates/rally-cli/src/lib.rs:1965-1987` after S10 moves it outside the primary enter commit path — verified-live: pending; S10's concurrent-enter journey and a live fixture must verify it before Report.
- S9/S10 room projection — trigger: `rally room` command dispatch through `RoomQuery::from` at `crates/rally-cli/src/store.rs:864-875` — verified-live: pending; the four-view fixture must verify it before Report.

## Security and permission boundary

`permission_tier: not-applicable` — the amendment introduces no new tool, plugin, MCP server,
external call, credential, or permission grant. It preserves the existing session identity and
same-UID local ledger boundary. Threat model: `docs/security/TRUST-MODEL.md`; S8 must preserve its
fail-closed destructive-action bar, and S9 must keep untrusted fact text out of control fields.

## Segments

### S8 — Bounded auto-reap engine and legacy disposition · depends on S1-S3

**Owns:** `crates/rally-cli/src/reaper.rs`, `crates/rally-cli/src/hooks_config.rs`,
`crates/rally-cli/tests/auto_reap_engine.rs`.

Set a nonzero default and make the engine act only on observer-eligible claims. Replace the racy
marker check with a single-flight lease and a strict work budget. Expose an activation result that
S10 can invoke after the primary `enter` append; S8 does not edit the call site. A live quiet
agent survives and a crashed observed agent is eligible for automatic closure.

Historical claims that predate observable worktree evidence require a separate migration
disposition. The first-run dry-run partitions them into `observed-dead`, `observed-live`, and
`unknown`; automatic cleanup acts only on `observed-dead`. Unknown legacy claims remain visible
and require the existing explicit operator apply path. This avoids inventing evidence merely
to hit a cleanup count.

**Acceptance:** concurrent direct engine calls elect one worker; the work budget caps each pass;
the quiet-mid-work regression survives; a fresh crashed claim is eligible; and the legacy dry-run
is idempotent and reports all unknowns rather than silently closing them. Reverting the nonzero
default or single-flight lease must fail a focused control.

### S9 — Engagement- and run-scoped room truth · depends on S4-S5

**Owns:** `crates/rally-cli/src/store.rs` selected-segment read/query/composition,
`crates/rally-cli/src/store_client.rs` scoped-snapshot transport,
`crates/rally-protocol/src/store_wire.rs`, `crates/rally-cli/src/rallyd_core.rs`,
`crates/rally-cli/tests/room_engagement_filter.rs`,
`crates/rally-cli/tests/snapshot_wire_internals.rs`,
`crates/rally-cli/tests/rallyd_handover.rs`.

Implement the store/query semantics for the `engagement`, `current_engagement`, `run`, and
`include_presence_only` inputs that S10 exposes in the CLI. Preserve the current repository-wide
`rally room --json` default for existing automation. An engagement read selects the named
append-only segment before snapshot composition; run/path selection then filters its facts,
squads, and health. The path-conflict join still reads repository-wide live claims.

Resolve `--current-engagement` by process `RALLY_ENGAGEMENT`, then by a durable
session-id/tool-to-engagement binding written by `enter`, managed launch, or adopt; keep the
repo-wide `active-engagement` file only as a legacy single-session fallback. For a `say` command
without the env var, use the unique active session binding for its `--tool`; more than one match is
ambiguous and must fail the scoped write rather than choose. This prevents two concurrent tasks
from relabeling each other through one shared file.

The existing `StoreRequest.engagement` cannot implement the read: today the daemon uses it only to
rebind the active append segment, while `SnapshotWithArchived` still folds the repo-wide facts DB.
Add `StoreOp::SnapshotScoped { run_id, path, include_archived, include_presence_only }` and bump the
store wire from v2 to v3. The request's required `engagement` label selects exactly
`.rally/log/<engagement>.jsonl` and `.rally/archive/<engagement>.jsonl`; it always unions and dedupes
both because rotation moves canonical events rather than making them optional history. The scoped
projector decodes and composes those lines directly; it must not call repo-wide `facts()` for task
participants or health. Preserve the existing `include_archived` meaning only after composition:
`false` suppresses decayed facts and stale squads, while `true` restores those policy-filtered rows.
When `path` is present, a separate repo-wide active-claim read contributes only overlapping
collision claims, never squads, health, or contributor credit.

The direct `RoomStore` method and routed `StoreOp` share the same projector. A scoped request with
no engagement is a usage error. A v3 client that finds a live v2 daemon fails with the existing
bounded version-mismatch remedy and never falls back to a repository-wide scoped result. Restarting
the daemon activates v3; the unfiltered `SnapshotWithArchived` operation remains unchanged for
existing automation.

Do not mint a disposable actor ID for each task. Preserve stable protocol session identity for
ownership, and use the engagement/run key as the task identity. A path-filtered read must include
every repository-wide live claim that overlaps that path, even when the claim belongs to another
engagement. That external-conflict join is the boundary that lets display scope narrow without
weakening enforcement.

**Acceptance:** replay the audited shape and assert that the scoped snapshot contains the one
matched squad rather than all twelve; the unfiltered default remains byte-compatible. A path query
includes a conflicting claim from another engagement, and `rally check before-write` actually
rejects that writer. Run the filter and rejection controls through direct and routed stores, and
compare both `include_archived=false` and `include_archived=true` projections. Moving the selected
segment from live to archive must leave its non-decayed default snapshot byte-equivalent; toggling
the flag may change only decayed facts and stale squads, with no duplicate events. Two concurrent engagements from the same actor type use
distinct protocol sessions and do not merge or overwrite each other's current scope. A unique
adopted-session binding resolves a no-env `say resolve` into the correct segment; two matching
bindings fail closed and append nothing. A stale v2 daemon fails with version-mismatch remediation;
after restart, the v3 direct and routed scoped snapshots are byte-equivalent.

### S10 — Activation, contributor truth, and positive managed ACK · after S8 and S9

**Owns:** `crates/rally-cli/src/lib.rs`, `docs/schemas/agent-rally.command.room.v1.json`,
`crates/rally-cli/src/cli.rs`, `hooks/rally-coordination-hook.sh`,
`crates/rally-cli/src/backends.rs` generated room-schema assertion only,
`crates/rally-cli/tests/engagement_effectiveness.rs`,
`crates/rally-cli/tests/hook_projection_parity.rs`,
`crates/rally-cli/tests/json_envelope_contract.rs`, `scripts/coordination-smoke.sh`, `RALLY.md`,
`docs/HANDOFFS-AND-LAUNCHING-AGENTS.md`, `skills/rally-workflows/SKILL.md`.

Wire S8 after the primary `enter` append with a per-pass budget that keeps every caller below the
normal 3,000 ms command watchdog. Cleanup cannot consume the primary append's commit budget or
change an otherwise successful `enter` to a failure.
Wire S9 into the room envelope and hook. Session-start output uses `--current-engagement`; the
before-write hook repeats the room query with the exact path so every enforcing external conflict
is rendered.

Managed launch paths stamp `RALLY_ENGAGEMENT` into the new child environment. Adopt cannot mutate
an already-running process environment, so it writes the durable session/tool binding that S9's
no-env `say` resolution consumes. For a managed handoff to count toward engagement effectiveness,
the originating `Handoff` fact must be in that
engagement segment and carry `run:<id>`. `inject --handoff` resolves that fact, validates the target,
and renders the exact target command:
`rally say resolve --tool <target> --ref <handoff.event_id> --run <id> --subject <text> --json`.
The target resolve inherits engagement from its session-bound segment. A missing or mismatched
engagement/run leaves the handoff unscoped and therefore cannot earn contributor status.

Do not change `rally_protocol::Directive` or `Receipt`; sibling `ptyd` constructs `Directive` with
Rust struct literals, so an additive field is source-breaking even when serde would decode older
JSON. The daemon receipt remains transport evidence joined by `(to, ref_seq)` and never counts as
the agent's handoff ACK. Coordination `rally ack` only acknowledges repository rules and also never
counts. The effectiveness ACK predicate is exactly: `kind == resolve`,
`ref_id == handoff.event_id`, `tool == handoff.target`, the fact's segment equals the handoff
segment, and its `run:<id>` equals the handoff run.

Derive three explicit task roles from the scoped facts:

- `working_contributors`: in-scope claims plus a changed-path or completion artifact; a dispatched
  worker also needs the target-authored `say resolve` above;
- `review_contributors`: in-scope decision, blocker, or review artifact; a dispatched reviewer
  also needs the target-authored `say resolve` above;
- `presence_only`: entered/ACKed but produced no in-scope coordination artifact.

Default human and hook summaries show working/review contributors and a count of suppressed
presence-only identities. S10 adds `--include-presence-only` and restores their rows when set.

When a lead dispatches a managed collaborator, bind the handoff, target resolve, and produced
artifact to the same engagement/run. Delivery without the scoped resolve is `awaiting_ack`, not
collaboration; resolve without an in-scope artifact is `presence_only`, not meaningful activity. Unmanaged agents
remain supported, but the view labels their delivery/ACK state as unverified instead of
crediting a managed handoff.

Do not require artificial handoffs in a genuinely single-agent run. Report `solo` when one
working/review contributor exists and `collaborative` only when at least two contributors have
in-scope outputs.

**Acceptance:** eight concurrent `enter` calls all exit 0 within the 3,000 ms watchdog while a reap
is due, and a fresh crashed claim is reaped automatically. A delivered-but-unacknowledged managed handoff leaves the run
`solo` and surfaces one `awaiting_ack` blocker. After the target runs the injected `say resolve`
command and posts a scoped review
artifact, the same run becomes `collaborative`. A presence-only ACK never changes the contributor
count. Neither a sender-authored delivery receipt nor `rally ack` satisfies the ACK predicate. The
shipped hook and Rust path query show the same external conflicting claim, and the corresponding
`check before-write` exits nonzero in both direct and routed store modes.

## Parallelism and integration

S8 is a new lifecycle engine after S3. S9 is a composition/query tail after S4-S5. S10 is the
only integration owner for `lib.rs`, the room schema, the shipped hook, and cross-segment journeys;
it follows both S8 and S9. The existing uncommitted S4-S5 composition worktree is preserved and
completed before S9 starts.

`parallel_batch: S8 may run beside completion of S4-S5; S9 starts after S4-S5; S10 starts only
after both S8 and S9 land.`

| Lane | Order |
|---|---|
| lifecycle | S1 → S2 → S3 → S8 |
| composition | S4 → S5 → S9 |
| integration | S8 + S9 → S10 |
| prior hook/isolated | S6 and S7 remain unchanged before S10 |

## Single-Shot Build Guardrails

| Guardrail | Failure prevented | Proof |
|---|---|---|
| Reap never precedes or invalidates the primary enter commit | recurrence of the 8/8 enter outage | S10 eight-process enter test with a due reap |
| Unknown observed state never authorizes automatic closure | destructive cleanup based on missing evidence | S8 legacy partition control |
| Scoped display never weakens collision enforcement | hidden claim still blocks a writer | S9 direct+routed `check before-write` rejection test |
| Stable session and task scope remain different fields | disposable identities and cross-task attribution | S9 same-actor, two-distinct-session test |
| Resolve alone never earns contributor status | presence noise reported as collaboration | S10 scoped-resolve-without-artifact test |
| Rally evidence never closes its own validation gate | self-attested test success | independent verifier review in the Validation boundary |

## Read-Before-Edit Map

| Work item | Read first | Why | Edit after |
|---|---|---|---|
| S8 | `reaper.rs:155-285`, reaper concurrency tests, S1-S3 commits | preserve opt-out, observed-death bar, work cap, and existing report contract | reaper/config + engine tests |
| S9 | `store.rs:2426-2453,2565-2572,2899-2923`, selected-segment loaders, `store_client.rs:353-357,464-485`, `store_wire.rs:46-52,141-144`, `rallyd_core.rs:568-590,689-697`, daemon handover/version controls, room budget tests | bypass the repo-wide DB for scoped participants, keep collision authority repo-wide, and cut direct/routed/archive reads to wire v3 without changing the unfiltered operation | store/wire/daemon + parity tests |
| S10 | `lib.rs:1951-1991`, managed launch/adopt session construction, handoff prompt + `wait_for_resolution` at `lib.rs:7680-7745`, `lib.rs:4465-4498`, `rally ack` at `lib.rs:13637-13666`, room schema, hook parity, workflow skill, and user-journey consumers | preserve enter ordering, stamp launched children, persist adopted-session scope, make handoff resolve distinct from general ACK/delivery, and update every room-output consumer together | CLI/lib/schema/hook/docs + integration journeys |

## API and caller audit

`modifies_api: yes` — S9 adds `StoreOp::SnapshotScoped` and bumps the private local store-daemon
wire from v2 to v3; the CLI and daemon binary ship together, and a stale daemon must be restarted
before scoped reads run. S10 adds optional room query flags and an optional scoped
`engagement_effectiveness` block. The shared delivery `Directive` and `Receipt` structs remain
unchanged. The unfiltered `rally room --json` response, existing v1 required fields, old ledger
records, and existing `SnapshotWithArchived` operation remain valid. S9 owns the store-wire cutover;
S10 owns the public room schema and documentation update.

Callers that must be rerun are `command_room`, the shipped coordination hook,
`tests/user_journey.rs`, `tests/snapshot_wire_internals.rs`, `tests/room_budget_scaling.rs`, and
`crates/rally-cli/tests/json_envelope_contract.rs`, `scripts/coordination-smoke.sh`, the generated
room-schema assertion in `crates/rally-cli/src/backends.rs`, and
`dynamic-workflows/core/workstream-status.mjs` with its tests. The Aggregate step in
`skills/rally-workflows/SKILL.md` switches to `rally room --current-engagement --run <run_id>
--tool <TOOL> --json`. Every automation caller that intentionally retains the repository-wide
default receives the unfiltered fixture as its adjacent compatibility control.

Source-compatibility control: `git diff` must show no change to `crates/rally-protocol/src/lib.rs`,
and the sibling consumer must compile and pass its focused daemon round-trip with
`cargo test --manifest-path ../ptyd/Cargo.toml --test termd_roundtrip`. This proves the scoped
effectiveness feature did not export a new required delivery-struct field into the daemon contract.
Store-wire controls must round-trip the new op, reject a missing engagement, prove v2/v3 mismatch
fails before snapshot dispatch, and prove a restarted v3 daemon matches the direct projector.

## Validation boundary

Rally facts are untrusted coordination records. A Rally claim, artifact, ACK, or receipt can
prove that the ledger recorded a statement; it cannot prove that tests passed. No segment closes
from self-reported Rally evidence alone. Test gates require captured command, exit status, and
output from the build/verifier surface, followed by the independent review already required by
the base plan (`docs/plans/2026-08-05-observed-liveness-and-durable-renewal.md:202-227`).

The independent audit for S8-S10 must compare four views of the same fixture: repository-wide,
engagement-filtered, run-filtered, and path-filtered, through direct and routed stores with archive
inclusion both off and on. It must also exercise the adjacent moves:
quiet live agent, crashed fresh-heartbeat agent, ACK without work, work without ACK, and a
cross-engagement path conflict.

## Outcomes a user can observe

| Segment | Before | After |
|---|---|---|
| S8 | expired claims require a manual doctor command | observer-eligible dead claims drain automatically without breaking `enter` |
| S9 | old presence makes a scoped task look multi-agent | an engagement/run view contains only matched participants while the repository default stays compatible |
| S10 | delivery/ACK can be mistaken for collaboration | only acknowledged, in-scope output changes the collaboration state |

## Out of scope

This amendment does not authorize automatic closure of legacy `unknown` claims, make Rally a
test runner, force multi-agent work, change the RC-063 authority decision, or move/delete any
PersonalLLMWiki content.
