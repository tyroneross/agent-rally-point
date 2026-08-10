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

The 2026-08-07 auditability extension adds S11-S13 after S10. It preserves the same
append-only and advisory charter while covering four gaps the first amendment did not own:
request-versus-interpretation provenance, typed backlog closure, a compact audit projection with
a real context ceiling, and read-only bounded room inventory. This revision also makes the compact
projection a typed critical-operations brief rather than a blind character cut, and requires every
time/statistical claim to carry machine-checked scope, clock, formula, denominator, and validation
provenance.

The 2026-08-10 read-deconfliction extension adds O33-A through O33-D. It separates
nonexclusive reading/activity from exclusive ownership: pure reads run in parallel with zero
claims, mutations keep exact path checks, and a reader of actively changing bytes carries a
provisional source token and revalidates before a decision. O33-A may commit on its isolated branch,
and O33-B is then built on top of that branch, but neither enters central integration, local main,
an installed plugin, a pushed ref, or a user-active worktree before post-O26 O33-C is complete and
the combined A+B+C gate passes. Engagement-bound reader context waits for S9/S10 rather than
recreating their storage or identity work.

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
| A read-only workstream packet still emits an unscoped `claim` and later `release` | real | not covered by S8-S13; `dynamic-workflows/core/packet.mjs:162-192` treats an empty owns list as a claim with no path |
| Dynamic-workflow Node tests are not continuously gated and the real-CLI fixtures can skip without a release binary | real | O33-B evidence is local/manual only; the combined O33 activation gate must build the current release CLI and run the Node suite with zero skips before integration |
| Codex invokes the before-write wrapper for path-bearing reads and opaque shell tools | real | O33-A classifies the native effect before the wrapper's repo walk or Rally resolution; launcher cost and host matcher narrowing remain O33-D evidence gates |
| A reader can inspect a file while another agent edits it but cannot bind its conclusion to the bytes/context it saw | real | O33-C after S9/S10; reading remains parallel and the evidence is provisional until token revalidation |

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
| Native PreToolUse tool/effect name | O33-A/D | Codex 0.144.3 source proves `apply_patch` uses `tool_input.command`; synthetic replay exists, while live matcher invocation and other-host captures remain absent | `config/host-integrations.json`; [Codex 0.144.3 source lines 479-483](https://github.com/openai/codex/blob/rust-v0.144.3/codex-rs/core/src/tools/handlers/apply_patch.rs#L479-L483); `.codex/hooks.json` deliberately has no matcher |
| Native hook schema and timeout units | O33-A | Codex 0.144.3 accepts only `description` plus `hooks`; Claude and Codex handler timeouts are seconds, not milliseconds | [Codex hook config source](https://github.com/openai/codex/blob/rust-v0.144.3/codex-rs/config/src/hook_config.rs#L9-L17); [Codex command runner](https://github.com/openai/codex/blob/rust-v0.144.3/codex-rs/hooks/src/engine/command_runner.rs#L98-L103); [Claude common hook fields](https://code.claude.com/docs/en/hooks#common-fields); `scripts/generate_host_surfaces.py` |
| Nonexclusive durable task-activity signal | O33-B | public `FactKind::Presence` accepts run/step lineage through `say` and only claim facts project into `active_claims` | `store.rs:283-295,4834-4839`; `cli.rs:182-190`; `lib.rs:2353-2370` |
| Dynamic-workflow continuous gate | O33 combined activation | absent: current CI dispatches the Rust quality/release scripts, while neither runs the Node suite | `.github/workflows/ci.yml:85-89`; `scripts/run-quality-gate.sh:1-79`; `docs/security/ISSUE-52-DISPOSITION-2026-08-06.md:405-428` |
| Engagement/run/path-scoped writer view | O33-C | S9/S10 target; do not implement a second projector | S9/S10 ownership and acceptance below |

## Capability Gap Map

| Capability | Current source of truth | Target behavior | Gap | Build action | Validation |
|---|---|---|---|---|---|
| Expired-claim lifecycle | reaper eligibility + disabled interval | observed-dead claims drain automatically | safe machinery is not activated; enter contention is unbounded | S8 engine + S10 activation | concurrent-enter and quiet-live controls |
| Task-scoped room | engagement segments + fact-only `RoomQuery` | engagement/run/path view derives its own contributors and health | squads/health remain repository-wide | S9 | audited one-plus-eleven replay |
| Collaboration state | scoped handoff/resolve/artifact facts and repo-wide coordination gate | collaboration requires a target-authored scoped resolve plus in-scope output | presence/general ACK can overstate contribution | S10 | handoff, resolve, and artifact state-transition journey |
| Read/write operation boundary | generic path extraction and read-only packet claims | pure reads/activity create zero ownership; typed mutations check every target | read tools can become claims; opaque shell cannot declare an honest effect/path | O33-A/B | zero-Rally-subprocess hook fixtures + zero-claim packet journey |
| Provisional reader evidence | turn-level status/claim context | reader gets active writer/intent, a path source token, and deterministic revalidation without waiting | no byte/context binding at final conclusion | O33-C after S9/S10 | active-writer adjacent-change journey |

## Activation Map

- S8 auto-reap — trigger: existing `command_enter` call site at `crates/rally-cli/src/lib.rs:1965-1987` after S10 moves it outside the primary enter commit path — verified-live: pending; S10's concurrent-enter journey and a live fixture must verify it before Report.
- S9/S10 room projection — trigger: `rally room` command dispatch through `RoomQuery::from` at `crates/rally-cli/src/store.rs:864-875` — verified-live: pending; the four-view fixture must verify it before Report.
- O33-A operation wrapper — trigger: native `PreToolUse` envelope enters `hooks/rally-coordination-hook.sh` — verified-live: pending; the source-proven Codex 0.144.3 command carrier and synthetic replay are verified, but live matcher invocation and Claude/Cursor captures remain O33-D. A stays on its isolated branch; B builds on top, and no central/local-main/install/push/user-active integration occurs before post-O26 C and the combined A+B+C gate.
- O33-B read-only activity — trigger: `owns: "read-only"` packet generation — verified-live: the isolated release-binary temp-room journey passed locally on 2026-08-10 with one run/step-scoped nonexclusive `presence` activity, one linked artifact, and zero active claims; packet assertions prove zero claim/check/release commands. This is manual evidence, not continuous CI coverage. A+B stay branch-held and inactive until C supplies the active-writer and run-scoped activity projections and the combined gate builds the current release CLI and runs the Node suite with zero skips.
- O33-C reader revalidation — trigger: consequential path read/final conclusion — verified-live: pending; blocked on S9/S10 stable scope until the active-writer journey passes.

## Security and permission boundary

`permission_tier: not-applicable` — the amendment introduces no new tool, plugin, MCP server,
external call, credential, or permission grant. It preserves the existing session identity and
same-UID local ledger boundary. Threat model: `docs/security/TRUST-MODEL.md`; S8 must preserve its
fail-closed destructive-action bar, and S9 must keep untrusted fact text out of control fields.
O33's effect registry is routing metadata, not authority: opaque shell and unknown tools receive no
automatic claim, so any shell mutation still requires the explicit strict before-write protocol.

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

## Research Context — 2026-08-07 auditability extension

`depth: standard`; `workflow: general`; `blocks_final_claims: false` because the research packet
and deterministic measurements exist at
`/Users/tyroneross/dev/research/topics/ai-agents/ai-agents.rally-auditability-signal-and-context-cost.md`
and `/Users/tyroneross/dev/research/analysis-runs/rally-auditability-20260807/results.json`.
The source policy uses primary standards and official engineering guidance: W3C PROV-DM for
agent/activity/plan provenance, OpenTelemetry for execution correlation, Azure Event Sourcing for
append-only history plus query projections, Google SRE for actionable signal separation, and the
peer-reviewed *Lost in the Middle* result for long-context retrieval risk.

The original analysis was a retrospective 86,400-second ledger-time slice from
`2026-08-07T04:25:06Z` through `2026-08-08T04:25:06Z` over four explicit workspace roots; it was
not 24 hours of continuous observation and it was not an unrestricted laptop scan. The revalidated
script records those roots, the capture clock, duration, input hashes, formulas, and reconciliation
checks. That slice found 2,556 events across 18 active rooms: 54.07% mechanical telemetry,
33.14% lifecycle control, 12.17% mixed-origin risk, and 0.63% explicit semantic coordination. In
this repo, 69 of 100 events were presence/read telemetry; only one of nine semantic-or-risk facts
carried evidence, and none carried a ref, URI, scope, or run marker. On the same copied ledger,
current source reduced system-health rows 153→4 and bytes 108,703→2,840, but the complete room still
emitted 194,876 bytes. The extension therefore optimizes both provenance completeness and the
consumer projection; neither metric alone is sufficient.

Critical-operation formats converge on the same control: stable fields prevent omission, priority
puts the decision first, a complete source remains available for drill-in, and the receiver closes
the loop. USMC O-SMEAC says standard order formats expedite understanding, prevent omissions, and
facilitate ready reference; AHRQ TeamSTEPPS combines SBAR with check-back; FAA readback requires the
receiver to repeat critical clearance elements with identity; Google SRE keeps the most important
live incident state at the top and requires explicit acknowledgement of command handoff. S12 adapts
those mechanisms without implying that one bounded message can contain all historical detail.

## Auditability-extension approach lenses

**Clean-sheet best approach:** keep the immutable event stream, but model a host-captured user
request, agent-authored interpretation, decision, work span, evidence, and outcome as distinct
linked records. Serve purpose-built read models: one compact engagement audit for an LLM, one full
historical ledger for forensic drill-in, and one read-only global inventory for the operator.

**Current-constraints approach:** reuse `Fact` plus its existing `ref`, `uri`, `evidence`, `scope`,
engagement segment, and `run:` markers. Add kind-specific validators and append-only closure facts
rather than widening every legacy fact or rewriting history. Build the compact view from S9's
scoped snapshot, not a second database.

**Bridge/backcast:** S11 defines and validates request→interpretation→decision→artifact lineage and
typed backlog closure. S12 projects that lineage as one deterministic critical brief: mandatory
fields first, delta since the last acknowledged brief, explicit omissions, and drill-in IDs under
a 12 KiB audit envelope and 4,000-character hook envelope. S13 reuses the same summaries for
explicit bounded filesystem discovery and emits machine-checked time/statistical provenance without
enabling or writing the global index.

**Recommendation:** execute the bridge. A new provenance service or authenticated identity system
would cross the settled local-first/same-UID authority boundary; structured linked facts close the
audit gap without pretending Rally can authenticate the agent that wrote them.

### S11 — Intent provenance and append-only closure · after S9-S10

**Owns:** `crates/rally-cli/src/event_envelope.rs`, `crates/rally-cli/src/store.rs` fact-kind and
projection rules, `crates/rally-cli/src/backlog.rs`, `crates/rally-cli/src/cli.rs` and
`crates/rally-cli/src/lib.rs` provenance/closure commands only,
`docs/schemas/agent-rally.fact.v1.json`, `docs/schemas/agent-rally.command.say.v1.json`,
`docs/schemas/rally-protocol-events.md`, and
`crates/rally-cli/tests/intent_provenance.rs`.

Add two public kinds, `user_request` and `intent_interpretation`, plus a typed terminal backlog
transition. A host adapter records `user_request` with a content SHA-256 and a local/private URI;
raw user text is opt-in and never copied into repeated coordination facts. The interpreting agent
records `intent_interpretation` with `ref=<request.event_id>`, its stable session, engagement,
`run:<id>`, normalized goal, constraints, non-goals, acceptance criteria, and confidence. Decisions
reference the active interpretation; artifacts reference the decision or backlog item they satisfy.
An interpretation can supersede an earlier interpretation only through a new fact that references
the prior one; history is never edited.

Extend `rally backlog update` with a terminal `done|superseded` transition that requires
`--ref <verified-artifact-or-decision>` and at least one resolvable evidence or URI value. The
projection keeps the original backlog fact in history and excludes its terminal successor from
`next` suggestions. Legacy facts and unscoped `say` commands remain valid outside strict
provenance mode.

This is provenance, not authentication. The output labels host-captured versus agent-authored
records and preserves the existing warning that same-UID writers and tool IDs are not independently
authenticated.

**Acceptance:** one fixture captures a private request by hash/URI, records two competing
interpretations by different sessions, supersedes one, links a decision and Git artifact to the
survivor, and closes the matching backlog item. The audit chain reconstructs each edge without raw
request text; the closed item disappears from `next`, while `room --include-archived` still returns
every original record. Missing request ref, run marker, terminal evidence, or a cross-engagement
reference fails loud and appends nothing. Pre-S11 ledger fixtures and schemas still round-trip.

### S12 — Compact engagement audit and complete context limits · after S11

**Owns:** new `crates/rally-cli/src/audit_view.rs`, `crates/rally-cli/src/next.rs` suggestion limits,
`crates/rally-cli/src/cli.rs` and `crates/rally-cli/src/lib.rs` audit command integration only,
`docs/schemas/agent-rally.command.audit.v1.json`, `hooks/rally-coordination-hook.sh`,
`crates/rally-cli/tests/audit_view_budget.rs`, `crates/rally-cli/tests/hook_projection_parity.rs`,
`RALLY.md`, and `skills/rally-workflows/SKILL.md`.

Add `rally audit --current-engagement --run <id> --path <path> --json`, built exclusively from S9's
scoped snapshot and S11's provenance/closure relations. The default envelope contains the current
request hash/ref, active interpretation and acceptance criteria, unresolved decisions/blockers,
active collision claims, unacknowledged scoped handoffs, verified artifacts, contributor state,
suppressed counts by category, exact rendered bytes, and opaque drill-in IDs. Presence/read/session
events never render individually. The exact pretty-printed response including the trailing newline
must be at most 12,288 bytes; if a never-cut safety item alone exceeds the ceiling, return an explicit
overflow error and drill-in command instead of a nominal PASS.

Define `critical_brief.v1` as the repeatable agent-to-agent transmission inside that envelope. Its
mandatory order is: (1) schema/repo/engagement/run/path plus `generated_at`, source sequence, and
brief hash; (2) user-request ref, active interpretation, objective, constraints, and acceptance
criteria; (3) current phase, accountable owner, and changes since the last acknowledged brief;
(4) blockers, conflicting claims, safety holds, and unresolved decisions; (5) next action, owner,
trigger/due condition, and stop condition; (6) verification artifacts with external refs; (7)
handoffs and their target-authored ACK state; and (8) suppressed counts plus opaque drill-in IDs.
The receiver acknowledges `brief_sha256` and `source_seq`; a general presence ACK does not close the
handoff. `generated_at` is not sampled during rendering: it is the explicit UTC capture/as-of value
that selected the source snapshot and is included in the brief-hash inputs. The same source state,
source sequence, and capture/as-of value therefore produce byte-identical field order, content, and
hash; a newly captured clock intentionally produces a different brief.

The 4,000-character hook boundary remains defense in depth against unbounded untrusted ledger text,
but blind tail clipping is not the composition strategy. The hook serializes a smaller
`critical_brief.v1` projection by field priority. It may elide bounded optional prose only after
preserving every mandatory field, and it must report each omitted category/count and a drill-in
command. If mandatory fields cannot fit, it emits a typed `brief_overflow` containing the source
sequence and retrieval command; it never emits a plausible-looking partial brief followed only by
`...[truncated]`.

**Path A vs Path B:** Path A keeps the existing free-text message and 4,000-character tail cut. It
retains the ARP-004 injection bound but cannot prove which operational facts survived. Path B uses
the typed priority brief above, preserves the same security boundary, and adds omission accounting
and closed-loop acknowledgement. Choose Path B because it prevents the adjacent failure the current
cap permits: legitimate low-priority prose consuming the envelope before a later blocker or action.

Make limits complete: `next --limit N` caps primary alternatives and suggested backlog items, while
separate explicit flags may widen either list. The shipped hook reads the scoped audit view and
continues its surface-on-change behavior; it does not call the repository-wide room projection.
Preserve full `room --json` for automation and forensic compatibility.

**Acceptance:** the audited 189-squad/153-health fixture renders under 12 KiB with one task's
contributors and zero repeated presence facts, while retaining an external overlapping claim and
every unresolved high-severity blocker. `--limit 1` returns at most one backlog suggestion. Reverting
engagement selection, closure filtering, presence suppression, exact envelope measurement, or the
limit on suggestions fails a focused control. A model-facing smoke captures actual bytes and the
3-4 bytes/token range; the gate is on bytes, not the estimate.

An adjacent-move fixture puts oversized optional prose before a late blocker, conflicting claim,
next action, evidence ref, and unacknowledged handoff. Today's blind `line(message, 4000)` fails by
dropping at least one late field. The typed brief passes only if all mandatory fields survive,
optional omissions carry exact counts/drill-in IDs, the serialized hook output stays within 4,000
characters, and the recipient's ACK references the exact brief hash and source sequence. Permuting
input iteration order at one pinned capture/as-of value produces the same bytes and hash.

### S13 — Explicit read-only room inventory · after S12 integration

**Owns:** `crates/rally-cli/src/discovery.rs`, `crates/rally-cli/src/cli.rs` and
`crates/rally-cli/src/lib.rs` inventory integration only,
`docs/schemas/agent-rally.command.inventory.v1.json`,
`crates/rally-cli/tests/global_inventory.rs`, and `docs/RALLY_ARCHITECTURE.md`.

Add `rally inventory --root <path>... --since <duration> --json`. It performs a bounded read-only
filesystem discovery of `.rally` rooms below explicit roots, prunes build/cache/vendor directories,
and never creates or updates `~/.agent-rally-point/rooms/v1/index.json`. For each room it reports
repo/worktree identity, current engagement, last ledger activity, active/expired/unknown claim counts,
managed/unmanaged session counts, semantic artifact/decision/handoff counts, provenance coverage,
and exact compact-view bytes. Default output suppresses individual presence identities and raw
subjects; `--include-presence-only` and per-room drill-in restore them.

Every numeric output carries `captured_at` in UTC, explicit roots, interval start/end,
`duration_seconds`, whether the interval is rolling or caller-supplied, inclusion boundary, input
manifest hashes, count/denominator/formula, and machine-readable validation results. A rolling
`--since 24h` slice ends at the command's captured clock and proves `duration_seconds=86400`; it is
labelled `retrospective_ledger_event_slice`, never “24 hours observed.” A caller-supplied interval
is never silently replaced by current time. “Laptop-wide” is allowed only when the reported roots
and pruned directories justify it; otherwise the result says “explicit-root inventory.”

Duplicate reporting distinguishes `events_in_repeated_key_groups` from
`events_repeating_a_prior_key`; the latter subtracts the first occurrence in each group. Percentages
use the emitted denominator and a declared rounding rule. Category counts, room partitions, event
totals, interval duration, and read-only ledger sequence all reconcile or the command exits nonzero
without a success envelope.

The command labels activity as `recent`, `stale`, or `unknown`; filesystem presence is never reported
as a live agent. Duplicate Git common directories and Rally manifests dedupe to one repo/worktree
record. Unreadable or corrupt rooms become per-room warnings rather than aborting the scan.

**Acceptance:** a fixture tree with an active room, presence-only dormant room, expired claims,
duplicate worktree pointer, malformed JSONL, and unrelated cache directories returns the exact room
set without writing any file. Before/after filesystem manifests and global-index hashes are
byte-identical. The same fixture proves presence-only records do not create a `recent contributor`
and that explicit drill-in can still retrieve them.

A clock-boundary fixture uses timezone-aware timestamps immediately before, at, and after both
interval endpoints; only the inclusive in-range records count. An 86,400-second rolling fixture,
zero-denominator fixture, category reconciliation fixture, and three-identical-key fixture prove
duration, percentage, and duplicate formulas. The last fixture must report three events in the
repeated group but only two events repeating a prior key. Removing any root, input hash, denominator,
formula, capture clock, or validation result fails schema parity.

### O33-A — Native operation classification and zero-cost Rally reads · branch-held before S10

**Owns:** `hooks/rally-coordination-hook.sh`, `tests/hooks/test_rally_coordination_hook.sh`,
`config/host-integrations.json`, `scripts/generate_host_surfaces.py`,
`tests/scripts/test_generate_host_surfaces.py`, `tests/hooks/test_install_rally_hooks.sh`,
`tests/hooks/test_node_absence_advisory.sh`, generated Claude/Codex/Cursor hook surfaces and release
identities, `docs/AUTO-COORDINATION-HOOKS.md`, and `docs/security/TRUST-MODEL.md`.

Classify a named native tool before generic path extraction, the wrapper's repo walk, or any Rally
subprocess. `pure_read` and `opaque_shell` return exact `{}` and create no status/check/claim; the
generated launcher may still run `git rev-parse` to locate the wrapper, which O33-D measures.
`mutation` parses declared targets. Native `file_path` values may be absolute only when physical
containment resolves them inside the Rally root. `apply_patch` accepts only cwd-relative
add/update/delete/move directive headers, using Codex 0.144.3's source-proven `tool_input.command`
carrier plus an explicitly legacy `tool_input.patch` adapter. It rejects empty, identity-whitespace,
malformed, root-equal, outside-root, or symlink-escaping targets as one atomic envelope. Only a truly
new target resolves its missing suffix from the nearest physical existing ancestor; any unresolved
suffix containing `..` rejects atomically. Only a truly
absent tool-name key receives legacy extraction; present null, blank, non-string, or unknown values
make zero Rally calls after the self-gate. Only an absent target alias is optional; a present
null/blank target invalidates a multi-target mutation. A present canonical or alternate tool-input
carrier must be an object and never falls back to an outer-envelope path when malformed. The direct
native hooks-enabled projection preserves default→user→repo→session precedence, including
`RALLY_HOOKS=on` overriding a repo-level off value. Every path-bearing Rally call uses attached
`--name=value` arguments so a valid filename beginning with `-` cannot be reparsed as an option.
Generated Codex hook JSON uses only its 0.144.3-supported `description` and `hooks` top-level keys.
Claude and Codex hook timeouts are rendered in seconds, converting the canonical 5,000/10,000 ms
values to 5/10 rather than silently creating multi-hour host ceilings.

At most 16 mutation targets enter the automatic route. Seventeen or more reject atomically with a
diagnostic and zero Rally calls; the agent must strict-check and claim every exact target manually.
This is a degraded-mode ceiling pending a batch CLI primitive, not a measured optimum. For accepted
targets, post one working status, complete every check, read ownership once, then create one atomic
aggregate repeated-path claim for targets not already covered by the agent's own claim. A denial,
timeout, invalid response, or ownership-read failure creates zero claims; an earlier proven denial
remains visible if a later check times out. An `allow: true` warning remains visible and does not
become a conflict. The Rally call budget is bounded at 400 + 400 + 4,000 + 400 + 1,000 = 6,200 ms,
leaving 3,800 ms under the generated 10-second host timeout; at 16 targets, checks receive 250 ms
each. The outer guard sends immediate `KILL` at the millisecond deadline rather than adding a
per-call TERM grace; without a millisecond-capable `timeout`/`gtimeout` or high-resolution Perl
guard, the classified mutation degrades before any Rally call. This proves an arithmetic ceiling,
not successful real-host latency. Native Windows drive/backslash containment remains `UNKNOWN`
outside the proven macOS/Linux wrapper.

Do not narrow Codex's matcher from another host's documentation. Keep it unset until O33-D captures
a real installed matcher result. The `command` payload carrier is source-proven and replayed, but
that does not prove matcher invocation. The wrapper is the correctness boundary; a native matcher
is only a process-launch optimization.

**RED controls:** `O33-A: path-bearing pure read returns exact empty JSON before Rally resolution`,
`O33-A: opaque shell read returns exact empty JSON without unscoped check`, `O33-A: unknown native
tool fails open once with bounded diagnostic and no claim`, `O33-A: named local write preserves
path-scoped check and auto-claim`, `O33-A: Codex 0.144.3 command patch checks every target and claims
once`, `O33-A: leading or trailing target whitespace rejects without Rally`, `O33-A: one empty
apply_patch directive rejects the whole mixed target set`, `O33-A: apply_patch target ceiling
rejects the whole envelope before Rally`, `O33-A: timeout after prior checks creates zero claims`,
`O33-A: timeout after a proven conflict preserves the strict denial`, `O33-A: allow-plus-warning
still produces one aggregate claim`, and `O33-A: an existing coarse own claim prevents a redundant
aggregate claim`. Adjacent fixtures cover a blank move destination versus a valid two-target move,
malformed tool-input carriers with an outer path, session-on/repo-off precedence, Claude
absolute-inside/outside paths, a root filename beginning with `-`, cwd-relative parent segments,
symlink-plus-parent traversal, a nested-new target beneath the nearest physical ancestor,
self-gated diagnostics, explicit null/blank/non-string tool names, zero-Rally no-node reads/writes,
an ignored-TERM child under the aggregate deadline, and a missing millisecond watchdog that
degrades before Rally. Each read/unknown/rejected fixture asserts an empty Rally subprocess log. Generator
tests pin registry parity, the evidence-gated absent Codex matcher, Codex's exact top-level schema,
and second-based Claude/Codex timeouts.

O33-A may commit only on its isolated branch. It must not be merged, cherry-picked, or checked out
into central integration, local main, an installed plugin, a pushed ref, or a user-active worktree.
Build O33-B on top of A in isolation; integrate the combined chain only after post-O26 O33-C is
complete and the A+B+C gate passes. The project Codex and Claude hooks are already active for new
sessions, so a local-main merge would activate A's read bypass while its turn-level writer context
can still be stale or omit a path. C supplies the path-scoped writer view, source token, and final
revalidation contract required by the user outcome.

### O33-B — Nonexclusive read-only task activity · after O33-A

**Owns:** `dynamic-workflows/core/packet.mjs`, `dynamic-workflows/core/workstream-status.mjs`,
their focused and empirical tests, `skills/rally-workflows/SKILL.md`,
`dynamic-workflows/PROTOCOL.md`, and `dynamic-workflows/README.md`. It does not edit `store.rs`,
the read-receipt projector, or O26 storage/wire paths.

An `owns: "read-only"` packet emits no `claim`, no before-write check, and no `release`. Reuse the
existing public `presence` fact as the minimum nonexclusive task-activity signal:

```text
rally say presence --tool <tool> --subject "<step>: <intent>" --summary activity:read-only \
  --status working --run <run> --step <step> [--parent-step <dep>...]
```

Read-only prohibits intentional changes to task/domain resources. The generated Rally coordination
records and ordinary transient tool state created by verification are the only permitted writes;
neither is task output or ownership. This distinction keeps the packet internally satisfiable when
a named verifier such as `cargo-clippy` creates a cache or build artifact.

The completion artifact references that activity event and uses the same run/step. Presence is
already excluded from `active_claims`; the activity is historical, not a lease, so a crashed reader
owns nothing and needs no reap. This also avoids overloading O26/R10's `FactKind::Read` cursor
checkpoint and `read_seq:<N>` summary contract.

Before O33-C exposes `active_activities`, `workstream-status.mjs` uses a transitional exact
`<tool-prefix>:<task.id>` plus `squads[].status == "active"` heuristic for read-only tasks only. It
returns that state in a separate `active[]` collection; it never reports the task as `claimed`,
never lets a legacy read-only claim hold the task, and never lets write-task presence replace a
claim. The heuristic is not run-scoped, so no A+B activation is allowed. O33-C replaces it with the
engagement/run-bound projection before the combined A+B+C gate.

**RED controls:** `read_only_packet_emits_activity_and_zero_claim_release`,
`read_only_packet_allows_only_coordination_and_transient_verifier_writes`,
`read_only_packet_runtime_creates_zero_active_claims`,
`read_only_active_exact_task_tool_is_nonexclusive_and_not_redispatched`, and adjacent positive
`write_packet_still_claims_checks_and_releases_every_owned_path`. The runtime fixture asserts one
run/step-qualified presence activity, feeds the real post-Presence room into `workstreamStatus`,
reports the task as nonexclusive `active`, leaves zero active claims, and creates one linked
completion artifact. The resume controls reject idle, other-prefix, and substring task tools,
legacy read-only claims, write-task presence without a claim, and false completion for the legal
task ids `__proto__`, `constructor`, and `prototype`.

### O33-C — Active-writer reader context and source-token revalidation · after S9 and S10

**Owns:** a new `crates/rally-cli/src/read_context.rs`, `cli.rs`/`lib.rs` command integration only,
`docs/schemas/agent-rally.command.read-context.v1.json`,
`crates/rally-cli/tests/read_context_journey.rs`, the O33 reader guidance in
`skills/agent-rally-point/SKILL.md`, and the shipped hook's S10 turn-level context integration. It
consumes S9's scoped projector and S10's stable session/engagement binding; it does not add another
store op, fact kind, cursor, or claim index.

`permission_tier: not-applicable` — this is a local read-only CLI projection with no external
service, credential, or new host permission; it cannot authorize a filesystem mutation.

Add a read-only `rally read-context --tool <tool> --current-engagement --run <run> --path <path>
--json`. It reports active overlapping writers with sanitized tool, intent, claim ref, effective
renewal sequence, and observed session state. It also hashes a canonical source tuple: repo/worktree
identity, normalized path, current file SHA-256, Git HEAD, path-scoped fact maximum, active claim and
renewal refs/sequences, writer-status sequence, engagement, and run. `captured_at` and unrelated room
facts are excluded from the hash. The response includes `source_token`, `provisional`, and an exact
revalidation command.

A reader never waits merely because a writer is active. It may inspect current bytes, but any audit
finding, decision, or final conclusion derived from those bytes is provisional. Immediately before
the conclusion it reruns with `--source-token <token>`: `unchanged` permits the conclusion;
`changed` requires reread/recompute; `writer_active` keeps the disclosure provisional even when the
bytes are unchanged. A read-before-write then obtains the normal exclusive claim/check and repeats
the read if its token changed.

**RED controls:** `active_writer_marks_read_provisional_without_blocking`,
`file_or_overlapping_writer_change_invalidates_source_token`,
`unrelated_room_fact_does_not_invalidate_source_token`,
`writer_completion_then_reread_closes_provisional_state`, and
`direct_and_routed_read_context_are_byte_identical`. The adjacent-move test changes an uncommitted
file without moving HEAD; the token must still change.

### O33-D — Native-envelope and quiesced performance proof · A synthetic gate now, full gate after S10

**Owns:** `tests/hooks/test_native_hook_envelopes.sh`,
`scripts/bench_hook_operations.py`, and captured results under
`research/analysis-runs/rally-read-deconfliction-<date>/`. It changes no production policy.

Capture real envelopes from each installed host/version before adding or changing a matcher. Store
only redacted tool/effect/path shape, host version, config hash, and expected classification. Codex
0.144.3's `apply_patch` `command` carrier already has source-level proof and synthetic replay; its
live matcher invocation still needs capture. Replay every capture through the wrapper and assert
exact output plus Rally subprocess count. A host without captured evidence keeps the wrapper-only
path and is labelled `UNKNOWN`, not inferred from Claude, Codex, or Cursor examples.

The benchmark script records UTC capture time, monotonic per-sample durations, iteration count,
machine/host versions, input hashes, output bytes, Rally subprocess counts, load average, and top CPU
consumers. It refuses to publish a comparison while the host is non-quiescent or orphaned test/agent
processes exceed the declared CPU bound. On the reference Mac, 200 warm alternating samples must
keep a named pure-read response at exactly two bytes, zero Rally subprocesses, and p95 at or below
75 ms; it also reports median/p95 versus the pre-O33 unscoped-check path rather than using hand math.
Mutation gates grade correctness by per-target counts and report latency separately; they do not
trade away one check per target to hit the read budget.

## Parallelism and integration

S8 is a new lifecycle engine after S3. S9 is a composition/query tail after S4-S5. S10 is the
only integration owner for `lib.rs`, the room schema, and cross-segment journeys; it follows both
S8 and S9. O33-A may commit on an isolated branch because it has no S9/S10 dependency, but it stays
branch-held. O33-B builds on top of that exact commit; the chain does not enter central integration
or local main until post-O26 O33-C is complete and the combined A+B+C gate passes. The existing
uncommitted S4-S5 composition worktree is preserved and completed before S9 starts.

S11-S13 are a second sequential tail after S10. Each segment that touches `cli.rs` or `lib.rs` runs
in its own worktree and integrates by exact commit to avoid shared-file clobbering. S13's discovery
logic can be drafted beside S11-S12, but its CLI integration lands after S12.

O33-A is isolated hook/generator work and may commit only on its branch. O33-B removes read-only
claim/release emission after O33-A by reusing Presence, leaving O26's `FactKind::Read` contract
untouched; B is built on top of A in isolation. O33-C waits for both S9 and S10 because its
correctness depends on their
path projection and stable task/session binding. No A-only or A+B central integration, local-main
checkout, install, user-active checkout, push, or O33-complete claim is allowed; after post-O26 C is
complete, the activation gate runs A+B+C together and only the combined chain integrates. O33-D's
synthetic replay and benchmark can run after A; native engagement-context proof completes after C.

`parallel_batch: S8 may run beside completion of S4-S5; S9 starts after S4-S5; S10 starts only
after both S8 and S9 land.`

`parallel_skipped_reason: S11-S13 integration serializes because each changes the public command
surface and S12 reads S11 closure/provenance while S13 reuses S12 compact summaries.`

| Lane | Order |
|---|---|
| lifecycle | S1 → S2 → S3 → S8 |
| composition | S4 → S5 → S9 |
| integration | S8 + S9 → S10 |
| prior hook/isolated | S6 and S7 remain unchanged before S10 |
| auditability | S10 → S11 → S12 → S13 |
| read/write boundary | O33-A → O33-B; S9 + S10 → O33-C; O33-D brackets A and C |

## Single-Shot Build Guardrails

| Guardrail | Failure prevented | Proof |
|---|---|---|
| Reap never precedes or invalidates the primary enter commit | recurrence of the 8/8 enter outage | S10 eight-process enter test with a due reap |
| Unknown observed state never authorizes automatic closure | destructive cleanup based on missing evidence | S8 legacy partition control |
| Scoped display never weakens collision enforcement | hidden claim still blocks a writer | S9 direct+routed `check before-write` rejection test |
| Stable session and task scope remain different fields | disposable identities and cross-task attribution | S9 same-actor, two-distinct-session test |
| Resolve alone never earns contributor status | presence noise reported as collaboration | S10 scoped-resolve-without-artifact test |
| Rally evidence never closes its own validation gate | self-attested test success | independent verifier review in the Validation boundary |
| Raw user text is not duplicated into the ledger | privacy leak through coordination history | S11 hash/URI fixture and raw-text absence assertion |
| Interpretation never masquerades as the user request | agent inference presented as user intent | S11 distinct kind/ref/session chain |
| Closed work never remains suggested | stale backlog drives duplicate work | S11 terminal successor + S12 `next --limit` controls |
| Full history never becomes the default LLM prompt | 49k-76k estimated-token room injection | S12 exact 12 KiB audit-envelope gate |
| A safety cap never chooses context by blind tail position | a late blocker/action disappears behind earlier prose | S12 typed priority brief + omission manifest + overflow control |
| A handoff is never assumed understood because it was delivered | receiver acts on a different interpretation | S12 hash/source-sequence check-back ACK |
| Inventory never mutates discovery state | an audit changes the thing it measures | S13 before/after filesystem and index hashes |
| A time/statistical claim never hides its clock or denominator | a retrospective slice is reported as observation time or duplicate groups as duplicate events | S13 timestamp/formula/reconciliation gates |
| A read operation never creates exclusive ownership | parallel readers block writers or create stuck claims | O33-A zero-Rally-subprocess fixtures + O33-B zero-claim runtime journey |
| Dynamic-workflow regressions cannot pass through an unarmed suite | a missing/stale release binary skips the real-CLI journey and CI reports green | combined O33 activation builds the current release CLI and requires the full Node suite to report zero skips |
| An opaque shell command is never guessed into a typed write path | command parsing gives false deconfliction confidence | O33-A exact `{}`/zero-Rally fixture plus explicit shell-mutation protocol |
| One malformed patch target invalidates the whole automatic check | partial target coverage hides an outside-repo or omitted mutation | O33-A malformed/escape atomic rejection fixture |
| A timeout never converts a proven denial into silence | a later invalid check erases an earlier active-writer conflict | O33-A conflict-then-timeout strict-denial fixture |
| A target ceiling is explicit and never truncates ownership | a large patch mutates an unchecked or unclaimed suffix | O33-A 17-target zero-Rally rejection plus documented manual strict fallback |
| A reader never finalizes against silently changed bytes | active writer changes an uncommitted file after the read | O33-C file-digest adjacent-move token test |
| Native matcher policy never comes from cross-host analogy | a host silently stops invoking mutation hooks | O33-D captured-envelope/version gate |

## Read-Before-Edit Map

| Work item | Read first | Why | Edit after |
|---|---|---|---|
| S8 | `reaper.rs:155-285`, reaper concurrency tests, S1-S3 commits | preserve opt-out, observed-death bar, work cap, and existing report contract | reaper/config + engine tests |
| S9 | `store.rs:2426-2453,2565-2572,2899-2923`, selected-segment loaders, `store_client.rs:353-357,464-485`, `store_wire.rs:46-52,141-144`, `rallyd_core.rs:568-590,689-697`, daemon handover/version controls, room budget tests | bypass the repo-wide DB for scoped participants, keep collision authority repo-wide, and cut direct/routed/archive reads to wire v3 without changing the unfiltered operation | store/wire/daemon + parity tests |
| S10 | `lib.rs:1951-1991`, managed launch/adopt session construction, handoff prompt + `wait_for_resolution` at `lib.rs:7680-7745`, `lib.rs:4465-4498`, `rally ack` at `lib.rs:13637-13666`, room schema, hook parity, workflow skill, and user-journey consumers | preserve enter ordering, stamp launched children, persist adopted-session scope, make handoff resolve distinct from general ACK/delivery, and update every room-output consumer together | CLI/lib/schema/hook/docs + integration journeys |
| S11 | `store.rs:434-465`, `event_envelope.rs`, `backlog.rs`, fact/say schemas, S9 engagement/run projector, and the historical/current metadata-coverage results | reuse existing optional provenance fields without conflating user request and interpretation or breaking legacy rows | envelope/store/backlog/CLI + provenance journey |
| S12 | `store.rs:3995-4065,4484-4523`, `next.rs` suggestion construction, `hooks/rally-coordination-hook.sh:1373-1379`, hook caps/dedup, S9/S10 scoped room schema, measured 0.2.0/0.2.1 outputs, AHRQ SBAR/check-back, FAA readback, USMC O-SMEAC, and Google SRE handoff/state-document guidance | build one bounded consumer view, preserve never-cut collision truth, make omissions explicit, close the communication loop, and make list limits apply to every rendered list | audit module/next/CLI/hook/schema/docs + exact-byte/adjacent-move/ACK controls |
| S13 | `discovery.rs:116-316,592-611`, global-status tests, manifest/worktree resolution, and `research/analysis-runs/rally-auditability-20260807/rally_audit.py` plus its original/current result artifacts | preserve opt-in indexed discovery while adding an explicit read-only scan whose activity labels cannot imply process liveness, and make clock/scope/formula provenance mandatory | discovery/CLI/schema/docs + no-write/time/math fixtures |
| O33-A | generated host matchers/launcher, the wrapper's generic path extraction, Codex 0.144.3 `command` source, hook/install/generator/no-node tests, and native tool envelopes available in fixtures | classify effect before wrapper repo/Rally work, preserve every accepted mutation target as an all-or-none transaction, and keep matcher/Windows uncertainty explicit | hook/config/generator/docs + subprocess-count/timeout/containment fixtures |
| O33-B | `packet.mjs:150-215`, `workstream-status.mjs`, public Presence lineage, protocol/skill packet examples | replace the unscoped read-only claim/release lifecycle with nonexclusive run/step activity and keep transitional resume state distinct from claims without editing O26 storage | packet/status/protocol/skill + CLI runtime journey |
| O33-C | S9 path projector, S10 engagement/session binding and hook context, file/git digest inputs | bind consequential reader evidence to exact bytes and overlapping writer state without waiting or creating a claim | new read-context command/schema/journey + hook/skill guidance |
| O33-D | installed host versions/configs, real redacted envelopes, pre-O33 hook, CPU/process state | prove native routing and cost with captured inputs and scripted clocks/statistics | replay + quiesced benchmark artifacts only |

## API and caller audit

`modifies_api: yes` — S9 adds `StoreOp::SnapshotScoped` and bumps the private local store-daemon
wire from v2 to v3; the CLI and daemon binary ship together, and a stale daemon must be restarted
before scoped reads run. S10 adds optional room query flags and an optional scoped
`engagement_effectiveness` block. The shared delivery `Directive` and `Receipt` structs remain
unchanged. The unfiltered `rally room --json` response, existing v1 required fields, old ledger
records, and existing `SnapshotWithArchived` operation remain valid. S9 owns the store-wire cutover;
S10 owns the public room schema and documentation update.

S11 adds optional public fact kinds and terminal backlog semantics without adding required fields to
legacy `Fact`. S12 adds a new audit command schema and changes `next --limit` semantics so every
consumer of suggested backlog arrays must be rerun. S13 adds a new inventory envelope; existing
`status --global` and the opt-in global index remain compatible. The shared `Directive` and
`Receipt` protocol stays unchanged throughout S11-S13.

O33-A and O33-B add no Rust API or wire field. O33-B reuses advertised `presence` with existing
lineage fields and keeps `FactKind::Read` reserved for O26/R10 cursor checkpoints. O33-C adds a public
`read-context` command/envelope only after S9/S10, using their existing scoped read operation; it
does not bump the store wire or add a fact kind. O33-D adds no API.

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

S11-S13 callers that must be rerun are `command_say`, `command_backlog`, `command_next`,
`command_room`, `command_status`, the shipped hook, `tests/user_journey.rs`,
`tests/json_envelope_contract.rs`, `tests/hook_projection_parity.rs`, docs-schema parity, and every
consumer found by project-wide grep for `suggested_backlog_items`, `RoomSnapshot`, and global-status
envelopes. No model-facing guide may continue prescribing unfiltered `rally room --json` as the
first read after S12.

O33 callers that must be rerun are every generated host hook surface, the global hook installer,
the native wrapper, dynamic-workflow packet rendering/lint/injection tests, the rally-workflows and
agent-rally-point skills, S10 hook projection parity, and direct/routed room/read-context journeys.
The pre-O33 write packet and Claude edit matcher remain adjacent compatibility controls.

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

For S11-S13 the independent audit must also replay the revalidated historical 86,400-second slice
and a newly captured rolling 24-hour slice: repeated idle presence,
a completed-but-planned backlog item, one evidence-rich free-text artifact with empty structured
refs, a health-heavy room, and a 125-room discovery tree. It compares full ledger, scoped room,
compact audit, `next --limit 1`, and inventory outputs. Git SHA/test evidence is checked outside
Rally. The audit script records exact roots, UTC clock, interval duration, input hashes, formulas,
denominators, reconciliation verdicts, and response bytes before interpreting time, percentage,
duplicate, or token impact.

For O33 the independent audit must compare five operation classes: pure read, consequential read,
read-before-write, mutation, and destructive/admin mutation. It records native envelope hash,
effect class, normalized targets, exact hook bytes, subprocess counts, source token, revalidation
verdict, monotonic timing samples, UTC capture clock, formulas, and machine/process quiescence. Rally
facts may prove that an activity/claim was recorded; they do not prove file bytes or test success.

## Outcomes a user can observe

| Segment | Before | After |
|---|---|---|
| S8 | expired claims require a manual doctor command | observer-eligible dead claims drain automatically without breaking `enter` |
| S9 | old presence makes a scoped task look multi-agent | an engagement/run view contains only matched participants while the repository default stays compatible |
| S10 | delivery/ACK can be mistaken for collaboration | only acknowledged, in-scope output changes the collaboration state |
| S11 | agent prose can blur user request, interpretation, and outcome; completed work can remain planned | linked immutable request/interpretation/decision/artifact records and terminal backlog successors preserve the distinction |
| S12 | a model may consume 49k-76k estimated tokens, while a blind 4,000-character cut can drop a late critical fact | a deterministic priority brief fits its transport, exposes omissions/drill-in IDs, and requires hash-bound receiver acknowledgement |
| S13 | answering “all open Rally rooms on this laptop” requires an ad hoc scan whose time and root scope can be misunderstood | one explicit read-only command returns bounded room/activity/provenance status with checked clock, scope, formulas, and no global-index mutation |
| O33-A | a path-bearing read or opaque shell call can run an unscoped before-write check and create noise | internal prerequisite: named reads/shell return `{}` with zero Rally calls, while accepted typed mutation targets check completely before one aggregate claim; activation waits for A+B+C |
| O33-B | `owns: read-only` creates an unscoped exclusive claim that can outlive the reader | the packet records nonexclusive activity and completion with zero claim/release facts |
| O33-C | a reader must either wait for a writer or risk finalizing against moving bytes | it reads in parallel with writer/intent context, marks evidence provisional, and revalidates an exact source token before conclusions |
| O33-D | hook cost and matcher safety are inferred from synthetic examples or noisy hosts | redacted native captures and a quiesced scripted benchmark report exact routing, bytes, calls, and statistics |

## Out of scope

This amendment does not authorize automatic closure of legacy `unknown` claims, make Rally a
test runner, force multi-agent work, change the RC-063 authority decision, or move/delete any
PersonalLLMWiki content. S11 does not authenticate same-UID agents or store raw user requests by
default. S13 does not enable persistent global indexing, scan outside explicit roots, or treat a
process/room/path observation as proof of human or agent identity. O33 does not serialize pure
reads, parse arbitrary shell text into authority, infer native matcher support, make read activity a
claim, or let read context authorize a destructive/admin action. O33-A/B do not duplicate O26
storage work; O33-C does not land before S9/S10 stable scope exists. O33-A does not claim native
Windows support, eliminate the temporary 16-target ceiling, or activate the read bypass alone.
