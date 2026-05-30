<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Assessment — agent-rally-point session (R1–R9 + Rally Flow), 2026-05-30

Produced by a **rally-flow multi-agent assessment** (4 read-only assessors — thread-work, repo-health, rally-mechanism, coordination-effectiveness — → 1 synthesis). Read-only; this doc is the durable record. **Improvements** (forward actions) and **Lessons Learned** (retrospective insight) are kept strictly separate, per request.

## Executive summary

The session delivered a coherent, well-evidenced body of work coordinated entirely through rally facts + the ORCHESTRATION board with no user relay: the **Rally Flow** dynamic-workflows module (L1 protocol/lint/skills + L6 durable fan-out/resume, 37/37 tests), a full rally-cli security/correctness hardening pass (B4–B9 HIGHs), doc/module hygiene (B14/B15), and an **8-step persistence arc (R1–R8)** making the rally ledger per-repo, build-loop-independent, findable, retrospectable, rotatable, and replay-idempotent. The repo is healthy and shippable at HEAD (`cargo test --all` green, dynamic-workflows 37/37, clippy + fmt clean, no dead code/stubs).

**Dominant strength:** verification-as-a-system — the lead re-ran gates itself rather than trusting subagent envelopes, and a standing read-only Codex coordinator acted as an adversarial peer-reviewer that caught three silent-corruption substrate defects the lead had shipped.

**Dominant weakness:** a recurring **shallow-negative-claim** pattern — five times the lead asserted something didn't exist / didn't work / was a false positive from a cheap surface signal, and a deeper probe reversed it. All five were caught + corrected in-session, but the asymmetry is the real defect: **a wrong negative is self-sealing (it tells everyone to stop looking)** whereas a wrong positive gets caught downstream.

## Strengths

- **Verification discipline as a system** — re-ran `cargo test --all` / `npm test` / `git diff --stat` instead of trusting envelopes; caught B5/B7 released-without-landing via source-check; confirmed 0 `crates/**` files in lead-lane work.
- **Standing read-only Codex coordinator as adversarial referee** — no lane to defend, re-derived state from source/disk each heartbeat; caught the no-op `release` (missing `--ref`), the R8 replay dup-seq double-count, and the stale-binary write-drop. The closest thing to an automated referee in a facilitator model.
- **R5/R8 segmented-ledger reconcile correctly engineered** — distinct-seq-count comparison (not raw lines / not max-seq), monolith-exclusion, dropped brittle contiguity assertion; each red→green test-backed and verified on real committed data.
- **Per-engagement segmented JSONL ledger** — durable + canonical, SQLite a rebuildable derivative; survives DB deletion; `merge=union` makes concurrent worktree appends conflict-free.
- **MERGES.md context-log discipline** — every merge carries Context-why / Evidence / Lead-audit / Blast-radius+reversibility; honestly shows its own ⚠️-pending gaps.
- **11-field instruction contract eliminated "what next?" round-trips** — measurable: free-text handoffs caused standby; the contract removed it.
- **Cohesive code** — 7,759 lines across 13 single-responsibility modules, no production dead code, no TODO/FIXME/unimplemented in `src`.
- **The facilitates-not-coordinates charter held** — a fresh lead resumes from board + `rally room`; gaps are all in substrate *trustworthiness*, not the coordination philosophy.

## Improvements (forward actions — backlog candidates)

> These are NEW work items. Lessons (how we worked) are the separate section below.

| Priority | Item | Backlog |
|----------|------|---------|
| **HIGH** | **Deterministic read+write path + visible receipts** — bring read-state onto the same append-only ledger as writes (see "Read/write determinism" below). Today writes are deterministic (ledger) but the read cursor is a gitignored last-writer-wins side-file advanced *only* on `rally enter`, surfaced by no command. | **R10** |
| **HIGH** | **Post-mutation readback** — after any mutating rally command, re-read room/ledger and assert the new fact's seq is present before reporting success; every read/write returns the resolved room id, never null-on-ok. Kills the release-no-op + stale-binary-write-drop classes at one anchor. | R9-readback |
| **HIGH** | **Ship R9 stale-binary write-drop guard** — rally embeds a build-id/version; `rally enter` warns/fails when the invoked binary disagrees with the repo's expected line. (Two binaries with a ~10h mtime gap are still on PATH.) | **R9** |
| **HIGH** | **`inject --require-ack` → session-bound receipt** — today `--require-ack` proves task *resolution* (`wait_for_resolution`, lib.rs:1011-1039), not receipt. Have the injected agent emit a `seen`/`ack` fact on wake *before* it resolves → two-stage proof (seen → resolved). | B13 |
| MED | **Pull-path delivery trace + `rally room --readers`** — stamp a `picked_up` fact when a target's `rally next` first surfaces a handoff; add a board projection of each tool's last-seen seq vs max_seq (who's behind). | B12 |
| MED | **Land B10/B11** — `rally enter` rejects duplicate squad id + records tier + auto-asserts lead; canonical-path one-owner-per-file. Both guarantees the facilitator model rests on are still manually enforced. | B10/B11 |
| MED | **Update `docs/RALLY_ARCHITECTURE.md`** — still R1-era prose ("ledger.jsonl is the source of truth"); contradicts R5/R8 segmented design. | B-arch-doc |
| MED | **`rally whoami`** — report tool id / clone / worktree / expected binary in one call (reduce the two-clone-split identity confusion). | B-whoami |
| MED | **Harden parallel-launch id-reservation race** — `rally_run_reserves_numbered_ids_under_parallel_launch` flakes in isolation; retry-on-collision or `#[ignore]` + tracking note. | B11-race |
| LOW | **Filter migration monolith from `refresh_log_index`** — committed `index.json` double-counts 489 phantom events (canonical replay already excludes it; the advisory index doesn't). | B-index-monolith |
| LOW | **Commit-ledger cadence policy** — committed history (489) lags on-disk (521); commit on a cadence (merge=union is conflict-free) or document `.rally/log/` as a live working-tree artifact. | B-ledger-cadence |

### Read/write determinism — the recommended approach (R10)

**The issue (verified):** writes go through a *deterministic* path — append-only `.rally/log/<engagement>.jsonl`, seq-ordered, replayable, committed, with `facts.db` a derived cache. **Reads don't.** The cursor (`.rally/cursors.json`) is a mutable last-writer-wins JSON with one `updated_at`, **gitignored** (not durable/committed/replayable), **advanced only by `rally enter`** (lib.rs:183 — not by `next`/`room`/`say`, so it's "last-entered-at", not "last-read"), and **surfaced by no CLI command**. That asymmetry is why receipts "weren't visible" and is the same family as the stale-binary silent-write-drop (state living off the canonical path).

**Does a single deterministic read+write path make sense? Yes — with one caveat.** Bring read-state onto the same append-only ledger:

- A **consumption checkpoint** becomes a lightweight `cursor`/`seen` **fact** in the ledger — appended when a tool *advances* its consumption (via `rally next` / explicit ack), **debounced (one checkpoint per consumption batch, NOT one per poll)** — this is the caveat: never log every read, or read-amplification bloats the ledger.
- **`cursors.json` is demoted to a derived cache** (like `facts.db`), rebuilt by replaying the cursor facts. Single source of truth = the one ledger; replay reconstructs both the room *and* who-read-what.
- **Surface it:** `rally room --readers` (per-tool last-seen seq vs `max_seq` = who's behind) + `rally whoami`.
- **Payoff:** read-state becomes durable, committed, replayable, and visible — the same guarantees writes already have — and the read/write asymmetry that hid receipts (and enabled the stale-binary drop) is closed structurally.

**Alternatives considered:** (a) *reads-as-events, every read* → purest single-path but read-amplification kills it; (b) *infer from each tool's own posts* → inference, not a receipt; (c) *keep the side-file, just surface it* → fixes visibility but leaves read-state off the deterministic/replayable path. Recommendation: **(checkpoint facts + derived cache)** — the deterministic single path without amplification.

## Lessons Learned (retrospective — how we worked)

> Insight about the *process*, each with why + how-to-apply. Distinct from the forward actions above.

1. **Verify before asserting a negative.** Any claim that "X doesn't exist / didn't work / is a false positive / is done / not a bug" must cite the *exhaustive* check that ruled it out; a closing claim touching another squad's lane enters as PROPOSED (peer-confirm), not BINDING. — *Why:* the session's dominant defect — 5× a negative conclusion from a cheap signal (one self-report, one grep, an exit code, marker-absence, one of two binaries) was reversed by a deeper probe. A wrong negative is **self-sealing** (it stops investigation; with merge authority delegated it can suppress a real fix); a wrong positive gets caught downstream. — *How:* add a machine-stored `verified_by` field (literal command + output) on every closing/negative fact; run a disprove-by-source gate (grep ALL callers, read the actual field, check BOTH binaries, read back state); default cross-lane closing claims to peer-confirm.

2. **Trust the effect, not the call.** An exit code / `--help` success / "delivered" status proves the command *ran*, not that state *changed* — read back ground truth before reporting success. — *Why:* the release no-op and the stale-binary write-drop are silent-corruption bugs — they corrupt shared truth without erroring, so nothing downstream catches them. — *How:* automatic readback after mutations (assert `max_seq` advanced / new fact present); after an install/rebuild confirm *which* binary on PATH ran; promote source-over-artifact from habit to a cheap-boundary gate.

3. **Make ground truth self-declaring, not inferred.** Put the fact IN the identifier instead of extrapolating from a weak signal. — *Why:* model attribution was over-generalized from one self-report (both Codex squads tagged "gpt-5.4-mini"; one was actually 5.5). The session found the fix itself: `host-llm-role-number` squad ids encode the model so the lead never infers it. — *How:* generalize self-declaring ground truth — squad-id-encodes-model (done), post-mutation readback (ledger declares the write landed), session-bound ack (the session declares it woke).

4. **A context-free read-only verifier is the highest-leverage role in a facilitator model.** — *Why:* the read-only Codex coordinator caught exactly the three silent-corruption defects the acting lead was blind to, precisely because it had no lane to defend and re-derived state each heartbeat. — *How:* standing rule for any 2+ agent build — one persistent read-only squad, full room visibility, zero edit authority, re-derives ground truth each heartbeat, posts divergences as risks; pair with the peer-confirm gate.

5. **A consistent handoff shape is a protocol, not an agent-quality issue.** — *Why:* free-text handoffs caused Codex squads to request a contract and stand by; the 11-field contract removed the round-trips. "What next?" is a protocol gap, not a slow agent. — *How:* issue every cross-agent handoff in the fixed 11-field shape from the first handoff; keep tier instructions host-relative.

6. **A dependency's schema is not your feature.** — *Why:* `facts.db.subscriber_cursors` exists but is created by the `factstr-sqlite` dependency, holds zero rows, and is referenced nowhere in rally — reasoning it backed read-tracking would be wrong (rally's read-position state is the separate `cursors.json`). Same shallow-claim root in reverse: presence-of-marker ≠ behavior. — *How:* grep the project's own source+tests for a table/field before asserting it does (or doesn't) implement a behavior.

7. **Multiple working trees + a gitignored source-of-truth ledger is a handoffs/simplicity gap.** — *Why:* the two-clone split caused a route.mjs commit on the wrong branch (cherry-picked, e1589dd) and kept the resume-ledger gitignored so the coordination story didn't travel with commits until R1 made it tracked. — *How:* pin the primary checkout to main + branch work in dedicated worktrees; make the coordination ledger canonical+tracked from day one; `rally whoami` so identity is queried, not inferred.

8. **A reversible, authorized, clearly-instructed action must be executed on first instruction — never re-surfaced as an "open call."** — *Why:* the lead identified the two-clone consolidation as the right fix, then deferred it across **multiple turns** as "your open call / say the word," forcing the user to repeat the directive ("I've said multiple times just make agent-rally-point the only one"). It was **reversible** (re-clone from origin), **authorized** (a direct instruction), and **determinate** (one unambiguous outcome) — exactly the profile that should auto-execute; deferring it was *more* costly than acting. Same root as the manufactured-fork / permission-ask anti-patterns, with a sharper trigger: **re-presenting a directive the user has already given is itself the violation.** — *How:* when about to write "your open call" / "say the word" / "want me to…" about something **reversible the user already asked for**, treat that phrase as the stop signal and execute that turn instead. A directive stated ≥1 time and not yet done = act now; surface only genuinely irreversible/destructive choices or true outcome-changing ambiguity.

## Top recommendations (ranked)

1. **Headline systemic fix:** the verify-before-asserting-a-negative rule — machine-stored `verified_by` on every closing fact + cross-lane closing claims peer-confirm before binding. Retires the dominant defect class.
2. **R9 / R9-readback:** post-mutation readback in the rally wrapper + stale-binary guard — kills release-no-op and stale-binary-drop at one anchor; charter-safe.
3. **R10:** deterministic read+write path — read checkpoints as ledger facts, cursor as derived cache, `rally room --readers` surfaces receipts.
4. **Standing read-only verifier squad** doctrine for every 2+ agent build, paired with the peer-confirm gate.
5. **Substrate-trust backlog:** B10/B11 (id + path enforcement), B12 (reader projection + picked_up trace), B13 (session-bound receipt), the parallel-launch race.
6. **State-artifact sweep:** monolith-filter `refresh_log_index`; update `RALLY_ARCHITECTURE.md` to the R5/R8 design.
7. **Commit-ledger dogfood policy** so a fresh clone and the dogfood machine agree on event count.
