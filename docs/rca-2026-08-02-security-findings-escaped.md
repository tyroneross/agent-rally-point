<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# RCA — why seven security findings escaped every gate

**Tier:** L2 (three Critical findings shipped to `main` and sat there).
**Trigger:** GitHub issue #52, an independent third-party audit, found 7 findings — 3 Critical,
1 High, 2 Medium, 1 Low — in code that had passed CI, pre-push gates, multiple solicited audit
rounds, and build-loop's own review phases.
**Blameless.** The question is which control should have caught each class and why it stayed
dormant on the real input. Not who missed it.

## Summary

The controls were not missing. Most of them ran. They were **pointed at the wrong question.**

Every prior security review in this repo was scoped to *is this mechanism implemented
correctly?* None was scoped to *should this mechanism exist here at all?* The auditors answered
the question they were handed, correctly, several times in a row. Lattice was the first reviewer
that got to choose its own question, and it immediately found three Criticals in code that had
already survived four rounds of solicited audit.

That is the root cause. The specific gate gaps below are how it expressed itself.

## Evidence

All evidence is from this repo, read directly rather than assumed.

### E1 — The provisioner was audited four times and never questioned

`git log --follow hooks/ensure-rally-binary.sh`:

```
d2e915f harden(provision): flock-based lock where available (close the reclaim TOCTOU)
18736b3 fix(provision): gate the rally-on-PATH fast-path through liveness (Codex round-4)
90bfd18 fix(provision): bash-3.2 lock handoff + signal-death liveness (round-3 audits)
89d1b87 harden(provision): close round-2 residuals (fd-detach, perl timer, lock handoff)
07c9d47 harden(provision): close auditor f9–f12 + C1–C6 (fail-closed, atomic lock, timeouts)
9a6da2a fix(provision): checksum-verify + background download + lock-race fix (auditor f2/f3/f4)
0ef5f48 feat(plugin): auto-launch on install + offer setup on first session
```

Four numbered audit rounds. Findings f2, f3, f4, f9–f12, C1–C6. Every one of them made the
download-and-execute path *more correct*: fail-closed checksum verification, atomic lock
creation, TOCTOU closure, bounded timeouts, signal-death detection.

Not one asked whether a SessionStart hook should be downloading and executing a binary.

The script's own header even documents the residual risk it cannot close — checksum and binary
share one authority, sigstore verification is out of band only. That admission survived four
audits as an accepted limitation, because every round was scoped to "harden this path", and
inside that scope the admission is correct and complete.

ARP-001 is what you find when you are allowed to ask a different question.

### E2 — The linter was patched twice by denylist, never redesigned

`git log dynamic-workflows/core/workstream-lint.mjs`:

```
0280480 fix(dynamic-workflows): extend shell-safety lint to output/owns/id + document CLI asymmetry (f1, f4, f6)
07f2bd3 fix(workstream-lint): reject shell-unsafe intent + owns paths (f4)
```

Same shape. A finding named specific dangerous characters. The fix rejected those characters.
The next finding named more fields. The fix extended to those fields. Two rounds of closing
exactly what was reported.

Nobody asked the two structural questions: *why is a denylist the shape of this check?* and
*why is descriptor-supplied text being rendered into a bash block at all?* Both were still open
when Lattice arrived, and `owns` still permitted `;` `|` `&` `>` `(` `)` after both rounds.

Meanwhile `PROTOCOL.md:13` claimed the linter "proves a plan is safe to fan out". The claim got
stronger while the property stayed absent.

### E3 — `security-reviewer` ran exactly once, on the wrong build

`security-reviewer` appears once in `.build-loop/state.json`, in run
`bl-20260709T193157Z-codex-017210` — the `rallyd` single-writer daemon build. It returned
`pass (0 critical/high, 3 medium, 3 low)` and produced SEC-001 through SEC-006.

It ran there because that build looked like security work to the trigger: a daemon, unix
sockets, pid handling, a wire protocol.

It did **not** run on:

- `bl-20260611-arp-provision-hardening` — goal recorded as *"ARP auto-launch + binary
  auto-provision; tested install on Claude/Codex/Cursor"*. This is the run that built ARP-001.
- `bl-20260730T234011Z-codex-256381` — the v0.1.7 host-sync release, which regenerated all four
  committed hook-registration surfaces.
- Any dynamic-workflows work. There is no recorded build-loop run for the linter or the packet
  renderer at all.

So the reviewer that exists for exactly this purpose never saw any of the three Critical
surfaces. `triggers.riskSurfaceChange` gates its dispatch, and a build whose stated goal is
"binary auto-provision" did not set it.

### E4 — The independent auditor never rendered a verdict. Not once.

Across all recorded runs, `judge_decisions` contains **17 entries, all
`judge_id: independent-auditor-hook`, all `status: packet_emitted`, all `verdict: pending`.**

Not one adjudicated. The deterministic pre-commit hook did its job — it assembled and emitted an
audit packet on 17 commits. The LLM-grade adjudication that turns a packet into a verdict never
happened on any of them.

An audit queue where 17 of 17 items are `pending` is not an audit. It is a backlog that looks
like an audit from the outside, which is worse, because the run reports show an auditor in the
pipeline.

### E5 — "Enforcement" was self-asserted in a commit message

`git log crates/cockpitd/src/transport/ws.rs`:

```
e60714e feat(cockpit): G1 authz enforcement loop + G2 multi-block turns + project overview
6939361 feat(cockpit): F1-F3 security hardening — WS approval e2e, audit log, authz policy
```

The commit that introduced ARP-003's false boundary calls itself an *enforcement loop*. It
pauses an event pump. The child process it is supposedly gating is a separately spawned CLI that
never sees the decision.

Nothing in the pipeline compares a security claim against the behaviour of the code making it.
`fact-checker` traces rendered *data* to its source. It does not trace a *claim* — in a commit
message, a doc, a code comment, or UI copy — to the implementation that would have to be true
for the claim to hold.

Three claims of this exact type shipped: "authz enforcement" (enforces nothing),
"proves a plan is safe to fan out" (proves no such thing), and "Rally does not install host
hooks" (four committed hook-registration files say otherwise).

## Why each finding class escaped

| Finding | Control that should have caught it | Why it stayed dormant |
|---------|-----------------------------------|----------------------|
| ARP-001 | `security-reviewer` | Never dispatched. `riskSurfaceChange` was not set by a build whose goal was literally "binary auto-provision". Hook registration, installers, and download-and-chmod paths are not in the trigger's vocabulary. |
| ARP-002 | `security-reviewer`; plan-time design review | Never dispatched — no recorded run at all for dynamic-workflows. Two solicited rounds patched the reported characters and left the shape. |
| ARP-003 | Doc/claim-vs-behaviour check | No such control exists. `fact-checker` grades rendered data, not security claims. "Enforcement" was asserted and never graded. |
| ARP-004 | Threat model naming a second writer | The single-operator model was implicit. It lived in one code comment. No rubric asked "what if a second contributor can write this file?" |
| ARP-005 | `security-reviewer` | Would likely have caught it — Cockpit is recognizably security-shaped. It never ran on the Cockpit commits. |
| ARP-006 | `security-reviewer`; supply-chain rubric | Same dormancy. "Gate executes code from the tree it is gating" is a trust-on-clone question nothing in the pipeline asks. |
| ARP-007 | `leak-scanner` / robustness review | Partially in scope, not run. The JSONL cursor defect is the same shape as RC-001/005/010 already in the register — an ack for the wrong step — and the register did not generalize into a check. |

## Root causes

**RC-A — Solicited review answers the question you asked.**
Four audit rounds on the provisioner, two on the linter, all correct, all scoped by the person
requesting them. A reviewer handed "harden this download path" will harden the download path. It
will not tell you the path should not exist. Independence is not about the reviewer's competence;
it is about who chooses the question. This repo had never had a reviewer that chose.

**RC-B — The risk trigger's vocabulary was written from the wrong examples.**
`riskSurfaceChange` recognizes daemons, sockets, auth, and secrets. It does not recognize the
three surfaces that produced every Critical here: code that *auto-loads on repo trust*, code that
*acquires and executes an artifact*, and code that *renders stored strings into a shell or a
model context*. The trigger was built from a memory of what security code looks like, not from
where authority actually crosses.

**RC-C — No control compares a claim to its implementation.**
Three false claims shipped and hardened over time. Documentation drifted toward reassurance while
the code stayed put. Nothing reads a security claim and asks the code whether it is true.

**RC-D — The audit queue never drained, and nothing noticed.**
17 of 17 packets `pending`. A gate that emits without adjudicating reports as present and
functions as absent. This is the same defect shape the register's working hypothesis already
names: *an operation returns success for a step that is not the step the caller cares about.*
Packet-emitted reported as audited.

## Build-loop levers, ranked by fix strength

Ranked by the standard hierarchy: **eliminate → impossible-state → automated-block → detect.**
These are proposals for the build-loop repo. **None are implemented here** — this run does not
edit build-loop. They are routed to build-loop-memory and a follow-up queue item for a subsequent
run, and they need Tyrone's approval before landing.

### L1 — Adopt a Lattice-style repo-trust audit rubric and report format (automated-block + detect)

**Strongest available lever, because the spec already exists.** Issue #52 is not just a bug
report; it is the best available specification for the control build-loop is missing. Adopt its
method as a rubric for `security-reviewer`, or as a new `repo-trust-auditor` skill.

Five method dimensions, all absent from build-loop's current review surface:

1. **Trust-on-clone threat model.** What executes or gains authority when a repo is merely opened
   and trusted? Auto-loaded hooks, installers, tracked binaries, symlinks, SVG payloads, git
   bundles. This dimension alone produces ARP-001 and ARP-006.
2. **Doc-claims-vs-implementation contradiction hunting.** Extract security and behaviour claims
   from docs, comments, and UI copy; grade each against the code. Produces ARP-002's "proves
   safe", ARP-003's "enforcement", and the SKILL.md hook contradiction.
3. **False-security-boundary detection.** Find controls that *observe* but do not *enforce*.
   Produces ARP-003.
4. **Prompt-to-shell and prompt-to-context render-path tracing.** Follow stored data to any point
   where it becomes a shell string or model context. Produces ARP-002 and ARP-004.
5. **Report format with epistemic sections.** Stable per-project finding IDs, severity,
   `file:line` citations, per-finding **Required fix**, plus **Positive containment
   observations** and **Unreviewed surface**. The last two are what make a report auditable —
   without them a reader cannot separate "checked and sound" from "never looked".

Cite issue #52 as the exemplar spec. Fix strength: this is the only lever that would have caught
all three Criticals.

### L2 — Auto-set `riskSurfaceChange` from diff shape, not from goal text (impossible-state)

Make it structurally impossible to touch an authority boundary without the security reviewer
dispatching. Set `triggers.riskSurfaceChange: true` automatically when a diff touches:

- hook-registration files (`.claude/settings.json`, `.codex/hooks.json`, `.cursor/hooks.json`,
  `hooks/hooks.json`, `core.hooksPath` config, any `SessionStart`/`PreToolUse` wiring)
- installers and provisioners (`install*.sh`, `ensure-*`, anything invoking `curl`/`wget` +
  `chmod +x`, `cargo install`, `pip install`, `npm i -g`)
- any code interpolating a variable into a shell string, or into model context
  (`additionalContext`, `systemMessage`, system-prompt assembly)
- token/auth comparison, bind-address selection, process spawn with a caller-supplied CWD

Detection is a diff-path plus regex classifier — cheap, deterministic, no LLM. Directly fixes
RC-B. This is the single highest-value mechanical change.

### L3 — Fail the run when the audit queue does not drain (automated-block)

17 of 17 `pending` should have been a hard error long ago. Add a gate: if a run emitted audit
packets and any remain `verdict: pending` at Phase 4G, the run is `outcome: partial`, never
`pass`, and the report carries a mandatory block naming the un-adjudicated commits. Build-loop
already has this pattern for owed verification; extend it to cover hook-emitted packets. Fixes
RC-D.

### L4 — Doc-claims fact-check rule (detect)

Extend `fact-checker` (or add a `claim-auditor`) with a claims pass: extract sentences making
security or behaviour assertions — "does not install", "proves", "enforces", "blocks", "verifies",
"prevents", "sandboxed", "safe" — from docs, READMEs, skill files, code comments, and commit
messages in the diff, then grade each against the implementation. Emit contradictions as findings
with both citations. Fixes RC-C. Would have caught ARP-002's and ARP-003's headline claims and
the SKILL.md contradiction without any security expertise.

### L5 — Standing "assume a second untrusted contributor" line in the security rubric (detect)

Add to `security-reviewer`'s rubric a mandatory question: *if a second contributor could land a
commit, and a second process could run as this user, what changes?* Any file that is both
committed and read as trusted state (like `.rally/log/*.jsonl`) must be named. Requires the
reviewer to state the assumed trust model explicitly in its verdict, so an implicit model cannot
survive review. Fixes RC-A's threat-model half. Would have surfaced ARP-004.

### L6 — Adversarial-injection test requirement for renderers (automated-block)

Any code that emits shell text or model context from stored data must ship an adversarial test
proving hostile input is rejected or neutralized. Enforce at the plan gate: if a chunk touches a
render path, the plan must name the adversarial test, and Phase 4 blocks without it. Fixes the
ARP-002/ARP-004 class structurally rather than per-character.

### L7 — Escalate "solicited review is not independent" from a memory note to a gate (eliminate)

The lesson already exists in memory. It did not fire here because nothing enforces it. Make it
structural: for any run where `riskSurfaceChange` is set, the auditor must be dispatched with the
authority to choose its own scope — not handed a scoped question — and its verdict must be
recorded before push. Rounds of "close finding f4" do not count as review. Fixes RC-A directly,
and is the highest-leverage lever conceptually even though L1 and L2 are more mechanical.

## What this repo changed as a result

Register entries RC-013 through RC-019, one per finding, each closing only on a proven
adversarial control. Triage in
[`security/AUDIT-2026-08-02-issue-52-triage.md`](security/AUDIT-2026-08-02-issue-52-triage.md).
The trust model that was previously implicit is now written down in
[`security/TRUST-MODEL.md`](security/TRUST-MODEL.md), including what remains undefended.
