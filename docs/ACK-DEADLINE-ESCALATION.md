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
  live managed session. A target with no managed session can still be woken
  through Easy Terminal's pane inbox (`inbox.enqueue`, ref and client_id = the
  handoff id) when every gate passes: the sender may direct the target (lead,
  target consent, or self, the same rule `rally inject` applies); exactly one
  live ptyd pane is bound to the identity (`PTYD_SOCKET_PATH`, else
  `RALLY_PTYD_SOCKET`, pins the socket; otherwise two bindings are
  ambiguous); the pane reports a working directory inside this room's
  repository; and ET's router does not own the identity. ptyd types the
  Rally-rendered prompt when the pane is idle, or a provider hook hands it
  over when busy. That reports `queued_pane_inbox` with `pane_message_id` and
  a `wake_event_id` (wake fact, reason `queued_awaiting_receipt`). The prompt
  is addressed to the resolved pane id, so ptyd cannot file it in another
  pane. A later handoff to the same target reports `coalesced_pane_wake`
  naming the earlier wake (whose `rally next` lists every open handoff) only
  when that wake is under 10 minutes old, targets the same pane, came from a
  sender with at least the new sender's authority (lead > consent/assignment
  > self/bootstrap; a lead is never coalesced under a peer), and ptyd's
  `inbox.show` still reports its message `queued`. Known limit: an
  `inbox.enqueue` that times out (3 s) reports `record_only` even if ptyd did
  queue the prompt. None of these is an ack: the prompt asks the target to
  record an artifact against the handoff (a receipt also closes it), and the
  handoff stays open until then. Any failed gate or enqueue degrades to
  `record_only` with the reason.
  Result in `data.delivery {mode,status,detail,ack_by,pane_message_id?,wake_event_id?}`. The `say handoff`
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
