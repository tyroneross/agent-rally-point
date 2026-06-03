<!-- SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> | SPDX-License-Identifier: Apache-2.0 -->

# Agent Wake / Inject — Test Protocol & Signals

> **Historical (2026-05-28).** Wake protocol validated against the legacy `herdr` delivery primitive. Herdr was removed in Plan F; the Easy Terminal app daemon socket was renamed `herdr.sock` → `ptyd.sock` and the CLI is now `ptyd`. The protocol shapes (idle gate, channel-confirm vs status-flip, paste/submit handling) generalize to ptyd, but the literal commands below are out of date.

**Audience:** codex (and any peer agent) helping validate cross-agent wake.
**Goal:** confirm that one agent can *wake an idle peer by injecting a prompt*, reliably and bidirectionally, with **no watcher daemon** — and know how to tell whether it worked.

---

## 1. What we are testing

Waking = inject a prompt into a target agent's input and submit it, so an **idle** agent starts a fresh turn on that prompt. Two layers:

- **Delivery primitive:** `herdr` (the always-on terminal/agent daemon) pushes keystrokes into a pane.
- **Bridge:** `scripts/rally_wake.py` resolves a tool name → its idle herdr pane, gates on idle, injects, and confirms via channel post or post-restart Herdr status. No `agent-rally-watcher` process is required.

## 2. Status going in (validated vs not)

| Direction | State | Notes |
|---|---|---|
| claude → codex | ✅ validated | `rally_wake.py` / manual herdr reached codex with no watcher daemon |
| codex → claude | ✅ validated | approach A worked: Herdr direct payload + `Enter` x2, with token echo proof |
| direct full payload | ✅ validated | `WAKE-DIRECT-T4` proved long Herdr payload delivery; collapsed paste needs `Enter` x2 |

Everything below is how to rerun it and read the result.

## 3. herdr toolbox (read-only state + actions)

```bash
herdr agent list                       # all agents: agent, pane_id, agent_status (idle|working|done|blocked)
herdr agent get <pane>                 # one agent's status
herdr agent read <pane> --source recent --lines 80 --format text   # read its screen
herdr pane send-keys <pane> Enter      # Herdr submit key
herdr wait agent-status <pane> --status working --timeout 12000    # post-restart liveness check
```

**Current panes (RE-VERIFY with `herdr agent list` before each run — they can change):**
- claude (the human-driving session, wake target) = `w652e4f81649201-3`
- codex (you) = `w652e4f81649201-4`

To re-identify claude's pane if unsure: claude echoes a unique token in its terminal, you `herdr agent read` each `claude` pane and grep for it.

## 4. Attribution discipline (mandatory)

Always embed a **unique token** in the injected prompt, e.g. `WAKE-7F3A`. For direct reply tests, a wake only counts as proven when the woken agent's response **echoes that exact token** — that distinguishes an injected turn from the human typing. For coordination work, a Rally channel post is stronger proof because it shows the woken agent acted, not just that text appeared in its pane.

## 5. The clean codex → claude reverse-wake test

**Precondition — claude must be COMPLETELY IDLE.** A wake into a busy agent merges into its current turn and proves nothing. So:
1. claude announces "I am now idle, go" and stops.
2. You confirm before injecting: `herdr agent get w652e4f81649201-3` → must show `"agent_status":"idle"`. If not idle, wait or abort.

**Run it:**
```bash
# pick a token, then inject + submit into claude's idle pane
python3 scripts/rally_wake.py --herdr-pane w652e4f81649201-3 \
  "[codex reverse-wake WAKE-7F3A] You were idle. Reply echoing WAKE-7F3A to prove codex woke you." \
  --herdr-submit collapsed
# Herdr does: agent send -> send-keys Enter -> send-keys Enter for collapsed/direct payloads.
```

**Success signals, strongest first:**
- The doorbell only nudges; the woken agent acts by **posting back to the Rally channel**. Confirm by a new line in `changes.jsonl`: `rally_wake.py --confirm-channel <changes.jsonl>` prints `{"woke": true, "confirm": "confirmed:channel-post"}`.
- A direct reply test can be confirmed by the woken agent echoing the exact fresh token.
- Post-restart Herdr status can confirm liveness/delivery when the target agent session loaded the Herdr v4 integration.
- **Do NOT treat TUI scraping as authoritative.** Two scrape methods were tried and both failed:
  - status-flip (`herdr wait agent-status --status working`) → **false negative** (a succeeded wake never surfaced `working` within 12s).
  - token-echo (count token in pane buffer) → **false positive** (counted the *un-submitted input echo* as a reply; 2026-05-28).
- The channel is the best machine-verifiable proof the agent actually acted.

**Backend submit rules:**
- Herdr uses `Enter`, not `C-m`. `herdr pane send-keys <pane> C-m` returns `invalid_key`.
- Herdr inline/short prompts submit with one `Enter`.
- Herdr full-length/collapsed prompts need two `Enter` keys: first expands `[Pasted Content]`, second submits.
- tmux uses `C-m` for short doorbells.
- For tmux or no-integration routes, put the payload in the mailbox (channel / `rally next` / a file) and wake with a short doorbell.

**Failure signals (and what each means):**
- Status never leaves `idle` → the submit (Enter) did not fire. Try a different submit approach (§6).
- `herdr agent read` shows the prompt staged in claude's input as `[Pasted Content N chars]` but not sent → paste landed, submit didn't. Try §6.
- claude wakes but its reply does **not** echo the token → ambiguous; re-run with a fresh token, and confirm claude was idle at inject time.

## 6. Submit approaches to try (Claude Code's TUI may differ from codex's)

The shipped `rally inject` (origin/main `backends.rs`) and `rally_wake.py` may use different key sequences. Route by backend:

- **Herdr direct/collapsed:** `herdr agent send <pane> "<text>"` then **two** `herdr pane send-keys <pane> Enter`.
- **Herdr inline/short:** `herdr agent send <pane> "<short text>"` then one `herdr pane send-keys <pane> Enter`.
- **Rally native Herdr backend:** `herdr pane send-text <pane> $'\x15'` (Ctrl-U clear) → `herdr pane send-text <pane> "<text>"` → `herdr pane send-keys <pane> enter`.
- **tmux/no-integration:** short doorbell only, then `tmux send-keys ... C-m`, with channel-confirm.

Record which route worked — that's a real finding about the host backend and agent TUI.

## 7. "Going completely idle" / trying approaches — you have latitude

You (codex) may:
- Wait for claude to be idle and only then inject (preferred).
- Try approaches A→D in order until one flips claude to `working`.
- Read claude's pane freely (`herdr agent read`) to diagnose staged-but-unsent input.
- Abort and report if none work — that's a valid result (it tells us Claude Code's pane needs a different submit primitive).

## 8. Log the result

Append to the coordination file's **Verifier feedback log** (`.build-loop/coordination/rally-diff-integration-assessment-2026-05-28.md`) and post to the channel:
- direction tested, token used, idle-precondition confirmed (y/n)
- PASS/FAIL, which submit approach worked (A/B/C/D)
- evidence: channel post when available, token echo for direct reply tests, and any post-restart Herdr status confirmation as a liveness signal

## 9. Forward direction (claude → codex), for completeness
Already validated, but to reproduce: `python3 scripts/rally_wake.py --tool codex "<prompt with token>" --require-idle` → expect delivery, then confirm with token echo or channel post. Treat Herdr status as authoritative only after the target session has restarted with the v4 Herdr integration loaded.
