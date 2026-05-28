<!-- SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> | SPDX-License-Identifier: Apache-2.0 -->

# Agent Wake / Inject — Test Protocol & Signals

**Audience:** codex (and any peer agent) helping validate cross-agent wake.
**Goal:** confirm that one agent can *wake an idle peer by injecting a prompt*, reliably and bidirectionally, with **no watcher daemon** — and know how to tell whether it worked.

---

## 1. What we are testing

Waking = inject a prompt into a target agent's input and submit it, so an **idle** agent starts a fresh turn on that prompt. Two layers:

- **Delivery primitive:** `herdr` (the always-on terminal/agent daemon) pushes keystrokes into a pane.
- **Bridge:** `scripts/rally_wake.py` resolves a tool name → its idle herdr pane, gates on idle, injects, and confirms the flip to `working`. No `agent-rally-watcher` process is required.

## 2. Status going in (validated vs not)

| Direction | State | Notes |
|---|---|---|
| claude → codex | ✅ validated | codex flipped idle→working→done from `rally_wake.py` / manual herdr |
| codex → claude | ❌ NOT validated | prior attempt was confounded: claude was **not idle**, and there was **no attribution token**, so we could not prove the injected text fired claude vs. came from the human |

**The codex → claude clean re-test is the priority.** Everything below is how to run it and read the result.

## 3. herdr toolbox (read-only state + actions)

```bash
herdr agent list                       # all agents: agent, pane_id, agent_status (idle|working|done|blocked)
herdr agent get <pane>                 # one agent's status
herdr agent read <pane> --source recent --lines 80 --format text   # read its screen
herdr wait agent-status <pane> --status working --timeout 12000    # block until it flips
```

**Current panes (RE-VERIFY with `herdr agent list` before each run — they can change):**
- claude (the human-driving session, wake target) = `w652e4f81649201-3`
- codex (you) = `w652e4f81649201-4`

To re-identify claude's pane if unsure: claude echoes a unique token in its terminal, you `herdr agent read` each `claude` pane and grep for it.

## 4. Attribution discipline (mandatory)

Always embed a **unique token** in the injected prompt, e.g. `WAKE-7F3A`. A wake only counts as proven when the woken agent's response **echoes that exact token** — that distinguishes an injected turn from the human typing.

## 5. The clean codex → claude reverse-wake test

**Precondition — claude must be COMPLETELY IDLE.** A wake into a busy agent merges into its current turn and proves nothing. So:
1. claude announces "I am now idle, go" and stops.
2. You confirm before injecting: `herdr agent get w652e4f81649201-3` → must show `"agent_status":"idle"`. If not idle, wait or abort.

**Run it:**
```bash
# pick a token, then inject + submit into claude's idle pane
python3 scripts/rally_wake.py --pane w652e4f81649201-3 "[codex reverse-wake WAKE-7F3A] You were idle. Reply echoing WAKE-7F3A to prove codex woke you." 
# (rally_wake.py does: agent send -> send-keys Enter -> send-keys Enter, then waits for 'working')
```

**Success signals (you can observe all of these):**
- `rally_wake.py` prints `{"woke": true, ... "status_after": "working"}`, OR
- `herdr wait agent-status w652e4f81649201-3 --status working --timeout 12000` resolves to `working` within ~10s, AND
- after claude responds, `herdr agent read w652e4f81649201-3 --source recent` shows claude's reply **containing `WAKE-7F3A`**.

**Failure signals (and what each means):**
- Status never leaves `idle` → the submit (Enter) did not fire. Try a different submit approach (§6).
- `herdr agent read` shows the prompt staged in claude's input as `[Pasted Content N chars]` but not sent → paste landed, submit didn't. Try §6.
- claude wakes but its reply does **not** echo the token → ambiguous; re-run with a fresh token, and confirm claude was idle at inject time.

## 6. Submit approaches to try (Claude Code's TUI may differ from codex's)

The shipped `rally inject` (origin/main `backends.rs`) and `rally_wake.py` use *different* key sequences. If one doesn't submit, try the next:

- **A (rally_wake.py default):** `herdr agent send <pane> "<text>"` then **two** `herdr pane send-keys <pane> Enter` (1st Enter expands the `[Pasted Content]` collapse, 2nd submits). Works on codex's TUI.
- **B (rally's herdr backend):** `herdr pane send-text <pane> $'\x15'` (Ctrl-U clear) → `herdr pane send-text <pane> "<text>"` → `herdr pane send-keys <pane> enter` (lowercase `enter`, single).
- **C:** replace `Enter` with `C-m` (carriage return): `herdr pane send-keys <pane> C-m`.
- **D (short-message path):** send a SHORT prompt (under ~the paste-collapse threshold) so it types directly into the input instead of collapsing to `[Pasted Content]`, then a single `Enter`. Long prompts collapse and need the double-Enter of approach A.

Record which approach worked — that's a real finding about Claude Code TUI submit behavior.

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
- evidence: the `status_after`, and the token echo from claude's reply

## 9. Forward direction (claude → codex), for completeness
Already validated, but to reproduce: `python3 scripts/rally_wake.py codex "<prompt with token>" --require-idle` → expect `{"woke": true, "status_after": "working"}` and codex acts on the prompt.
