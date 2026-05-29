#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""rally_wake — backend-agnostic DOORBELL wake for idle agents (tmux | herdr).

Relationship to `rally inject`: `rally inject` is the shipped, in-binary delivery
path for managed sessions started by `rally run` (it knows the session's pane and
backend). `rally_wake.py` is a standalone, backend-agnostic doorbell for *idle,
unmanaged* TUIs — it resolves a tool name → its herdr/tmux pane, gates on idle, and
confirms via channel post rather than TUI scraping. Use `rally inject` for managed
sessions; use this script for waking an external agent terminal that Rally did not
launch. Live-test protocol and per-backend routing: `docs/WAKE_TEST_PROTOCOL.md`;
roadmap context: `docs/WAKE_COORDINATION_PLAN.md`.

Design: doorbell + mailbox.
  The wake is a SHORT nudge, not a payload. A short message stays INLINE in the
  agent TUI — it never crosses the paste-collapse threshold that turns input into a
  `[Pasted Content]` placeholder. Herdr submits inline text with `Enter`; tmux
  submits inline text with `C-m`. The long content lives in
  the "mailbox" (the Rally channel, `rally next`, or a file) and the woken agent
  PULLS it.

Why short matters (researched 2026-05-28): a pasted blob is wrapped in
bracketed-paste; an Enter sent with it is treated as a literal newline, and under
tmux `extended-keys-format csi-u` the CR is re-encoded and dropped entirely
(anthropics/claude-code#43169). A short inline nudge avoids the paste path. Herdr
does not accept `C-m`; use `Enter`, and use 2x `Enter` for collapsed direct payloads.

Confirmation is via the CHANNEL (a changes.jsonl line bump = the agent posted back),
not TUI scraping — scraping false-positives on the injected text's own echo.
Herdr status confirmation is useful only after the target agent session has been
restarted with the Herdr v4 integration loaded.

Examples:
  rally_wake.py --tool codex      "Unread in Rally — run: rally next --tool codex --json"
  rally_wake.py --tmux-target rev:0.0 "Read .build-loop/coordination/<file> then continue"
  rally_wake.py --herdr-pane w652... "doorbell" --confirm-channel ~/.agent-rally-point/apps/<slug>/changes.jsonl
"""
import argparse, json, subprocess, sys, time


def run(cmd, parse=False):
    p = subprocess.run(cmd, capture_output=True, text=True)
    if p.returncode != 0:
        sys.exit(f"{' '.join(cmd)} failed: {p.stderr.strip() or p.stdout.strip()}")
    return json.loads(p.stdout) if parse else p.stdout


# ---- herdr backend (has live agent-status; supports idle-gating + resolution) ----
def herdr_agents():
    return run(["herdr", "agent", "list"], parse=True)["result"]["agents"]


def herdr_resolve(tool=None, pane=None, require_idle=False):
    if pane:
        return next((x for x in herdr_agents() if x["pane_id"] == pane),
                    {"pane_id": pane, "agent_status": "unknown", "agent": "?"})
    pool = [a for a in herdr_agents() if a["agent"] == tool]
    if not pool:
        return None
    idle = [a for a in pool if a["agent_status"] == "idle"]
    if require_idle and not idle:
        return None
    return (idle or pool)[0]


def herdr_submit_keys(text, mode, max_inline_chars):
    if mode == "inline":
        return ["Enter"]
    if mode == "collapsed":
        return ["Enter", "Enter"]
    if len(text) > max_inline_chars:
        return ["Enter", "Enter"]
    return ["Enter"]


def herdr_send(target, text, mode, max_inline_chars):
    run(["herdr", "agent", "send", target, text])
    keys = herdr_submit_keys(text, mode, max_inline_chars)
    for key in keys:
        run(["herdr", "pane", "send-keys", target, key])
    return keys


# ---- tmux backend (no agent-status API; address pane explicitly, doorbell is low-harm) ----
def tmux_send(target, text):
    run(["tmux", "send-keys", "-t", target, "-l", text])  # -l = literal, NO paste bracket
    run(["tmux", "send-keys", "-t", target, "C-m"])        # submit
    return ["C-m"]


def herdr_wait_status(target, status, timeout):
    p = subprocess.run(
        [
            "herdr",
            "wait",
            "agent-status",
            target,
            "--status",
            status,
            "--timeout",
            str(int(timeout * 1000)),
        ],
        capture_output=True,
        text=True,
    )
    return {
        "matched": p.returncode == 0,
        "status": status,
        "output": (p.stdout or p.stderr).strip(),
    }


def channel_lines(path):
    try:
        with open(path) as f:
            return sum(1 for _ in f)
    except OSError:
        return -1


def main():
    ap = argparse.ArgumentParser(description="backend-agnostic doorbell wake (tmux|herdr)")
    ap.add_argument("message", help="SHORT doorbell nudge — point to the mailbox, don't carry the payload")
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--tool", help="herdr: resolve an idle agent of this tool (claude|codex|...)")
    g.add_argument("--herdr-pane", help="herdr pane id")
    g.add_argument("--tmux-target", help="tmux target, e.g. session:window.pane")
    ap.add_argument("--require-idle", action="store_true", help="herdr only: skip if none idle (don't interrupt)")
    ap.add_argument("--confirm-channel", help="changes.jsonl path; success = a new line appears (agent posted back)")
    ap.add_argument("--confirm-timeout", type=float, default=45.0)
    ap.add_argument("--max-nudge-chars", type=int, default=300, help="warn above this — paste-collapse risk")
    ap.add_argument("--herdr-submit", choices=["auto", "inline", "collapsed"], default="auto",
                    help="herdr only: auto=1 Enter for inline, 2 Enters above max-nudge-chars")
    ap.add_argument("--confirm-status", choices=["idle", "working", "blocked", "done", "unknown"],
                    help="herdr only: optional post-restart liveness check via herdr wait agent-status")
    ap.add_argument("--confirm-status-timeout", type=float, default=12.0)
    ap.add_argument("--dry-run", action="store_true")
    a = ap.parse_args()

    if len(a.message) > a.max_nudge_chars:
        print(json.dumps({"warning": f"nudge is {len(a.message)} chars > {a.max_nudge_chars}; "
                          "paste-collapse risk — keep doorbells short, put detail in the mailbox"}),
              file=sys.stderr)

    if a.tmux_target:
        backend, target, agent, status = "tmux", a.tmux_target, "?", "unknown"
    else:
        t = herdr_resolve(tool=a.tool, pane=a.herdr_pane, require_idle=a.require_idle)
        if t is None:
            print(json.dumps({"woke": False, "reason": "no idle herdr target" if a.require_idle
                              else "no herdr agent for tool"}))
            sys.exit(3)
        backend, target, agent, status = "herdr", t["pane_id"], t.get("agent"), t.get("agent_status")

    if a.dry_run:
        submit_keys = (["C-m"] if backend == "tmux"
                       else herdr_submit_keys(a.message, a.herdr_submit, a.max_nudge_chars))
        print(json.dumps({"would_wake": target, "backend": backend, "agent": agent,
                          "status": status, "submit_keys": submit_keys}))
        return

    base = channel_lines(a.confirm_channel) if a.confirm_channel else None
    if backend == "tmux":
        submit_keys = tmux_send(target, a.message)
    else:
        submit_keys = herdr_send(target, a.message, a.herdr_submit, a.max_nudge_chars)

    # Honest confirmation: woke=true ONLY when the channel shows the agent posted back.
    # Without --confirm-channel/--confirm-status we report delivery only (woke=None).
    woke, outcome = None, "delivered (unconfirmed — pass --confirm-channel to verify)"
    if a.confirm_channel:
        deadline = time.monotonic() + a.confirm_timeout
        woke, outcome = False, "delivered:no-channel-post-within-timeout"
        while time.monotonic() < deadline:
            if channel_lines(a.confirm_channel) > base:
                woke, outcome = True, "confirmed:channel-post"
                break
            time.sleep(2)
    status_confirm = None
    if a.confirm_status:
        if backend != "herdr":
            status_confirm = {"matched": False, "status": a.confirm_status,
                              "output": "--confirm-status is herdr-only"}
        else:
            status_confirm = herdr_wait_status(target, a.confirm_status, a.confirm_status_timeout)
            if not a.confirm_channel:
                woke = status_confirm["matched"]
                outcome = "confirmed:herdr-status" if woke else "delivered:no-status-match-within-timeout"
    print(json.dumps({"woke": woke, "backend": backend, "target": target,
                      "agent": agent, "submit_keys": submit_keys, "confirm": outcome,
                      "status_confirm": status_confirm}))


if __name__ == "__main__":
    main()
