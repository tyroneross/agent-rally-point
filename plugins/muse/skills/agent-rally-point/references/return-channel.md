## Return-Channel Contract

Every task or handoff must say how the result gets back to its intended
recipient. Record five fields before dispatch: **recipient**, **primary
channel**, **backup channel**, **ACK deadline**, and **fallback action**. Include
the result date/time zone, original context, current status, and evidence or
verification gaps in the returned payload.

**Handoffs that need action: inject first, watch for the reply, escalate on
silence.** `rally say handoff --target <tool>` records the handoff and, by
default (`--deliver inject`), injects it into the target's pane when the target
is a live `rally run`-managed session. Read `data.delivery.status`: `injected`
means the pane write was attempted (not proof of receipt); `queued_pane_inbox`
means the target has no managed session but its one bound Easy Terminal pane
got a wake queued in its inbox (typed when idle, handed over by hook when
busy; you must be lead or invited by the target); `coalesced_pane_wake` means
an earlier wake to it is still pending and covers this handoff. Neither is
proof of receipt: the handoff closes when the target records an artifact or
receipt against it. `record_only` names
why it could not inject (unmanaged/stale target, or inject authorization
refused) — the receiver must then pull it, so use the backup channel now. Add
`--ack-within 10m` for any handoff you are waiting on. Keep polling
`rally next --tool "$TOOL" --json`; once the deadline passes with no receiver
ack, `next` writes ONE durable `risk` fact (`no-response: <tool> did not ack
handoff <id>`) and lists the handoff under `data.overdue_handoffs`. Then run the
declared backup once (re-inject with `rally inject <tool> --handoff <id>`, post a
backup locator, or reroute/tell the user). The entry clears only when the
receiver answers through the inbox's `ack_command` (typed acceptance keeps work open). `--deliver record`
opts out of the inject.

Use two delivery paths with different failure modes:

- A `rally run`-managed session may use `rally inject --handoff <event-id>` as
  the primary notification. The referenced targeted Rally handoff is the
  durable backup and system of record.
- An unmanaged agent uses a targeted Rally handoff as primary. Use a configured
  watcher/native notification, a room-visible locator, or the human-facing
  task reply as backup; do not pretend `rally inject` can reach it.
- A file, dashboard, issue, review, or external document may be the primary
  result surface. Post its absolute path or stable URL in a targeted Rally
  handoff as the backup locator.

A successful send or inject proves only that the sender or transport accepted
the message. Only a target-authored ACK proves receipt. If the ACK deadline
passes, execute the declared backup once and reference the original handoff;
do not create a second task or repeatedly inject the same instruction. If the
primary channel is unavailable at dispatch time, use the backup immediately
and state why.

## Observe Replies Efficiently

Ordinary `inject --handoff` returns pending; use `--wait-ack` (or compatible
`--require-ack`) only for an explicit bounded wait. Poll several IDs with one
`rally replies --ref <id> --ref <id> --json` instead of repeated request/reply
locates. Rows contain the original, typed state and counted reply from one
current-room read, honoring withdrawals. Use `locate` when an ID is missing in
this room; never treat a capped `recent` result as canonical ACK evidence.
`done` alone is not verified work; inspect `completion_status` and evidence.
Recover accepted assignments through `room.open_handoffs`, since acceptance
removes their inbox obligation while their work remains open.

Inspect status-post results: NotStarted proves no mutation began; unknown
outcomes require locating the event before retrying. Never silently drop a
refused post or blindly resend an ambiguous mutation. Interactive status posts
have a bounded ownership wait; explicit hook budgets remain authoritative.
