#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""rally_wake — backend-agnostic DOORBELL wake for idle agents (tmux | herdr).

Design: doorbell + mailbox.
  The wake is a SHORT nudge, not a payload. A short message stays INLINE in the
  agent TUI — it never crosses the paste-collapse threshold that turns input into a
  `[Pasted Content]` placeholder — so a single carriage return (`C-m`) submits
  RELIABLY, on tmux or herdr, for Claude Code or Codex. The long content lives in
  the "mailbox" (the Rally channel, `rally next`, or a file) and the woken agent
  PULLS it.

Why short matters (researched 2026-05-28): a pasted blob is wrapped in
bracketed-paste; an Enter sent with it is treated as a literal newline, and under
tmux `extended-keys-format csi-u` the CR is re-encoded and dropped entirely
(anthropics/claude-code#43169). A short inline nudge avoids the paste path, which
is why a fixed "2x Enter" was intermittent and a short doorbell + single C-m is not.

Confirmation is via the CHANNEL (a changes.jsonl line bump = the agent posted back),
never TUI scraping — scraping false-positives on the injected text's own echo.

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


def herdr_send(target, text):
    run(["herdr", "agent", "send", target, text])      # short inline nudge
    run(["herdr", "pane", "send-keys", target, "C-m"])  # single CR submits


# ---- tmux backend (no agent-status API; address pane explicitly, doorbell is low-harm) ----
def tmux_send(target, text):
    run(["tmux", "send-keys", "-t", target, "-l", text])  # -l = literal, NO paste bracket
    run(["tmux", "send-keys", "-t", target, "C-m"])        # submit


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
        print(json.dumps({"would_wake": target, "backend": backend, "agent": agent, "status": status}))
        return

    base = channel_lines(a.confirm_channel) if a.confirm_channel else None
    (tmux_send if backend == "tmux" else herdr_send)(target, a.message)

    # Honest confirmation: woke=true ONLY when the channel shows the agent posted back.
    # Without --confirm-channel we report delivery only (woke=None) — never a TUI-scrape guess.
    woke, outcome = None, "delivered (unconfirmed — pass --confirm-channel to verify)"
    if a.confirm_channel:
        deadline = time.monotonic() + a.confirm_timeout
        woke, outcome = False, "delivered:no-channel-post-within-timeout"
        while time.monotonic() < deadline:
            if channel_lines(a.confirm_channel) > base:
                woke, outcome = True, "confirmed:channel-post"
                break
            time.sleep(2)
    print(json.dumps({"woke": woke, "backend": backend, "target": target,
                      "agent": agent, "confirm": outcome}))


if __name__ == "__main__":
    main()
