<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# PLAN — Daemon-first inject routing (move 2)

> **Status:** SPEC ONLY — no implementation in this plan. Follow-up to the
> 2026-06-09 inject submit-semantics fix (`backends.rs` framed-write + `lib.rs`
> inject watchdog budget). That fix repaired the **no-daemon tmux fallback**;
> this plan describes the **daemon-first** path it falls back *from*.
> **Target line:** `main`. **Run via:** `/build-loop:run` when scheduled.

---

## 1. Why

`rally inject` today has two delivery realities, and the reliable one is the
exception, not the rule:

- **No-daemon path (today's default):** `inject` writes a typed Directive to the
  `.rally` ledger AND synchronously puppets the target's TUI via tmux/cmux
  keystrokes. The keystroke write is fragile by nature (L7/SEC-017): it depends
  on pane-id resolution, bracketed-paste support, and submit semantics that
  differ per host TUI. The 2026-06-09 fix made that write *atomic and
  submitting* (ptyd `frame_line` port), but it is still keystroke puppeting.
- **Daemon path (the north star):** when `rally-termd` (terminal-rally-point) is
  reachable, the agent is **registered with the daemon**, the Directive lands in
  the daemon's inbox, the daemon performs the PTY-write against the pane it owns,
  and it posts a **Receipt** back to the ledger. The CLI never touches the TUI.

The architecture rule from L7/L8 (SEC-016/017) is already decided: **the ledger
is coordination truth + receipts; PTY keystroke injection is capability-gated,
emergency-only.** This plan operationalizes that for `inject` specifically:
daemon-routed delivery is the DEFAULT when a daemon is reachable; the framed
tmux write becomes the explicit **fallback** for the no-daemon case.

## 2. Target routing (when daemon reachable)

```
rally inject <target> --handoff <id>
        │
        ▼
  ledger: append typed Directive (Deliver + Addition)   ← already happens today
        │
        ▼
  rally-termd subscribes (kernel file-event) ──► authorize(sender→target→verb)
        │                                              (SEC-017 capability matrix)
        ▼
  daemon performs PTY-write into the pane IT owns  (no CLI keystroke puppeting)
        │
        ▼
  daemon posts Receipt(seq) ──► ledger   ──►  inject's ACK wait resolves on the
                                              target-authored Receipt/resolve fact
```

Key property: `inject`'s existing `wait_for_resolution` ACK poll (now correctly
budgeted by the 2026-06-09 watchdog fix) already accepts a `Receipt` fact as an
ACK (`store::FactKind::Receipt` is in its accept set). So the CLI side needs
**no ACK-wait change** — it already resolves on a daemon-posted Receipt.

## 3. What `rally run` must register

For daemon routing to be possible, `rally run` (and `rally adopt`) must register
the launched/adopted session WITH the daemon, not only record a `ManagedSession`
locally:

- On `rally run --name X codex`: after the backend session starts, register the
  pane identity with `rally-termd` (`agent.register`), binding the logical
  agent-id (`codex`, `claude_code:01`) to the daemon-owned pane.
- The `ManagedSession` record gains a daemon-binding marker (e.g.
  `daemon_registered: true` + the daemon's pane handle) so `resolve_inject_target`
  can branch: **daemon-registered → ledger-only (daemon delivers)**;
  **not registered → managed-session dual-delivery (framed tmux fallback)**.
- `InjectTarget::LedgerAgent` (already present) is the daemon-routed arm; this
  plan makes `rally run` *populate* it for managed sessions, instead of it being
  reachable only for externally-registered ptyd panes.

## 4. Fallback contract (no daemon)

When no daemon is reachable (daemon down, not installed, or session not
registered), `inject` keeps today's behavior: ledger Directive + the **framed
tmux write** (the 2026-06-09 atomic `send-keys -H` bracketed-paste frame + CR).
This is the degraded-but-correct path. The envelope must label which path ran:

- `delivery_path: "daemon"` — Directive written, daemon owns the PTY-write.
- `delivery_path: "tmux_framed_fallback"` — CLI performed the framed write.

(cmux stays the documented separate-submit fallback — no raw-byte send.)

## 5. Acceptance criteria (for the future build, NOT this plan)

1. With `rally-termd` reachable and the target registered, `inject --handoff`
   writes ONLY the ledger Directive — no tmux/cmux keystrokes fire (assert via a
   `--tmux-bin` spy that records zero `send-keys`).
2. The daemon posts a Receipt; `inject`'s ACK wait resolves `ack_state: "acked"`
   on it with no manual keystroke.
3. With no daemon, `inject` falls back to the framed tmux write and reports
   `delivery_path: "tmux_framed_fallback"`.
4. `rally run`/`rally adopt` register the session with the daemon; a `rally
   sessions` view shows the daemon binding.
5. SEC gate: the daemon's `authorize(sender→target→verb)` capability matrix is
   re-reviewed before the PTY-write swap is flipped live (L8 — a capability that
   turns ledger input into keystrokes is a major change behind a hard security
   gate).

## 6. Out of scope here

Implementation, the daemon protocol wire format, and the `rally-termd` side. This
document exists so the no-daemon framed-write fix is understood as the *fallback
tier* of a two-tier design, and so the daemon-first work has a written target.
