<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Triage — issue #52 security audit (Lattice)

**Source:** GitHub issue #52, "Feedback - from Lattice", author `supsup`.
**Reviewed commit:** `fdfc750` (main).
**Triaged:** 2026-08-02.
**Findings:** 7 — 3 Critical, 1 High, 2 Medium, 1 Low.

This was the first genuinely independent security read of this repo. Everything before
it was self-authored or solicited. That distinction turned out to matter — see
[`rca-2026-08-02-security-findings-escaped.md`](../rca-2026-08-02-security-findings-escaped.md).

## How findings were triaged

Four dispositions:

| Disposition | Meaning |
|-------------|---------|
| **fix-now** | The defect is real, bounded, and fixable in this run with an adversarial test. |
| **redesign** | The fix is an architecture change. Land a fail-safe now; register the redesign. |
| **threat-model-scoping** | Partly a documentation gap — the deployment this was built for is narrower than the one the finding assumes. Say so honestly, fix what is cheap. |
| **low** | Real, worth fixing, not urgent. |

Register practice is unchanged and binding: **every finding gets an entry in
[`ROOT-CAUSE-REGISTER.md`](../ROOT-CAUSE-REGISTER.md), and an entry closes only when an
adversarial control is proven** — a test that rejects the hostile input and that fails
when the fix is reverted. Changed prose never closes an entry.

## Triage table

| ID | Severity | Disposition | Register | What lands |
|----|----------|-------------|----------|------------|
| ARP-001 | Critical | fix-now | RC-013 | Provisioning removed from every lifecycle hook. Explicit user-run installer with client-side attestation verification. |
| ARP-002 | Critical | fix-now | RC-014 | Strict identifier allowlists, shell-quoting on every rendered value, descriptor-supplied `validation` no longer rendered as runnable shell, every "proves safe" claim removed. |
| ARP-003 | Critical | redesign | RC-015 | Fail-safe: every claim that the event pump enforces tool authorization is removed, `tool_blocked` marked advisory. Broker redesign registered with its acceptance test defined. |
| ARP-004 | High | fix-now + threat-model-scoping | RC-016 | Ledger prose sanitized and quoted at the hook→context boundary. Signed writer identity registered as follow-up. |
| ARP-005 | Medium | fix-now | RC-017 | Per-connection principal, owner binding on session and approval operations, repo_path allowlist, constant-time token compare, non-loopback bind refusal. |
| ARP-006 | Medium | fix-now | RC-018 | Pre-push gate no longer executes gate scripts from the commit being pushed. |
| ARP-007 | Low | fix-now | RC-019 | JSONL quarantine, AppleScript argument separation, pinned Python deps, sink path constraints. |

## Per-finding reasoning

### ARP-001 — trust-on-open code execution → fix-now

Accurate, and the most serious finding in the set. Verified independently before triage:
`hooks/rally-coordination-hook.sh` invoked `ensure-rally-binary.sh` on the `start` phase in
**both** branches — when `.rally/` is present (~`:467-470`) and when it is absent
(~`:71-77`, comment: "Wire ensure-rally-binary on start even in no-.rally repos"). So merely
opening and trusting the repo in a host that auto-loads its committed hook registration could
download a release binary, `chmod +x` it, and write it to `~/.local/bin/rally`.

The script's own header (`:20-32`) already admitted the limit: the checksum comes from the same
GitHub release authority as the binary, so it defends transit corruption and not account
compromise, and the sigstore attestation was verified only out of band by a human. That
admission is honest and it is also the finding — a control the code knows is insufficient was
still on the automatic path.

The disposition is not "add better verification to the automatic path". It is that a lifecycle
hook is the wrong place for provisioning at all. Detection and advice are fine from a hook.
Downloading and executing are not.

### ARP-002 — linter described as a safety proof → fix-now

The core claim is accurate. One detail in the audit was slightly stale by the time of triage:
`workstream-lint.mjs` had already been tightened to reject `" $ ` ` in `intent`, `output`, and
`owns`. That does not rescue the finding:

- `owns` still permitted `;` `|` `&` `>` `<` `(` `)`. A denylist of three characters is not a
  safety property.
- `task.validation` needed only to be a non-empty string, and `packet.mjs` emitted it verbatim
  into a ```bash block under a heading telling the agent to "run these verbatim".
- `runId` and `toolPrefix` were interpolated into command text after only a non-empty check.

So the audit's headline stands: a descriptor could pass lint and produce a packet that executes
attacker-chosen shell. The claim in `PROTOCOL.md:13` that the linter "proves a plan is safe to
fan out" was an overclaim, and it is the kind of overclaim that causes harm — it tells a reader
the boundary is somewhere it is not.

### ARP-003 — Cockpit approval gate does not gate → redesign

Accurate and unfixable in this run without an architecture change. Cockpit spawns
`codex exec --json` and `claude -p --output-format stream-json` as child processes and reads
their stdout. The "authorization gate" observes a `tool_call` **after** it appears in the event
stream. Pausing the event pump does not pause the child. Denial emits `tool_blocked` and stops
forwarding a result; it does not stop the tool.

That is a false security boundary, which is worse than no boundary, because an operator reading
"blocked" concludes something was prevented. The real fix is to make Cockpit the tool broker, or
to integrate each CLI's native pre-execution approval callback so the child cannot act until
Cockpit resolves the request.

Neither fits this run's budget honestly. The fail-safe lands instead: remove every claim of
enforcement, mark `tool_blocked` as advisory with its true meaning ("not forwarded to the UI",
not "prevented"), and register the redesign with its adversarial acceptance test written down so
the follow-up has a definition of done. Half-integrating a native approval callback would produce
exactly the same failure this finding describes — a boundary that looks real and is not.

### ARP-004 — unsigned ledger prose enters privileged context → fix-now + threat-model-scoping

Accurate by design, which is the uncomfortable part. Peer-authored `subject`, `evidence`,
`intent`, and `file` strings flow from the ledger through the SessionStart hook into
`additionalContext` / `systemMessage`. Any writer to `.rally/` can put prose into a high-trust
model channel.

Two halves, triaged differently:

- **The injection boundary is fix-now.** Sanitizing and quoting peer prose before it reaches
  model context is cheap, testable, and does not require a protocol change. It lands in this run
  with an adversarial test.
- **Authenticated writer identity is threat-model-scoping.** Rally's current trust model is
  same-UID, single operator — `crates/rally-protocol/src/ledger.rs:45-63` already says so. Under
  that model, "a same-UID process can write to the ledger" is not a privilege boundary being
  crossed; the process already has your privileges. The finding is correct that this model was
  never written down where a user would find it, and correct that a second contributor on the
  same repo breaks it. Signed/MACed facts are a real protocol change and are registered as a
  follow-up rather than half-built here.

The honest statement of what is and is not defended after this run lives in
[`TRUST-MODEL.md`](TRUST-MODEL.md).

### ARP-005 — one bearer token, no ownership isolation → fix-now

Accurate and bounded. Cockpit already does the two hardest things right: loopback by default,
fail closed on a missing token. What it lacks is a principal — after authentication every
connection is the same actor, so any client can steer any session and resolve any approval by
UUID, and `repo_path` becomes a child CWD anywhere the service can read.

All five sub-items are ordinary hardening with clear adversarial tests. Fix now.

### ARP-006 — pre-push gate runs code from the pushed commit → fix-now

Accurate. The hook builds a detached worktree at the pushed commit and runs that commit's copy of
`run-quality-gate.sh` and `check-release-parity.sh`. Pushing a branch executes that branch's gate.
The auditor noted the hook was not active in their clone, which limits exposure but not the
defect.

The fix is the distinction the finding implies: the gate's **code** should be trusted and pinned;
the gate's **subject** is the untrusted pushed tree.

### ARP-007 — watcher hardening → fix-now (low)

Four small real items. The JSONL one is the most interesting because it is the same shape as
RC-001/RC-005/RC-010 already in the register: **an operation reports success for a step the
caller does not care about.** A malformed line is skipped, the cursor advances, and the consumer
is told it is current. It is not. That earns its place in the register on pattern grounds even at
Low severity.

## What the audit got right that is worth keeping

The report's structure is better than anything in this repo's own review surface, and two
sections are the reason:

- **Positive containment observations** — what was checked and found sound. Without it, a reader
  cannot tell "reviewed and fine" from "not reviewed".
- **Unreviewed or only partially reviewed surface** — an explicit statement of what the audit did
  *not* cover (26 tracked git bundles, ~69 MB under `archive/bundles/`, no `cargo audit`, no
  dependency-code review, no raster payload decoding, no runtime behaviour of third-party CLIs).

An audit that names its own blind spots is more useful than one that does not, because the blind
spots are where you look next. Both sections are being proposed as a required report format for
build-loop's own security review — see the levers list in
[`rca-2026-08-02-security-findings-escaped.md`](../rca-2026-08-02-security-findings-escaped.md).

## Follow-ups registered, not fixed here

| Item | From | Why deferred |
|------|------|--------------|
| Cockpit tool-broker redesign, or native pre-execution approval integration | ARP-003 | Architecture change. Acceptance test defined in RC-015. |
| Signed / MACed facts with authenticated writer identity | ARP-004 | Protocol change across `crates/rally-protocol` and every writer. |
| Distinguish committed historical log from live trusted state | ARP-004 | Needs a protocol-level provenance field; pairs with fact signing. |
| Import and inspect the 26 tracked git bundles | audit "unreviewed surface" | Not a code fix; a separate review task. |
| `cargo audit` / `cargo deny` dependency-vulnerability pass | audit "unreviewed surface" | The audit explicitly did not do this. Neither has this repo. |
| `ios/Cockpit` must send a stable `client_id` | RC-022 | Swift change plus a device test. The CLI half is fixed; iOS reconnects would otherwise orphan their own sessions. |
| `docs/plans/COCKPIT-WIRE.md` needs the new wire fields | ARP-003/005 | Document optional `hello.client_id`, the `forbidden` and `repo_path_denied` error codes, and `tool_blocked`'s advisory metadata. |
| A `rally show <event-id>` command | ARP-004 | The audit's ideal — inject an opaque ID and make the agent open the fact separately — is materially better with a single-fact reader. None exists; the preamble points at `rally room --json` instead. |
| `ClaudeAdapter::send` runtime panic | RC-021 | Pre-existing crash on any live Claude session; found while testing, out of scope for both findings. |
| Fix the `claim` / `check` lease-expiry disagreement | RC-020 | `check` treats an expired lease as advisory; `claim` hard-refuses on it. |
| Quote peer prose at the source in `crates/rally-cli` | ARP-004 | The hook boundary is sanitized, but `agent_visible` messages are assembled in the CLI. Defence belongs at both ends. |
