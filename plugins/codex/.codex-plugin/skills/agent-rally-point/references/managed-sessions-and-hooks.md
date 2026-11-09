## Managed Sessions

Use managed sessions for reliable live delivery into visible panes:

```bash
rally run codex --backend tmux --task "<bounded prompt>" --json
rally run claude --backend tmux --json
rally inject <session|name|tool> --handoff <event-id> --json
rally capture <session|name|tool> --json
```

Prefer `rally run codex --task "<prompt>"` for one bounded Codex assignment.
Rally passes the prompt to the long-lived `codex exec` child over private stdin,
stores the final response under `.rally/task-results/`, then releases
exact-session claims and closes the session. It removes a clean worktree or
reports the recovery path for a dirty one. That exit releases the synced-task
writer lease. Private result files remain until the operator archives or deletes
them; Rally applies no automatic retention window. The invoking shell may retain
the `--task` command in its history.
Use plain `rally run codex` only when the session must remain interactive for
later `rally inject` steering, and stop that persistent session explicitly when
finished. A task-scoped session rejects later injection because its prompt is
fixed at launch.

Rally does not keep agents awake by itself. Treat `rally next --tool "$TOOL"
--json` as the wake-intent check and `rally inject ... --handoff <event-id>` as
the focused delivery path for managed sessions. Host adapters decide whether to
use native wake, prompt injection, pane notification, resume-only context, or CI
policy.

Watchers must stay narrow: they may detect a transition and notify or inject
through the host's native mechanism, but they must not edit files, resolve
blockers, publish facts on behalf of an agent, or behave like hidden
schedulers.

For Herdr-managed panes, submit injected text with Herdr's `Enter` key, not
tmux-style `C-m`. Full-length payloads can collapse behind `[Pasted Content]`;
submit those with two Enters, where the first expands and the second submits.
Short inline nudges need one Enter. After installing Herdr's Claude/Codex
integrations, restart the agent session before treating Herdr `agent_status` as
authoritative. Even post-restart, use a Rally channel post as the strongest
confirmation that the woken agent acted on the handoff.

When delegating work from inside herdr, keep the user's main tab clean. Start
new helper agents in the workspace's `agents` tab whenever one exists. Discover
the tab with `herdr tab list`, then start the agent with `herdr agent start
... --tab <agents-tab-id> --no-focus -- ...` or use a Rally backend option that
targets that tab when available. Only place helper agents in the active tab when
the user explicitly asks for that.

Agents should call `rally check before-write` explicitly before shared edits.

## Hooks: what auto-loads, and what it does

This repo **does** ship host hooks, and opening the repo in a host that trusts it
**auto-loads them**. Say this plainly rather than assuming it — it is a real trust
decision the user is making.

Committed hook registrations: `.claude/settings.json` (Claude Code),
`.codex/hooks.json` (Codex), `.cursor/hooks.json` (Cursor), `hooks/hooks.json`
(Claude plugin surface).

| Event | What the hook does |
|-------|--------------------|
| SessionStart | Registers presence, reads room state, emits a sanitized advisory message. If `rally` is missing, prints the install command. |
| PreToolUse (edits) | Checks whether the path is claimed by another live agent. Advisory. |
| UserPromptSubmit | Refreshes idle status. |
| Stop | Records that the write completed. |

The hooks are advisory by default, and three opt-in switches make them block.
Default posture: PreToolUse returns `permissionDecision: "allow"` with a warning,
so the edit goes through, and every hook exits 0 even when Rally is broken — in
every posture, because a refusal travels in the hook's JSON, not its exit code.

| Switch (off unless set) | Effect |
|-------------------------|--------|
| `RALLY_HOOK_STRICT=1` | The hook emits `permissionDecision: "deny"` on a high-severity PreToolUse signal (`severity == "stop"` or `allow == false`). Stop remains advisory because it marks a turn boundary, not run completion. |
| `rally check before-write --strict` | Exits 4 on a stop finding. Step 2 of the loop above passes `--strict`. |
| `RALLY_BEFORE_WRITE_FAILCLOSED=1` or `--fail-closed` | `check before-write` exits 4 when its snapshot read times out, instead of exiting 0. |

Under `RALLY_HOOK_STRICT=1` an unscoped blocker becomes a hard deny on every edit
by every agent in the room, so strict mode is a real availability risk — see
[`docs/security/TRUST-MODEL.md`](../../../docs/security/TRUST-MODEL.md).

They do **not** download, build, `chmod +x`, or install anything. Provisioning was
removed from every lifecycle hook (see RC-013). Installing the `rally` binary is an
explicit step a human runs: `scripts/install-rally.sh` or
`cargo install --path crates/rally-cli`.

Off switches: `RALLY_HOOKS=off` (session), `rally hooks off --scope repo` (repo),
`rally hooks status` (check). Full detail:
[`docs/security/TRUST-MODEL.md`](../../../docs/security/TRUST-MODEL.md).
