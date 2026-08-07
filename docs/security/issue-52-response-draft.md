<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# DRAFT — reply to issue #52

**NOT POSTED.** Draft for Tyrone to review, edit, and post himself. External comms need his
sign-off.

**v2, 2026-08-06.** v1 (2026-08-03) predated the full disposition audit and is stale in five
specifics, all corrected below: the bundle inventory is **18, not 26**, and it is now **closed with
a result** rather than listed as open work; `rally locate --json` is a **third** unsanitized sink
under ARP-004; the register range is RC-013..RC-068, not RC-013..RC-025; two ARP-R findings
(R-03, R-04) shipped fixes with no register entry and one (R-10) cannot be recovered at all; and
ARP-005 is partial, not fixed — a live probe against the shipped binary proved the impersonation
still works.

**Posting checklist, from GitHub's maintainer guidance (sources at the bottom of this file):**

1. Acknowledgement is **4 days late** — that is the single biggest miss here. Lead with it; do not
   explain it away.
2. Credit the reporter by name in the comment and in the release notes.
3. Publish a repository security advisory (GHSA) for ARP-001/002/003 **with a fixed version set**.
   An advisory published without one makes Dependabot warn users with no safe version to move to.
4. Say "security fix" explicitly in the release notes — not "hardening", not "cleanup".
5. Close #52 **only after** the deferred items have their own tracking issues, and link them. Three
   findings are not fully closed; closing #52 without those links would overstate the state.

---

Thank you, and apologies for the four-day silence — that was the wrong response time for a report
of this quality, regardless of what I was doing with it.

All seven findings were real. The three Criticals sat in code that had passed CI, a pre-push gate,
and four rounds of solicited audit. Every prior review asked *"is this mechanism implemented
correctly?"* Yours asked *"should this mechanism exist here at all?"* and found three Criticals in
an afternoon. The blameless RCA on why they escaped is at
[docs/rca-2026-08-02-security-findings-escaped.md](https://github.com/tyroneross/agent-rally-point/blob/main/docs/rca-2026-08-02-security-findings-escaped.md).

Your report triggered two further review cycles. **32 findings total** now carry a written
disposition — your ARP-001..007, an eleven-finding repo-trust re-assessment (ARP-R-01..R-11), and a
fifteen-finding design audit (D1..D15). Every one is graded at
[docs/security/ISSUE-52-DISPOSITION-2026-08-06.md](https://github.com/tyroneross/agent-rally-point/blob/main/docs/security/ISSUE-52-DISPOSITION-2026-08-06.md),
including the seven that were deferred or unexamined with nobody having written down why.

## Disposition of your seven

| ID | Severity | State | Control |
|----|----------|-------|---------|
| ARP-001 | Critical | **Fixed, controlled** | Hooks detect and advise only. Provisioning moved to an explicit installer behind SHA256 + client-side `gh attestation verify`. 10 assertions, gated in CI and in the pre-push hook. |
| ARP-002 | Critical | **Fixed, controlled** | Identifier allowlists, quoted rendering, descriptor `validation` no longer renders as shell. 49 injection tests, one per bypass shape. |
| ARP-003 | Critical | **Fail-safe only — redesign registered** | Every enforcement claim removed; `tool_blocked` now reads "not forwarded", not "prevented". The broker redesign is a named follow-up with its acceptance test pre-written. |
| ARP-004 | High | **Injection boundary fixed; writer auth deferred** | Peer prose arrives sanitized, length-capped, quoted behind a fixed preamble. 12/12 + 5/5 parity tests, both gated. Writer authentication does not exist — see below. |
| ARP-005 | Medium | **Partial** | Owner binding, repo allowlist, constant-time compare, bind refusal landed. A token holder can still impersonate any `client_id`; I reproduced it live against the shipped binary rather than assuming the fix held. |
| ARP-006 | Medium | **Fixed** | Gate code executes pinned from the installed tree. A modified gate in a pushed branch provably does not run. |
| ARP-007 | Low | **Fixed** | Malformed JSONL quarantines without advancing the cursor; AppleScript uses argument separation (live-probed inert); deps pinned; sink paths constrained. |

Each finding has an entry in
[docs/ROOT-CAUSE-REGISTER.md](https://github.com/tyroneross/agent-rally-point/blob/main/docs/ROOT-CAUSE-REGISTER.md).
An entry closes only on a proven adversarial control — a test that rejects the hostile input **and
fails when the fix is reverted**. Changed prose closes nothing.

## Where I have to correct my own record

Three things the follow-up audit found, stated because a disposition table you cannot check is
worth nothing:

- **The register was wrong in both directions.** Entries marked open whose fix had shipped, and
  entries whose stated mechanism the code no longer contains. The cause is structural: nothing
  wrote an entry when a finding *arrived*, and nothing updated its state at *merge*. Three of the
  eleven ARP-R findings never got an entry, and one (R-10) is now unrecoverable — it exists in no
  commit, test, comment, or log across 636 commits. Both ends are now one line each in the merge
  checklist.
- **Eight register entries specify an adversarial control that was never written.** That reads,
  six months on, exactly like an entry that was closed. The register had `fixed` and `controlled`
  and no way to say *"control specified, not yet written"*.
- **The fix run shipped five defects of the class it was fixing.** ARP-001's fix removed
  provisioning from the hook while the same hook still preferred `./target/debug/rally` twenty
  lines away, every test green. In-run review caught all five (RC-024).

## Notes on specific findings

**ARP-001.** You were right that the fix is not "verify harder on the automatic path". A lifecycle
hook is the wrong place to acquire and execute anything. The rule now: **a hook may observe and
inform; it may not acquire or execute.** The explicit installer refuses rather than degrades when
either verification check cannot complete.

**ARP-002.** One correction for the record: `intent`, `output`, and `owns` already rejected `"`,
`$`, and `` ` `` before your review, so those line cites were slightly stale. It does not rescue
the finding — `owns` still permitted `;` `|` `&` `>` `(` `)`, and the renderer put `validation`
verbatim into a bash block. The history shows two prior rounds each rejecting exactly the
characters the previous finding named: the denylist treadmill your report implicitly criticizes.
It is an allowlist now. The "proves a plan is safe to fan out" claim is gone — it grew stronger
over time while the property stayed absent.

**ARP-003.** Agreed, and not pretending to fix it in one pass. Making Cockpit the broker, or
integrating each CLI's native pre-execution approval, is an architecture change. Half-integrating
would reproduce exactly the defect you found: a boundary that looks real and is not.

**ARP-004.** Correct, and partly a documentation failure — the trust model was same-UID
single-operator, written in one code comment where no user would look. It now lives at
[docs/security/TRUST-MODEL.md](https://github.com/tyroneross/agent-rally-point/blob/main/docs/security/TRUST-MODEL.md),
including what remains undefended. Two residuals I want stated rather than buried: **`--json`
returns peer `subject`/`summary`/`evidence` verbatim**, and the follow-up audit found
`rally locate --json` is a *third* such sink that the skill docs did not warn about. And **writer
authentication is absent** — there are zero cryptographic primitives in the codebase. A related
finding (ARP-R-01/RC-063) makes the bound sharper than your report claimed: because a fact appended
directly to the segment file never passes the write boundary, **every** authority gate — lead
transfer, claim close, breadth, field bounds — is bypassable by a local process that can write
`.rally/`. The gates are advisory. The docs now say so, and a false comment in the source that
implied otherwise was the first thing corrected.

**ARP-007.** The JSONL item interested me most: it shares a shape with three defects already in my
register — an operation reporting success for a step the caller does not care about. Cursor
advanced, record lost, consumer told it was current.

## The bundles — you flagged them, and they are now measured

Your "unreviewed surface" section named the tracked git bundles. That item is **closed with a
result**, and the count was **18, not 26**:

All 18 were `git bundle verify`-ed, fetched into throwaway bare repos, and scanned blob-by-blob
after decompression — 1,252–1,812 blobs each, 626 distinct paths, including files deleted in
history. The scanner was mutation-validated first: a `.env`, an `id_ed25519`, AWS credentials, a
JWT, a `ghp_` token and a Slack token were planted then deleted so they existed only in history;
all 11 detector classes fired, none missed. **Zero credentials.** Two hits, both benign.

What that does **not** clear: the history contains the maintainer's machine paths, hostname, and
coordination ledger, present by inspection rather than "not ruled out". `.rally/log/`,
`.rally/archive/`, and `archive/` are de-tracked going forward, so a fresh clone starts with an
empty room — but the history is not being rewritten. That is a deliberate call: a rewrite is
irreversible, breaks every existing clone and both forks, and buys little once a tag is public.

## What I took from the report itself

Two sections did work nothing in my review pipeline was doing:

- **Positive containment observations** — without them a reader cannot separate "checked and sound"
  from "never looked at".
- **Unreviewed surface** — naming your own blind spots makes the report auditable and tells me
  where to look next. It is why the bundle scan above happened at all.

I adopted your method as a standing review rubric — trust-on-clone threat modeling,
doc-claims-vs-code contradiction hunting, false-boundary detection, prompt-to-shell and
prompt-to-context render-path tracing, and the report format including both epistemic sections —
with this issue cited as the exemplar.

Out of curiosity: what produced this report? If it is tooling you have written about anywhere, I
would like to read it.

## Still open, tracked separately

- **ARP-003** — Cockpit broker redesign, or native pre-execution approval integration.
- **ARP-005** — per-client Cockpit credentials; `client_id` is still self-asserted.
- **ARP-004 / RC-063** — authenticated writer identity. Until it exists, the authority gates are
  advisory and the docs say so rather than implying otherwise.
- **`cargo audit` / `cargo deny`** — you noted you had not run one. Neither had I. Still true.

## What this audit did not establish

Stated so the table above is not read as more than it is: no full `cargo test --workspace` run, no
`cargo audit`, no iOS build (so the Cockpit Swift findings are code-read only), and no live push
through the pre-push gate. Where a claim rests on a test I did not execute, the disposition
document names the test and says so rather than asserting it passes.

Maturity, plainly: Rally is proven on a small number of fresh macOS installs driven by one
operator. Multi-user was never the target, and your report is why that is now written down as an
explicit non-target rather than an unstated assumption.

Thanks again.

---

## Sources for the posting checklist above

- [What to do when you receive a vulnerability report — GitHub Blog](https://github.blog/security/vulnerability-research/a-maintainers-guide-to-vulnerability-disclosure-github-tools-to-make-it-simple/)
- [Best practices for writing repository security advisories — GitHub Docs](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/best-practices-for-writing-repository-security-advisories)
- [About coordinated disclosure of security vulnerabilities — GitHub Docs](https://docs.github.com/en/code-security/security-advisories/about-coordinated-disclosure-of-security-vulnerabilities)
- [About repository security advisories — GitHub Docs](https://docs.github.com/code-security/security-advisories/repository-security-advisories/about-repository-security-advisories)
