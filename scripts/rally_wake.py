#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""rally_wake — auto-inject a prompt into an idle agent with NO watcher daemon.

The watcher (agent-rally-watcher) was a long-running tail-and-dispatch process.
This needs none of that: herdr is already the always-on daemon that tracks every
agent's live status. This script is the missing bridge the watcher never had —
it resolves a tool name to its herdr pane, gates on idle, and injects + submits.

Two ways to make it "automatic":
  1. Post-time wake (no daemon at all): call this from the coordination post path,
     so sending a handoff to a peer also wakes that peer in the same action.
  2. Idle-trigger (tiny loop, not the watcher package):
     `herdr wait agent-status <pane> --status idle` then drain + wake.

Usage:
  rally_wake.py <tool> "<prompt>"        # wake an idle agent of <tool> (claude|codex|...)
  rally_wake.py <tool> "<prompt>" --require-idle   # skip if none idle (don't interrupt work)
  rally_wake.py --pane <pane_id> "<prompt>"        # target an exact pane
  rally_wake.py <tool> "<prompt>" --dry-run        # resolve + print target, don't inject
"""
import argparse, json, subprocess, sys, time


def herdr(*args, parse=True):
    p = subprocess.run(["herdr", *args], capture_output=True, text=True)
    if p.returncode != 0:
        sys.exit(f"herdr {' '.join(args)} failed: {p.stderr.strip() or p.stdout.strip()}")
    return json.loads(p.stdout) if parse else p.stdout


def list_agents():
    return herdr("agent", "list")["result"]["agents"]


def resolve(tool=None, pane=None, require_idle=False):
    if pane:
        a = next((x for x in list_agents() if x["pane_id"] == pane), None)
        return a or {"pane_id": pane, "agent_status": "unknown", "agent": "?"}
    pool = [a for a in list_agents() if a["agent"] == tool]
    if not pool:
        return None
    idle = [a for a in pool if a["agent_status"] == "idle"]
    if require_idle and not idle:
        return None  # caller decides: skip rather than interrupt working agent
    return (idle or pool)[0]


def inject(pane, text, wait_working=True):
    herdr("agent", "send", pane, text, parse=False)        # paste literal
    herdr("pane", "send-keys", pane, "Enter", parse=False)  # expand paste
    herdr("pane", "send-keys", pane, "Enter", parse=False)  # submit
    if wait_working:
        herdr("wait", "agent-status", pane, "--status", "working", "--timeout", "12000", parse=False)
        st = next((a["agent_status"] for a in list_agents() if a["pane_id"] == pane), "unknown")
        return st
    return "sent"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("tool", nargs="?", help="agent tool id: claude | codex | ...")
    ap.add_argument("message")
    ap.add_argument("--pane", help="exact herdr pane id (overrides tool resolution)")
    ap.add_argument("--require-idle", action="store_true", help="skip if no idle agent (don't interrupt)")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--no-wait", action="store_true")
    a = ap.parse_args()

    target = resolve(tool=a.tool, pane=a.pane, require_idle=a.require_idle)
    if target is None:
        print(json.dumps({"woke": False, "reason": "no idle target" if a.require_idle else "no agent for tool"}))
        sys.exit(3)
    pane = target["pane_id"]
    if a.dry_run:
        print(json.dumps({"would_wake": pane, "agent": target.get("agent"), "status": target.get("agent_status")}))
        return
    result = inject(pane, a.message, wait_working=not a.no_wait)
    print(json.dumps({"woke": result == "working" or result == "sent", "pane": pane,
                      "agent": target.get("agent"), "status_after": result}))


if __name__ == "__main__":
    main()
