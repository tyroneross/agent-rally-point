<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# DRAFT — reply to issue #52

**NOT POSTED.** This is a draft for Tyrone to review, edit, and post himself. External comms
need his sign-off. Delete this file or leave it as the record once posted.

Rewritten 2026-08-03 against the direct-writing style guide: shortest wording that preserves
meaning, strong verbs, data over adverbs, certainty matched to evidence, standalone scannable
units, full GitHub URLs (relative links break in issue comments). Accuracy fixes vs the prior
draft: ARP-005 downgraded from "Fixed" to partial (a live probe proved token-holder
impersonation still works), register range corrected to RC-013..RC-025, and the RC-024
remediation-defects story added.

Everything below the line is the proposed comment body.

---

Thank you. This is the most useful thing anyone has sent this project.

All seven findings were real. The three Criticals sat in code that had passed CI, a pre-push
gate, and four rounds of solicited audit. Every prior review asked "is this mechanism
implemented correctly?" Your report asked "should this mechanism exist here at all?" and found
three Criticals in an afternoon. I wrote up why the gap existed as a blameless RCA:
[docs/rca-2026-08-02-security-findings-escaped.md](https://github.com/tyroneross/agent-rally-point/blob/main/docs/rca-2026-08-02-security-findings-escaped.md).

## Disposition

| ID | Severity | State | Control |
|----|----------|-------|---------|
| ARP-001 | Critical | Fixed | Hooks detect and advise only. Provisioning moved to an explicit installer, fail-closed behind SHA256 + client-side `gh attestation verify`. |
| ARP-002 | Critical | Fixed | Identifier allowlists, quoted rendering, and descriptor `validation` no longer renders as shell. 49 injection tests, one per bypass shape. |
| ARP-003 | Critical | Fail-safe only; redesign registered | Every enforcement claim removed; `tool_blocked` now reads "not forwarded", not "prevented". Broker redesign is a named follow-up with its acceptance test pre-written. |
| ARP-004 | High | Injection boundary fixed; signing deferred | Peer prose arrives sanitized, length-capped, and quoted behind a fixed preamble. Writer authentication is a registered protocol follow-up. |
| ARP-005 | Medium | Partial | Owner binding, repo allowlist, constant-time compare, and bind refusal landed. A token holder can still impersonate any `client_id` — a live probe proved it — so per-client credentials are a registered follow-up. |
| ARP-006 | Medium | Fixed | Gate code executes pinned from the installed tree; the pushed tree is subject only. A modified gate in a pushed branch provably does not run. |
| ARP-007 | Low | Fixed | Malformed JSONL quarantines without advancing the cursor, AppleScript uses argument separation (live-probed inert), deps pinned, sink paths constrained. |

Each finding has an entry in
[docs/ROOT-CAUSE-REGISTER.md](https://github.com/tyroneross/agent-rally-point/blob/main/docs/ROOT-CAUSE-REGISTER.md)
(RC-013..RC-025). An entry closes only on a proven adversarial control: a test that rejects the
hostile input and fails when the fix is reverted. Changed prose closes nothing.

One number from the remediation worth stating plainly: the fix run itself shipped five defects
of the class it was fixing — ARP-001's fix removed provisioning from the hook while the same
hook still preferred `./target/debug/rally` twenty lines away, with every test green. In-run
review caught all five (RC-024). That is the same failure mode as the original misses, committed
while fixing them, which is why the register rule exists.

## Notes on specific findings

**ARP-001.** You were right that the fix is not "verify harder on the automatic path". A
lifecycle hook is the wrong place to acquire and execute anything. The rule the codebase now
follows: a hook may observe and inform; it may not acquire or execute. Offline hard-failure is
acceptable in an explicit installer, so that path refuses rather than degrades when either
verification check cannot complete.

**ARP-002.** One correction for the record: `intent`, `output`, and `owns` already rejected
`"` `$` `` ` `` before your review, so those line cites were slightly stale. The correction does
not rescue the finding — `owns` still permitted `;` `|` `&` `>` `(` `)`, and the renderer put
`validation` verbatim into a bash block. The history shows two prior rounds that each rejected
exactly the characters the previous finding named — the denylist treadmill your report
implicitly criticizes. It is an allowlist now, and descriptor `validation` renders as
non-executable prose, with named local recipes for the case where a real command is wanted. The
"proves a plan is safe to fan out" claim is gone; it grew stronger over time while the property
stayed absent.

**ARP-003.** Agreed, and not pretending to fix it in one pass. Making Cockpit the broker, or
integrating each CLI's native pre-execution approval, is an architecture change.
Half-integrating would reproduce the defect you found: a boundary that looks real and is not.
The fail-safe landed instead, and the redesign is registered with its adversarial acceptance
test written down — a definition of done rather than a vibe.

**ARP-004.** Correct, and partly a documentation failure. Rally's trust model was same-UID
single-operator, written down in one code comment where no user would look. Under that model
your finding crosses no privilege boundary; under the model a reader would reasonably assume, it
does. The trust model now lives where people will find it, including what remains undefended:
[docs/security/TRUST-MODEL.md](https://github.com/tyroneross/agent-rally-point/blob/main/docs/security/TRUST-MODEL.md).

**ARP-007.** The JSONL item interested me most: it shares a shape with three defects already in
my register — an operation reporting success for a step the caller cares about. Cursor advanced,
record lost, consumer told it was current.

## What I am taking from the report itself

Two sections of your report do work nothing in my review pipeline was doing:

- **Positive containment observations** — without them a reader cannot separate "checked and
  sound" from "never looked at".
- **Unreviewed surface** — naming your own blind spots (26 tracked git bundles, no `cargo
  audit`, no raster decoding) makes the report auditable and tells me where to look next.

I adopted your method as a standing review rubric — trust-on-clone threat modeling,
doc-claims-vs-code contradiction hunting, false-boundary detection, prompt-to-shell and
prompt-to-context render-path tracing, and the report format including both epistemic sections —
with this issue cited as the exemplar. Out of curiosity: what produced this report? If it is
tooling you have written about anywhere, I would like to read more.

## Still open, tracked

- Cockpit broker redesign or native pre-execution approval integration (ARP-003).
- Per-client Cockpit credentials; `client_id` is still self-asserted (ARP-005).
- Signed facts with authenticated writer identity (ARP-004).
- Importing and inspecting the 26 tracked git bundles you inventoried.
- A `cargo audit` / `cargo deny` pass. You noted you did not run one; neither had I.

Maturity, stated plainly: Rally is proven on a small number of fresh macOS installs driven by
one operator. Multi-user was never the target, and your report is the reason that is now written
down as an explicit non-target rather than an unstated assumption.

Thanks again.
