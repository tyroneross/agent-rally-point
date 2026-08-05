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

Provenance (ARP-R-11 D2): the nudge lands as a USER TURN in the recipient's
pane, so it carries the same `[rally: UNVERIFIED SENDER <who>] ` label the
Rust inject path prefixes (`backends.rs::inject_provenance_label`). Name the
sender with `--tool`; an unnamed wake renders `(none stated)` rather than
being silently unlabelled.

Examples:
  rally_wake.py --tmux-target rev:0.0 --tool claude_code:01 \
      "Read <coordination-file> then continue"
  rally_wake.py --tmux-target main:0.0 "doorbell" \
      --confirm-channel <repo_root>/.rally/ledger.jsonl
"""
import argparse, json, os, re, subprocess, sys, time


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


# ---- provenance label (ARP-R-11 D2) ---------------------------------------
# The wake writes into a live agent's input, where it lands as a USER TURN —
# indistinguishable from something the human operator typed. The Rust inject
# path labels every delivery (`backends.rs`, RC-041 gap 3A); this path did not,
# so a peer could ring a doorbell that read as the operator's own instruction.
#
# Same MIRROR rule as the sanitizer above: the wording, the scrub, and the
# order of operations are transcriptions of the Rust originals, not a second
# design. Do NOT invent a different label here — two spellings of one label is
# a label the recipient cannot rely on.
WAKE_LABEL_MARK = "UNVERIFIED SENDER"          # backends.rs::INJECT_LABEL_MARK
WAKE_SENDER_NONE_STATED = "(none stated)"      # ::INJECT_SENDER_NONE_STATED
WAKE_LABEL_REMOVED = "[trust-label-removed]"   # ::INJECT_LABEL_REMOVED


def wake_provenance_label(sender):
    """Mirror of backends.rs::inject_provenance_label.

    NON-FORGEABLE BY STRUCTURE: `sender` is filtered to the `validate_agent_id`
    allowlist rather than merely sanitized, so it cannot contain the `]` that
    ends the label and cannot append a second, better-looking one after it."""
    rendered = re.sub(r"[^A-Za-z0-9:_-]", "", sender or "")
    return "[rally: {} {}] ".format(WAKE_LABEL_MARK,
                                    rendered or WAKE_SENDER_NONE_STATED)


def _eq_ignore_ascii_case(a, b):
    """Rust's `char::eq_ignore_ascii_case`, NOT Python's `str.lower()`.

    `str.lower()` is Unicode-aware, so U+212A KELVIN SIGN lowercases to `k` and
    a payload spelling the marker with it would be scrubbed here but not by the
    Rust side. A rule the two implementations state differently is a rule with
    a hole (same reasoning as `_is_invisible_or_reordering` above)."""
    if "A" <= a <= "Z":
        a = chr(ord(a) + 32)
    if "A" <= b <= "Z":
        b = chr(ord(b) + 32)
    return a == b


def _match_label_mark(chars, start, words):
    """Mirror of backends.rs::match_label_mark — match `words` at `start`,
    allowing any run of whitespace between words. Returns one past the match.

    `str.isspace()` and Rust's `char::is_whitespace` disagree only on the C0
    separators U+001C–U+001F, and `sanitize_wake_text` has already removed
    every one of those by the time this runs."""
    i = start
    for n, word in enumerate(words):
        if n:
            ws_start = i
            while i < len(chars) and chars[i].isspace():
                i += 1
            if i == ws_start:
                return None
        for wc in word:
            if i >= len(chars) or not _eq_ignore_ascii_case(chars[i], wc):
                return None
            i += 1
    return i


def strip_wake_label_mark(text):
    """Mirror of backends.rs::strip_inject_label_mark — remove any forged copy
    of the marker from a payload. The label is worthless if the payload can
    carry its own, and removing it SILENTLY would let a payload delete the
    evidence of its own attempt, so the scar is visible.

    Call AFTER `sanitize_wake_text`: the sanitizer removes the zero-width
    characters a payload would otherwise hide inside the marker
    (`UNVERIFIED​ SENDER`), so scrubbing second sees the text the human
    will see."""
    words = WAKE_LABEL_MARK.split(" ")
    chars = list(text)
    out, i = [], 0
    while i < len(chars):
        end = _match_label_mark(chars, i, words)
        if end is not None:
            out.append(WAKE_LABEL_REMOVED)
            i = end
        else:
            out.append(chars[i])
            i += 1
    return "".join(out)


def deliverable_wake_text(sender, text):
    """The exact text this script delivers: sanitized, label-scrubbed, then
    prefixed with this delivery's provenance line — the same three steps, in
    the same order, as backends.rs::deliverable_inject_text."""
    return wake_provenance_label(sender) + strip_wake_label_mark(
        sanitize_wake_text(text))


def self_test(path):
    """Grade this file's sanitizer against the shared fixture list. Exit 0 when
    every case matches; print each mismatch and exit 1 otherwise.

    `failures` grades `sanitize_wake_text` alone (the rule the fixture list
    states). `deliverable_failures` grades the COMPOSITION the pane actually
    receives against the same list, which is what the Rust side grades via
    `plan_delivery`: label + scrub(sanitize(input)). Grading only the leaf
    would let the two sides compose the same functions in different orders and
    still both report green."""
    with open(path, encoding="utf-8") as f:
        cases = json.load(f)["cases"]
    failures, deliverable_failures = [], []
    label = wake_provenance_label("selftest")
    for case in cases:
        got = sanitize_wake_text(case["input"])
        if got != case["expected"]:
            failures.append({"name": case["name"],
                             "expected": case["expected"], "got": got})
        want = label + strip_wake_label_mark(case["expected"])
        delivered = deliverable_wake_text("selftest", case["input"])
        if delivered != want:
            deliverable_failures.append({"name": case["name"],
                                         "expected": want, "got": delivered})
    print(json.dumps({"cases": len(cases), "failures": failures,
                      "deliverable_failures": deliverable_failures},
                     ensure_ascii=True))
    return 1 if failures or deliverable_failures else 0


# ---- tmux target validation (ARP-R-11 D1) ---------------------------------
# `--tmux-target` was passed straight to `tmux -t`. `-t` takes a required
# argument, so a hostile value cannot become a flag by itself — but an
# unvalidated one selects a pane by tmux's own fuzzy rules (a bare name matches
# any session with that prefix), so a typo or a crafted value rings the wrong
# agent's doorbell, and there is no confirmation channel that would notice.
# Refuse anything outside the documented target grammar instead.
#
# Grammar per tmux(1) "COMMANDS": the id forms %pane / @window / $session, a
# bare session name, or [session]:[window][.pane]. Session and window names in
# this repo are `rally-*` / plain identifiers; the allowlist below deliberately
# excludes the `{...}`, `+`/`-` and `!` relative forms, which no rally caller
# needs and which are exactly the shapes that resolve to "whatever is current".
_TMUX_NAME = r"[A-Za-z0-9_][A-Za-z0-9_.-]*"
TMUX_TARGET_RE = re.compile(
    r"\A(?:"
    r"[%@$][0-9]+"                                       # %pane / @window / $session
    r"|" + _TMUX_NAME +                                  # bare session name
    r"|(?:" + _TMUX_NAME + r")?"                         # [session]
    r":(?:" + _TMUX_NAME + r")?"                         # :[window]
    r"(?:\.%?[0-9]+)?"                                   # [.pane]
    r")\Z",
    re.ASCII,
)


def validate_tmux_target(target):
    """Return `target` unchanged, or raise ValueError. Refuses rather than
    escapes: there is no quoting that makes an unknown target shape safe, and a
    doorbell rung at the wrong pane is silent by design (no confirmation).

    `\\A`/`\\Z` rather than `^`/`$` on purpose — `$` also matches before a
    trailing newline, which would admit `rev:0.0\\nkill-server`."""
    if not isinstance(target, str) or not target:
        raise ValueError("tmux target is required")
    if len(target) > 128 or not TMUX_TARGET_RE.match(target):
        raise ValueError(
            "refusing tmux target {!r}: expected session:window.pane, a bare "
            "session name, or a %pane/@window/$session id".format(target))
    return target


# ---- tmux backend (no agent-status API; address pane explicitly, doorbell is low-harm) ----
def _hex_tokens(text):
    """UTF-8 bytes as the lowercase 2-hex-digit tokens `send-keys -H` expects,
    one per byte — the same encoding as backends.rs::hex_tokens."""
    return ["{:02x}".format(b) for b in text.encode("utf-8")]


def tmux_wake_commands(target, text, sender=""):
    """THE chokepoint: the only place in this script that names tmux.

    Returns the complete argv for ONE `tmux` invocation. Every guard is applied
    here so that "wrote to a pane" and "sanitized + labelled + validated" are
    the same code path, not two that a future caller has to remember to pair
    (`tests/scripts/test_rally_wake.py` asserts that structurally).

    ARP-R-11 D1 — two separate hazards, both closed by the encoding:
      * `--` terminates flag parsing. CONFIRMED against tmux 3.6a
        `arguments.c::args_parse_flags`: the exact token `--` is consumed and
        stops the flag loop, so every later value is positional.
      * the payload is sent as `-H` hex tokens rather than `-l` text. A hex
        token can neither begin with `-` (flag) nor end with `;` (tmux 3.6a
        `cmd-parse.y` ends a command at any argument with an unescaped trailing
        semicolon — which matters now that the three writes share one
        invocation). `-l` alone would leave both shapes reachable from payload
        text. This is also the encoding the Rust inject path already uses.

    ARP-R-11 D3 — what IS and IS NOT atomic after this change:
      IS: one process, one client→server round trip. tmux runs a `;`-separated
          command sequence in order and, per tmux(1), "if a command in the
          sequence encounters an error, no subsequent commands are executed" —
          so a failed C-u cannot be followed by a payload, and a failed payload
          cannot be followed by a bare Enter. The old two-`tmux`-process shape
          could leave exactly those half-states.
      IS NOT: a lock on the pane. Each command is a separate item on the
          server's queue, and nothing stops the pane's human (or another
          client) from typing between our C-u and our payload. The window is
          reduced from two process spawns to intra-server sequencing; it is not
          eliminated, and no caller should assume it is."""
    target = validate_tmux_target(target)
    body = deliverable_wake_text(sender, text)
    # C-u first: clears whatever is already sitting at the prompt, so a stale
    # half-typed line cannot be prefixed to the nudge (and, with it, submitted).
    # C-u / C-m need no `--`: they are literals here, never caller-controlled.
    return (
        ["tmux", "send-keys", "-t", target, "C-u", ";",
         "send-keys", "-t", target, "-H", "--"]
        + _hex_tokens(body)
        + [";", "send-keys", "-t", target, "C-m"]        # submit
    )


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
    ap.add_argument("--tool", default="",
                    help="the sender id to name in the provenance label; "
                         "unset renders " + WAKE_SENDER_NONE_STATED
                         + " (self-asserted — rally authenticates nothing)")
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

    # Build BEFORE branching, so --dry-run exercises the same validation and the
    # same argv the real wake would run (a dry run that skipped the guards would
    # certify a target that the live path then refuses).
    try:
        commands = tmux_wake_commands(target, a.message, a.tool)
    except ValueError as exc:
        sys.exit(json.dumps({"error": str(exc), "target": target}))

    if a.dry_run:
        print(json.dumps({"would_wake": target, "backend": backend, "agent": agent,
                          "status": status, "submit_keys": ["C-m"],
                          "label": wake_provenance_label(a.tool),
                          "argv": commands}, ensure_ascii=True))
        return

    base = channel_lines(a.confirm_channel) if a.confirm_channel else None
    run(commands)
    submit_keys = ["C-m"]

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
