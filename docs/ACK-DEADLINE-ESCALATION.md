# Handoff delivery and no-response escalation

Status: implemented 2026-09-21 (branch `handoff-escalation-20260921`).

## Problem

A targeted `rally say handoff` asking a peer to commit files was recorded but
never noticed in rally; the peer acted silently later and the sender had no
signal. Recording is pull-only: nothing pushed the handoff to the peer and
nothing turned silence into an escalation.

## What already existed (reused, not duplicated)

- `rally inject <target> --handoff <id> [--require-ack --timeout-seconds N]` —
  pane delivery for `rally run`-managed sessions with sender frames, lead/consent
  authorization (`inject_authority_decision`), stale/renumbered-session refusal
  (`resolve_inject_target`), and honest grades (`sent_unverified` vs acked).
  `--require-ack` waits synchronously at most 600s; no durable deadline.
- Obligations/inbox: open targeted handoffs stay in the RECEIVER's
  `rally inbox`/`next` until the receiver acks. Nothing on the SENDER side.
- Reaper: expires unanswered handoffs after days (cleanup, not escalation).
- `rally watch` / `tools/agent-rally-watcher`: detect-and-notify for unmanaged
  agents; no ack deadline.
- Return-channel contract (docs + skill) told agents to declare an ACK deadline
  and backup, but rally did not track or enforce it.

## What was added

- `rally say handoff --deliver inject|record` (default `inject`). After the
  durable commit, a targeted handoff is injected through the existing
  `command_inject_inner` path (same safety gates) when the target resolves to a
  live managed session. Otherwise it degrades to `record_only` with a reason.
  Result in `data.delivery {mode,status,detail,ack_by}`. The `say handoff`
  watchdog gets inject's default budget unless `--deliver record`.
- `--ack-within 90s|10m|2h` stores an `ack-by:<iso>` evidence marker.
- Sender's `rally next`: open handoffs it authored past `ack-by` are listed in
  `data.overdue_handoffs`; one `risk` fact (`no-response: ...`, `ref` = handoff)
  is written per handoff (deduped via `current_risks`). `next --audit` reports
  without writing. Receiver ack closes the handoff, which clears the entry.

## Limits

- Escalation is evaluated when the sender runs `rally next` (no daemon timer).
- The risk fact is a durable log; it is not auto-resolved on ack.
- Ledger-only (termd-registered, non-managed) targets are not pane-injected.
