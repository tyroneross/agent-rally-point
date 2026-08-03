<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# DRAFT — reply to issue #52

**NOT POSTED.** This is a draft for Tyrone to review, edit, and post himself. External comms
need his sign-off. Delete this file or leave it as the record once posted.

Everything below the line is the proposed comment body.

---

Thank you. This is the most useful thing anyone has sent this project.

Seven findings, and the three Criticals were all real. More to the point, they were in code that
had already passed CI, a pre-push gate, and four numbered rounds of solicited audit. I went back
through the history to understand why, and the answer is uncomfortable and worth saying out loud:
every prior review was scoped to *is this mechanism implemented correctly?* Nobody had ever been
in a position to ask *should this mechanism exist here at all?* Your report asked the second
question and found three Criticals in an afternoon.

I wrote that up as a blameless RCA, because the process failure is more valuable to me than the
individual bugs: [`docs/rca-2026-08-02-security-findings-escaped.md`](../rca-2026-08-02-security-findings-escaped.md).

## Disposition

| ID | Severity | Disposition | State |
|----|----------|-------------|-------|
| ARP-001 | Critical | Fixed | Provisioning removed from every lifecycle hook. |
| ARP-002 | Critical | Fixed | Allowlists, quoting, descriptor `validation` no longer rendered as shell. |
| ARP-003 | Critical | Fail-safe now, redesign registered | Enforcement claims removed. Broker redesign is a named follow-up. |
| ARP-004 | High | Fixed at the injection boundary; signing deferred | Ledger prose sanitized before it reaches model context. |
| ARP-005 | Medium | Fixed | Per-connection principal, owner binding, repo allowlist, constant-time compare, bind refusal. |
| ARP-006 | Medium | Fixed | Gate scripts no longer execute from the commit being pushed. |
| ARP-007 | Low | Fixed | Quarantine, AppleScript arg separation, pinned deps, sink path constraints. |

Each finding has an entry in [`docs/ROOT-CAUSE-REGISTER.md`](../ROOT-CAUSE-REGISTER.md)
(RC-013..RC-019). A register entry here closes only on a proven adversarial control: a test that
rejects the hostile input and that **fails when the fix is reverted**. Changed prose does not
close anything. Where I claim "fixed" below, that mutation evidence is recorded in the entry.

## Notes on specific findings

**ARP-001.** You were right that the fix is not "verify harder on the automatic path". It is that
a lifecycle hook is the wrong place to acquire and execute anything. The hook now detects and
advises only. Provisioning moved to an explicit installer a human runs on purpose, and because
offline hard-failure is acceptable there, it is fail-closed: SHA256 plus client-side
`gh attestation verify` before the file is made executable, refusing rather than degrading if
either check cannot complete.

The rule I took from this: **a lifecycle hook may observe and inform; it may not acquire or
execute.**

**ARP-002.** Accurate. One small correction for the record: `intent`, `output`, and `owns` had
already been tightened to reject `" $ ` ` before your review, so those specific line cites were
slightly stale. It does not rescue the finding — `owns` still permitted `;` `|` `&` `>` `(` `)`,
`validation` needed only to be non-empty, and the renderer put it verbatim into a bash block. The
history shows two prior rounds that each rejected exactly the characters the previous finding
named, which is the denylist treadmill your report implicitly criticizes. It is an allowlist now,
and descriptor-supplied `validation` is no longer rendered as runnable shell at all — it renders
as non-executable prose, with an optional named recipe from a local registry for the case where a
real command is wanted.

The "proves a plan is safe to fan out" line is gone. That claim was getting stronger over time
while the property stayed absent.

**ARP-003.** I agree completely and I am not going to pretend to fix it in one pass. Making
Cockpit the actual broker, or integrating each CLI's native pre-execution approval, is an
architecture change. Half-integrating it would reproduce the exact defect you found: a boundary
that looks real and is not.

So the fail-safe landed instead — every claim that the event pump enforces tool authorization is
removed, and `tool_blocked` is marked advisory with its true meaning ("not forwarded", not
"prevented"). The redesign is registered with its adversarial acceptance test written down, so
the follow-up has a definition of done rather than a vibe.

**ARP-004.** Correct, and partly a documentation failure on my side. Rally's trust model was
same-UID single-operator, and that was written down in exactly one code comment
(`crates/rally-protocol/src/ledger.rs:45-63`) — nowhere a user would look. Under that model your
finding is not a privilege boundary being crossed; under the model a reader would reasonably
assume, it is.

The injection boundary is fixed now: peer prose is sanitized, length-capped, newline-stripped, and
quoted behind a fixed hook-authored preamble that tells the model the following is peer-authored
and unverified. Authenticated writer identity and signed facts are a protocol change and are
registered as follow-ups rather than half-built.

The trust model is now written down where people will find it, including what remains undefended:
[`docs/security/TRUST-MODEL.md`](TRUST-MODEL.md).

**ARP-006.** Fixed via the distinction your finding implies: the gate's *code* should be pinned
and trusted, the gate's *subject* is the untrusted pushed tree.

**ARP-007.** All four items fixed. The JSONL one interested me most because it is the same shape
as three defects already in my register — an operation reporting success for a step the caller
does not care about. Cursor advanced, record lost, consumer told it was current.

## What I am taking from the report itself

Two sections of your report do work that nothing in my own review pipeline was doing:

- **Positive containment observations** — without them a reader cannot separate "checked and
  sound" from "never looked at".
- **Unreviewed or only partially reviewed surface** — naming your own blind spots (the 26 tracked
  git bundles, no `cargo audit`, no dependency-code review, no raster decoding) makes the report
  auditable and tells me where to look next.

I am adopting your method as a rubric — trust-on-clone threat modelling, doc-claims-vs-code
contradiction hunting, false-security-boundary detection, prompt-to-shell and prompt-to-context
render-path tracing, and that report format including both epistemic sections. This issue is
being cited as the exemplar spec for it.

## Still open, tracked

- Cockpit tool-broker redesign or native pre-execution approval integration (ARP-003).
- Signed/MACed facts with authenticated writer identity (ARP-004).
- Distinguishing committed historical log from live trusted state (ARP-004).
- Importing and inspecting the tracked git bundles you inventoried.
- A real `cargo audit` / `cargo deny` vulnerability pass. You noted you did not run one. Neither
  had I.

Maturity, stated plainly since it is relevant to how much weight to put on any of this: Rally is
proven on a small number of fresh macOS installs driven by one operator. Multi-user was never the
target and your report is the reason it is now written down as a non-target rather than an
assumption.

Thanks again.
