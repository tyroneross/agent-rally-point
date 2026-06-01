<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr | SPDX-License-Identifier: Apache-2.0 -->
# Handoffs & Launching Agents (Claude + Codex)

Practical guide for handing a build off to a fresh agent session and launching managed Claude/Codex agents into a Rally room. Distilled from a live run (2026-05-31 Easy Terminal redesign).

## TL;DR

```bash
cd <repo>                                   # rally is repo-local — cwd must be the repo
rally run claude --name <label>             # launch a managed Claude in tmux, auto-numbered (claude-<label>-01)
rally inject <session> --text "<prompt>"    # deliver the first instruction…
tmux send-keys -t rally-<session-target> Enter   # …then SUBMIT it (inject does NOT press Enter — see gotcha)
rally capture <session> --lines 30          # snapshot what it's doing
rally attach <session>                      # watch live  ·  rally stop <session> to halt
```

## 1. Launching a managed agent

`rally run <claude|codex|opencode|gemini> [--name <label>] [--backend tmux|herdr|cmux] [--dry-run] [--json]`

- **Always `--dry-run --json` first** to see the exact `tmux new-session …` command and the resolved `session_id` / `target` / `tool` before spawning.
- Run ids auto-number active agents: `claude-<label>-01`, `tool=claude_code:<label>-01`. The tmux target is `rally-<agent>-<label>-NN`.
- Default backend is `tmux`. `herdr`/`cmux` launch into those multiplexers instead.
- The agent starts at its normal prompt with **auto mode on** (Claude Code) — it does nothing until you inject an instruction.

## 2. Injecting the instruction (and the Enter gotcha)

`rally inject <session|name|tool> --text "<prompt>"` delivers text into the session's input. **It pastes but does NOT submit** — the prompt sits as `[Pasted text #1]`. You MUST follow with an Enter:

```bash
rally inject claude-foo-01 --text "Read docs/HANDOFF.md and continue …"
tmux send-keys -t rally-claude-foo-01 Enter        # submit
```

- `--require-ack` requires `--handoff <event-id>` or `--ref` — it does **not** work with free `--text`. For a plain steer, omit it.
- `delivered=true ack=false` is normal for a `--text` inject (no ack channel).
- Inject only works against a **`rally run`-managed** session. Fact-only / externally-launched agents are not injectable — hand those off via a committed doc instead.

## 3. The handoff itself — a committed doc is the source of truth

Rally `handoff` *records* (`rally say handoff …` / `agent_rally.py handoff …`) can be policy-rejected (e.g. no active run/lease) and return opaquely. **Do not rely on the record alone.** The durable handoff is a **committed `docs/HANDOFF-<date>-<topic>.md`** that the incoming session reads first. The record is a pointer to it.

A good handoff doc contains: mission · what's DONE (with the **canonical branch + HEAD**, build status) · PENDING in priority order · verification gaps · cleanup register · conventions/gotchas · key paths · coordination instructions.

**Pin the branch.** State the canonical branch and HEAD explicitly and have the incoming session assert it (`git branch --show-current`). Worktree-isolated workers drift onto side branches and fast-forward to catch up; a handoff that says "on main" when the work is really on a feature branch will mislead the next session.

## 4. Inject prompt template (codex + claude)

```
You are taking over <project>. FIRST read docs/HANDOFF-<date>.md, then <plan>.
Canonical branch = <branch> @ <HEAD> (verify with `git branch --show-current`); build green.
Continue PENDING work in priority order: (1)… (2)…
RULES: verify with <build cmd> (grep the success line); <project-specific gotchas>;
do NOT <gated/irreversible action> without <gate>.
Coordinate via rally (you are <session>); post progress with `rally say`, surface decisions to the user.
Commit each chunk; keep <branch> green. Start by reading the handoff doc + confirming your plan.
```

## 5. Claude vs Codex specifics

| | Claude (`rally run claude`) | Codex (`rally run codex`) |
|---|---|---|
| Launch | tmux, starts at prompt, auto mode | tmux; Codex TUI / `codex exec` per backend |
| Inject | `rally inject … --text` **+ Enter** | same; some Codex modes are non-injectable (fact-only) → use the doc + `rally say` |
| Permissions | fresh session may prompt to trust folder / approve tools — pre-approve or run in an already-trusted repo | Codex sandbox/approval per its config |
| Coordination | reads CLAUDE.md + rally room on entry | reads AGENTS.md + rally room on entry |
| Identity | `claude_code:<name>` | `codex:<name>` |

Keep **CLAUDE.md** (Claude) and **AGENTS.md** (Codex) at the repo root current — each host reads its own on entry, so the handoff context should live there or be linked from there.

## 6. Monitoring

- `rally room [--since <seq>]` — coordination posts (`rally say`/handoffs).
- `rally capture <session> --lines N` — one-shot screen snapshot (or `tmux capture-pane -t <target> -p`).
- `rally attach <session>` — live attach.
- `rally watch [--tool <id>] [--on-activity <cmd>]` — continuous room watcher (supports `--print-launchd`/`--print-systemd` for a background service).
- `rally roster` — who's live, where, doing what.

## 7. Shared-working-tree hazard

Rally deconflicts **file ownership, not git branch/checkout state**. Multiple agents in ONE working tree → commits land on whoever's branch is checked out. For true parallelism give each agent its **own git worktree** (`git worktree add`), or hand off serially (one driver at a time on a shared tree). A takeover (this session stands down, the new one drives) is safe on a shared tree.

## 8. Standing down cleanly

After handing off: `rally stop <your-session>` (releases claims) or just let the takeover own the room. Leave the handoff doc committed and the canonical branch green.
