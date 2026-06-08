<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr | SPDX-License-Identifier: Apache-2.0 -->
# Handoffs & Launching Managed Agents

Practical guide for handing a build off to a fresh agent session and launching
managed agents into a Rally room. Distilled from a live run (2026-05-31 Easy
Terminal redesign).

## TL;DR

```bash
cd <repo>                                   # rally is repo-local — cwd must be the repo
rally run <claude|codex|opencode|gemini> --name <label>
rally inject <session> --text "<prompt>"    # deliver and submit the first instruction
rally capture <session> --lines 30          # snapshot what it's doing
rally attach <session>                      # watch live  ·  rally stop <session> to halt
```

## 1. Launching a managed agent

`rally run <claude|codex|opencode|gemini> [--name <label>] [--backend tmux|cmux] [--dry-run] [--json]`

- **Always `--dry-run --json` first** to see the exact `tmux new-session …` command and the resolved `session_id` / `target` / `tool` before spawning.
- Run ids auto-number active agents: `claude-<label>-01`, `tool=claude_code:<label>-01`. The tmux target is `rally-<agent>-<label>-NN`.
- Default backend is `tmux`; `cmux` launches into cmux instead. The legacy `herdr` backend (and its `--herdr-bin` / `--herdr-socket` flags) were removed in Plan F. For Easy Terminal / ptyd integration, use the `.rally` ledger directly: rally writes Directives and the `rally-termd` daemon subscribes.
- Self-relaunch guard: an agent hosted by an Easy Terminal socket must not launch a build/relaunch lane back into its own host. Start build/relaunch workers outside that ET instance, or detach first; `RALLY_ALLOW_SELF_HOSTED_ET_LAUNCH=1` is only for a consciously detached/non-relaunch launch.
- The agent starts at its normal prompt with **auto mode on** (Claude Code) — it does nothing until you inject an instruction.

## 2. Injecting the instruction

`rally inject <session|name|tool> --text "<prompt>"` delivers text into the session input and submits one Enter through the managed backend:

```bash
rally inject claude-foo-01 --text "Read docs/HANDOFF.md and continue …"
```

- `--require-ack` requires `--handoff <event-id>` or `--ref` — it does **not** work with free `--text`. For a plain steer, omit it.
- `delivered=true ack=false` is normal for a `--text` inject (no ack channel).
- Inject only works against a **`rally run`-managed** session. Fact-only / externally-launched agents are not injectable — hand those off via a committed doc instead.
- Capture before the first real inject if the host can show startup prompts. Codex may stop on an update/trust prompt; clear that deliberately before injecting work so the handoff lands at the agent prompt, not in the startup menu.
- If `rally capture` shows the prompt pasted but not acted on, wait briefly and send one bounded backend-specific Enter as troubleshooting. That is a fallback, not the normal contract.

## 3. The handoff itself — Rally is the source of truth

Rally `.rally/log/**` records are the coordination source of truth. A committed `docs/HANDOFF-<date>-<topic>.md` is a durable payload for longer context; the Rally fact points to it, and managed sessions receive focused work through `rally inject`.

Default communication order:

1. Post targeted `handoff`, `decision`, `risk`, `blocker`, `resolve`, and `artifact` facts to the owning repo's Rally ledger.
2. Use `rally inject` to deliver the first instruction or urgent steering into a `rally run`-managed session.
3. Use a committed handoff doc only when the payload is too long or durable enough to review outside the ledger.

A good handoff doc contains: mission · what's DONE (with the **canonical branch + HEAD**, build status) · PENDING in priority order · verification gaps · cleanup register · conventions/gotchas · key paths · coordination instructions.

**Pin the branch.** State the canonical branch and HEAD explicitly and have the incoming session assert it (`git branch --show-current`). Worktree-isolated workers drift onto side branches and fast-forward to catch up; a handoff that says "on main" when the work is really on a feature branch will mislead the next session.

## 4. Inject prompt template

```
You are taking over <project>. FIRST read docs/HANDOFF-<date>.md, then <plan>.
Canonical branch = <branch> @ <HEAD> (verify with `git branch --show-current`); build green.
Continue PENDING work in priority order: (1)… (2)…
RULES: verify with <build cmd> (grep the success line); <project-specific gotchas>;
do NOT <gated/irreversible action> without <gate>.
Coordinate via rally (you are <session>); post progress with `rally say`, surface decisions to the user.
Commit each chunk; keep <branch> green. Start by reading the handoff doc + confirming your plan.
```

## 5. Runtime specifics

| Runtime | First-class `rally run` | Notes |
|---|---:|---|
| Claude Code | yes | Reads `CLAUDE.md`; fresh sessions may prompt to trust the folder or approve tools. |
| Codex CLI | yes | Reads `AGENTS.md`; behavior depends on Codex sandbox/approval config. |
| OpenCode | yes | Use the same Rally core loop and handoff/inject contract. |
| Gemini CLI | yes | Use the same Rally core loop and handoff/inject contract. |
| Cursor agent | no | Use manual onboarding until adopted by a managed backend. |
| Qwen/Gemma CLI | no | Use manual onboarding until adopted by a managed backend. |
| Aider | no | Use manual onboarding until adopted by a managed backend. |

Keep **CLAUDE.md** (Claude) and **AGENTS.md** (Codex) at the repo root current — each host reads its own on entry, so the handoff context should live there or be linked from there.

For every non-first-class host, use
[`ANY-AGENT-ONBOARDING.md`](ANY-AGENT-ONBOARDING.md). The short version:
choose a stable tool id, run `whoami` / `enter` / `ack` / `next`, post targeted
Rally facts, and do not assume `rally inject` is available unless
`rally sessions --json` lists the session.

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
