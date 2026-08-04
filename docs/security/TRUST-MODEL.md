<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Trust model

What Agent Rally Point defends against, what it does not, and who it assumes you are.

This document exists because the model used to be implicit. It was written down in one
place — a comment in `crates/rally-protocol/src/ledger.rs` — and nowhere a user would look.
An independent audit (issue #52) found seven findings, and several of them were not bugs so
much as an unstated assumption meeting a reader who did not share it.

Last reviewed: 2026-08-02, against commit `fdfc750` plus the fixes from that audit.

## Who Rally assumes you are

**One operator, on one machine, running several coding agents you started yourself.**

That is the deployment Rally was built for and the one it is proven on. Every agent in the
room runs as your UID. Every agent can already read your files, run your shell, and use your
credentials. Rally coordinates them; it does not sandbox them, and it never could — a
coordination layer cannot be a privilege boundary between processes that all have your
privileges.

If that describes you, the model holds.

## Where the model stops holding

Three situations break it. Each is real, and Rally's current answer to each is stated plainly
rather than dressed up.

### A second contributor you do not fully trust

`.rally/log/*.jsonl` is committed git content. It replays on a fresh clone. A contributor who
can land a commit can seed facts that a later agent reads as coordination truth.

**What is defended now:** peer-authored prose no longer reaches model context raw. The
SessionStart hook sanitizes it — control characters and newlines stripped, length capped,
value wrapped as quoted data behind a fixed hook-authored preamble that says the following is
peer-authored and unverified. An injected subject cannot forge an instruction line.

**What is not defended:** the fact itself is still unauthenticated. Writers self-supply
`--tool` and role. There is no signature, so a fact claiming to come from `codex:01` may not
have. Committed history and live state are not distinguished at the protocol level.

**A single fact can no longer take the room down (RC-037, RC-038).** Two denial-of-service
paths were live and independently reproduced, both reachable from one committed ledger line:

- One `claim --scope workspace:zzz` conflicted with every later claim of every path by every
  agent, permanently, because a `workspace:` scope overlapped every scope regardless of
  identifier and `append_fact` hard-errors on a claim conflict. Overlap is now decided by
  identifier, not by type: an opaque root contains nothing but itself.
- One `blocker` with no scope flipped `check before-write` to `allow: false` for every agent,
  which `RALLY_HOOK_STRICT=1` turns into a hard deny on every edit. An unscoped blocker from a
  non-lead is now a warning.

**Room-wide effects are checked against the lead seat, and that check is bypassed by one flag.**
`workspace:*` / `repo:*` claims and unscoped freezes compare `fact.tool` against the room lead.
`fact.tool` is **self-asserted** — the same thing this document says two paragraphs above, and the
same thing `skills/agent-rally-point/SKILL.md` tells agents. So passing `--tool <lead-id>` satisfies
the gate. Both bypasses are live-reproduced against the release binary:

```
$ rally say claim --tool honest-lead --scope 'workspace:*' --subject grab   # issued by a rogue
$ rally say claim --tool someone-else --path src/lib.rs --subject work
{"error":"claim conflict: honest-lead holds workspace:* ... "}              # room-wide lockout, restored
```

`rally say blocker --tool <lead-id>` restores the room-wide deny the same way. And the seat itself
is not defended: `rally lead assign --tool rogue --to rogue` succeeds against a **live incumbent**,
so "first join" is not the bar either.

**What the gate actually buys, stated exactly:** it stops the accidental case and the honest one —
an agent that names itself truthfully and has no standing. It does not stop an adversary, and it is
one flag deep. Do not read it as an authorization boundary. Closing it needs authority bound to
something the writer cannot choose (a session identity correlated to a registered session), which
does not exist yet.

An earlier version of this section claimed the gate "raises the bar from any writer to the first
writer". That was wrong, and it was wrong in the specific way this repo's register warns about —
a claim about a control that drifted toward reassurance while the control stayed put. It is
recorded here rather than quietly rewritten.

**If this is your situation:** review `.rally/log/` diffs in pull requests the way you review
code. It is executable content in the sense that matters — it steers agents.

### An untrusted process running as your user

Same UID means same authority. A local process that can write `.rally/` can write facts. A
local process that can read your environment can read `COCKPIT_TOKEN`.

**What is defended now:** Cockpit binds loopback only, refuses to start without a token, and
compares that token in constant time. Sessions and approvals are bound to the connection that
created them, so one authenticated client can no longer steer another's session or resolve
another's approval. `repo_path` must fall inside a configured allowlist, so a token holder
cannot launch an agent at an arbitrary readable path.

**What is not defended:** a process running as you can read the token from your environment
and then act as a legitimate client. Constant-time comparison and owner binding raise the cost
of a stolen or guessed token; they do not defend against an attacker who is already you.

### A compromised release, account, or dependency

**What is defended now:** nothing on the automatic path, because there is no longer an
automatic path. Lifecycle hooks do not download, build, or execute anything. Installing the
`rally` binary is an explicit step you run on purpose, and that step is fail-closed: SHA256
verification before the file is made executable, plus client-side provenance verification via
`gh attestation verify`. If `gh` is missing or the attestation does not verify, the download
path refuses rather than degrading, and points you at `cargo install --path crates/rally-cli`.

**What is not defended:** the source you build from is the source you cloned. If you clone a
malicious fork, verified compilation of malicious source produces a malicious binary. Provenance
verification tells you the artifact came from this repo's release pipeline. It does not tell you
this repo is trustworthy. That judgement is yours and it is made before you clone.

Rally also does not audit its own dependency tree for known vulnerabilities. No `cargo audit` or
`cargo deny` vulnerability pass has been run. The issue #52 auditor did not run one either and
said so.

## Opening this repo runs code. Here is exactly what.

This repo commits hook registrations for four hosts:

- `.claude/settings.json` — Claude Code
- `.codex/hooks.json` — Codex
- `.cursor/hooks.json` — Cursor
- `hooks/hooks.json` — the Claude plugin surface

Opening the repo in one of those hosts and trusting it at the first prompt **auto-loads these
hooks**. That is the intended behaviour and it is the whole reason the coordination works
without setup. It is also a real trust decision, so it is stated here rather than buried.

What the hooks do, after the issue #52 fixes:

| Event | Action |
|-------|--------|
| SessionStart | Register presence in the room. Read room/next/status. Emit a sanitized advisory message. If `rally` is missing, print how to install it. |
| PreToolUse (edits) | Check whether the path you are about to write is claimed by another live agent. Advisory by default. |
| UserPromptSubmit | Refresh idle status. |
| Stop | Record the write completed. |

What the hooks no longer do, as of this audit's fixes: download a binary, `chmod +x` anything,
run `cargo install`, execute a repo-shipped binary to probe it, or write to `~/.local/bin`.
Provisioning moved out of the hook path entirely. See RC-013.

The hooks fail open. They never block an edit, and they exit 0 even when Rally is broken.

### Turning it off

| Scope | Command |
|-------|---------|
| This session | `RALLY_HOOKS=off` |
| This repo | `rally hooks off --scope repo` |
| Check current state | `rally hooks status` |
| Suppress the prompt only | `RALLY_HOOK_PROMPT=off` |

There is no way to make the committed hook registration files not exist while still using the
coordination they wire up. If you do not want committed host hooks in your checkout, this repo
is not for you, and that is a legitimate position.

## What a control here means

The register (`docs/ROOT-CAUSE-REGISTER.md`) uses `controlled` to mean one specific thing: an
adversarial test exists that rejects the hostile input, and that test fails when the fix is
reverted. Not "we changed the code". Not "we updated the docs". A control that has never been
exercised against a real attack string is a hypothesis.

Findings closed as `controlled` in this audit cycle have that evidence recorded in their register
entry, including the mutation result — failed before the fix, passed after.

## Maturity

Honest statement: Rally is proven on a small number of fresh macOS installs, driven by one
operator. It has not been through a broad multi-machine, multi-user deployment. Expect edge
cases on Linux, on hosts other than the four wired here, and in any configuration with more than
one human.

The parts most likely to surprise you are the ones this document says are not defended.
