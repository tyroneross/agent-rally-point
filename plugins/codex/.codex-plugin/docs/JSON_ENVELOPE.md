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

## How to parse safely

```python
import json, subprocess
out = subprocess.check_output(["rally", "<cmd>", "--json"])
envelope = json.loads(out)
if envelope.get("data", {}).get("watchdog_timeout"):
    raise RuntimeError(envelope["data"]["reason"])
result = envelope["data"][envelope["command"]]  # always works
```

```bash
rally <cmd> --json | python3 -c "
import json, sys
d = json.load(sys.stdin)
if d.get('data', {}).get('watchdog_timeout'):
    raise SystemExit(d['data']['reason'])
print(d['data'][d['command']])
"
```

## Per-command field map

| Command | `data[command]` fields | Siblings in `data` |
|---------|------------------------|-------------------|
| `init` | `init: { repo_root, manifest, pointers, docs, ledger_dir, room_cmd }` | — |
| `enter` | `enter: { tool, session_id, room_id, cursor, entry, attention, warnings?, mission? }` | `room` |
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
| `run` | `run: { mode, session, commands }` | — |
| `inject` | `inject: { mode, session, handoff?, require_ack, ack?, wake_intent?, commands, sender_tool, content_fact?, delivered, delivery_state, delivery_reason, delivery_detail, reached_target, queued }` | — |
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

**`inject.ack` shapes.** The `ack` key is always present. Its value is an object when an ACK wait runs and `null` otherwise. `--require-ack` requests the wait explicitly; `--handoff` and `--ref` request it implicitly. Dry-run mode never waits.

| Scenario | `ack` value |
|----------|-------------|
| Resolve, receipt, or artifact arrived | `{ "received": true, "resolved": true, "handoff_closed": true, "blocked": false, "decision": false, "event_id": "...", "tool": "...", "expected_tool": "...", "kind": "...", "subject": "..." }` |
| Blocker arrived | `{ "received": true, "resolved": false, "handoff_closed": false, "blocked": true, "decision": false, "event_id": "...", "tool": "...", "expected_tool": "...", "kind": "blocker", "subject": "..." }` |
| Decision arrived | `{ "received": true, "resolved": false, "handoff_closed": false, "blocked": false, "decision": true, "event_id": "...", "tool": "...", "expected_tool": "...", "kind": "decision", "subject": "..." }` |
| Timed out before target evidence | `{ "received": false, "resolved": false, "assume_received": false, "timed_out": true, "waited_seconds": N, "after_seq": N, "expected_tool": "...", "ignored_resolves": N, "ignored_target_responses": N, "fallback_plan": { ... } }` |

**`ok` reports command execution, not persistence or delivery.** `ok: true` / exit 0 means Rally produced a valid command result. A dry run writes nothing, and a ledger-write failure is represented inside a successful envelope. For inject, use `reached_target` to determine arrival and `queued` to determine whether a durable copy remains.

After any required ACK wait, branch on the final fields in this order:

| Field | Question it answers |
|-------|--------------------|
| `reached_target` | Did the message actually arrive? Only `true` means yes. |
| `queued` | If not, is it still reachable later? `true` means a durable copy remains queued. |
| `delivery_reason` | Why — typed. See the enum in `docs/schemas/agent-rally.command.inject.v1.json`. |
| `delivery_detail` | What to do about it, in one sentence. |

`delivered` and `delivery_state` are attempt-time compatibility fields. `delivery_state` uses `pending`, `delivered`, `seen`, `acted`, `failed`, or `sent_unverified`. A later target ACK can therefore make `reached_target: true` and `queued: false` while those attempt-time fields remain unchanged.

Queued outcomes include `sent_unverified`, `queued_awaiting_receipt`, `queued_no_managed_session`, `queued_awaiting_poll`, `policy_rejected_urgent_addition`, `failed_backend_inject`, and `failed_daemon_send`. `sent_unverified` means a pane write occurred without verified receipt, so the durable directive remains queued. `policy_rejected_urgent_addition` means SEC-009 intentionally skipped synchronous transport; resend without `--urgent` if delivery is intended.

An ACK timeout is also **`ok: true` / exit 0**: the message is in the channel and only the optional target evidence did not arrive in the window. Check `ack.received` for acknowledgement and `ack.resolved`, `ack.blocked`, or `ack.decision` for its outcome. **Do not re-inject after an ACK timeout** — the message is already in the channel.

**`doctor --compact-log` `entries[]` shapes** (internally tagged by `entry`):

| `entry` | Fields |
|---------|--------|
| `presence_run` | `{ first_seq, last_seq, first_at, last_at, count, tools: { <tool>: <heartbeats> } }` — 2+ consecutive presence/heartbeat lines collapsed into one summarized entry |
| `event` | `{ seq, occurred_at, event_type, tool?, subject?, payload? }` — any other line passed through; `payload` carries the full fact payload unchanged |

## Notes

- `room` output: `data.room` is the full `RoomSnapshot`. The command name and the sibling key share the same name `room` — `data["room"]` is unambiguous because `command` field says `"room"`.
- `next` output: `data.next` is the `NextResult`. Other fields (`tool`, `role`, `paths`, `wake_intent`, `room`) are siblings in `data`.
- `say` output: `data.say.fact` holds the written `Fact`. `data.room` and `data.verified` are shared contextual payloads.
- `enter` output: `data.enter` holds the enter result (tool, cursor, entry, attention, warnings, mission). `data.room` is the room summary sibling.
- Session actions (`attach`, `capture`, `stop`) share the schema `agent-rally.command.session-action.v1` but each nests under its own action name.
- `hook` output: `hook capabilities --json` is a standard envelope with `data.hook` holding the
  contract version, supported phases, effect registry, and target ceiling. **`hook <phase>` is
  the one deliberate exception in this document**: its stdout is the HOST's envelope (Claude's
  `hookSpecificOutput`, Codex's `systemMessage`, and so on), not rally's, because the host
  parses it directly. It carries no `ok`/`data` and `--json` on it is accepted and ignored.
- The contract test `tests/json_envelope_contract.rs` drives off the `COMMANDS` list and asserts `data[command]` exists for every subcommand. In practice it is a hand-enumerated list rather than a loop over `COMMANDS` (which is `pub(crate)` and not visible to an integration test), so a new command needs its own `envelope_<cmd>` test added — it will not be covered automatically.
