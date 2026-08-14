# Adopted tmux sessions always classify stale on tmux 3.6a (probe format newline collapse)

**Date:** 2026-08-07 · **Status:** ⚠️ **NOT REPRODUCIBLE — diagnosis below is disproven; do not implement the fix.** See "2026-08-13 re-test" · **Origin:** OC task e9ac385c (Operations Center interactive-peer presence, commit e06f95d) reported `rally adopt` panes as `inject_status=stale_managed_session` while `rally run claude` sessions were injectable.

## 2026-08-13 re-test — the stated root cause does not hold

The underscore-substitution mechanism below could not be reproduced, and neither could the symptom.
No code change is warranted. ✅ Verified on macOS, tmux 3.6a, rally 0.2.1+8d21f2c.

**1. tmux 3.6a does not substitute control characters in format output.** All four combinations,
byte-checked with `od -c`:

| Command | Format contains | Output |
|---|---|---|
| `display-message -p` | real newline | `A \n B \n` — preserved |
| `display-message -p` | literal `\n` (2 chars) | `A \ n B \n` — literal, not `_` |
| `list-panes -a -F` | real newline | `A \n B \n A \n B \n` — preserved |
| `list-panes -a -F` | literal `\n` (2 chars) | `A \ n B \n A \ n B \n` — literal |

No `_` appears in any case. Note the original evidence at step 2 used the *literal* two-character
`\n`, whereas Rust's `"\n"` in the probe emits a *real* newline — the two were conflated.

**2. The probe itself returns correctly delimited output.** Running the exact command from
`probe_tmux_liveness` against live sessions yields `ambient-t2\n@1\n%1\nrallyprobe\n@3\n%3\n` —
three tokens per pane, session name matchable.

**3. The end-to-end symptom is gone.** Re-running the reproduction below verbatim (clean repo,
`rally init`, `tmux new-session -d -s adoptrepro`, `rally adopt …`) gives:
`liveness: "live"`, `liveness_source: "backend_probe"`, `injectable: true`,
`inject_status: "live_managed_session"` — the opposite of the reported failure.

**4. No fix landed in between.** `git log 542c884..8d21f2c -- crates/rally-cli/src/backends.rs` is
empty; the probe format string is still newline-delimited. The symptom therefore resolved through
something outside this code path, or the original environment differed in a way not captured here
(⚠️ the original tmux server's options were not recorded and cannot now be inspected).

**Do not apply the "Fix direction" below.** Changing the format to space-delimited would edit
working code to satisfy a mechanism that does not exist, and would lose the exact per-line
delimiting the current format provides. Reopen only with a fresh byte-level `od -c` capture of the
probe output showing the failure.

## Verdict

`rally adopt` is not broken. The **tmux liveness probe** is broken on tmux 3.6a (and any tmux that sanitizes control characters out of format output): the probe's newline-delimited format string collapses into underscore-joined tokens, so no session name or pane id ever matches, and **every tmux-backed session — adopted or run — classifies Stale**. `rally run claude` escapes only because `--backend auto` resolves to **ptyd** on this machine (the rally-owned socket `~/.local/share/rally/ptyd.sock` is live), and ptyd liveness is a daemon `pane.list` RPC that never touches the tmux probe.

✅ Verified by clean-repo reproduction with the installed binary (0.2.0+542c884) and by running the probe command directly. The binary-vs-source skew suspected in the original report is ruled out: `crates/rally-cli/src/backends.rs` is byte-identical between 542c884 and v0.2.1-3-gd44ef90.

## Causal chain

1. `probe_tmux_liveness` (`crates/rally-cli/src/backends.rs:1355`) runs
   `tmux list-panes -a -F '#{session_name}\n#{window_id}\n#{pane_id}'`
   expecting three lines per pane.
2. tmux 3.6a replaces control characters in format *output* with `_`. Verified directly:
   - `tmux display-message -p 'line1\nline2'` → `line1_line2`
   - `tmux display-message -p 'a\tb'` → `a_b` (tabs too)
   - the probe emits `adoptrepro_@0_%0` — **one** token per pane, not three lines.
3. `target_tokens` (`backends.rs:1940`) whitespace-splits the output; `classify_probe_output` (`backends.rs:1903`) marks a target Live only if a token equals it exactly. `adoptrepro_@0_%0` matches neither the session name `adoptrepro` nor the pane id `%0` → **Stale**.
4. `projected_session_liveness` (`crates/rally-cli/src/lib.rs:7178`) gives a definitive backend probe authority over the heartbeat TTL (the P1c fix), so a fresh heartbeat cannot rescue the verdict.
5. Stale → `managed_session_injectability` returns `injectable:false, inject_status:stale_managed_session` (`lib.rs:7087`), and `rally inject` refuses (exit 1).

This also explains every variant tried in the original report failing identically (session name, `%0` pane id, `rally-` prefixed name, `RALLY_PARENT_PID`, explicit `--tmux-bin`): the probe never matches *any* target, so nothing on the adopt side can help.

## Reproduction (clean repo, installed rally 0.2.0+542c884, tmux 3.6a, macOS)

```
mkdir scratch && cd scratch && git init && git commit --allow-empty -m init
rally init
tmux new-session -d -s adoptrepro
rally adopt adoptrepro --tmux adoptrepro --tool adoptrepro --agent claude --json   # succeeds
rally sessions --json   # liveness:stale, liveness_source:backend_probe, injectable:false
tmux list-panes -a -F '#{session_name}
#{window_id}
#{pane_id}'
# → adoptrepro_@0_%0        <-- newlines collapsed to underscores
```

## Scope and blast radius

- **Affected:** all `backend:tmux` sessions on tmux ≥ 3.6a *(⚠️ version boundary untested — reproduced on 3.6a only; the underscore substitution is not named in the tmux CHANGES file, so the first affected release is unconfirmed)*. That includes sessions minted by `rally run --backend tmux` and every `rally adopt --tmux`, which has no ptyd path at all. cmux (`list-workspaces`, no format string) and ptyd (daemon RPC) probes are unaffected.
- **Consequences beyond inject:** a permanent false Stale also feeds the orphan reaper's staleness criterion and `build_agent_injectability`'s room advice ("run `rally sessions --reap`"), which recommends reaping *live* panes.
- **Why it looked adopt-specific:** on machines where the rally ptyd daemon is running, `auto` routes `rally run` to ptyd, leaving tmux-probe breakage visible only through adopt.
- The probe format dates to dc342d4 (2026-06-06, "fix(rally): project managed session liveness") — it was not broken by a recent rally change.

## Why tests never caught it

`classify_probe_output` unit tests feed synthetic probe output containing real newlines; the journey test (`stale_managed_session_projects_reaps_and_blocks_inject`, `tests/user_journey.rs:1712`) uses stub bins. Nothing executes a real tmux ≥ 3.6a format round-trip.

## Fix direction (not implemented here)

Stop putting control characters in the tmux format string. Two candidates:

1. **Minimal:** change the format to space-delimited — `-F '#{session_name} #{window_id} #{pane_id}'`. `target_tokens` already whitespace-splits, so matching works unchanged. Verified the output survives: `adoptrepro @0 %0`. Session names containing spaces mis-tokenize, but they already did under whitespace-split matching, so this loses nothing.
2. **Stricter:** two probes (`list-panes -a -F '#{session_name}'` and `-F '#{pane_id}'`) with exact per-line matching — fixes the spaces-in-names limitation too, at the cost of a second subprocess.

Either way, add a regression test that asserts the format string contains no control characters, since the live-tmux behavior is untestable in CI stubs.
