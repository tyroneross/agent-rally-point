<!--
SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Rally Command Semantics

This table is the agent-facing contract for whether a command changes durable
coordination state, local caches, or managed runtime state. It exists so audit
and review agents can choose commands deliberately instead of assuming every
JSON command is read-only.

Definitions:

- **Ledger write:** appends or rotates canonical `.rally/log/**` or legacy
  replay state.
- **Cache write:** may create, rebuild, or update derived local files such as
  `.rally/facts.db`, `.rally/cursors.json`, or room discovery hints.
- **Runtime write:** starts, injects into, adopts, attaches to, captures from,
  stops, or otherwise touches a managed terminal/backend surface.
- **Audit-safe:** safe for review use when small derived cache writes are
  acceptable. It does not mean "no filesystem writes."

## Primary Loop

| Command | Ledger write | Cache write | Runtime write | Audit-safe | Notes |
|---|---:|---:|---:|---:|---|
| `rally whoami` | no | yes | no | yes | Self-locates repo, host runtime, lead, mission, and build id; may open/rebuild the room cache. |
| `rally enter` | yes | yes | no | no | Records presence, lead context, build-id drift, duplicate tool risks, and read cursor advancement. Attention is capped at 128 rows and reports total/emitted/omitted counts. |
| `rally ack` | yes | yes | no | no | Records that the tool ingested current rules, guardrails, lead, and mission. |
| `rally next` | yes | yes | no | no | Projects actionable work and records wake/read state for the calling tool. |
| `rally next --audit` | no | no | no | no | Projects the same actionable work without presence, wake, or read-checkpoint facts; derived caches may still rebuild. |
| `rally room` | no | yes | no | yes | Projects current room state from the ledger; use for ownership/blocker inspection. `--actionable` removes stale roster and closed artifact inventory from this output only, while retaining current system-health alerts. The canonical ledger, default room view, and `locate` history remain unchanged. |
| `rally check before-write` | no | yes | no | yes | Evaluates claim/decision/risk state; hooks may pair it with a separate claim write. |
| `rally check before-complete` | no | yes | no | yes | Reports only claims owned by the exact `--tool` plus current session; a sibling session sharing the tool label is not the owner. Manual CLI workflows must export one stable `RALLY_SESSION_ID` before claiming. The check rejects an invocation whose only identity is its short-lived Rally process, so an unpinned lifecycle cannot silently pass with a stranded claim. |
| `rally check liveness` | optional | yes | no | conditional | Advisory mode may scan all conflicted squads or filter one exact `--tool`. `--enforce` requires both `--tool <exact-target>` and `--actor <release-author>` and can release only that selected target's takeover-eligible claims. |
| `rally session ensure` | yes | yes | no | no | Mints or reuses one parent-exported lease, records exact-session presence, and reports identity, visibility, blocking, atomic-claim, lifecycle-close, and delivery guarantees independently as `enforced`, `advisory`, or `unmanaged`. Adapter flags are attestations, not capability discovery. |
| `rally session close` | yes | yes | no | no | Requires `--session-id` or parent `RALLY_SESSION_ID` plus the parent-exported one-time `RALLY_SESSION_CLOSE_TOKEN`; never guesses authority from the short-lived CLI process. Appends one `session.closed` transition and releases only claims whose tool and authoring session both exactly match, across engagements. |
| `rally session current` | no | yes | no | yes | Returns at most 128 unclosed registered leases, exact freshness counts, omission counts, and the effective adaptive `window_secs`. Stale/unknown leases remain explicit; this view never closes them or changes claim authority. |
| `rally session history` | no | yes | no | yes | Returns the newest explicit active/closed lease transitions with a caller-bounded limit of 1–100. Canonical history remains in `.rally/log/**`. |
| `rally say <kind>` | yes | yes | no | no | Appends durable coordination facts: claim, release, blocker, resolve, decision, artifact, handoff, risk, lesson, standby, wake, backlog-item, mission. |

### Referenced handoff targeting

`rally say handoff --ref <event-id>` uses a fail-closed target contract before
it appends presence or the handoff:

- A referenced artifact or request targets its exact author tool and
  `from_session_id`. Omit `--target`; an explicit different target is rejected.
  Exact unmanaged CLI/terminal identities remain addressable because no runtime
  registry can probe them. A managed identity must resolve to exactly one live
  runtime; stale or unknown managed liveness fails closed.
- A handoff reply must be authored by the exact tool and session bound as the
  original receiver. It targets the original author automatically and must pass
  `--handoff-state acked`, `accepted`, or `rejected`. Delivery,
  receiver-authored ACK, acceptance, rework, and resolution remain separate
  facts; a successful pane or daemon write is not semantic ACK.
  Rework is recorded as `handoff.rejected` with the change evidence, followed
  by a new `handoff.requested`; Rally does not infer it from subject prose.
- An intentional review or reroute of an artifact/request must pass
  `--target-policy third-party` and
  `--target <exact-session-id>`. A tool or name alias is accepted only when it
  resolves to one live managed session. Zero, multiple, stale, and unknown matches
  fail before append with a stable `handoff_target_*` error prefix and a
  corrective `rally sessions --json` command.
  This compatibility policy cannot reply to an existing handoff; only its exact
  bound receiver can do so. A reroute cites the original artifact/request in a
  new handoff.

Fact v1 persists the routing bridge in Rally-owned evidence keys:
`protocol:bridge_version=fact-v1`, `protocol:event_kind`,
`protocol:target_policy`, `protocol:to_session_id`,
`protocol:ref_event_id`, `protocol:causation_id`,
`protocol:correlation_id`, `protocol:handoff_id`, and
`protocol:idempotency_key`. Caller-supplied `protocol:*` evidence and duplicate
keys are rejected. The storage boundary requires every key on each new
referenced handoff. Existing markerless facts still replay unchanged.

Retries use a logical operation id derived from the complete semantic request,
including the author session, target session, state/subject, payload, evidence,
and causal identifiers. `--idempotency-key <key>` may supply that operation id.
The same key with the same semantic payload returns the first canonical fact;
the same key with any different semantic field fails with
`handoff_idempotency_conflict`. Distinct ACK, accept, reject, or rework facts
therefore receive distinct operation identities.

## Managed Sessions

| Command | Ledger write | Cache write | Runtime write | Audit-safe | Notes |
|---|---:|---:|---:|---:|---|
| `rally run` | yes | yes | yes | no | Starts Claude/Codex/OpenCode/Gemini in a managed worktree or shared checkout. Backends: `auto`, `tmux`, `cmux`, `ptyd`. |
| `rally sessions` | no | yes | optional | yes | Lists managed sessions; `--reap` tombstones stale sessions and is not audit-safe. |
| `rally inject` | yes | yes | yes | no | Writes a directive and may deliver through ptyd/tmux/cmux; `--handoff` waits for target-authored evidence. |
| `rally attach` | no | yes | yes | no | Attaches to a managed runtime surface when supported by that backend. |
| `rally capture` | no | yes | yes | no | Reads managed session output through the backend. Treat as runtime-touching even though it does not mutate the ledger. |
| `rally stop` | yes | yes | yes | no | Stops/tombstones a managed session. |
| `rally adopt` | yes | yes | yes | no | Registers an already-running tmux/cmux target as managed. |

## Inspection And Maintenance

| Command | Ledger write | Cache write | Runtime write | Audit-safe | Notes |
|---|---:|---:|---:|---:|---|
| `rally recent` | no | yes | no | yes | Reads recent room facts; `--all` remains scoped by global-index settings. |
| `rally locate` | no | yes | no | yes | Locates an event id in known room segments. |
| `rally inbox` | no | yes | no | yes | Open obligations addressed to the tool: targeted handoffs and targeted `requires_ack` artifacts. Cleared only by a receiver-authored ack (`say receipt`/`resolve`/`artifact` referencing the item). Unaffected by age, read cursor, recency decay, or reaper expiry — a reaper-expired handoff leaves `open_handoffs` but stays here, annotated `stale`. `--limit` caps rendered rows only; `count` is exact. |
| `rally status --global` | no | yes | no | yes | Workspace-scoped overview of indexed rooms; does not write facts. |
| `rally hooks status` | no | no | no | yes | Shows effective hook policy after session, repo, user, and default resolution. |
| `rally hooks on/off` | no | no | no | no | Writes `.rally/config.json` or `~/.config/rally/config.json`; hook runtime reads it before room work. |
| `rally hooks prompt` | no | no | no | no | Writes startup prompt mode (`once`, `always`, `off`) for repo or user scope. |
| `rally hooks room-detail` | no | no | no | no | Writes room-detail level (`brief`, `verbose`) for repo or user scope; default `brief`, one-session override via `RALLY_HOOK_ROOM_DETAIL`. |
| `rally board` | no | yes | no | yes | Projects in-flight claims and backlog from facts. |
| `rally dag` | no | yes | no | yes | Read-only causation view for a run id. |
| `rally wake-due` | no | yes | no | yes | Read-only standby projection; emits suggested commands, never executes them. |
| `rally mission` | no | yes | no | yes | GET is read-oriented; `--set`, `--may`, and `--must-check` append mission facts. |
| `rally backlog list` | no | yes | no | yes | Listing is read-oriented; `add` and `done` append facts. |
| `rally check-ci` | no | yes | no | yes | Read-only CI health gate; strict mode changes exit code, not ledger state. |
| `rally doctor` | no | yes | no | yes | Dry inspection by default; `--apply` rewrites the discovery index and is not audit-safe. `--binary-skew` is read-only and never exits non-zero on skew. |
| `rally enter` | yes | yes | no | no | Stale-state reaping is off by default — an audit found it closed a live agent's claim and widened RC-044. Opt in via `coordination.auto_reap_interval_secs` (config) or `RALLY_AUTO_REAP_INTERVAL_SECS` (env); supported cleanup is `rally doctor --reap-stale --apply`. |
| `rally retrospective` | no | yes | no | yes | Writes the requested retrospective output file, not ledger facts. |
| `rally rotate` | yes | yes | no | no | Moves old segments into archive unless `--dry-run` is used. |
| `rally migrate-legacy` | yes | yes | no | no | Replays legacy room data into repo-local ledger segments. |
| `rally init` | yes | yes | no | no | Creates/refreshes manifest and doc pointer blocks. |
| `rally version` | no | no | no | yes | Pure process metadata. |
| `rally watch` | no | yes | optional | conditional | `--once`/projection-only use is audit-friendly; `--on-activity` executes an external command. |
| `rally route-findings` | yes | yes | no | no | Converts verified findings into risks or handoffs. |
| `rally worktree gc` | optional | yes | yes | no | Dry-run is inspection; apply removes worktrees/branches after its safety checks. |

## Simplification Direction

Do not simplify Rally by deleting established commands first. Agents already
depend on the command names and JSON envelopes.

Simplify in this order:

1. Keep the command names stable and group them in docs by user intent.
2. Move each command implementation out of `crates/rally-cli/src/lib.rs` into a
   command module with one owner and focused tests.
3. Add a true no-write audit mode after the command semantics are executable
   enough to enforce, not just documented.
4. Only then consider aliases or deprecations, with compatibility warnings and
   envelope-contract tests.
