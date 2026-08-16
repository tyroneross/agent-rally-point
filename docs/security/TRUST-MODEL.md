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

Last reviewed: 2026-08-10, against base `61e4a789` plus the O33-A changes committed with this document.

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

`.rally/log/*.jsonl` is no longer committed (see §"What a fresh clone of this repository gives
you"), so a fresh clone starts empty. That removes the seeded-room problem for THIS repo and does
not remove the underlying exposure: the ledger is still a file on a shared machine, and a project
that chooses to commit it — the default before 2026-08-04, and still a defensible choice for a
private repo — gets a room a contributor can seed with facts a later agent reads as coordination
truth.

**What is defended now:** peer-authored prose no longer reaches model context raw. The
SessionStart hook sanitizes it — control characters and newlines stripped, length capped,
value wrapped as quoted data behind a fixed hook-authored preamble that says the following is
peer-authored and unverified. An injected subject cannot forge an instruction line.

Since the room message became a single short line (v0.2.5), two further controls apply to the
lifecycle message. Every closing guillemet is followed by the hook-authored literal
`(untrusted)`, which a peer cannot forge because `scrub()`'s allowlist excludes guillemets and
`prose()` rewrites them to `"`. And the headline segment is hook-authored narration: it carries
no peer prose at all, and the actor it names must match a host pattern with a lowercase short
id, else it renders as the fixed literal `a peer`. That second gate exists because the
identifier-shape gate alone admits `human:HALT` and `sudo:EXEC`, which would otherwise have read
as instructions in the one position the preamble calls hook narration.

The same change added a third surface that the two controls above do not cover. The message now
renders a `rally ...` command, and that command is deliberately NOT wrapped in guillemets, because it
has to stay copy-pasteable. Its `--ref`, `--target` and `--id` values are peer-influenced: they come
from a ledger fact. `safeCommand()` is the whole defence there. It forces every value token through
the same `isBareShape()` + `scrub()` gate that `ident()` uses, requires the `--tool` argument to equal
the reading agent's own id (which arrives on argv, never from the ledger), constrains flags to
`--[a-z-]+`, rejects any residual quote character, and rejects `rally say claim` outright so no
message can advise a claim takeover. Any rejection falls back to `rally next --tool <you> --audit
--json`. If you weaken `safeCommand()`, you are removing a shipped defence, not tidying a helper:
`test_room_message_contract.sh` cases G-p and G-k fail when it is reverted.

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
  non-lead is now a warning. The room-wide deny is still reachable under strict mode by a writer
  that passes `--tool <lead-id>`; §"Opening this repo runs code" below states what that costs.

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

`rally say blocker --tool <lead-id>` restores the room-wide deny the same way.

**The seat itself is now gated, to exactly the same depth and no further (ARP-R-01).** Until
2026-08-04 the seat had no gate at all: `rally lead assign --tool rogue --to rogue` succeeded
against a **live incumbent** and against a `--user-designated` one, and
`rally lead relinquish --tool rogue` vacated it to null. One command under a rogue's own honest
name granted every room-wide capability above. A transfer now requires a leaderless room, an
actor that IS the incumbent, an incumbent silent past the reclaim window, or an explicit
`--force` that records the seizure and names who it displaced.

That closes the honest-name path and nothing beyond it. `rally lead assign --tool <incumbent>
--to rogue` still succeeds, because the gate compares `fact.tool`, and `fact.tool` is the same
self-asserted field this document has already said twice cannot be trusted. So all three
room-wide controls — the wildcard claim, the freeze, and the seat — now share one residual
rather than the seat being an open door beneath the other two.

**What the gates actually buy, stated exactly:** they stop the accidental case and the honest one
— an agent that names itself truthfully and has no standing. They do not stop an adversary, and
they are one flag deep. Do not read them as an authorization boundary. Closing them needs
authority bound to something the writer cannot choose (a session identity correlated to a
registered session), which does not exist yet. `crates/rally-cli/tests/lead_seat_authz.rs`
contains a test named `impersonation_is_not_stopped_and_this_test_says_so` that ASSERTS the
bypass still works, so this paragraph cannot quietly drift out of date: if identity ever becomes
authoritative, that test fails and forces the rewrite.

**Two things the fix also corrected, both of which had made the room harder to reason about.**
The ledger recorded a seizure as authored by the agent that GAINED the seat, so the one field an
investigator would read named the wrong agent; the actor is now the author and the beneficiary is
the target. And the freeze verdict was computed against the CURRENT lead on every check rather
than against the lead when the blocker was written, so the same fact id armed into a room-wide
deny once its author later took the seat, and a legitimate freeze disarmed the moment anyone else
took it. Authority is now decided once, as of the fact's own position in the ledger.

An earlier version of this section claimed the gate "raises the bar from any writer to the first
writer". That was wrong, and it was wrong in the specific way this repo's register warns about —
a claim about a control that drifted toward reassurance while the control stayed put. It is
recorded here rather than quietly rewritten.

**If this is your situation:** if you commit `.rally/log/`, review its diffs in pull requests the
way you review code. It is executable content in the sense that matters — it steers agents.

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
| PreToolUse (named pure read) | Return `{}` before the wrapper's repo walk or Rally resolution. No status, check, claim, or Rally subprocess. The generated Codex launcher may first run `git rev-parse` to locate the wrapper. |
| PreToolUse (opaque shell) | Return `{}` with no Rally call; command text is not accepted as proof of a read or a write target. |
| PreToolUse (named mutation) | Validate every declared target, complete every path check, then create one all-or-none aggregate repeated-path claim. A denial, timeout, invalid response, or containment failure creates zero claims. Advisory by default. |
| PreToolUse (unknown/malformed) | In an enabled Rally repo, return `{}` plus one bounded atomically rate-limited diagnostic; no Rally status, check, or claim. Outside Rally or when hooks are disabled, return silently. |
| UserPromptSubmit | Refresh idle status. |
| Stop | Record the write completed. |

What the hooks no longer do, as of this audit's fixes: download a binary, `chmod +x` anything,
run `cargo install`, execute a repo-shipped binary to probe it, or write to `~/.local/bin`.
Provisioning moved out of the hook path entirely. See RC-013.

**Native effect classification is routing, not authorization.** The hook trusts
the host envelope only enough to choose whether automatic deconfliction is
possible. A name in `pure_read` does not grant filesystem authority, and a tool
or plugin that lies about its name/effect can still mutate with the operator's
UID. Opaque shell tools are intentionally not parsed: `rg` and `rm` share the
same command carrier, and shell text is not a typed resource declaration. An
agent using shell for a mutation must make the exact claim and strict
before-write check explicitly. The registry prevents accidental read claims; it
does not sandbox tools.

`apply_patch` target extraction reads directive headers only. It never scans
added/deleted body text for path-like strings. Codex 0.144.3's source-proven
carrier is `tool_input.command`; `tool_input.patch` is retained only for named
legacy adapters. Add/update/delete/move directive paths must be relative to the
validated event `cwd`. Other named mutation envelopes such as Claude
`Write`/`Edit` may carry an absolute `file_path`, accepted only when physical
containment resolves it inside the Rally root. One identity-whitespace, empty,
malformed, root-equal, outside-root, or symlink-escaping target rejects the
entire automatic check before Rally runs. Only an absent target alias is
optional; a present null/blank move destination invalidates the transaction. A
new nested path resolves from the nearest physical existing ancestor; an
unresolved suffix containing `..` rejects atomically rather than relying on
lexical normalization through missing directories. A
present `tool_input`, `toolInput`, or `input` carrier must be an object and never
falls back to an outer-envelope `path` when malformed. Unknown tool names and
malformed envelopes never inherit the old generic `path` fallback. Their
rate-limit marker under `.rally/.hook-seen` is created atomically only after the
Rally self-gate and hooks-enabled check.
Path-bearing Rally arguments use attached `--name=value` form, so a valid
filename beginning with `-` cannot be reparsed as a CLI option.

The wrapper rejects more than 16 declared mutation targets atomically, emits a
bounded diagnostic, and makes zero Rally calls. It never claims a checked
prefix. Larger mutations require an explicit strict check and exact manual
claim for every target; a future batch primitive should remove this degraded
mode. The current worst-case Rally budget is 400 ms hook settings + 400 ms
working status + at most 4,000 ms total path checks + 400 ms room + 1,000 ms
aggregate claim = 6,200 ms, leaving 3,800 ms under the generated 10-second host
timeout. At the 16-target ceiling, each check receives 250 ms. This arithmetic
is enforced by immediate `KILL` at each millisecond deadline; no per-call TERM
grace is added. Without a millisecond-capable `timeout`/`gtimeout` or
high-resolution Perl guard, classified mutation coordination degrades before
Rally and creates no automatic claim. The arithmetic proves the configured
bound, not real-host latency; O33-D owns quiesced installed-surface measurement.
Native Windows drive/backslash containment is `UNKNOWN` and unsupported by the
currently proven macOS/Linux wrapper.

The classifier and rendered coordination output require `node`. When `node` is
absent during `PreToolUse`, the hook first applies the Rally self-gate, emits a
once-per-session mutation degradation warning, returns exact `{}`, and makes no
Rally status/check/claim call. It cannot prove a native effect from JSON, so a
successful no-node hook exit is not deconfliction evidence. Lifecycle phases
retain their separate fail-open status behavior.

Pure reads receive active-writer context at SessionStart/UserPromptSubmit, not
through a per-read Rally call. Until the planned engagement-bound source-token
projection lands, a reader who uses a file for a decision must treat it as
provisional when a writer is active and re-read/revalidate immediately before
the conclusion. Parallel reading is allowed; stale evidence is not.

O33-A is therefore a branch-held prerequisite, not a safe standalone
activation. It may be committed only on its isolated branch and must not be
merged, cherry-picked, or checked out into central integration, local main, an
installed plugin, a pushed ref, or any user-active worktree until O33-B and
O33-C also pass the combined A+B+C gate. Build B on top of A in isolation and
integrate the combined chain only after post-O26 C is complete. The project
Codex and Claude hooks are already active for new sessions, so a local-main
merge would itself activate A's read bypass. O33-C supplies path-scoped writer
context, a source token, and deterministic final revalidation; turn-level
context alone can be stale or omit the relevant path.

**The hooks fail open by default, and three opt-in switches make mutation checks fail closed.**
Known reads, opaque shell tools, and unknown/malformed operations return `{}` before that gate.
For a named mutation, the default posture surfaces a warning while the edit goes through, and
every hook exits 0 even when Rally is broken. The exit code stays 0 in every posture — a refusal
travels in the hook's JSON, not its exit status. Each switch below is off unless you set it:

| Switch | What it does |
|--------|--------------|
| `RALLY_HOOK_STRICT=1` | The hook emits `permissionDecision: "deny"` (PreToolUse) or `decision: "block"` (Stop) on a high-severity signal — `severity == "stop"` or `allow == false`. Low-severity findings stay advisory. Codex PreToolUse stays fail-open because Codex rejects the Claude `permissionDecision` field. |
| `rally check before-write --strict` | Exits 4 when a stop finding is present, so a wrapper that reads the exit code aborts the write. The canonical agent loop in `README.md`, `RALLY.md`, and `skills/agent-rally-point/SKILL.md` passes `--strict`, so this path is on for anyone who copies it. |
| `RALLY_BEFORE_WRITE_FAILCLOSED=1` or `--fail-closed` | Makes `check before-write` exit 4 when its snapshot read exceeds the watchdog timeout, instead of exiting 0 with a neutral envelope. Applies to `check before-write` only. `--fail-open` on the same call reasserts the default. |

One more fail-closed path is **not** opt-in, and it is not an edit gate: a mutating command
(`rally say`, `enter`, `inject`, `lead handoff`, `backlog add`, ...) exits 4 when the watchdog
fires before the write commits, so a caller cannot read a timeout as a successful append. Your
edit is unaffected, and the hooks that call these commands still exit 0.

**What strict mode does to the room-freeze denial of service (RC-038).** Under
`RALLY_HOOK_STRICT=1` an unscoped blocker is not advisory. It flips `check before-write` to
`allow: false` for every agent in the room, and strict mode converts that into
`permissionDecision: "deny"` on every edit by every agent — a room-wide halt from one committed
ledger line, persisting until someone resolves the blocker. The lead-seat gate narrows who can
raise it: an unscoped blocker from a non-lead now degrades to a warning. That gate compares
self-asserted `--tool`, so `rally say blocker --tool <lead-id>` still lands it, exactly as the
live reproduction above shows. **Strict mode plus one forged `--tool` still freezes the room.**
Do not enable `RALLY_HOOK_STRICT=1` in a room whose `.rally/log/` you would not review line by
line before trusting it.

### Turning it off

| Scope | Command |
|-------|---------|
| This session | `RALLY_HOOKS=off` |
| This repo | `rally hooks off --scope repo` |
| Check current state | `rally hooks status` |
| Suppress the prompt only | `RALLY_HOOK_PROMPT=off` |
| Room-message detail | `rally hooks room-detail --brief` (default) / `--verbose` for the full roster; `RALLY_HOOK_ROOM_DETAIL` overrides for one session |

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

## What a fresh clone of this repository gives you (ARP-R-06)

Until 2026-08-04 this repo committed its own live coordination room. Cloning it did not give you
the project's history — it gave you the maintainer's working state: 3,680 facts replaying into
93 unreleased claims, 60 open handoffs addressed to specific agent seats, 84 agent identities,
and a foreign lead seat, on paths that do not exist in your checkout. It also carried 956
occurrences of the maintainer's hostname and home directory across 25 tracked files, with active
claim scopes naming personal files and private sibling repositories, plus 18 git bundles
(68.6 MiB) holding full pre-sanitization history of this repo **and of repositories that are not
this one**.

`.rally/log/`, `.rally/archive/`, `.rally/RETROSPECTIVE.md`, and `archive/` are no longer tracked.
A fresh clone now starts with an empty room, which is the correct state for someone else's
repository. `.rally/manifest.json` still ships, so an agent landing in the clone still finds the
rally point.

**What de-tracking does not do, stated plainly because the difference matters.** Every one of
those bytes is still in git history. `git log` recovers all of it, including the bundles. Removing
them for real means rewriting history and force-pushing, which is irreversible, breaks every
existing clone and fork, and is a decision that has not been made. Until it is:

- Treat this repository's history as containing the maintainer's machine paths, hostname, and
  coordination ledger.
- The bundles **have** been audited for credentials, and the audit found none. A regex sweep over
  compressed packfiles is not a credential audit, so this was not one: on 2026-08-05 all 18 bundles
  were `git bundle verify`-ed, fetched into throwaway bare repos, and scanned blob-by-blob after
  decompression — 1,252–1,812 blobs each across 626 distinct paths, including files deleted in
  history. The scanner was mutation-validated first: a `.env`, an `id_ed25519`, AWS credentials, a
  JWT, a `ghp_` token and a Slack token were planted and then deleted so they existed only in
  history; all 11 detector classes fired, none missed. Two hits total, both benign — a repeated
  `deadbeef` test placeholder in `PtydProtocolTests.swift`, and a Swift variable assignment matching
  the regex. Zero hits for private keys, cloud keys, provider tokens, JWTs, connection strings, or
  `.env` / `.npmrc` / `.netrc` / `id_rsa` / `.pem` files. Two independent false-negative checks (a
  626-path inventory and an entropy sweep) came back empty.

  What this does **not** clear is the machine paths, hostname, and coordination ledger described
  above — those are still in the history and are not secrets to be ruled out, they are present by
  inspection. De-tracking the bundles was a size fix, not a privacy fix.

If you are evaluating this repo for internal use, that history is what you are taking on.

**Resetting a room you inherited.** If you cloned before this change, or you want a clean room in
a fork: delete `.rally/log/`, `.rally/archive/`, and the derived caches (`.rally/facts.db`,
`.rally/room.db`, `.rally/*.json` other than `manifest.json`), then run any `rally` command. The
room rebuilds empty from the absent ledger.
