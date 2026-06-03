#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""rally_wake — tmux DOORBELL wake for idle agents (unmanaged TUIs only).

Scope after Plan F (2026-06).
  This script covers ONE narrow case: waking an external agent terminal that
  Rally did NOT launch and that lives inside a tmux pane you can address
  explicitly. For everything else, use the canonical wake paths:

  - **Managed sessions** (started by `rally run`): use `rally inject`. The
    in-binary path knows the session's pane and backend.
  - **Easy Terminal / ptyd** (agent panes managed by the ET app daemon):
    use the `.rally` ledger directly. `rally inject` appends a typed
    Directive; `rally-termd` subscribes and performs the PTY-inject. This
    script does NOT shell `ptyd` or `et` — that would re-create the
    herdr-era coupling that Plan F deliberately deleted (see
    `crates/rally-cli/tests/arch_no_herdr_dep.rs`).
  - **Unmanaged tmux pane**: this script. tmux has no agent-status API, so
    the doorbell is unconditional (no idle gate); confirmation is via a
    Rally channel post, never TUI scraping.

  The legacy `herdr` backend was removed in this script alongside the Rust
  removal of `Backend::Herdr`. The `herdr` binary was the daemon-CLI that
  Easy Terminal replaced with `ptyd`; the right successor for ptyd-managed
  agents is the ledger, not a Python shim.

Design: doorbell + mailbox.
  The wake is a SHORT nudge, not a payload. A short message stays INLINE in
  the agent TUI — it never crosses the paste-collapse threshold that turns
  input into a `[Pasted Content]` placeholder. tmux submits inline text
  with `C-m`. The long content lives in the "mailbox" (the Rally channel,
  `rally next`, or a file) and the woken agent PULLS it.

Why short matters (researched 2026-05-28): a pasted blob is wrapped in
bracketed-paste; an Enter sent with it is treated as a literal newline, and
under tmux `extended-keys-format csi-u` the CR is re-encoded and dropped
entirely (anthropics/claude-code#43169). A short inline nudge avoids the
paste path.

Confirmation is via the CHANNEL (a changes.jsonl line bump = the agent
posted back), not TUI scraping — scraping false-positives on the injected
text's own echo.

Examples:
  rally_wake.py --tmux-target rev:0.0 "Read <coordination-file> then continue"
  rally_wake.py --tmux-target main:0.0 "doorbell" \
      --confirm-channel <repo_root>/.rally/ledger.jsonl
"""
import argparse, json, subprocess, sys, time


def run(cmd, parse=False):
    p = subprocess.run(cmd, capture_output=True, text=True)
    if p.returncode != 0:
        sys.exit(f"{' '.join(cmd)} failed: {p.stderr.strip() or p.stdout.strip()}")
    return json.loads(p.stdout) if parse else p.stdout


# ---- tmux backend (no agent-status API; address pane explicitly, doorbell is low-harm) ----
def tmux_send(target, text):
    run(["tmux", "send-keys", "-t", target, "-l", text])  # -l = literal, NO paste bracket
    run(["tmux", "send-keys", "-t", target, "C-m"])        # submit
    return ["C-m"]


def channel_lines(path):
    try:
        with open(path) as f:
            return sum(1 for _ in f)
    except OSError:
        return -1


def main():
    ap = argparse.ArgumentParser(
        description="tmux DOORBELL wake for unmanaged agent TUIs. "
        "For ptyd/Easy Terminal use `rally inject` (ledger). "
        "For managed `rally run` sessions use `rally inject`."
    )
    ap.add_argument("message",
                    help="SHORT doorbell nudge — point to the mailbox, don't carry the payload")
    ap.add_argument("--tmux-target", required=True,
                    help="tmux target, e.g. session:window.pane")
    ap.add_argument("--confirm-channel",
                    help="changes.jsonl path; success = a new line appears (agent posted back)")
    ap.add_argument("--confirm-timeout", type=float, default=45.0)
    ap.add_argument("--max-nudge-chars", type=int, default=300,
                    help="warn above this — paste-collapse risk")
    ap.add_argument("--dry-run", action="store_true")
    a = ap.parse_args()

    if len(a.message) > a.max_nudge_chars:
        print(json.dumps({"warning": f"nudge is {len(a.message)} chars > {a.max_nudge_chars}; "
                          "paste-collapse risk — keep doorbells short, put detail in the mailbox"}),
              file=sys.stderr)

    backend, target, agent, status = "tmux", a.tmux_target, "?", "unknown"

    if a.dry_run:
        print(json.dumps({"would_wake": target, "backend": backend, "agent": agent,
                          "status": status, "submit_keys": ["C-m"]}))
        return

    base = channel_lines(a.confirm_channel) if a.confirm_channel else None
    submit_keys = tmux_send(target, a.message)

    # Honest confirmation: woke=true ONLY when the channel shows the agent posted back.
    # Without --confirm-channel we report delivery only (woke=None).
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
                      "agent": agent, "submit_keys": submit_keys, "confirm": outcome}))


if __name__ == "__main__":
    main()
