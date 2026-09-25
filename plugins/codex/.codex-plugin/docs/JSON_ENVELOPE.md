# JSON Envelope Contract

Every `rally <cmd> --json` response has the shape:

```json
{
  "ok": true,
  "product": "rally",
  "command": "<cmd>",
  "schema": "agent-rally.command.<cmd>.v1",
  "data": { "<cmd>": { ... }, ... }
}
```

**Rule:** `data[command]` always holds the command's primary result object. The key matches the `command` field exactly — kebab preserved (e.g. `data["wake-due"]`, `data["check-ci"]`). Shared/contextual payloads (`room`, `verified`, `warnings`) appear as sibling keys in `data` where noted.

The watchdog fail-open response is the one transport-level exception. Rally still exits 0 so a
host hook is never gated, but `ok` is false because the requested command did not complete:

```json
{
  "ok": false,
  "product": "rally",
  "command": "watchdog",
  "schema": "agent-rally.command.watchdog.v1",
  "data": {
    "watchdog_timeout": true,
    "reason": "command did not complete before the watchdog deadline; coordination failed open",
    "elapsed_ms": 3001
  }
}
```

Callers must test `data.watchdog_timeout` before reading `data[command]`. The elapsed value is
measured wall time, not the configured budget. This replaces the former neutral
`{"ok":true,"product":"rally"}` response, which made a watchdog timeout indistinguishable from a
successful command with missing data.

## Error envelope

When a command fails outright (before it can build a `data[command]` result — a usage error, a
not-found lookup, a claim refusal, an I/O failure, and so on), Rally writes a *different*, smaller
envelope to **STDERR**, not stdout, and exits nonzero:

```json
{"ok":false,"product":"rally","error":"<message>","exit_code":<n>}
```

This shape has no `command`, `schema`, or `data` field — it is not the success envelope with `ok`
flipped. Source: `crates/rally-cli/src/output.rs` (`CliError::error_text`), fed by
`crates/rally-cli/src/error.rs` (`RallyError::exit_code`). See "Exit codes" below for what `<n>`
can be.

This is distinct from the `ok: false` results documented elsewhere in this file
(`watchdog`, `partial_commit`, `mutation_outcome_unknown`, a failed `doctor` mode, a failed
`migrate-legacy`): those are commands that ran to completion and produced a `data[command]`
result on **stdout**, just one reporting a failure outcome. The error envelope above is for a
command that did not produce a result at all.

## How to parse safely

A caller must branch on the process exit code before trusting `data` to exist:

```python
import json, subprocess

proc = subprocess.run(["rally", "<cmd>", "--json"], capture_output=True, text=True)
if proc.returncode != 0 and not proc.stdout.strip():
    # Hard failure: no result envelope at all. The error envelope is on stderr.
    error = json.loads(proc.stderr)
    raise RuntimeError(f"rally {error['error']} (exit {error['exit_code']})")

envelope = json.loads(proc.stdout)
if envelope.get("data", {}).get("watchdog_timeout"):
    raise RuntimeError(envelope["data"]["reason"])
if not envelope["ok"]:
    # Command ran, but reports a failure outcome (partial_commit, a failed
    # doctor mode, a strict check/check-ci stop, ...). data[command] is still
    # present; read it for the specifics before deciding whether to retry.
    pass
result = envelope["data"][envelope["command"]]  # always works when data is present
```

```bash
rally <cmd> --json > /tmp/rally_out.json 2> /tmp/rally_err.json
if [ ! -s /tmp/rally_out.json ]; then
  # nothing on stdout: read the error envelope from stderr instead
  python3 -c "import json,sys; e=json.load(open('/tmp/rally_err.json')); sys.exit(f\"rally: {e['error']} (exit {e['exit_code']})\")"
fi
python3 -c "
import json
d = json.load(open('/tmp/rally_out.json'))
if d.get('data', {}).get('watchdog_timeout'):
    raise SystemExit(d['data']['reason'])
print(d['data'][d['command']])
"
```

## Exit codes

`RallyError::exit_code()` (`crates/rally-cli/src/error.rs`) sets the default; specific commands
override it for their own pass/fail or strict-mode semantics.

| Code | Meaning | Where it comes from |
|------|---------|----------------------|
| 0 | Success, or a non-strict advisory result (warn mode) | Default |
| 1 | Generic command/message/I/O/JSON error; `OutcomeUnknown`; `IncompatibleWire` | `RallyError::exit_code()` — `Command`, `Message`, `OutcomeUnknown`, `IncompatibleWire`, `Io`, `Json` variants |
| 1 | Command ran but reports a failure outcome on stdout: `partial_commit`, `mutation_outcome_unknown`, a failed `doctor` mode (`--reap-stale` with write failures, `--ledger-health` unhealthy, bare `doctor`), `migrate-legacy` with `outcome_unknown > 0` | `crates/rally-cli/src/lib.rs` (`output_after_committed_error`, `command_doctor`, `command_migrate_legacy`) |
| 2 | Usage error, including a claim refusal: a conflicting claim, or an unauthorized `workspace:*`/`repo:*`-breadth claim | `RallyError::Usage` — `crates/rally-cli/src/store.rs:5455-5475` (`claim_authority::breadth_violation`, `claim_authority::detect_conflict`) |
| 3 | Not-found lookup | `RallyError::NotFound` |
| 4 | `NotStarted` (rejected before any durable side effect; safe to retry) | `RallyError::NotStarted` |
| 4 | `rally check <phase> --strict` with a stop finding; `rally check coordination --strict` when not passing; `rally check-ci --strict` with offenders; `rally routes --probe <actor>` when the probed route never reaches `ready` | `crates/rally-cli/src/check.rs:91`; `crates/rally-cli/src/lib.rs:7340-7341` (coordination); `crates/rally-cli/src/check_ci.rs` (`build_check_ci`); `crates/rally-cli/src/runtime_routes.rs:450` |
| 4 | `rally setup --component <c> --apply` when the plan is `blocked`, requires permission that was not granted, or the apply attempt itself fails | `crates/rally-cli/src/runtime_setup.rs:514-655` (`render`, `command`) |
| variable | `rally run codex --task ...` (the internal `TaskWorker` path): exits with the bounded child process's own exit code, clamped to `u8` | `crates/rally-cli/src/lib.rs:8409-8448` |

Not every `with_exit_code` call site could be tied to a documented command from static reading
alone; the table above covers every site found under `crates/rally-cli/src/{lib,check,check_ci,
runtime_routes,runtime_setup}.rs`. `session ensure --strict` is a capability *attestation* flag
(it asserts the host invokes Rally's native before-write transaction) and was not found to change
`session ensure`'s own exit code.

## The outer `.rally/log/<engagement>.jsonl` wrapper

Each line of a segment file is a `LedgerLine` (`crates/rally-cli/src/store.rs:3570-3578`), not the
raw fact:

```json
{"seq": 42, "occurred_at": "2026-09-24T18:03:11Z", "event_type": "claim", "payload": { /* the full Fact */ }, "engagement": "main"}
```

| Field | Source |
|-------|--------|
| `seq` | Canonical monotonic sequence number: max existing seq + 1 at append (`crates/rally-cli/src/store.rs` append path) |
| `occurred_at` | `now_string()` (`crates/rally-cli/src/lib.rs:20308`) — RFC 3339, UTC, second precision, `Z` suffix, e.g. `2026-09-24T18:03:11Z` |
| `event_type` | `fact.kind.as_str()` — the fact's `FactKind` as a string (`"claim"`, `"say"`, `"artifact"`, …) |
| `payload` | The fact itself, serialized (`serde_json::to_value(&fact)`) — see [`docs/schemas/agent-rally.fact.v1.json`](schemas/agent-rally.fact.v1.json) |
| `engagement` | The per-row engagement tag; omitted (not `null`) on older pre-R5 migrated lines that carry no tag |

Replaying these lines in append order rebuilds `facts.db` verbatim — this is what
`rebuild_db_from_segments` (`crates/rally-cli/src/store.rs:11289`) does, and why `.rally/facts.db`
is a derived cache rather than a canonical store.

## Per-command field map

| Command | `data[command]` fields | Siblings in `data` |
|---------|------------------------|-------------------|
| `init` | `init: { repo_root, manifest, pointers, docs, ledger_dir, room_cmd }` | — |
| `enter` | `enter: { tool, session_id, room_id, cursor, entry, attention, attention_total, attention_emitted, attention_omitted, warnings?, mission? }` | `room` |
| `say` | `say: { fact }` | `room`, `verified`, `warnings?` |
| `room` | `room: RoomSnapshot` | `query`, `readers?`, `mission?` |
| `next` | `next: NextResult` | `tool`, `role`, `paths`, `wake_intent?`, `room` |
| `check` | `check: { phase, tool, path?, allow, mode, findings, agent_visible }` | — |
| `watchdog` | transport exception: `{ watchdog_timeout: true, reason, elapsed_ms }` | — |
| `locate` | `locate: { event_id, located?, warnings }` | — |
| `recent` | `recent: { all, limit, rows, warnings }` | — |
| `retrospective` | `retrospective: { output_path, action, engagements, total_facts, total_engagements }` | — |
| `rotate` | `rotate: { threshold_days, threshold_source, cutoff_utc, dry_run, rotated, skipped, … }` | — |
| `status` | `status: { repos, warnings }` | — |
| `migrate-legacy` | `migrate-legacy: { slugs_found, facts_read, facts_migrated, facts_skipped_existing, warnings }` | — |
| `doctor` | `doctor:` mode-dependent — `--canonical-paths` `{ non_canonical, suffix_collisions, warnings }` · `--prune-rooms` `{ live, stale, applied, warnings }` · `--reap-stale` `{ claims_reaped, lead_relinquished, applied }` · `--sweep-corrupt` `{ rally_dir, kept, swept, bytes_reclaimable, applied, keep, max_age_days, warnings }` · `--compact-log` `{ log_file, total_lines, presence_lines, presence_runs, lines_saved, unparseable_lines, entries, warnings }` | — |
| `version` | `version: { version, build_id }` | — |
| `whoami` | `whoami: { tool?, repo_root, repo_id, room_id, worktree, build_id, cwd }` | `repo_id` is stable repo identity; `room_id` is the active engagement label |
| `sessions` | `sessions: { sessions: [...] }` | — |
| `session` ensure | `session: { action: "ensure", lease: { raw_session_id, session_id, endpoint_id, tool, adapter, reused, capabilities }, environment, shell_export, daemon }` | — |
| `session` close | `session: { action: "close", tool, session_id, released_claim_ids, close_fact }` | — |
| `session` current | `session: { action: "current", sessions, total, emitted, omitted, fresh, stale, unknown, window_secs, history_command }` | — |
| `session` history | `session: { action: "history", transitions, total, emitted, omitted, limit }` | — |
| `run` | `run: { mode, session, commands }` | — |
| `inject` | `inject: { mode, session, target_kind, handoff, require_ack, ack, verified_received, ack_state, fallback_plan, wake_intent, commands, sender_tool, content_fact, delivered, delivery_state, directive_seq, directive_to, delivery_path, daemon_receipt_state?, daemon_delivery_error?, delivery_reason, delivery_detail, reached_target, queued, target_injectability? }` | — |
| `attach` | `attach: { mode, action, session, output?, commands }` | — |
| `capture` | `capture: { mode, action, session, output?, commands }` | — |
| `stop` | `stop: { mode, action, session, output?, commands }` | — |
| `backlog` | `backlog: { action, items, added? }` | — |
| `board` | `board: { lanes, backlog, delta }` | — |
| `route-findings` | `route-findings: { findings_total, routed, unowned, routed_findings }` | — |
| `check-ci` | `check-ci: { pass, mode, receipt_threshold_secs, offenders }` | — |
| `dag` | `dag: { run_id, nodes, edges, facts_scanned }` | — |
| `wake-due` | `wake-due: { due: [...] }` | — |
| `mission` (GET) | `mission: { text?, set_by?, set_at?, envelopes }` | — |
| `mission` (SET) | `mission: { action, fact }` | — |

**Schema coverage: 16 of 48 top-level commands have a schema file under `docs/schemas/`.** The
count is derived from the `CliCommand` enum (`crates/rally-cli/src/cli.rs:10-85`, 48 variants) and
the 17 `docs/schemas/agent-rally.command.*.v1.json` files. `adopt`, `check`, `dag`, `enter`,
`inbox`, `inject`, `locate`, `next`, `recent`, `room`, `run`, `say`, `session` (the
`SessionLifecycle` variant), `session-action` (the `Session` variant, covering `attach`/`capture`/
`stop`), `sessions`, and `wake-due` have a schema file. The 17th file, `watchdog`, documents the
transport-level fail-open exception above rather than a distinct top-level command. The remaining
32 commands have no schema file; their shapes are documented only in the field map above or not
at all.

**`inject.ack` shapes.** The `ack` key is always present. Its value is an object when an ACK wait runs and `null` otherwise. `--require-ack` requests the wait explicitly; `--handoff` and `--ref` request it implicitly. Dry-run mode never waits.

| Scenario | `ack` value |
|----------|-------------|
| Resolve, receipt, or artifact arrived | `{ "received": true, "resolved": true, "handoff_closed": true, "blocked": false, "decision": false, "event_id": "...", "tool": "...", "expected_tool": "...", "kind": "...", "subject": "..." }` |
| Blocker arrived | `{ "received": true, "resolved": false, "handoff_closed": false, "blocked": true, "decision": false, "event_id": "...", "tool": "...", "expected_tool": "...", "kind": "blocker", "subject": "..." }` |
| Decision arrived | `{ "received": true, "resolved": false, "handoff_closed": false, "blocked": false, "decision": true, "event_id": "...", "tool": "...", "expected_tool": "...", "kind": "decision", "subject": "..." }` |
| Timed out before target evidence | `{ "received": false, "resolved": false, "assume_received": false, "timed_out": true, "waited_seconds": N, "after_seq": N, "expected_tool": "...", "ignored_resolves": N, "ignored_target_responses": N, "fallback_plan": { ... } }` |

**`ok` reports command execution, not persistence or delivery.** `ok: true` / exit 0 means Rally produced a valid command result. A dry run writes nothing, and a ledger-write failure is represented inside a successful envelope. For inject, only `reached_target: true` proves arrival and only `queued: true` proves that a durable queued copy remains; use `delivery_reason` and `delivery_detail` when either field is false.

After any required ACK wait, branch on the final fields in this order:

| Field | Question it answers |
|-------|--------------------|
| `reached_target` | Did the message actually arrive? Only `true` means yes. |
| `queued` | Did Rally confirm a durable queued copy? Only `true` proves one remains. |
| `delivery_reason` | Why — typed. See the enum in [the inject v1 schema](schemas/agent-rally.command.inject.v1.json). |
| `delivery_detail` | What to do about it, in one sentence. |

`delivered` and `delivery_state` are attempt-time compatibility fields. `delivery_state` uses `pending`, `delivered`, `seen`, `acted`, `failed`, or `sent_unverified`. A later target ACK can therefore make `reached_target: true` and `queued: false` while those attempt-time fields remain unchanged.

Queued inject outcomes include `sent_unverified`, `queued_awaiting_receipt`, `queued_no_managed_session`, `policy_rejected_urgent_addition`, `failed_backend_inject`, and `failed_daemon_send`. `sent_unverified` means a pane write occurred without verified receipt, so the durable directive remains queued. `policy_rejected_urgent_addition` means SEC-009 intentionally skipped synchronous transport while leaving the durable directive queued; do not re-inject it.

`queued: false` means Rally did not confirm a durable queued copy; it does not prove that no bytes were written. In particular, `failed_ledger_write` can represent a failure while syncing data that was already appended. Treat that result as ambiguous: inspect the existing directive and target evidence, and do not re-inject unless absence is independently established.

An ACK timeout is also **`ok: true` / exit 0**: optional target evidence did not arrive in the window. Check `ack.received` for acknowledgement and `ack.resolved`, `ack.blocked`, or `ack.decision` for its outcome. Then apply the final truth fields: if `reached_target` is `true`, the message arrived; if `queued` is `true`, inspect the existing durable directive or runner and do not re-inject; if both are `false`, follow `delivery_reason` and `delivery_detail` without inferring that a retry is safe from the booleans alone.

**`doctor --compact-log` `entries[]` shapes** (internally tagged by `entry`):

| `entry` | Fields |
|---------|--------|
| `presence_run` | `{ first_seq, last_seq, first_at, last_at, count, tools: { <tool>: <heartbeats> } }` — 2+ consecutive presence/heartbeat lines collapsed into one summarized entry |
| `event` | `{ seq, occurred_at, event_type, tool?, subject?, payload? }` — any other line passed through; `payload` carries the full fact payload unchanged |

## Notes

- `room` output: `data.room` is the full `RoomSnapshot`. The command name and the sibling key share the same name `room` — `data["room"]` is unambiguous because `command` field says `"room"`.
- `next` output: `data.next` is the `NextResult`. Other fields (`tool`, `role`, `paths`, `wake_intent`, `room`) are siblings in `data`.
- `say` output: `data.say.fact` holds the written `Fact`. `data.room` and `data.verified` are shared contextual payloads.
- `enter` output: `data.enter` holds the enter result (tool, cursor, entry, bounded attention, total/emitted/omitted counts, warnings, mission). `data.room` is the room summary sibling.
- Session actions (`attach`, `capture`, `stop`) share the schema `agent-rally.command.session-action.v1` but each nests under its own action name.
- New managed-session records carry `adapter_schema`, `adapter_id`, `adapter_delivery_grade`, and `adapter_operations`. These describe the selected control adapter's declared transport capabilities; they never prove receiver acknowledgement or per-session operation execution. Legacy records may omit all four fields and remain readable.
- Session lease lifecycle and views (`session ensure|close|current|history`) use `agent-rally.command.session.v1`. `ensure.environment` exports the one-time `RALLY_SESSION_CLOSE_TOKEN` required by `close`. When `--resource` is present, `ensure.admission` reports `state: granted`, canonical exclusive `resources`, and the exact `claim_id`; a conflict returns an error before presence is appended. Capability fields are independent three-level guarantees: `enforced`, `advisory`, or `unmanaged`; do not infer host write blocking from identity or visibility. Current/history rows are separately bounded and always carry exact omission counts; `current.window_secs` is the effective adaptive freshness window.
- `run --resource` adds `data.run.admission`. A dry run reports `state: planned` without a `claim_id`; a real run reports `state: granted` and records the same resources and claim id on `data.run.session`. The child process starts only after the grant.
- `hook` output: `hook capabilities --json` is a standard envelope with `data.hook` holding the
  contract version, supported phases, effect registry, target ceiling, and the host-specific
  coordination contract. **`hook <phase>` is
  the one deliberate exception in this document**: its stdout is the HOST's envelope (Claude's
  `hookSpecificOutput`, Codex's `systemMessage`, and so on), not rally's, because the host
  parses it directly. It carries no `ok`/`data` and `--json` on it is accepted and ignored.
- The contract test `tests/json_envelope_contract.rs` drives off the `COMMANDS` list and asserts `data[command]` exists for every subcommand. In practice it is a hand-enumerated list rather than a loop over `COMMANDS` (which is `pub(crate)` and not visible to an integration test), so a new command needs its own `envelope_<cmd>` test added — it will not be covered automatically.
