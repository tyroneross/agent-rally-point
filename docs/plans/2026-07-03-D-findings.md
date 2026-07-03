# PLAN-D findings: silent mutating-write drops

Date: 2026-07-03
Owner lane: codex implementation + durability test
Verifier lane: claude concurrency test + CI verification

## Executive finding

The bug class is broader than `rally say`: the watchdog used one fail-open posture for every timed-out invocation, so a mutating command could return `ok:true` before its primary durable append committed. The fix is a command-posture split plus a commit-signal contract: uncommitted mutation timeouts fail closed; committed mutation timeouts may return success only with `data.watchdog.committed=true` and `projection_complete=false`.

## Watchdog posture matrix

| Invocation class | Examples | Timeout posture | Commit signal |
|---|---|---:|---|
| Read-only projection | `room`, `next`, `lead show`, `sessions` without reap/apply | fail open | none |
| Before-write gate | `check before-write` with `--fail-closed` or `RALLY_BEFORE_WRITE_FAILCLOSED=1` | fail closed | before-write stop envelope |
| Primary ledger mutation | `say`, `enter`, `ack`, `status post`, `backlog add/update/done`, `lead assign/handoff/relinquish`, `mission --set/--may/--must-check`, `route-findings`, session adoption/reservation/reap paths | fail closed until canonical segment append commits; committed success after signal | `mark_watchdog_command_commit()` after segment append |
| Dry-run-capable mutation | `inject`, `run`, `stop`, `rotate` | dry-run remains read-only; non-dry-run classified as mutation | ledger-backed paths signal where they append; non-ledger file/process mutations can only fail closed |
| Apply-style maintenance | `hooks on/off/prompt`, `init`, `migrate-legacy`, `doctor --apply`, `worktree-gc --apply` | fail closed | residual: no committed-success signal for non-ledger file rewrites |

## Defect-class audit

| Area | Finding | Disposition |
|---|---|---|
| `append_fact` cost | PLAN-D v2 was right: `append_fact` was not O(1); it took `mutation.lock` and ran reconcile before append. | Fixed for `append_fact`: open uses lenient cache open and writes canonical segment before commit signal. |
| `open_at` cost | Room open reconciled SQLite before commands could even reach append. | Fixed for `open_at_with_engagement` and `open_existing_at`: open no longer performs full reconcile. Read projections reconcile in `facts()`. |
| Commit-vs-retry duplication | A timeout after segment append but before projection/readback must not report failure, because the caller would retry and double-append. | Fixed with a thread-local commit signal armed only around primary mutations. |
| Auxiliary appends | `command_say` has best-effort risk appends and checkpoints with `let _ = ...`; those are not the primary user fact. | Left as advisory residual. They should not flip committed status for the primary command. |
| `route-findings` | It appends routed handoffs/risks plus a summary artifact and was initially unguarded. | Fixed at command boundary with the mutation commit guard. |
| `check --enforce` classifier | A broad classifier would treat read-only `check` phases with `--enforce` as mutating. | Fixed to classify only `check liveness --enforce`. |
| SQLite-first/segment-second tear | `append_fact` still writes SQLite before the canonical segment. A crash between those writes can leave disposable SQLite ahead of JSONL. | Residual P2. Current fix marks commit only after segment append, so watchdog success cannot be based on SQLite-only state. |
| Session conditional appends | `append_session_fact_if_context` still reconciles before append. | Residual P2 for performance; now uses the same commit-signal marking after segment append. |
| Unbounded flock | `acquire_room_mutation_lock` itself is still unbounded. | Residual P2. Watchdog process exit bounds callers, but there is not yet a lock-acquisition timeout/error path. |
| R9 stale-binary guard | The write-drop fix depends on all active agents using a fresh binary. | Audit finding only in this lane. Keep binary freshness/install checks in the verification lane before relying on live hook behavior. |
| Test path | PLAN-D named `tests/watchdog_write_durability.rs`, but the repo root is a virtual Cargo workspace. | Corrected to executable path `crates/rally-cli/tests/watchdog_write_durability.rs`. |

## Verification target

The durability test pins the watchdog timeout below deterministic work:

- Pre-commit block: `RALLY_TEST_BLOCK_MS` forces a mutation timeout before any append. Expected result: exit 4, `ok:false`, `watchdog-timeout-uncommitted-mutation`, zero replayed handoffs.
- Post-commit block: `RALLY_TEST_BLOCK_AFTER_COMMIT_MS` sleeps after the canonical segment append. Expected result: exit 0, `ok:true`, `committed:true`, `projection_complete:false`, exactly one replayed handoff.

Claude's concurrency lane should keep the same invariant under SQLite/flock contention: zero dropped primary facts and zero duplicate facts when the watchdog budget is shorter than the contended work.
