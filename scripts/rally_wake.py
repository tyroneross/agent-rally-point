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
import argparse, json, os, subprocess, sys, time


def run(cmd, parse=False):
    p = subprocess.run(cmd, capture_output=True, text=True)
    if p.returncode != 0:
        sys.exit(f"{' '.join(cmd)} failed: {p.stderr.strip() or p.stdout.strip()}")
    return json.loads(p.stdout) if parse else p.stdout


# ---- sanitization (RC-041 gap 3D) -----------------------------------------
# This script used to hand the raw message straight to `tmux send-keys -l`,
# which made the Rust chokepoint comment false: `sanitize_inject_text`
# (crates/rally-cli/src/backends.rs) claims "no future caller can route around"
# it, and this caller did.
#
# The fix is a MIRROR, not a redirect. Routing through `rally inject` was the
# preferred shape and does not work here: `rally inject` resolves its target to
# a managed session record or a rally-termd agent id, and this script exists
# precisely for a pane rally never launched, addressed as `session:window.pane`.
# Sending through the CLI would write a ledger directive nobody delivers and
# leave the pane untouched - a silent no-op in place of a wake. So the rule is
# implemented twice and GRADED ONCE, against
# crates/rally-cli/tests/inject_sanitizer_cases.json, from both sides.
#
# Deliberately NOT using `unicodedata.category`, even though Python ships the
# full table and Rust does not: a rule the two implementations state
# differently is a rule with a hole. The Rust enumeration is canonical; the
# ranges below are its transcription, in the same order, with the same
# known limit (unassigned Cn is not covered beyond the noncharacters).
SANITIZER_FIXTURES = os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    "crates", "rally-cli", "tests", "inject_sanitizer_cases.json",
)


def _is_invisible_or_reordering(cp):
    """Mirror of backends.rs::is_invisible_or_reordering - see it for the why
    of each range."""
    if cp & 0xFFFE == 0xFFFE:          # every U+xFFFE / U+xFFFF: noncharacter
        return True
    return (
        cp == 0x00AD                                    # SOFT HYPHEN
        or 0x0600 <= cp <= 0x0605                       # Arabic number signs
        or cp in (0x061C, 0x06DD, 0x070F, 0x08E2)       # ALM, ayah, SAM
        or 0x0890 <= cp <= 0x0891
        or cp == 0x180E                                 # MONGOLIAN VOWEL SEP
        or 0x200B <= cp <= 0x200F                       # ZWSP..RLM
        or cp in (0x2028, 0x2029)                       # Zl, Zp
        or 0x202A <= cp <= 0x202E                       # bidi embed/override
        or 0x2060 <= cp <= 0x206F                       # word joiner..deprecated
        or 0xFDD0 <= cp <= 0xFDEF                       # noncharacters
        or cp == 0xFEFF                                 # BOM / ZWNBSP
        or 0xFFF9 <= cp <= 0xFFFB                       # interlinear annotation
        or 0x1D173 <= cp <= 0x1D17A                     # musical format
        or 0xE0000 <= cp <= 0xE00FF                     # TAG block
        or 0xE000 <= cp <= 0xF8FF                       # private use (BMP)
        or 0xF0000 <= cp <= 0xFFFFD                     # private use (plane 15)
        or 0x100000 <= cp <= 0x10FFFD                   # private use (plane 16)
    )


def sanitize_wake_text(text):
    """Drop every control/format/reordering character except TAB.

    Same contract as the Rust side: TAB survives, C0/C1/DEL do not, and neither
    does anything invisible or direction-flipping. A newline is dropped because
    the submit key is sent separately - a body newline would submit a partial
    line."""
    return "".join(
        ch for ch in text
        if ch == "\t"
        or not (_is_control(ch) or _is_invisible_or_reordering(ord(ch)))
    )


def _is_control(ch):
    """General category Cc, matching Rust's char::is_control exactly."""
    cp = ord(ch)
    return cp <= 0x1F or 0x7F <= cp <= 0x9F


def self_test(path):
    """Grade this file's sanitizer against the shared fixture list. Exit 0 when
    every case matches; print each mismatch and exit 1 otherwise."""
    with open(path, encoding="utf-8") as f:
        cases = json.load(f)["cases"]
    failures = []
    for case in cases:
        got = sanitize_wake_text(case["input"])
        if got != case["expected"]:
            failures.append({"name": case["name"],
                             "expected": case["expected"], "got": got})
    print(json.dumps({"cases": len(cases), "failures": failures}, ensure_ascii=True))
    return 1 if failures else 0


# ---- tmux backend (no agent-status API; address pane explicitly, doorbell is low-harm) ----
def tmux_send(target, text):
    # `-l` writes the argument literally, so a control byte in `text` would be
    # a keystroke, not content: the same keystroke-injection class the Rust
    # framer closes. Sanitize first, always.
    run(["tmux", "send-keys", "-t", target, "-l", sanitize_wake_text(text)])
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
    ap.add_argument("message", nargs="?",
                    help="SHORT doorbell nudge — point to the mailbox, don't carry the payload")
    ap.add_argument("--self-test", nargs="?", const=SANITIZER_FIXTURES, default=None,
                    metavar="FIXTURES_JSON",
                    help="grade sanitize_wake_text against the shared fixture list and exit")
    # Required for a real wake, but NOT for --self-test, which touches no pane.
    ap.add_argument("--tmux-target",
                    help="tmux target, e.g. session:window.pane (required to wake)")
    ap.add_argument("--confirm-channel",
                    help="changes.jsonl path; success = a new line appears (agent posted back)")
    ap.add_argument("--confirm-timeout", type=float, default=45.0)
    ap.add_argument("--max-nudge-chars", type=int, default=300,
                    help="warn above this — paste-collapse risk")
    ap.add_argument("--dry-run", action="store_true")
    a = ap.parse_args()

    if a.self_test:
        sys.exit(self_test(a.self_test))
    if not a.tmux_target:
        ap.error("--tmux-target is required")
    if a.message is None:
        ap.error("a doorbell message is required")

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
