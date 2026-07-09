# Inject Timeout RCA - 2026-07-09

## Summary

`rally inject --require-ack --timeout-seconds 60` was used against Codex ids that
were visible in the room but were not active managed sessions. The command wrote
ledger wake/directive facts and then waited for target-authored ACKs. With two
targets, each allowed to wait 60 seconds, the caller's outer 2 minute shell
timeout killed the loop before the second result could be printed.

## Root Cause

Rally exposed presence and managed-session state on separate surfaces:

- `rally room` showed the target agents as visible/present.
- `rally sessions` was empty, so there was no tmux/cmux/daemon-backed pane for
  live injection.
- `rally inject` correctly degraded to ledger-only delivery for valid agent ids,
  but callers had to infer non-live delivery from `target_kind=ledger_agent`,
  `delivery_path=ledger_only`, `delivered=false`, and `fallback_plan`.

That made a presence-only agent look operationally similar to an injectable
managed session. The missing product signal was a per-agent "can Rally inject
into this live pane right now?" status.

## Contributing Factors

- `--handoff` implies ACK waiting even when `--require-ack` is also passed.
- The caller ran two sequential 60 second waits under a 2 minute outer command
  timeout.
- The target ids were valid agent ids, so Rally did not reject them; it queued
  ledger-only wake work instead.
- The room already had `unmanaged-agent` telemetry, but that was not a compact
  per-agent status field.

## Fix

Rally now surfaces injection readiness directly:

- `rally sessions --json` includes `injectable`, `inject_status`, and
  `inject_via` for each managed session.
- `rally room --json` includes `agent_injectability[]` for every visible squad
  agent, including presence-only agents. Presence-only agents report
  `injectable=false` with a reason pointing to `rally run` or `rally adopt`.

This makes the pre-inject check deterministic: if the row is not injectable,
callers should not expect synchronous pane delivery or a quick ACK.

## Operator Guidance

Before urgent or ACK-required injection:

1. Run `rally room --json` and check `data.agent_injectability[]`.
2. If `injectable=false`, use `rally run <agent>` or
   `rally adopt <tool> --tmux <target>` / `--cmux <target>`.
3. If a ledger-only wake is acceptable, do not wrap multiple
   `--timeout-seconds 60` injections in a 2 minute outer timeout without
   accounting for worst-case wait time.
