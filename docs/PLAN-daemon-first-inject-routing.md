<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# PLAN — Daemon-first inject routing (move 2)

> **Status:** CLI SIDE IMPLEMENTED (2026-06-09). The rally-CLI registration +
> routing + labeling is shipped and CI-tested against a test-double daemon
> (`crates/rally-cli/tests/daemon_inject_routing.rs`). The real ptyd daemon
> side already exists (`agent.register` verb + Receipt loop; shared
> `rally-protocol`), but the LIVE FLIP is gated — see §7. Follow-up to the
> 2026-06-09 inject submit-semantics fix.
> **Target line:** `main`. **Run via:** `/build-loop:run` when scheduled.

## Implementation status (CLI side, 2026-06-09)

| Criterion | Status |
|---|---|
| 1. Daemon-routed inject writes ledger-only, ZERO send-keys, `delivery_path:"daemon"` | DONE — `daemon_registered_session_injects_ledger_only_zero_send_keys` (tmux-bin spy) |
| 2. ACK resolves on the daemon Receipt | UNCHANGED — `inject`'s ACK wait already accepts `Receipt` |
| 3. No-daemon → framed tmux fallback, `delivery_path:"tmux_framed_fallback"` | DONE — `no_daemon_session_falls_back_to_framed_tmux` |
| 4. `rally run`/`adopt` register with daemon; `rally sessions` shows the binding | DONE — `ManagedSession.daemon_registered`/`daemon_pane`; `try_register_session_with_daemon` |
| 5. SEC gate on the `authorize()` capability matrix before the PTY-write swap | See §7 — CLI does not flip the swap; gate applies to the live-flip step |

What shipped on the CLI: `daemon_client.rs` (fail-open `agent.register` client +
unambiguous-socket resolution), `ManagedSession{daemon_registered,daemon_pane}`,
`try_register_session_with_daemon` wired into `rally run` + `rally adopt`,
`command_inject_managed` skips the legacy tmux write for a daemon-registered
session and labels `delivery_path`, and `InjectData.delivery_path`.

## 7. Remaining step — the live daemon flip

The CLI is fail-open: until the rally-termd daemon actually OWNS the pane that
`rally run` launches, `agent.register` returns `pane_not_found` and the session
stays on the framed-tmux fallback (zero behavior change). Flipping the live
daemon path on requires, in order:

1. ptyd owns the launched pane (ptyd-managed pane, not a bare tmux/cmux pane),
   so `agent.register(pane, identity)` resolves a real pane handle.
2. The daemon `authorize(sender→target→verb)` capability matrix is re-reviewed
   (criterion 5) — a capability that turns ledger input into keystrokes in
   ANOTHER agent's pane is a major change behind a hard security gate. This was
   reviewed for the CLI change (no new PTY-write authority added on the CLI
   side); the live flip needs the daemon-side re-review before enabling.

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
