<!-- SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Proposal — Rally Point session liveness (north-star-aligned)

**Date:** 2026-06-06
**Audience:** any coordinating agent (Claude Code + Codex) and the human owner. Host-neutral; evidence-cited so either host can act directly.
**Scope:** **Rally Point source only** — the `rally stop` correctness + derived-liveness fix. The minimal-PATH host-hook hygiene that surfaced alongside it is a **separate, non-Rally workstream** (§6 pointer → `~/.claude/HOOK-AUDIT-2026-06-06.md`); per Codex+Claude agreement the two are **never bundled into one PR**.
**Status:** findings converged (two independent investigations agree). Machine-level repoint already shipped (§3). Two repo-source changes proposed (Tier 1 + Tier 2) — both touch contended files; see §7 Coordination.

---

## 0. Why this doc exists

Two symptoms surfaced in the fleet: (a) Codex sessions spamming `hook exited with code 127`, and (b) `rally sessions` listing dead "zombie" sessions. Independent Claude and Codex investigations reached the **same split and conclusions**, not the same root cause: the `127` spam was stale host hook configuration, while the zombie-session bug is current Rally source behaviour. This doc owns the Rally Point workstream (session liveness + command behaviour); host/global hook hygiene is split out to its own artifact (§6). This note records the converged evidence, the machine fix already applied, and — for the `rally stop` bug — a fix written to serve the *long-term* Rally objective, not just silence the symptom.

The governing rule for the proposed fixes: **anchor every change to a NORTH_STAR invariant.** A change that only mutes a symptom without restoring an invariant is rejected.

---

## 1. North-star anchors (what these bugs actually violate)

From `NORTH_STAR.md`:

- **Charter — facilitator, never executor.** Rally *records and advises*; it never gates/executes.
- **Invariant 3 — one owner per path; work never blocked, mistakes stay fixable.**
- **Invariant 4 — trustworthy results: room-stamped, fail-loud, read-back-verified, _liveness-aware_.**
- **Invariant 5 — host-neutral: one substrate for every coding host.**
- **Scale — correct at _thousands of agents and many terminals_.**

Mapping:

| Symptom | Invariant violated | Why it matters at scale |
|---|---|---|
| `127` hook spam (out-of-tree wrapper) | #5 host-neutral; Charter (wrapper enforced deny/block) | A loose per-machine wrapper is the opposite of "one substrate"; it drifts the moment the CLI changes. |
| Zombie sessions (`rally stop` on dead pane) | #4 **liveness-aware**; #3 ownership truth | At thousands of agents, terminals crash/are killed constantly. Liveness that depends on a clean shutdown call is liveness you don't have. Stale sessions hold stale claims → ownership lies → the human referee you removed comes back. |

---

## 2. Converged findings (evidence)

**F1 — The `127` WAS the stale global Codex wrapper (now fixed), not PR46 and not Rally core.** *(Historical — the live `~/.codex/rally-hook.sh` is now a thin shim; see §3. The description below is the pre-2026-06-06 state.)*
Before the repoint, `~/.codex/hooks.json` invoked a `~/.codex/rally-hook.sh` that (pre-Rust-rewrite, installed out-of-tree) called **removed** subcommands (`rally start` / `rally hook`) and ran **bare `node` under `set -euo pipefail`** before any guard. Hook subprocesses inherit a **minimal PATH** (`/usr/bin:/bin`) — `node` (version-manager dir) and `rally` (`~/.local/bin`) are absent → the command substitution returned 127 → `set -e` aborted the whole script *before it ever reached the removed subcommand*.
*Stale-wrapper evidence preserved at `~/.codex/rally-hook.sh.bak`.* Reproduced (both investigations): `env -i PATH=/usr/bin:/bin bash ~/.codex/rally-hook.sh.bak before-write` → **exit 127**; the current shim / in-repo hook under the same env → **exit 0**.

**F2 — The zombie-session bug is current code, in the `rally stop` ordering.** In `crates/rally-cli/src/lib.rs`, anchor on `SessionAction::Stop` (current main around `lib.rs:3716-3730`):
```rust
SessionAction::Stop => {
    let commands = backend_runner.stop_commands(&live_target);
    if !dry_run {
        backend_runner.stop(&live_target)?;          // `?` short-circuits if the pane is already gone
        // worktree cleanup is best-effort (`let _ = run_worktree::cleanup(...)`)
        remove_session_record(&session.session_id)?; // appends the "stopped" fact
    }
    (commands, None)
}
```
If the tmux pane is already gone, `backend_runner.stop` returns `Err`, the `?` returns early, and the **"stopped" fact is never appended** (`remove_session_record` → `session_fact(…, "stopped")`) — so the session stays "active" in every projection (`rally sessions`, `find_session`). Exactly the seq-1873 incident. *(Line numbers drift across worktrees — anchor on the symbols `SessionAction::Stop` / `remove_session_record` / `find_session`. On the current main checkout at this edit, `remove_session_record` is around `lib.rs:3762` and `find_session` is around `lib.rs:4157`.)*

**F3 — PR46 would not fix either symptom**, and is only partially present on local `main`. Present: `--produces/--depends` evidence markers (`lib.rs:959`), `check-ci` command (`cli.rs:556,697`), read-receipt projection (`store.rs:1335`, R10). Absent: `next.stale_bases[]` / `next.coordination_required` (grep-empty in `next.rs`), and the PR46 `room.receipts[]` *snapshot field* (distinct from the R10 read-receipt projection that exists).

**F4 — CI is on the older command.** `.github/workflows/rally-gate.yml:17` runs `check before-complete --tool ci`, not the implemented `check-ci --strict`.

**F5 — The in-repo hook is already correct.** `hooks/rally-coordination-hook.sh` self-gates on `.rally/` (lines ≈26, 59-62), fail-opens on missing binary/timeout (≈92-104), and is advisory-only by default (deny/block only behind opt-in `RALLY_HOOK_STRICT=1`). The design is sound; the problem was purely the un-migrated out-of-tree wrapper.

**F6 — The minimal-PATH 127 is a HOST-WIDE class, not a Rally-only bug** (added 2026-06-06 after a second-pass repro). Hook subprocesses get a minimal PATH; `node` (`~/.nvm/versions/node/<v>/bin`), `rally` (`~/.local/bin`), and `terminal-notifier` (`/opt/homebrew/bin`) are all absent there (`jq` at `/usr/bin/jq` is fine). Multiple **active, non-Rally** host hooks call these bare and 127 under minimal PATH:
- Claude vault-capture (secret scanner) — bare `node` at `~/.claude/settings.json:117` (PreToolUse) + `:201` (UserPromptSubmit). *Reproduced exit 127.*
- Claude `Notification` — bare `terminal-notifier` at `~/.claude/settings.json:57`.
- `openai-codex` plugin hooks — bare `node` at `~/.claude/plugins/cache/openai-codex/codex/1.0.2/hooks/hooks.json:9,20,31` (these are **Claude plugin-cache hooks**, not Codex's own hook surface).

**Live-vs-fragile distinction (matters for priority):** what is *proven* for the non-Rally hooks (vault-capture, Notification, openai-codex plugin) is only that they **exit 127 under a forced minimal PATH** (`env -i`). The user-observed 127 *spam* was the Rally wrapper (F1, now fixed) — there is no captured transcript of these other hooks failing live, so live behavior depends on each host's actual hook env. Claude Code appears to give hooks a richer PATH (vault-capture fired every Write/Bash this session with no failure → works today in Claude, but fragile). The durable fix (Workstream B) is justified for portability/robustness regardless; "broken right now" is **not** asserted for any specific host without a live failure trace.

**Intentional blocking hooks are NOT in scope** (correctly identified, not bugs): `pre-tool-use-guardian.py`, `write-path-guard.py`, IBR loop Stop hook, codex stop-review-gate, NavGator stale-data SessionStart advisory. These block/advise by design.

---

## 3. Already done (machine-level, reversible) — do not duplicate

`~/.codex/rally-hook.sh` has been **repointed to a thin shim** that `exec`s the in-repo canonical hook (the same output `scripts/install_rally_hooks.sh --repoint-codex` produces). Prior wrapper saved to `~/.codex/rally-hook.sh.bak`. Verified: exit 0 across all four phases under minimal PATH and from a worktree cwd; no auto-claim noise (rally doesn't resolve from worktrees → silent fail-open). Revert: `scripts/install_rally_hooks.sh --uninstall --repoint-codex`.

This closes F1 **only for the stale Codex Rally wrapper**. It does **not** address the broader host-hook class in F6 (vault-capture, Notification, codex-plugin hooks) — that is a separate workstream (§6), deliberately not conflated with the Rally session-liveness fix (§4).

---

## 4. The `rally stop` fix — two tiers (do BOTH)

The temptation is Tier 1 alone (make `stop` tombstone on a dead pane). That silences seq-1873 but leaves the real gap: **a session that crashes and never calls `rally stop` at all is still a zombie.** At thousands of agents, that is the *common* case. So Tier 1 restores correctness of the explicit-stop path; **Tier 2 restores Invariant #4 itself** — liveness becomes a *derived* property, not a side effect of clean shutdown.

### Tier 1 — explicit-stop correctness (small, immediate)
Make the backend kill best-effort and **always** tombstone, mirroring the worktree-cleanup pattern already two lines below:
```rust
SessionAction::Stop => {
    let commands = backend_runner.stop_commands(&live_target);
    if !dry_run {
        let _ = backend_runner.stop(&live_target);   // dead pane ≈ already stopped; never abort the tombstone
        if let (Some(path), Some(branch)) = (session.worktree_path.as_deref(), session.branch.as_deref()) {
            let repo = repo_root().unwrap_or_else(|_| PathBuf::from("."));
            let _ = run_worktree::cleanup(&repo, path, branch, "git");
        }
        remove_session_record(&session.session_id)?; // ALWAYS append the "stopped" fact
    }
    (commands, None)
}
```
*Test:* `rally stop <id>` where the backend target is absent → assert a `session`/`stopped` fact is appended to the ledger and `rally sessions` excludes it. (Ledger append stays deliberate — consistent with Invariant #1.)

**Rationale / tradeoff:** Tier 1 is the smallest correctness fix and should land first if another agent is already refactoring sessions/backends. Its benefit is immediate: an explicit operator cleanup command becomes trustworthy even when the pane died first. The cost is that backend stop errors become best-effort; if the backend is unreachable for reasons other than "already gone", Rally still tombstones because the operator explicitly requested stop. That is acceptable for `stop` because the durable source of truth is the operator's deliberate teardown fact, not a successful host kill. If desired, the implementation can preserve the backend error in output/evidence without blocking the tombstone.

### Tier 2 — liveness as a derived projection (the durable objective)
Project backend liveness at read time so crashed/killed sessions self-heal without anyone calling `stop`. Design (refined with Codex 2026-06-06):

- **Separate projection type — do NOT overload `ManagedSession`.** Add a read-only `ProjectedSession` / `SessionView` that wraps the persisted record plus derived liveness. The persisted `ManagedSession` (ledger truth) is never mutated or widened to carry transient liveness.
- **Tri-state liveness, not boolean:** `live | stale | unknown`. `unknown` is required — a probe can fail to answer (backend unreachable, permission, race); collapsing that into `stale` would falsely retire live work. Boolean cannot express "couldn't determine."
- **Probe behind `BackendRunner`, not ad-hoc `tmux` in `lib.rs`.** Add e.g. `BackendRunner::liveness(&[targets]) -> Vec<Liveness>` so the tmux/other-backend detail stays in the backend abstraction (Invariant #5 host-neutral) and is mockable in tests.
- **Batch probes per backend** (one call for N targets, not N calls) so `rally sessions` stays cheap at thousands of agents.
- **Pure read — no ledger mutation on read** (Invariant #1; the projection is a disposable derived cache, Invariant #2). The ledger stays canonical.
- **Explicit stale results in command semantics:** `rally sessions` may surface `stale`/`unknown`. `find_session` / `inject` must **not** silently fall back to treating a stale managed session as ledger-only — they return an explicit stale-session result/error so a caller never injects into a dead pane.
- **Reaping stays a deliberate write:** `rally stop` (Tier 1) or an explicit `rally sessions --reap`; never an implicit append hidden inside a read. `--reap` tombstones only `stale` (not `unknown`) sessions and is idempotent.
- **Update the contract:** if the `sessions` output shape gains liveness, bump the JSON schema for `agent-rally.command.sessions.*` and the envelope tests.

*Why this is the north-star answer, not gold-plating:* Invariant #4 says results must be *liveness-aware*. A registry whose truth depends on every agent shutting down cleanly is not liveness-aware at scale — it's hope. Probing for liveness is "facilitator, never executor" (it observes and reports; it does not kill or schedule). Same shape Rally already uses for presence/claims/DAG: **derive state from facts + reality, expose it; the host acts.**

*Tests for Tier 2:* (1) externally-killed pane, no `stop` called → projects `stale`; (2) probe-unanswerable → projects `unknown` (not `stale`); (3) `find_session`/`inject` on a stale target → explicit stale result/error, no ledger-only fallback; (4) `rally sessions --reap` tombstones only `stale`, is idempotent; (5) batched probe issues one backend call for N targets.

**Rationale / tradeoff:** Tier 2 is the durable fix because it makes liveness a derived read model rather than an assumed side effect of clean shutdown. The benefits are better routing, fewer false injectable sessions, and clearer operator decisions when many agents are running. The costs are higher implementation surface: response schema changes, backend probing, command semantics, and tests across `sessions`, `find_session`, and `inject`. Keep this separate from PR46/CI/receipt work so a blocked schema discussion does not delay Tier 1.

**Interaction with other in-flight work:** This work is complementary to direct-injection ACK/transport fixes and handoff-state-machine fixes. Those changes can prove whether an injected handoff was received; this proposal proves whether a managed session should be advertised as live/actionable in the first place. If another branch changes `command_inject` or ACK semantics, preserve this guardrail: a target that matches a stale managed session must fail as stale and must not silently fall through to ledger-only delivery. If another branch changes session schemas, merge by adding liveness only to the projected session view, not the persisted `ManagedSession` fact payload.

---

## 5. Scope guard

Tier 2 is **liveness projection only** — probe + surface + deliberate reap. It must NOT grow into health-checking, restarting, or rescheduling agents — that would cross the Charter into executor territory. If a proposed line makes Rally *act on* a dead session beyond recording a fact, it's out of scope.

---

## 6. Host hook hygiene — SEPARATE workstream, NOT in this PR (pointer only)

The minimal-PATH bare-binary 127 class (F6) is **broader than Rally and must not be bundled into the Rally PR** — it keeps this PR reviewable and keeps host/global config out of repo source. It is tracked as its own host-local task with its own artifact:

**→ `~/.claude/HOOK-AUDIT-2026-06-06.md`** — full inventory (Claude `settings.json`, Codex `hooks.json`, enabled plugin hooks), per-hook purpose, what to harden + why, removal candidates. Documentation only; nothing edited/removed there yet.

Why it is not a Rally concern: the in-repo `rally-coordination-hook.sh` is already correct (F5); the only Rally-side hook action (repointing the stale Codex wrapper) is **already done** (§3). Everything else in F6 (vault-capture, Notification, openai-codex plugin hooks) is host/plugin config the Rally repo does not own.

The durable hook rule (logic in-repo + thin shims; resolve binaries absolutely + real fail-open; security-hooks-must-run vs advisory-hooks-may-fail-open; advisory-not-enforcing; fewer global hooks) is captured in the host audit doc and in build-loop's `plugin-hygiene-lessons.md` §17 + `hooks-reference.md`. A Rally doc references it; the Rally PR does not implement it.

---

## 7. Coordination (who does what)

These are **contended `lib.rs` / repo-source edits in a lead-coordinated room** (`whoami`: lead `claude_code:l4`, mission "single owner does reconciliation / no fleet deploy without owner go," historical/active work has touched `lib.rs`, session routing, and injection semantics). Therefore:

- **Do not fork.** Route Tier 1 + Tier 2 through the lead; claim `lib.rs`, `backends.rs`, schema files, or tests before editing; one owner per path.
- Suggested sequencing for this Rally-liveness PR: F1 (done) → Tier 1 (tiny, unblocks seq-1873) → Tier 2 (liveness projection) → focused validation. No push to origin without owner go (mission).
- Keep adjacent work separate unless the lead explicitly pulls it in: CI gate update to `check-ci --strict` can be a small adjacent PR; PR46 gaps (`next.stale_bases[]`, `room.receipts[]`) stay separate; host hook hygiene stays in `~/.claude/HOOK-AUDIT-2026-06-06.md`.
- If another agent is already editing injection ACK/transport, coordinate at the symbol level. Liveness should gate stale managed-session resolution before delivery; ACK work should prove receipt after delivery. They are related but not interchangeable.

---

## 8. Acceptance criteria

- [ ] Fresh Codex session (repo + unrelated dir) → no `hook ... 127`; SessionStart surfaces room context only where `.rally/` exists. *(F1 — done; confirm on next live session.)*
- [ ] `rally stop` on a session whose pane is gone → appends a `stopped` fact; `rally sessions` excludes it. *(Tier 1)*
- [ ] Externally-killed pane (no `stop`) → projects `stale`; probe-unanswerable → `unknown` (not `stale`); `find_session`/`inject` return an explicit stale result (no ledger-only fallback); `--reap` is idempotent and reaps only `stale`. *(Tier 2)*
- [ ] Fail-open test for the **in-repo** `rally-coordination-hook.sh` shells out under `env -i PATH=/usr/bin:/bin` → exit 0, no deny/block — runs from **repo CI** via a Rust or integration test. *(Workstream A — repo)*
- [ ] `cargo test -p rally-cli` green, including the new liveness + hook tests.
- [ ] Optional adjacent cleanup: CI runs `check-ci --strict` instead of `check before-complete`. *(F4; separate if it would distract from liveness.)*
- [ ] **Host hook hygiene is OUT OF SCOPE for this PR** — tracked separately in `~/.claude/HOOK-AUDIT-2026-06-06.md`, verified host-locally (`env -i PATH=/usr/bin:/bin`), never in repo CI. Listed here only so reviewers know it is intentionally excluded, not forgotten.

---

## Appendix — evidence index

All line numbers are the `main` checkout as of 2026-06-06 and **drift across worktrees** — anchor on the symbol names; re-grep before editing.

| Ref | Symbol / file (main-checkout line) | Claim |
|---|---|---|
| F1 | `~/.codex/rally-hook.sh.bak` (stale wrapper: `set -e` + bare `node`); `~/.codex/hooks.json` | stale wrapper → 127 under minimal PATH (now repointed) |
| F2 | `SessionAction::Stop` (`lib.rs`, current main around `3716-3730`); `remove_session_record` (current main around `3762-3770`) | `?` on `backend_runner.stop` short-circuits tombstone on dead pane |
| F2 | `find_session` (current main around `4157`) | projection has no liveness check |
| F3 | `lib.rs:959`; `cli.rs:556,697`; `store.rs:1335` (R10 receipts); `next.rs` (no `stale_base`) | PR46 partially present |
| F4 | `.github/workflows/rally-gate.yml:17` | CI on `check before-complete`, not `check-ci` |
| F5 | `hooks/rally-coordination-hook.sh` (self-gate ≈59-62, fail-open ≈92-104) | in-repo hook is advisory + fail-open |
| §1 | `NORTH_STAR.md:21-40` | charter + invariants (esp. #4 liveness-aware) |
