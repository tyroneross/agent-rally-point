<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Issue #52 disposition audit — every finding, verified at source

**Date:** 2026-08-06 · **HEAD:** `5fa2da2` · **Tagged release:** v0.2.0 at `542c884` (pushed) ·
**Local main is 5 commits ahead of `origin/main`.**

## What this document is

Every finding from the three review cycles that followed issue #52 gets one bucket, one piece of
evidence, and one rationale. Findings assessed: **ARP-001..007** (Lattice, issue #52),
**ARP-R-01..R-11** (Claude repo-trust re-assessment), **D1..D15** (Codex design review).

**Not-fixed with a written reason is a valid outcome. Not-fixed with nobody having looked is not.**
Where a finding had no assessment, this document writes one.

## Method, and its limits

I read the code, not the register. `docs/ROOT-CAUSE-REGISTER.md` was wrong in both directions this
cycle, and this audit found four more instances of each — entries marked open whose fix had shipped,
and entries whose fix detail describes a mechanism the code no longer contains.

Verified first-hand: the ARP-005/RC-063 authority chain (reproduced live against the shipped
`rally 0.2.0+542c884` binary), the PII measurement (counted across all 2,585 reachable blobs), the
D3/D4/D5/D8/D10/D11/D12/D13/D14 code sites, repo visibility, and four test suites executed
(`test_context_sanitization.sh` 12/12, `test_sanitizer_block_parity.sh` 5/5,
`test_git_identity_gate.sh` 11/11, `dynamic-workflows/tests/injection.test.mjs` 52/52) plus five
Rust renewal controls. Delegated to four parallel read-only agents and cross-checked against my own
greps: the ARP-001/002 hook and linter surfaces, the Cockpit surfaces, the watcher, and the ARP-R
set.

**Not verified:** no full `cargo test --workspace` run, no `cargo audit`, no iOS build, no live push
through the pre-push gate. Where a claim rests on a test I did not execute, this document names the
test and says so rather than asserting it passes.

The working repository was not modified. The live reproduction ran in an isolated scratch repo.

## Verdict summary

| Bucket | ARP-00x | ARP-R | D | Total |
|---|---|---|---|---|
| FIXED-AND-CONTROLLED | 1 | 6 | 5 | **12** |
| FIXED-UNPROVEN | 1 | 1 | 1 | **3** |
| MITIGATED | 4 | 2 | 2 | **8** |
| CONSCIOUSLY-DEFERRED | 1 | 1 | 5 | **7** |
| UNEXAMINED before this audit | 0 | 1 | 0 | **1** |

Seven items entered this audit either UNEXAMINED or deferred without a written rationale. All seven
now have one — written in §5, not merely flagged.

---

## 1. ARP-001 through ARP-007 (issue #52, Lattice)

| ID | Finding | Bucket | Evidence | Rationale / residual |
|---|---|---|---|---|
| **ARP-001** | Trusting the repo auto-runs provisioning on the host | **FIXED-AND-CONTROLLED** | `tests/hooks/test_no_autoprovision.sh`, 10 assertions, executed by `scripts/check-release-parity.sh:229-245` → `.github/workflows/ci.yml:89` and `.githooks/pre-push:591`. No provisioning call survives in `hooks/rally-coordination-hook.sh`; `RALLY_BIN` resolves only from PATH (`:259`), `$HOME/.local/bin` (`:265`), or the bare name (`:267`), and a caller-supplied value outside the repo is required (`:230-244`). `hooks/ensure-rally-binary.sh` exits 3 unless `RALLY_EXPLICIT_INSTALL=1`, set on exactly one line (`scripts/install-rally.sh:215`). | Closed. The SEC-001 follow-on (hook executing `./target/debug/rally`) is independently closed with a positive control. Two named residuals sit on *other* paths, not the open-the-repo path: RC-036 (the static guard is a filename grep over six files) and RC-035 (`install_rally_hooks.sh --global` still deep-copies repo command strings into `~/.claude/settings.json` with no shape check). |
| **ARP-002** | The "safe" workstream linter permitted command injection | **MITIGATED** | Fix is real and I executed the control: `dynamic-workflows/tests/injection.test.mjs` **52/52 pass**. Positive allowlists at `workstream-lint.mjs:109-158`; descriptor `validation` renders as inert prose (`packet.mjs:258-270`) with commands drawn only from the local `VALIDATION_RECIPES` registry; every "proves a plan is safe" claim removed from `PROTOCOL.md:13-21` and `README.md:16-18`. | **Residual: no gate runs the control.** `grep -n "node \|npm \|\.mjs\|dynamic-workflows"` across `check-release-parity.sh`, `run-quality-gate.sh`, and `ci.yml` returns nothing. The property holds today; nothing would catch its removal. This is the repo's own RC-023 defect class. See §5.7. |
| **ARP-003** | Cockpit's approval gate does not control child tool execution | **CONSCIOUSLY-DEFERRED** | Unchanged mechanism at `crates/cockpitd/src/transport/ws.rs:906` — `notify.notified().await` pauses the event pump, never the child. Codex spawns with `stdin(Stdio::null())` (`adapter/codex.rs:120-136`), so no denial can even be delivered. The **fail-safe** is FIXED-AND-CONTROLLED: `tool_blocked` carries `advisory: true` / `enforced: false` / a semantics string (`ws.rs:952-960`), asserted by `e2e.rs:2302 arp003_tool_blocked_is_marked_advisory`, and the acceptance test is written as a self-failing spec (`e2e.rs:2286 arp003_execution_gate_definition_of_done`, `#[ignore]`d, `panic!`s if run). | Rationale, `docs/security/AUDIT-2026-08-02-issue-52-triage.md:96-100`: *"Neither fits this run's budget honestly. The fail-safe lands instead… Half-integrating a native approval callback would produce exactly the same failure this finding describes — a boundary that looks real and is not."* Sound. Two residuals the deferral does not cover: `ios/Cockpit/.../ApprovalView.swift:16-47` still renders a shield + Deny/Allow and reads no advisory metadata (dead code — `ApprovalBannerView` has exactly one occurrence, its own declaration), and three docs still overclaim (§5.10). |
| **ARP-004** | Unsigned ledger prose enters privileged agent context | **MITIGATED** | Injection boundary controlled: `tests/hooks/test_context_sanitization.sh` **12/12** and `test_sanitizer_block_parity.sh` **5/5**, both executed by me and both gated in CI. GAP 2A enumerates every `additionalContext`/`systemMessage`/`agent_message`/`permissionDecisionReason` sink and requires each to emit the sanitized value or match an exact-text allowlist. ARP-R-04 added write-boundary field bounds (`rally-protocol/src/ledger.rs:212-288`) and control-character rejection on `tool`/`target`/`role` (`write_authority.rs:161-178`). | Two residuals, both deferred with written rationale. (1) **The `--json` sink** returns peer `subject`/`summary`/`evidence` verbatim — `lib.rs:7745`, `check.rs:210/220/239`, `board.rs:90/121/205`, `next.rs:864`. Register `:1170`: *"an envelope change landing beside three security fixes is how a downstream break gets attributed to the wrong commit."* (2) **Writer authentication is absent** — zero cryptographic primitives in `rally-protocol` or `rally-cli`. `TRUST-MODEL.md:48-50` states it. New in this audit: `rally locate --json` is a third unsanitized sink and is not named in SKILL.md (§5.11). |
| **ARP-005** | One bearer token grants global Cockpit control | **MITIGATED** | Owner binding is real and adversarially tested. `auth.rs:43-53` constant-time compare; principal per connection `:91-93`; enforcement on send/steer/close/approve (`ws.rs:445-449, 465-469, 693-697, 485-521`); `repo_path` canonicalized into an allowlist (`policy.rs:115-167`); non-loopback bind refused without `COCKPIT_ALLOW_NON_LOOPBACK=i-understand-the-risk` (`policy.rs:186-204`). Twelve `arp005_*` tests in `crates/cockpitd/tests/e2e.rs`. | **Residual: `client_id` is self-asserted**, so any token holder inherits any client's sessions including `steer`. Pinned by `e2e.rs:1846 arp005_client_id_impersonation_is_not_prevented`, which asserts the bypass and is written to fail the day it is fixed — the strongest honest form of a documented residual. Calibration: the default `COCKPIT_REPO_ALLOWLIST` is `$HOME` (`policy.rs:11-12`), which bounds the escape but not much. Two attached items are weaker: **RC-021 is UNEXAMINED** (§5.8) and RC-022's iOS half is unstarted. |
| **ARP-006** | Pre-push hook executes code from the commit being pushed | **MITIGATED** | Gate scripts resolve from a pinned ref outside the pushed tree (`.githooks/pre-push:235-257, 371-406`); five paths pinned (`:221`); working-tree host tests absent from or differing from the pin are refused (`check-release-parity.sh:195-226`). Controls: `test_prepush_pinned_gate.sh`, `test_prepush_gate_scope.sh` (20 assertions), `test_prepush_pin.sh` (17), all run in CI at `ci.yml:97-111`. | **RC-046 is still open and I verified the branch directly.** `.githooks/pre-push:264-266` warns and continues when the pin ref does not resolve; both the vacuity check and the env-pin ack sit inside `:288`, so both are skipped, and every dispatcher runs from the pushed tree at rc=0. Two tests *assert* rc=0 on that path, locking it in. Second residual, documented candidly at `:51-62`: the pin decides who dispatches, never what runs — `cargo test` still compiles the pushed tree's `build.rs`, proc macros, and test bodies. |
| **ARP-007** | Watcher integrity and hardening gaps | **FIXED-UNPROVEN** | All three substantive controls are implemented correctly: quarantine without cursor advance (`watcher.py:170-192`), AppleScript `on run argv` with list-form `subprocess.run` (`dispatch.py:124-152`), sink containment with `O_NOFOLLOW` (`dispatch.py:50-114`). Well-designed tests exist, including a live-`osascript` injection test with a marker-file assertion (`tests/test_dispatch.py:146`). | **No gate executes any of them.** Zero hits for `tools/`, `pytest`, or `uv` in `ci.yml`, `check-release-parity.sh`, `run-quality-gate.sh`, or `.githooks/pre-push`. The register grades RC-019 `controlled`; the repo's own bar is `check-release-parity.sh:158` — *"A control no gate executes is a hypothesis."* Dependency pinning is nominal: `pyproject.toml:29` declares a range, `uv.lock` pins `watchfiles==1.2.0`, and nothing runs `uv sync --locked`. Compounding: **RC-027** — the hardened component tails `~/.agent-rally-point/apps/<slug>/changes.jsonl` and no crate in this repo writes it (every `crates/` hit is a reader, a doc comment, or a test fixture). |

---

## 2. ARP-R-01 through ARP-R-11 (Claude repo-trust re-assessment)

| ID | Finding | Bucket | Evidence | Rationale / residual |
|---|---|---|---|---|
| **ARP-R-01** | The lead seat was unauthenticated; every room-wide control rooted in it | **MITIGATED** | Gate is real and I confirmed it live: `rally lead assign --tool codex:rogue --to codex:rogue` against a live incumbent returns exit 2 with a refusal naming the four legitimate transfer conditions. `write_authority::assert_lead_transfer_authorized` is called from `DirectRoomStore::append_fact` (`store.rs:2046`), so it binds the daemon path too. Controls: `tests/lead_seat_authz.rs` (11), `tests/room_freeze_admission_time.rs` (4), `tests/write_authority_daemon_parity.rs`. | Residual is pinned by `lead_seat_authz.rs:361 impersonation_is_not_stopped_and_this_test_says_so`. **New in this audit: the residual is wider than `--tool` impersonation.** The gate sits at the CLI/store write boundary; the ledger is a file. A hand-appended JSONL line moves the seat with no CLI call at all — reproduced end to end against the shipped v0.2.0 binary. See §5.1. |
| **ARP-R-02** | The claim-takeover gate covered two of the four kinds that close a claim | **FIXED-AND-CONTROLLED** | Authorization moved to `write_authority::assert_claim_close_authorized`, called once from `append_fact` and keyed off `claim_authority::closes_active_claim` — the same predicate the projection uses, so a fifth closing kind cannot add a fifth bypass. `tests/claim_takeover_authz.rs`, 12 tests including the `claim_expired` alias and receipt-strip-then-seize; mutation-validated, 7 of 12 die on revert. | Closed. Bounded by RC-063 like every `fact.tool` gate. |
| **ARP-R-03** | Four docs claimed hooks "never block" while opt-in paths do | **FIXED-UNPROVEN** | `README.md:13`, `RALLY.md:166-177`, `SKILL.md:248-256`, `TRUST-MODEL.md:176-186` now name the same three opt-in switches — `RALLY_HOOK_STRICT=1`, `check before-write --strict`, `RALLY_BEFORE_WRITE_FAILCLOSED=1`. The in-tree marker (`cli.rs:1053`) covers a different sentence: `rally lead --help` claiming "never enforces". | **No test binds the prose to the behaviour.** Behaviour is tested (`test_rally_coordination_hook.sh:441-566`, `tests/check_perf_failclosed.rs`); nothing ties the docs to it, which is exactly RC-C. Three stale residuals: `docs/AUTO-COORDINATION-HOOKS.md:31-32` still says "never blocks an edit" unqualified with the correction 125 lines later; `RALLY.md:230-233` says the seat gates **two** room-wide capabilities where `cli.rs:1059-1063` says **three**; `cli.rs:528` still reads "records/exposes only". **ARP-R-03 has no register entry.** |
| **ARP-R-04** | Retrospective renderer + write-boundary field bounds | **FIXED-AND-CONTROLLED** (one residual) | `retrospective.rs:120` opens `mod untrusted`; `Span(String)` at `:160` has no constructor outside the module and no way back to `String`, so the raw `Fact` is unreachable from the renderer. Bounds at `ledger.rs:212-288` (subject 4 KiB, summary 16 KiB, whole-fact 64 KiB), thresholds measured over 6,792 real facts. Eleven tests in `tests/retrospective_sanitizer.rs`; parity by `write_authority_daemon_parity.rs:314`. | Residual, and the code's own comment (`ledger.rs:191`) says ARP-R-04 "shipped with three of four field families covered" without naming the fourth. Determined from source: **`status`, `severity`, `ref_id`** are rendered by `SafeFact` but appear in neither `FactTextFields` nor the identity control-character check — covered only at the renderer, which is the posture that produced ARP-R-04. **ARP-R-04 has no standalone register entry** (bullet only, register `:2491`). |
| **ARP-R-05** | The pre-push pin only warned on a vacuous default pin | **FIXED-AND-CONTROLLED** | `.githooks/pre-push:315-321` now refuses a vacuous default pin unless `RALLY_PREPUSH_ACK_VACUOUS_PIN=1`, with the principle stated at `:317` — *"a check that passes on every normal push certifies nothing."* `hooks/ensure-rally-binary.sh` joined the pin set (`:221`) as compare-only; "all gates green" appears nowhere as emitted text. Control: `tests/hooks/test_prepush_pin.sh`, 17 assertions, mutation-validated three ways. | Closed. Operator cost: pushing `main` now requires the ack env var. |
| **ARP-R-06** | A fresh clone inherited the maintainer's live room and 68.6 MiB of foreign history | **MITIGATED** for clones · **CONSCIOUSLY-DEFERRED** for history | De-tracking verified: 0 tracked `.rally/log/` files, 0 tracked bundles, `archive/` and `.rally/*` ignored with the un-ignore footgun removed (`.gitignore:84-128`). HEAD carries 4 `/Users/tyroneross` occurrences, all in files that quote the path as evidence. | History is unchanged and stays unchanged — the operator's decision, recorded with its rationale in §5.2. The forward control that does close the class does not exist yet; §5.2 specifies it against the working template already in-tree. |
| **ARP-R-07** | `rally init` taught `rally room --json` with no untrusted-data caveat; six doc links were dead outside this repo | **FIXED-AND-CONTROLLED** (one residual) | One `POINTER_DOCS` source (`init.rs:59-70`) drives both `pointer_block()` and `build_manifest()`; links emit only when the target resolves (`:165-171`); the caveat is emitted at `:135`. Controls: five integration tests in `tests/init_consumer_repo.rs` including `room_json_pointer_carries_untrusted_data_caveat:246`, plus a unit assertion at `init.rs:519-533`. | Residual, verified by me: **this repo's own `CLAUDE.md:14` and `AGENTS.md:14` still carry the pre-fix block** — `- **Current state:** \`rally room --json\`` with no caveat. The generator is fixed; the files it would have generated were never regenerated. These are the two files every agent reads on entry. Tests run against temp fixtures, so nothing catches it. |
| **ARP-R-08** | `ident()`'s density heuristic was vowel-biased; shell-shaped text rendered bare | **FIXED-AND-CONTROLLED** | Default inverted at `hooks/rally-coordination-hook.sh:758-759` — quote everything, render bare only on a strict positive shape. `isBareShape()` at `:805-820`: ≤64 chars, no `?`, ≤2 words per part, every word ≥3 chars, ≤4 words overall. `clipId()` now runs after the shape judgement (defect B). Control: `test_rally_coordination_hook.sh` Test 17 (`:915-1180`), five cases including `now-run-rm-rf`, `curl-x-sh`, `chmod-a-x` as both scopes and tool ids. | Closed, and it **supersedes RC-040's still-open item 1** — see §4. Residual stated in-code at `:794-797`: a two-word value still renders bare, because two words per part is the floor real ids need. `renderScopes()` joins with `", "` so N two-word scopes cannot weld into a sentence. |
| **ARP-R-09** | The register read "NOT fixed" while a commit was titled "close RC-041" | **FIXED** (documentation reconciliation) | Register `:1289-1292` records the correction in place: *"Both were partly right and the register was the one that misled: 3C and 3D were addressed there, 3A and 3B were not. Corrected in place rather than re-titled, so the drift is legible."* | Correct handling. No test is possible or needed. The underlying RC-041 3B remains an owner decision (register `:1300`: *"Decision owed, deliberately not taken this run"*). |
| **ARP-R-10** | — | **UNEXAMINED** | **This finding does not exist in any artifact in this repository.** Searched: `git log --all --grep`, `git log --all -S`, `git grep` across all 636 commits, the full worktree including ignored and untracked paths, `.rally/log/` and `.rally/archive/`, all three `.rally.bak-*` trees, `git stash list`, `git log -g`, CHANGELOG, docs, tests. The identical search returns hits for ARP-R-09 and ARP-R-11, so the apparatus works and the absence is real. | Assessment written in §5.6. No content is invented for it. |
| **ARP-R-11** | `rally_wake.py` had no `--` terminator, no target validation, no provenance label | **FIXED-AND-CONTROLLED** | All five elements at `scripts/rally_wake.py`: `--` terminator plus `-H` hex payload (`:348-354`), target shape validation using `\A`/`\Z` rather than `^`/`$` (`:277-302`, documented because `$` would admit `rev:0.0\nkill-server`), provenance label with forged-label scrub (`:154-231`), one invocation for clear+payload+submit (`:312`). Control: `tests/inject_security.rs:613 arp_r11_the_python_wake_path_has_one_chokepoint` — an AST analyzer with an inline mutant negative control, replacing a parity test that graded a spelling and matched zero lines. | Closed. |

---

## 3. D1 through D15 (Codex design review)

Register entries were pinned at `006d417`. v0.2.0 tagged at `542c884`; five commits merged locally
after it. **The register does not reflect that merge, and four D findings changed state because of
it.**

| ID | Register | Finding | Bucket at HEAD | Evidence |
|---|---|---|---|---|
| **D1/D6** | RC-065 | Routing dropped four `#[serde(skip)]` snapshot projections, so the daemon changed behaviour | **FIXED-AND-CONTROLLED** | `SnapshotInternals` side-channel keeps the public schema unchanged. `tests/snapshot_wire_internals.rs` — `direct_and_routed_compose_the_same_room:508`, `routed_next_advances_the_read_checkpoint:572`, `routed_next_coalesces_wake_intents:608`, plus the adjacent-move structural test `every_skipped_snapshot_field_rides_the_wire_side_channel:707`. Mutation-validated four ways. |
| **D2** | RC-066 | Three sources stated three different demotion contracts | **FIXED-AND-CONTROLLED** | Made structural, not documented: `author_liveness: Option<Liveness>` became `author_past_heartbeat_window: bool` (`relevance.rs:140`), so there is no `Liveness` value left in the ranking path to disagree about. Control: `store.rs:9939 heartbeat_gap_demotes_even_when_liveness_is_not_provably_stale`, which asserts its own premise first and also asserts the author stays visible in `squads` (the adjacent error). |
| **D3** | RC-053 | Lease renewal wrote to a sidecar no expiry path reads | **FIXED-AND-CONTROLLED** — ⚠️ **register says `mechanism` / NOT fixed** | Commit `20e69ad` took the option the register said was owed: renewal is now a durable `FactKind::ClaimRenewed` fact, folded into the active-claim projection by `claim_authority::latest_renewed_lease` (`:200-210`) and applied at `:170` and `:189`. The reaper reads the same projection (`reaper.rs:416-418`). **I executed the controls — 5/5 pass:** `reaper::durable_renewal_after_original_expiry_survives_reap`, `store::claim_lease_renewal_appends_durable_event_and_projects_effective_lease`, `claim_lease_renewal_is_monotonic_and_retry_idempotent`, `sec001_takeover_guard_tests::reaper_lease_expired_close_is_refused_after_concurrent_renewal`, and the caller `tests::heartbeat_renews_every_owned_claim_durably` — plus daemon parity `rallyd_core::routed_renewal_appends_the_same_durable_fact_as_direct_mode`. The fix went further than the register asked: ownership and monotonicity are re-checked under the mutation lock (`store.rs:1984-2000`), and a `ClaimExpired` append is refused there if the lease was renewed (`:1973-1979`). One gap: the register's control specified an intervening unrelated `Claim` from a second tool, and no test includes that step. It is structurally moot now that renewal is a ledger fact rather than a sidecar edit — but nothing pins that. |
| **D4** | RC-054 | The byte budget is a bucket allocator and can report `over_budget: false` while over budget | **CONSCIOUSLY-DEFERRED** — rationale written in §5.3 | All four sub-claims hold at HEAD. `BUDGETED_BUCKETS` is still four entries (`store.rs:3712-3717`); `over_budget` is derived once from the initial reserve and cleared when it fits (`:3877-3881`, `:3830`) while two later subtractions use `saturating_sub`; `emitted_bytes` is computed at `:3833` before `composition` is assigned at `:3841`, so it excludes the block it is reported in; the `if over_budget \|\| !buckets.is_empty()` guard at `:3832` emits no composition block at all when every bucket has exactly one item. `agent_injectability` is built after composition returned (`lib.rs:2989-2990`) and is unbudgeted. |
| **D5** | RC-055 | The never-cut classes have no structural bound | **CONSCIOUSLY-DEFERRED** — rationale written in §5.4 | Unchanged. `system_health` dedup keys on `f.subject.clone()` — the whole string (`store.rs:3432`) — while the four-element prefix list 30 lines above is only a classifier (`:3396-3403`). Measured previously: 731 health facts collapse to 250 distinct subjects, not 4. `handoff_assigned_to` still returns `true` for `None \| Some("all")` (`:3726-3731`), so a broadcast handoff is never-cut for every caller. |
| **D7** | RC-056 | The reaper reported success on a failed durable write | **FIXED-AND-CONTROLLED** | `ReapReport.write_failures` counts failed appends separately (`reaper.rs:124`), `applied` is now `apply && attempted_actions > 0 && write_failures == 0` (`reaper.rs:744`), and the lead-relinquish arm no longer discards its result (`:666-715`). Controls: `tests/reaper_write_integrity.rs` — `failed_lead_relinquish_write_is_not_reported_as_applied:284`, `failed_claim_expiry_write_is_not_reported_as_reaped:401`, and the negative control `successful_lead_relinquish_is_reported_and_lands:353`. Two gaps: the register is candid that a *partial* pass is honest but untested, and the third write site — the handoff-append failure at `reaper.rs:642-653` — has no test at all. |
| **D8** | RC-057 | The reaper's rate limit is a lock-free read-then-write and its stated bound is not established | **CONSCIOUSLY-DEFERRED** | Unchanged at HEAD: `std::fs::write` at `reaper.rs:273`, no lock, no `O_EXCL`. The false bound was deleted from the comment rather than made true — `:247` now names the absence explicitly. Rationale, register: *"building a second locking primitive inside `reaper.rs` to bound a feature that ships OFF by default would put the fix in the wrong layer… An honest unbounded comment beats a bound the code does not deliver."* Sound, and RC-051's precondition for re-enabling the default stays open because of it. |
| **D9** | — | Authority was re-derived against the current lead rather than the lead at the fact's own seq | **FIXED-AND-CONTROLLED** | Closed with ARP-R-01: authority is admission-time everywhere, `room_freeze_id` decided against `claim_authority::lead_as_of(facts, blocker.seq)`. Control: `tests/room_freeze_admission_time.rs` (4). |
| **D10** | RC-058 | The write path re-reads the whole ledger about five times per append | **MITIGATED** | The reaper completes now — memoized segment fold plus projections moved inside the arms that read them, and `--apply` bounded by `DEFAULT_REAP_APPLY_BUDGET_MS = 2000` (`reaper.rs:354-363`) with a forward-progress floor. Four full passes per verified append remain, enumerated in the register. Controls: `tests/reaper_scale.rs` plus deterministic global-floor unit tests. Rationale: *"This is staging, and it is named as such. It does not hide the cost."* Accurate — the cost is tabulated and the four remaining passes are listed. |
| **D11** | RC-059 | Rust and the hook disagree about who is present and which claims bind | **MITIGATED** — two of three halves closed. ⚠️ **register says `mechanism` / NOT fixed, and is wrong in both directions** | Commit `08fb2ab` deleted `leaseExpired()` outright (0 occurrences remain) and rewrote the claim filter to `.filter(c => c && c.tool !== tool)` (`hooks/rally-coordination-hook.sh:893`); presence now retains every non-`rally` squad (`:845-855`). Control: `tests/hook_projection_parity.rs:269`, which drives the real CLI **and the checked-in shell hook**, asserts its own premises, and asserts `prompt_visible == check_visible == room_visible`. **Residual, and it is the half the register named explicitly: handoffs still diverge.** `:897` still hides any handoff older than 24 h whose author is not in `recentlyActiveTools`, while it stays open in the room — and `grep -c handoff` on the parity test returns **0**. See §5.12 for a second, silent divergence found underneath it. |
| **D12** | RC-060 | `--include-archived` is complete only when no explicit budget is supplied | **CONSCIOUSLY-DEFERRED** — rationale written in §5.5 | Unchanged: `(Some(explicit), _) => Some(explicit)` resolves before `(None, true) => None` (`store.rs:3821-3822`). Neither of the two remedies the register named was taken. **Worse than the register records:** `RALLY.md:317-318` states the property unconditionally — *"`--include-archived` is the drill-in, so the budget does not apply to it. An escape hatch that is itself truncated is not an escape hatch."* The doc now contradicts the code using the exact sentence the register quotes as the justification. |
| **D13** | RC-061 | A third implemented policy with no consumer: envelope authorization | **FIXED-AND-CONTROLLED** (by deletion) — ⚠️ **register says `observed`** | Commit `25b4ef9` deleted the uncalled policy and its isolated tests: `authorize`, `required_role`, and `PrivilegedAction` return zero hits in `crates/rally-cli/src/event_envelope.rs` at HEAD. Deletion is the right close for dead policy — it removes the thing a future reader would mistake for a control. Durable envelope validation and dedup are unchanged. |
| **D14** | RC-062 | First-run corruption: the mechanism is structurally possible; causation is not claimed | **CONSCIOUSLY-DEFERRED** | Unchanged and correctly scoped. `read_db_event_stats` still opens a second independent pool on the same path (`store.rs:5134`) and can quarantine the file while the room-lifetime pool holds the old inode; `quarantine_corrupt_db` renames the main file fatally (`:5250`) and the `-wal`/`-shm` siblings best-effort with `let _ =` (`:5259-5265`). Rationale: *"What settling it requires, stated so nobody settles it cheaply: a repeated first-run concurrency test with pool-lifetime and WAL tracing… run N-consecutive. Wall-clock repro without that tracing distinguishes nothing."* Blast radius bounded to the derived cache; segments are canonical. |
| **D15** | RC-063 | Identity is descriptive, not authoritative, and that bounds every lead-gated fix | **CONSCIOUSLY-DEFERRED** — decision explicitly unmade | The register states both coherent options and refuses to pick: *"The choice is owed and unmade. This entry does not pick. It records that (1) and (2) are both coherent, that the status quo is neither."* That is honest. **New in this audit: the residual is wider than the register records, and I reproduced the wider version live against the shipped binary.** See §5.1. |

---

## 4. Where the register and the code disagree

The register was wrong in both directions. Correcting it is cheap and worth doing before the next
cycle reads it as ground truth.

**One cause explains most of it.** `git log -1 -- docs/ROOT-CAUSE-REGISTER.md` returns `240892d`,
which is an ancestor of the `v0.2.0` tag. The register has not been edited since before the release,
so it cannot reflect `25b4ef9`, `20e69ad`, `08fb2ab`, `52f343e`, or `5fa2da2` — and those five
commits are exactly what moved D3, D11, and D13. This is not carelessness in the entries; it is a
missing step at merge. The durable fix is a merge-time obligation, not another audit.

**Marked open; the fix has shipped.**

1. **RC-053 (D3)** reads `mechanism` / **NOT fixed**. Renewal is durable at HEAD with five passing
   controls including the production caller and daemon parity.
2. **RC-059 (D11)** reads `mechanism` / **NOT fixed**, and is wrong in both directions at once. The
   claims and presence halves were closed by `08fb2ab` with a parity test that drives the real hook.
   The handoff half — which the entry names explicitly — is untouched and has zero test coverage. The
   entry should be split, not flipped.
3. **RC-061 (D13)** reads `observed`. The dead policy was deleted by `25b4ef9`.
4. **RC-040 STILL-OPEN item 1** ("a ≤3-word directive renders bare") describes a density gate the
   code no longer contains. ARP-R-08 inverted the default; `codex:edit-freely-now` renders
   **quoted** at HEAD. The register carries both the superseded fix detail (`:1118-1130`) and the
   accurate ARP-R-08 entry (`:2473-2482`) without marking the first stale.

**Marked closed or understated; the code is weaker.**

5. **RC-019 (ARP-007)** reads `controlled`. No gate executes the watcher tests, and "dependencies
   pinned and locked" is true of the file and of no executed path.
6. **RC-036** reads as though the whole no-autoprovision guard is a filename grep. Tests 1–4 of that
   suite are behavioural, stubbing eight commands as recorders. The real residual is narrower: a
   provisioner using a verb outside that eight-command set, or any verb on an untested phase.
7. **RC-027** reads "nothing writes it". Precisely: no *in-repo* component writes it. An out-of-repo
   build-loop script writes the channel on a degraded fallback path only — 20 records, the newest a
   `"degraded-breadcrumb"`. The conclusion holds; the mechanism sentence should be narrowed.
8. **ARP-R-03 and ARP-R-04 have no register entries.** ARP-R-10 has no entry anywhere (§5.6).

**Stale line citations, no security impact:** RC-015 (`ws.rs:535-701` → `:806-1016`), RC-017
(`ws.rs:322-351` → `:445-483`), RC-034 (`:51-61` → `:51-62`; `ci.yml:97-110` → `:97-111`), RC-046,
and RC-015's `ApprovalView` → the type is `ApprovalBannerView`. Both the pre-push header (`:51-52`)
and `check-release-parity.sh:172-178` still say "three dispatcher scripts" where four now dispatch —
stale text that *understates* coverage.

---

## 5. New assessments

Each item below entered this audit either UNEXAMINED or deferred with no written rationale. Each now
has one.

### 5.1 The authority model — ARP-005's class, RC-063's mechanism, reproduced live at v0.2.0

**The finding.** Rally decides every "only the lead may X" control by comparing `fact.tool` against
the projected lead, and `fact.tool` is a string the writer chooses. The register bounds this at
RC-063 and the trust model states it plainly. What no document states is that **the gate protects
the CLI, and the ledger is a file.**

**Real at HEAD — reproduced end to end against `rally 0.2.0+542c884` in an isolated scratch repo.**

1. `claude_code:honest` enters an empty room and takes the first-join seat.
2. `rally lead assign --tool codex:rogue --to codex:rogue` is **refused**, exit 2. ARP-R-01's gate
   works exactly as documented.
3. One JSONL line appended to `.rally/log/<engagement>.jsonl` by hand — a `decision` fact with
   `subject: "role:lead"`, `tool: "codex:rogue"` — and `rally lead show --json` returns
   `current_lead: codex:rogue`.
4. As lead, the rogue claims `workspace:*`. Accepted.
5. Every other agent's file claim now fails: `claim conflict: codex:rogue holds workspace:*`.
   RC-037's room-wide lockout, fully restored, from one appended line.

The mechanism is structural, not a bug. `write_authority::assert_write_authorized` is called from
`DirectRoomStore::append_fact` (`store.rs:2041-2046`). A text append never reaches `append_fact`;
the projection reads segments directly. Every write-boundary control in the codebase —
lead transfer, claim close, breadth, field bounds — is bypassed the same way.

**What the shipped documentation claims versus what the code does.** The documentation is honest and
close to complete. `TRUST-MODEL.md:48-50` says facts are unauthenticated; `:92-98` says the gates
"do not stop an adversary and they are one flag deep… Do not read them as an authorization
boundary"; `:117-120` says "a local process that can write `.rally/` can write facts";
`CHANGELOG.md:333-338` repeats it; `SKILL.md:274-289` tells agents the same. `RALLY.md:235` is the
one shipped file that is thin, and it is a code comment.

One in-code claim is false. `lib.rs:13883-13885`: *"the gate itself is at the write boundary in
`write_authority::assert_lead_transfer_authorized`, so a hand-built fact or a routed daemon request
clears the same bar this command does."* True of a `Fact` constructed in Rust and passed to
`append_fact`. False of a line appended to the segment file — which is what a reader will understand
by "hand-built", and which is the move I just performed. This is RC-C committed inside the fix for
RC-C, exactly as RC-024 describes.

**Cost to fix.** Not patchable. Option (2) in RC-063 — a registry or daemon mints an opaque session
lease, `tool` and `role` derive from it, facts are stamped at a trusted boundary, and privileged
actions authorize against the session that held the seat at the relevant epoch — is a protocol
change across `rally-protocol` and every writer, plus a migration for the ledgers already in the
field. **Effort: L.** Option (1) — drop the claim, make every gate warn, remove "only the lead may"
from code, refusals, `TRUST-MODEL.md`, and SKILL.md — is **Effort: S** and is consistent with the
north star's "warnings over hard locks".

**Recommendation.** Two things, in order.

1. **Correct the one false in-code claim now.** Amend `lib.rs:13883-13885` to say the gate binds the
   CLI and daemon write paths and does not bind a direct file append. One sentence, no behaviour
   change, and it closes the only place the docs currently overclaim. An agent can do this.
2. **Decide (1) or (2) before the next release, not during it.** Continuing to ship gates without
   deciding produces controls whose suites pass and whose stated property is false — which is what
   RC-050 already found. This is the operator's call and it is the single largest open item in this
   audit.

Calibration, stated so the severity is not inflated: this requires local write access to `.rally/`
or the ability to land a commit. The single-operator trust model holds it in scope only for a repo
that commits its ledger — which this repo no longer does. It is a real and complete bypass of every
authority gate; it is not a remote one.

### 5.2 PII in the published history — deferred by decision, closed going forward

**The exposure, measured rather than estimated.** I scanned all 2,585 reachable blobs across every
ref:

| Artifact | Count | Distinct paths |
|---|---|---|
| `/Users/tyroneross` occurrences | **2,660** | 24 |
| `tyrones-macbook-pro` occurrences | **1,165** | 13 |
| `.rally/log` + `.rally/archive` blob objects | **71** | — |
| `dev/git-folder/build-loop` references | **334** | — |
| Git bundles | **18**, 65.1 MiB | `archive/bundles/`, `archive/stashes-*/` |

**Reach, verified not inferred.** `gh repo view` returns `"visibility":"PUBLIC"`, `isPrivate:false`,
2 forks, 2 stars. The `v0.2.0` tag resolves on `origin` at `542c884`. Anyone can fetch all of it
today. That is the correct severity anchor: not "a contributor could", but "the internet can".

**What the credential audit settled, and where that result lives.** A decompressed per-blob scan
with a mutation-validated scanner cleared all 18 bundles — zero credentials. That result exists only
as a Rally fact in this machine's ledger (seq 8058, 2026-08-05T07:56:07Z), and `.rally/log/` is now
de-tracked, so **the finding is not in the repository.** `TRUST-MODEL.md:259-260` still tells every
reader the bundles "have **not** been audited for credentials. …No secret is known to be present;
none has been ruled out either." That sentence is now false in the reassuring-negative direction and
should be corrected — an agent can do it, and it is the cheapest item in this audit.

**Disposition: CONSCIOUSLY-DEFERRED, and the decision is sound.** The operator's rationale, recorded
here as settled policy: *the history is not being scrubbed; future commits are.* A rewrite is
irreversible, breaks every existing clone and both forks, and buys little once a tag is public —
the cost lands immediately and in full, the benefit is retroactive and partial. Nothing in this
document proposes a rewrite, and the "rewrite or forward-fix" framing is not an open question.

**De-tracking is not a rewrite and it already landed.** `.rally/log/`, `.rally/archive/`,
`.rally/RETROSPECTIVE.md`, and all of `archive/` are out of the index; `.gitignore:84-128` closes the
un-ignore that re-admitted them. A fresh clone starts with an empty room. Verified: 0 tracked
bundles, 0 tracked ledger files, 4 remaining `/Users/tyroneross` occurrences in HEAD, every one a
finding quoting the path as evidence.

**The actual remedy is a content check at the commit boundary — and it does not exist.** I searched:
zero occurrences of `gitleaks`, `trufflehog`, `detect-secrets`, or any path/hostname pattern across
`.github/workflows/`, `scripts/`, `.githooks/`, and `tests/`. Nothing stops the next home path from
landing.

The template is already in-tree and working. `scripts/check-git-identity.sh` is wired into
`.githooks/pre-commit:34` (`--pending`, before the commit object exists) and `.githooks/pre-push:503-509`
(`--commits <sha> --not --remotes=origin`, backstop for commits made elsewhere). It reads a
config-driven allowlist, **fails closed** when the config cannot resolve, prints nothing on success,
and is itself in the pre-push pin set (`:221`). Its control is `tests/hooks/test_git_identity_gate.sh`
— **11 assertions, which I executed, 11/11 pass**, including a mutation check that a decoy allowlist
rejects a normally-allowlisted address.

**Recommendation — a sibling script, not an extension.** `check-git-identity.sh` reads only the
author and committer fields and never the commit body, deliberately, so that the required
`Co-Authored-By:` trailer keeps working. Widening it to read content would break that guarantee.
Add `scripts/check-content-hygiene.sh` with the same shape:

- Same two chokepoints, same `--pending` / `--commits` interface.
- **Scan only newly ADDED lines** in the diff, never whole files — the 24 paths already carrying
  these strings must not fail every future commit that touches them.
- Same `--not --remotes=origin` scoping on push, which excludes existing history structurally rather
  than by a date cutoff that drifts. Inherit the empirically-verified fallback at
  `.githooks/pre-push:489-501`: `--not --remotes=origin` silently excludes nothing when no
  `refs/remotes/origin/*` resolves, so a first push would otherwise scan all of history.
- Patterns in `config/content-hygiene-denylist.txt`, resolved the same way and **failing closed** when
  absent: home directory prefixes, machine hostnames, private sibling repo names.
- Silent on the clean path. Add it to `GATE_SCRIPT_PATHS` (`.githooks/pre-push:221`) so the gate's own
  code is pinned.
- Control: `tests/hooks/test_content_hygiene_gate.sh`, mutation-validated the way the identity gate
  is — a decoy denylist must reject a normally-clean line, or the pass proves nothing.

**Effort: S–M.** Roughly the size of the identity gate, which is a known quantity in this repo.

One calibration note, since it applies to this class of check. The "foreign GitHub noreply" detector
produced 78 false positives when run as a broad cross-repo detector. The shipped gate avoids that by
putting the allowlist first (`check-git-identity.sh:53-58`), so the pattern only fires on an address
that is neither known-good nor known-bad. A content gate should be built the same way: deny-by-default
on a narrow, curated pattern set, not a broad heuristic. The adoption cost — every new contributor
is blocked until allowlisted — is real and is the right trade for a public repo with two forks.

### 5.3 D4 / RC-054 — the budget's honesty signal can be wrong

**What it is.** `over_budget` is computed once, from the initial never-cut reserve. Two more
subtractions happen afterwards — the caller's assigned handoffs and the guaranteed top item of every
non-empty bucket — both with `saturating_sub`, neither able to raise the verdict. A response whose
reserve fit but whose guaranteed items did not is over budget and reports `over_budget: false`.
`emitted_bytes` is measured before the composition block it is reported in exists. And when every
budgeted bucket holds exactly one item, no composition block is emitted at all — while
`store.rs:535` documents its absence as *"the positive statement that this response is complete."*

**Real at HEAD.** Yes, all four parts, cited in §3. `RALLY.md:305-306` also states the property the
finding shows can be false: *"the room ships over budget and says so."*

**Why it was not fixed, and why that was reasonable.** RC-048 — the same class, one layer out — was
deferred with an explicit reason: *"Making three more sections budget-aware changes what every agent
reads on every room call; landing that beside three security fixes in a held release is how a
regression gets attributed to the wrong change."* That reasoning transfers. It was never written
down for D4.

**Cost.** The reporting half is small: recompute `over_budget` after the last subtraction, measure
`emitted_bytes` on the final serialized payload, and always emit the composition block. **Effort: S.**
The sizing half — bringing fixed fields, `totals`, `readers`, `mission`, and `agent_injectability`
inside the reserve — is **Effort: M** and changes what every agent reads.

**Recommendation.** Split it. Take the S half now: a signal that says the ceiling held when it did
not is worse than no signal, and that is this repo's own recurring lesson. Defer the M half to a
release where room composition is the headline change, not a passenger. Correct `RALLY.md:305-306`
in the same commit as the S half.

### 5.4 D5 / RC-055 — the never-cut classes have no structural bound

**What it is.** `system_health` is exempt from the budget and deduplicates on the complete subject
string, while the four-prefix constant 30 lines above reads like the key and is only a classifier.
`external-intake:` interpolates an absolute filesystem path into the subject (`lib.rs:2449` and `:2549`), so its
cardinality is bounded by the paths anyone ever passes. Measured: 731 health facts, **250** distinct
subjects. Separately, `handoff_assigned_to` treats an untargeted handoff as assigned to every caller,
so broadcast handoffs are never-cut for everyone, and 42 of 51 open handoffs are over 30 days old.

**Real at HEAD.** Yes, unchanged, cited in §3.

**Why it was not fixed.** The register records the coupling that makes it non-trivial:
`system_health` is never-cut *because a presentation bucket doubles as the enter-path dedup index* —
cutting a health row would let the enter-path guard re-append it, so a display decision became a
ledger-growth decision. Any bound has to move the guard onto its own keyed index first. That is a
real design dependency, and it is a legitimate reason not to have patched the dedup key in place.

**Cost.** Give the enter-path duplicate guard its own structure, then key `system_health` on prefix
class plus a bounded discriminator with an overflow row naming the suppressed count. Bounding or
expiring broadcast handoffs is separable and smaller. **Effort: M** for the pair.

**Recommendation.** Defer deliberately, with this rationale recorded. Take one cheap piece now:
stop interpolating an absolute path into the `external-intake:` subject (`lib.rs:2449` and `:2549`) — carry the
path in `evidence` where it does not drive the dedup key. That removes the one genuinely unbounded
contributor for **Effort: S** and does not touch the guard coupling. Note the second benefit: absolute
paths in subjects are also how home directories reach the ledger, so this narrows §5.2's forward
surface as well.

### 5.5 D12 / RC-060 — `--include-archived` plus an explicit budget silently truncates

**What it is.** `--include-archived` is documented as the complete escape hatch. Combined with an
explicit `--budget-bytes`, the ceiling still applies and omissions still occur, with no statement of
why.

**Real at HEAD.** Yes — `store.rs:3821-3822` resolves the explicit budget before the archived arm.
The register called this "a documentation-or-behaviour choice, not a security defect", which is
right.

**What the register missed.** The documentation is not merely silent; it asserts the opposite.
`RALLY.md:317-318`: *"`--include-archived` is the drill-in, so the budget does not apply to it. An
escape hatch that is itself truncated is not an escape hatch."* The second sentence is the register's
own argument for why this matters, printed as a guarantee the code does not provide. The composition
block's `drill_in` recommends this exact flag when `stale_facts` was omitted (`store.rs:3839-3843`),
so a caller who follows the tool's own advice with a budget set gets a truncated escape hatch and no
explanation.

**Cost.** Either one line of docs, or one line in the composition block naming the budget as the
cause when both flags were supplied. **Effort: S** for either.

**Recommendation.** Do both, and prefer the code half. The comment at `store.rs:3819-3820` —
*"An explicit `--budget-bytes` still wins, because that caller asked for a bound with their eyes
open"* — is a defensible behaviour, and the fix is to make the response say so. An agent can close
this without an operator decision.

### 5.6 ARP-R-10 — no such finding exists in this repository

**What it is.** Nothing. The ARP-R set is referenced as R-01..R-11, and R-10 appears in no commit
message, no register entry, no source comment, no test, no CHANGELOG line, no Rally fact in
`.rally/log/` or `.rally/archive/`, no `.rally.bak-*` tree, no stash, and no reflog entry across all
636 commits. The identical search returns hits for R-09 and R-11, so the search apparatus is sound.

**Assessment.** Three possibilities, and the evidence does not separate them: the reviewer numbered
R-01..R-11 and R-10 was withdrawn or merged into a neighbour before the report was recorded; R-10 was
recorded only in a session transcript that never reached an artifact; or the numbering skipped.
ARP-R-03 and ARP-R-04 are the precedent for the second — both are real findings with fixes in-tree
and neither has a register entry, so the recording step demonstrably drops findings in this cycle.

**Cost of closing it.** Zero code. Ask the reviewing session, or accept the gap.

**Recommendation.** Record it as unrecoverable and move on. The durable fix is not archaeology: it is
that a numbered finding set gets a register entry **when it is received**, before any of it is fixed.
Three of eleven ARP-R findings have no entry. That is the same shape as RC-D — the audit queue never
drained and nothing noticed. Worth one line in the register's standing rule.

### 5.7 Controls that no gate runs

**What it is.** Two adversarial suites are correct, thorough, and unwired.

- `dynamic-workflows/tests/injection.test.mjs` — 52 assertions, the sole regression protection for
  ARP-002. I ran it: 52/52. No gate invokes `node --test`.
- `tools/agent-rally-watcher/tests/` — quarantine, argv separation, sink containment, including a
  live-`osascript` injection test with a marker-file assertion. The sole regression protection for
  ARP-007. No gate invokes pytest or `uv`.

**Real at HEAD.** Verified by absence: zero hits for `node`, `npm`, `.mjs`, `pytest`, `uv`, or
`tools/` across `scripts/check-release-parity.sh`, `scripts/run-quality-gate.sh`,
`.github/workflows/ci.yml`, and `.githooks/pre-push`. The parity script does run two Python modules,
but only `tests/scripts/test_generate_host_surfaces.py` and `test_sync_host_integrations.py`.

**Why it matters here specifically.** This repo has already paid for this exact gap. RC-023: the
parity gate ran a hardcoded list of three hook suites while seven existed, so the two adversarial
controls closing RC-013 and RC-016 ran in no gate at all. The fix globbed `tests/hooks/test_*.sh`
and added an empty-glob refusal. Neither the Node suite nor the Python suite was brought into that
fix, and the register's own standard sits at `check-release-parity.sh:158` —
*"A control no gate executes is a hypothesis."*

**Cost.** For the Node suite: one step in `ci.yml` plus an empty-glob guard, mirroring `:97-111`.
**Effort: S.** For the watcher: the same, plus `uv sync --locked` to make the committed lock actually
bind. **Effort: S.**

**Recommendation.** Wire the Node suite now — ARP-002 was a Critical and its only regression
protection is currently manual. For the watcher, resolve RC-027 first: gating tests for a consumer
that reads a channel nothing writes buys nothing. Decide repoint-or-retire, then gate whatever
survives. **RC-019's `controlled` grade should be downgraded to `fixed` in the register until the
gate exists** — by the register's own definition it has not earned the mark.

### 5.8 RC-021 — `ClaudeAdapter::send` panics the daemon task. UNEXAMINED.

**What it is.** `crates/cockpitd/src/adapter/claude.rs:172-178` calls `Handle::block_on` from inside
a tokio worker thread. Any `send_prompt` or `steer` against a live Claude session panics the task and
kills the connection.

**Real at HEAD.** Yes, the code is unchanged. No test exercises the path — every Cockpit ownership
test routes through the codex-gated mock, and `e2e.rs:1773-1775` says so in a comment.

**Assessment.** This is not a security finding and it does not belong to any of the three review
sets, which is why nobody has assessed it. It is a **reliability finding of the highest practical
severity in Cockpit**: the Claude adapter's primary write path cannot work. It was found while
building the ARP-005 tests and has sat since.

**Cost.** The register names the shape: `Arc<Mutex<ChildStdin>>` plus a spawned write, rather than
blocking inside the runtime. **Effort: S** for the fix, **S–M** for a test that drives a real Claude
adapter.

**Recommendation.** Fix it, and gate it with a test that sends to a live adapter. Until then,
Cockpit's Claude support should be documented as non-functional for send and steer rather than listed
alongside Codex. An agent can do both.

### 5.9 RC-022 — the iOS client sends no `client_id`

**What it is.** `ios/Cockpit` sends `hello` with `protocolVersion` and `token` only — `HelloParams`
has exactly two fields (`CockpitClient.swift:346-353`), and `client_id`/`clientId` appear nowhere in
`ios/Cockpit/Sources` or `Tests`. Because ARP-005 bound sessions to the connection's principal, an
iOS client that reconnects on a network change arrives as a new principal and loses write access to
its own session.

**Real at HEAD.** Yes.

**Deferral rationale exists and is quotable**, `AUDIT-2026-08-02-issue-52-triage.md:178`:
*"Swift change plus a device test. The CLI half is fixed; iOS reconnects would otherwise orphan their
own sessions."* That is a valid reason to sequence it out of a security release.

**Recommendation.** Keep deferred, but note the interaction the triage row does not: the same
`client_id` that restores iOS control is self-asserted, so shipping it hands iOS the same
impersonation surface the CLI already has. That is acceptable — it is accident isolation, not
authentication — but it should be stated in the iOS change, not discovered later. **Effort: S** plus a
device test.

### 5.10 Documentation that still overclaims or has gone stale

Nine items, all small, all closable by an agent without an operator decision. Grouped because each
is one edit and because RC-C — *nothing reads a claim and asks the code whether it is true* — is the
pattern they share.

| # | File:line | Claim | Reality |
|---|---|---|---|
| 1 | `lib.rs:13883-13885` | "a hand-built fact… clears the same bar this command does" | False for a direct segment append. §5.1. |
| 2 | `TRUST-MODEL.md:259-260` | bundles "have **not** been audited for credentials… none has been ruled out" | A mutation-validated per-blob scan cleared all 18. §5.2. |
| 3 | `RALLY.md:317-318` | "the budget does not apply to it" | It does, whenever `--budget-bytes` is explicit. §5.5. |
| 4 | `RALLY.md:305-306` | "the room ships over budget and says so" | `over_budget` can be `false` while over. §5.3. |
| 5 | `RALLY.md:230-233` | the seat gates **two** room-wide capabilities | `cli.rs:1059-1063` says three (claim, freeze, its own transfer). |
| 6 | `CLAUDE.md:14`, `AGENTS.md:14` | `rally room --json` with no untrusted-data caveat | The pre-ARP-R-07 block, never regenerated, in the two files every agent reads first. |
| 7 | `docs/AUTO-COORDINATION-HOOKS.md:31-32` | "fail-opens (never blocks an edit)", unqualified | Three opt-in switches block; the correction is 125 lines later and names one. |
| 8 | `ios/Cockpit/README.md:11`, `docs/cockpit/CONVERGENCE.md:21` and `:61` | "approve/deny tool calls"; "Approval gating… security §9 value" | The gate is advisory. `crates/cockpitd/README.md` and `docs/cockpit/README.md` were corrected; this third README and CONVERGENCE were not. |
| 9 | `AUDIT-2026-08-02-issue-52-triage.md:181` | "A `rally show <event-id>` command… None exists" | `rally locate <event-id> --json` exists and returns a single fact. §5.11. |

Item 6 deserves emphasis. ARP-R-07's own lesson was *"the labelling had landed everywhere except the
file the product writes into the user's repo."* The same sentence now applies to this repo's own
entry-point files.

### 5.11 `rally locate --json` is a third unsanitized sink, and SKILL.md does not name it

**What it is.** `rally locate <event-id> --json` returns one fact with `subject` and `summary`
verbatim — I confirmed it against the hand-appended hostile fact from §5.1, which came back intact.
It is the single-fact reader ARP-004's ideal remedy asked for, so the triage's "none exists" row
(§5.10 item 9) is stale in the useful direction.

**But it does not deliver the remedy.** ARP-004's ideal was to inject an opaque ID and make the agent
open the fact separately. That only helps if the separate read is safe. `SKILL.md:282-286` warns
about `rally room --json` and `rally check before-write --json` by name and does not mention
`locate` — and `locate` is precisely what the composition block's own `drill_in` string recommends
(`store.rs:3834-3843`).

**Cost.** Adding `rally locate --json` to SKILL.md's `--json` warning: **Effort: XS.** Sanitizing the
JSON sink is the same schema decision RC-040 already deferred with a stated reason.

**Recommendation.** Add the name to SKILL.md now. Fold `locate` into the same decision as `room` and
`check` when the envelope question is taken up — it must not be a fourth sink discovered after the
other three are fixed.

### 5.12 The prompt still shows less than the enforcer — one layer below the filter that was just fixed

**What it is.** D11's invariant is directional: *the prompt may show more than the enforcer blocks; it
must never show less.* `08fb2ab` fixed the two filters that violated it. Two render-time caps below
those filters still violate it, and the parity test cannot see either.

1. **Claims are cut to eight with no remainder notice.**
   `hooks/rally-coordination-hook.sh:930` — `msg += \`Open claims: ${claims.slice(0, 8).join("; ")}. \``.
   The two adjacent lines both announce what they dropped: peers (`:928`) and agent status (`:929`)
   each append `(+N more)`. Claims do not. In a room with nine or more open claims, claims 9..N are
   absent from the prompt, `check before-write` still enforces every one of them, and nothing says a
   claim was omitted.
2. **Scopes are cut at 200 bytes per claim.** `SCOPE_BUDGET = 200` (`:868`), `break` at `:874`.
   This one is better behaved — `renderScopes` names the remainder — but it is the same class.

**Real at HEAD.** Yes, both cited above. The parity fixture uses three claims with one scope each, so
neither cap is exercised; the test passes and the invariant is still breakable.

**Why it survived.** The same reason the register gives for the filters: prompt density is a real
constraint and truncating a 120-character excerpt is the right call in isolation. What is not right
is truncating **silently** when the sibling lines two rows up do not.

**Cost.** Append `${claims.length > 8 ? \` (+${claims.length - 8} more)\` : ""}` at `:930`, matching
`:928` and `:929` exactly. Extend `hook_projection_parity.rs` with a nine-claim fixture and a handoff
case. **Effort: S** for both.

**Recommendation.** Do both in the commit that closes D11's handoff half — they are the same
invariant and the same test file. An agent can close this. Until then, D11 stays MITIGATED rather
than controlled: the parity test proves the two filters agree on three claims, not that the prompt
never shows less than the enforcer.

---

## 6. What is genuinely open

### The operator decides these. An agent cannot.

1. **The authority model (RC-063 / D15).** Make identity real, or drop the authority claim. The
   status quo is neither, and it now demonstrably fails against a text editor (§5.1). v0.2.0 shipped
   with this open, six hours after a room fact said *"Do not tag v0.2.0 — operator decision pending
   on authority model and bundle history."* That is the operator's prerogative; recording it keeps
   the decision deliberate rather than drifted. **This is the largest open item in this audit.**
2. **The Cockpit tool-broker redesign (ARP-003).** Architecture change with a written acceptance
   test. The fail-safe is correct and the deferral is sound; the redesign needs a slot, not another
   review.
3. **Per-client credentials for Cockpit (ARP-005).** One shared bearer token makes isolation
   impossible by construction. Not closable by another check on the same secret.
4. **RC-041 3B — whether `rally inject` should require an explicit `--anonymous`.** The register is
   right that the cost falls on the documented human flow, which makes it an operator call rather
   than an implementer's.
5. **The watcher: repoint or retire (RC-027).** Both are defensible. Shipping it as if it works is
   not.
6. **RC-046 — what an unresolvable env pin should mean.** Refuse, or require an ack. Two tests
   currently assert rc=0 on that path, so the decision and the test update land together.

### An agent can close these without a decision.

| Item | Effort | § |
|---|---|---|
| Correct the false "hand-built fact" claim at `lib.rs:13883-13885` | XS | 5.1 |
| Correct `TRUST-MODEL.md` on the bundle credential audit, and record the result in the repo | XS | 5.2 |
| The eight remaining doc corrections, including `CLAUDE.md` / `AGENTS.md` | S | 5.10 |
| Add `rally locate --json` to SKILL.md's untrusted-`--json` warning | XS | 5.11 |
| Wire `dynamic-workflows/tests/injection.test.mjs` into CI | S | 5.7 |
| D4's reporting half: recompute `over_budget` last, measure `emitted_bytes` on the final payload, always emit the composition block | S | 5.3 |
| D12: name the budget as the cause in the composition block when both flags were supplied | S | 5.5 |
| D5's cheap piece: move the absolute path out of the `external-intake:` subject into evidence | S | 5.4 |
| RC-021: restructure `ClaudeAdapter::send` off `Handle::block_on` | S | 5.8 |
| D11's handoff half + the silent 8-claim prompt cap, with parity-test coverage for both | S | 5.12 |
| `scripts/check-content-hygiene.sh` + `tests/hooks/test_content_hygiene_gate.sh`, both chokepoints, `--not --remotes=origin` scoping | S–M | 5.2 |
| Register corrections: RC-053, RC-059 (split, not flipped), RC-061, RC-040 item 1, RC-019's grade, RC-036's framing, RC-027's mechanism sentence, stale line cites; add entries for ARP-R-03, ARP-R-04, ARP-R-10 | S | 4 |

### Controls the register specifies and the tree does not contain

Each of these is a named adversarial control in a register entry with no implementation. They are
listed together because the pattern matters more than any one of them: an entry that specifies its
control and never gets it reads, six months later, exactly like an entry that was closed.

| Finding | Control the register specifies | Status |
|---|---|---|
| D5 / RC-055 | append 500 `external-intake:` facts with distinct paths, assert `system_health` stays bounded | absent — `grep external-intake` across `crates/rally-cli/tests/` returns nothing |
| D4 / RC-054 | a fixture whose reserve fits but whose guaranteed top-1 items do not, asserting `over_budget: true`; a second asserting `emitted_bytes` equals what the caller receives | absent — `room_budget_scaling.rs` has three tests, none touching `over_budget` or `emitted_bytes` |
| D10 / RC-058 | an instrumented counter asserting full segment folds per append does not grow | absent — only wall-clock proxies exist |
| D8 / RC-057 | N concurrent `rally enter`, assert `passes <= 2`, run N-consecutive | absent, and deliberately so |
| D11 / RC-059 | every claim the write path refuses is present in the rendered prompt | partial — three claims, zero handoffs, neither render cap |
| D12 / RC-060 | `room --include-archived --budget-bytes <small>`, assert completeness or a named cause | absent |
| D7 / RC-056 | write-failure honesty at all three reaper write sites | two of three; handoff site untested |
| ARP-R-06 | clone into a scratch dir, assert the room is empty and `git log -p` surfaces no `/Users/` path | absent (first half is closable; §5.2 supersedes the second) |

### What this audit did not establish

No full `cargo test --workspace` run, so the buckets citing tests I did not execute rest on the tests
existing and being gated, not on observed green. No `cargo audit` or `cargo deny` — still true of this
repo and still true of the original audit, which said so. No iOS build, so the Cockpit Swift findings
are code-read only. No live `git push` through the pre-push gate. RC-044 and RC-062 remain
unreproduced by design: the register is right that a wall-clock repro without pool-lifetime and WAL
tracing distinguishes nothing.

### Two structural observations

**The register is missing a write at both ends of a finding's life.** Three of eleven ARP-R findings
never got an entry, and one of those cannot be recovered at all — the rule is satisfied for findings
that get fixed and skipped for findings that arrive in a batch. At the other end, the file has not
been touched since `240892d`, before the v0.2.0 tag, so five merged commits moved three findings and
the register still describes the pre-merge code. **Entry at receipt, state update at merge.** Both
are one line in the merge checklist, and both are the same lesson as RC-D: a queue that never drains
reports as present and functions as absent.

**Eight register entries specify a control that does not exist.** That is not the same failure as a
wrong grade, and it is more durable: the entry reads as considered, the control reads as planned, and
nothing distinguishes "planned" from "done" six months later. The register already separates `fixed`
from `controlled` for the code. It has no vocabulary for a control that was specified and never
written. A `control: specified | written | validated` field would close that, and it would have made
every row in the table above visible without an audit.
