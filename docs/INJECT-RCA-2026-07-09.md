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

## Fix, part 2 — inject consumes the signal (WARN-and-wait)

The part-1 fields were display-only: a caller who did not voluntarily
pre-check `room --json` hit the identical full-timeout hang. `rally inject`
now consumes its own signal on the `ledger_agent` arm:

- **At wait start (t=0):** a stderr warning names the condition
  ("presence-only; no active managed session — waiting anyway") and the
  envelope carries a `target_injectability` object (same status vocabulary as
  `agent_injectability[]`).
- **On timeout:** the `fallback_plan` is stamped with `pre_diagnosis`, so the
  report carries the cause known at t=0, not just the symptom.
- **The wait is deliberately NOT short-circuited.** "Not injectable" means no
  *synchronous* ACK producer — a rally-termd-registered pane still delivers
  (and posts a Receipt that `wait_for_resolution` accepts), and a
  presence-only agent can post a Resolve when it next polls `rally next`.
  Fast-fail would discard those winnable ACKs and break ledger-only handoffs
  to polling agents; fast-return would assert a falsehood ("no producer").
  Advisory surfacing + the kept wait preserves the WARN-not-block posture
  (NORTH_STAR invariants 3–4).

The managed arm needs no diagnosis: stale/gone sessions are rejected loudly at
`resolve_inject_target`, so sessions reaching that arm are Live/Unknown and
their delivery truth is already synchronous (`delivered`/`delivery_state`).

## Operator Guidance

Before urgent or ACK-required injection:

1. Run `rally room --json` and check `data.agent_injectability[]`.
2. If `injectable=false`, use `rally run <agent>` or
   `rally adopt <tool> --tmux <target>` / `--cmux <target>`.
3. If a ledger-only wake is acceptable, do not wrap multiple
   `--timeout-seconds 60` injections in a 2 minute outer timeout without
   accounting for worst-case wait time.
