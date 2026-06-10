<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# PLAN — Daemon-first inject routing (move 2)

> **Status:** LIVE FLIP IMPLEMENTED (2026-06-10). `rally run --backend ptyd`
> (and `--backend auto` when the rally daemon is live) launches the agent as a
> pane OWNED by a rally-dedicated ptyd daemon; `inject`'s daemon arm delivers via
> the daemon `agent.send` RPC with ZERO tmux keystrokes. Security gate
> PASS_WITH_CONDITIONS satisfied (F1–F4 enforced; F5/F7 documented) — see §7.
> CI-tested against a stateful test-double daemon
> (`crates/rally-cli/tests/daemon_inject_routing.rs`). The earlier CLI-only
> registration/labeling shipped 2026-06-09.
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

## 7. The live daemon flip — IMPLEMENTED (ptyd pane-ownership)

The flip is now implemented behind the `ptyd` backend. `rally run --backend
ptyd` (and `--backend auto` when the rally daemon is live) launches the agent as
a pane **owned by a rally-dedicated ptyd daemon**, so `inject`'s daemon arm
delivers via the daemon's `agent.send` RPC with **zero tmux keystrokes**.

Delivery is **CLI-initiated `agent.send`** (the rally CLI opens the rally-owned
socket and issues the send itself). This is the **same-actor trust model** as the
tmux fallback: the CLI that wrote the Directive is the actor that performs the
delivery — no new cross-agent PTY-write authority is granted, so the SEC gate
(criterion 5) is satisfied by construction. The **autonomous rally-termd path**
(a daemon that SUBSCRIBES to the ledger and writes on its own initiative) remains
separate, gated, and **is NOT launched** by this work.

Security conditions (gate PASS_WITH_CONDITIONS), all enforced:

- **F1 — sanitize-before-send.** `inject`'s daemon arm runs `sanitize_inject_text`
  (strip C0 controls + ESC + CR/LF) BEFORE the `agent.send` RPC, the same
  paste-breakout hardening as the tmux path.
- **F2 — no orphaned panes on register failure.** If `agent.start` succeeds but
  `agent.register` fails, `rally run` reaps the just-spawned pane **by pane id**
  (`pane.close {pane_id}`, not name-addressed `agent.stop` — a label collision
  could otherwise reap the wrong pane) and falls back to a tmux launch with a
  LOUD `warning` field in the run envelope. Never a silent orphan.
- **F3 — rally-owned socket only.** The spawn path resolves the daemon socket as
  `$RALLY_PTYD_SOCKET` else `~/.local/share/rally/ptyd.sock` — it NEVER defaults
  into Easy Terminal's production daemon. The existing
  `detect_host_runtime`/`try_register_session_with_daemon` paths (for tmux
  sessions) are untouched.
- **F4 — receipt pane cross-check.** The `agent.send` Receipt's `pane_id` is
  checked against `session.daemon_pane`; a mismatch is a HARD
  `daemon_pane_mismatch` failure with NO fallback delivery (the directive stays
  Pending; `delivery_state: failed`).

### F5 — termd / CLI-RPC mutual exclusion

If a `rally-termd` instance is ever launched FOR an agent (the autonomous,
subscribe-and-write path), CLI-initiated `agent.send` delivery for that agent
**must be disabled**, OR termd must **dedup on existing matching Receipts** —
otherwise a single Directive could be delivered twice (once by the CLI, once by
termd) and post two Receipts. This work does not launch termd, so the two paths
are not concurrently active today; the constraint is recorded so a future termd
rollout preserves it.

### F7 — secrets must not ride injects

ptyd persists **redacted** input-history and bounded previews
(`pane.read_input` / `pane.events`, `PaneEventPreview { redacted }`). Redaction
is **heuristic**, not a guarantee. Therefore injects MUST NOT carry secrets:
treat `rally inject --text` as durable, daemon-persisted, and potentially
recoverable. Rotate any secret that transits an inject.

### Rally-owned socket policy

| Knob | Meaning |
|---|---|
| `RALLY_PTYD_SOCKET` | Rally-owned ptyd socket override. Default `~/.local/share/rally/ptyd.sock`. Also the **Easy Terminal opt-in** seam: pointing this at ET's socket is the ONLY supported way to route rally panes into ET's daemon. |
| `RALLY_PTYD_BIN` | ptyd binary for autostart on explicit `--backend ptyd`. Default: `ptyd` on PATH (installed at `~/.local/bin/ptyd`). |

`--backend ptyd` autostarts the rally daemon (`ptyd server` with
`PTYD_SOCKET_PATH=<rally socket>` + `PTYD_STATE_DIR=~/.local/share/rally/ptyd-state`,
detached via `setsid()` so a terminal-close SIGHUP cannot kill it) and waits ≤5s
for the socket; if it cannot start, the run FAILS with a clear error rather than
silently degrading.

### Fix pass (2026-06-10) — auditor FAIL remediation

The first cut of the live flip shipped CI-green but with a **submit-semantics
defect the test double could not catch**. The fixes below close it and the
correctness items the auditor flagged.

- **[A]/[B] ROOT CAUSE — `agent.send` now submits and resolves fast.** The CLI
  sends `{to, text, submit:true, confirm:"sent"}`, not a bare `{to, text}`. ptyd
  treats the presence of `to` as the **non-legacy** framing path
  (`src/main.rs:670-674`), where `submit` defaults **false** — a bare send would
  paste the directive into the agent's input box and **never submit** it (the L5
  inject-no-submit failure). `submit:true` makes ptyd's `frame_line` append the
  submitting CR (`src/comms.rs:48-50`). `confirm:"sent"` parses to
  `ReceiptState::Sent` (`src/comms.rs:198`), so `deliver_line` returns
  **immediately after the write** (`src/pane.rs:1640,1646-1662`) instead of
  blocking ≤4s for an echo — staying under the CLI's 3s round-trip read timeout
  (a `"seen"` default would read a successful write as "failed" and a retry would
  double-paste).
- **[D] Receipt fact is now an honest SENDER-authored delivery record, not a
  fake ACK.** It is authored as the **sender** identity (was: the target tool,
  spoofing the agent), `status` carries the **real** receipt state
  (`sent`/`seen`/`acted`, was: a fabricated `"delivered"`), and it carries **no
  `ref_id`** — it correlates to the Directive via `evidence:
  directive_seq:<n>`. The previous `ref_id: "directive:<tool>:<seq>"` could never
  match `wait_for_resolution` (which matches `ref_id == handoff`), so the stated
  "so the ACK wait resolves" purpose was dead; the dead comments are removed. The
  real ACK remains the **target's own** Resolve/Receipt against the handoff ref.
- **[E] Ptyd sessions pin their socket.** The spawn path records the exact
  rally-owned socket on `ManagedSession.daemon_socket` and every later
  send/stop/read pins the runner to it, so a session registered on socket A can
  never be `agent.send`'d on a re-resolved socket B. The tmux-session
  `detect_host_runtime` registration path (which legitimately probes ET) is
  unchanged.
- **[C] Workspace is list-then-reused by label.** `ensure_rally_workspace` first
  `workspace.list`s (`src/main.rs:489`) and reuses an existing `rally`-labeled
  workspace; it only `workspace.create`s when none exists — so N runs no longer
  create N workspaces + N orphan root shells.
- **[G] F2 reap is by pane id** (`pane.close`, see F2 above).
- **[H] Autostart detaches via `setsid()`** so the rally daemon survives the
  launching terminal closing.

### [F] Default backend is now `auto` (behavior change — read this)

The `--backend` default moved from `tmux` to **`auto`**. A plain `rally run
claude` now spawns a **ptyd-owned pane** whenever the rally daemon socket is
**live** (connectable, not merely present); otherwise it stays on tmux exactly as
before. Two consequences and the guardrails:

1. **Attach differs for ptyd panes.** A ptyd pane lives inside the daemon /
   EasyTerminal, not a tmux client — `rally attach` returns a clear usage error
   telling the user to open the pane in EasyTerminal or run `ptyd attach
   <pane>`. Pass `--backend tmux` to force the attachable tmux path.
2. **`auto` only prefers ptyd when the socket is LIVE** (`socket_is_live` =
   connect + answered `pane.list`); a stale socket file from a crashed daemon
   does NOT win.
3. **`auto`→ptyd is announced.** When `auto` selects ptyd, `rally run` emits one
   **stderr** line naming the selection + the `--backend tmux` override (JSON
   stdout is untouched), so the user is never silently switched.

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
This is the degraded-but-correct path. The envelope labels which path ran:

- `delivery_path: "daemon"` — Directive written; the rally ptyd daemon owns the
  pane and delivery is the **CLI-initiated `agent.send` RPC** (same-actor trust;
  see §7). On the daemon path the envelope also carries `daemon_receipt_state`
  (the Receipt's `sent|seen|acted`) on success, or `daemon_delivery_error` (RPC
  failure / the F4 `daemon_pane_mismatch`) on failure.
- `delivery_path: "tmux_framed_fallback"` — CLI performed the framed write.
- `delivery_path: "ledger_only"` — an externally-registered ptyd pane
  (`LedgerAgent`); the ledger write is already the daemon-delivered path.

(cmux stays the documented separate-submit fallback — no raw-byte send.)

**No tmux fallback for a daemon pane.** A daemon-registered session's pane is a
ptyd pane; tmux cannot reach it. So an `agent.send` RPC failure does NOT fall
back to tmux keystrokes — the directive stays Pending and the envelope reports
the failure honestly (`delivery_state: "failed"` + `daemon_delivery_error`).

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
