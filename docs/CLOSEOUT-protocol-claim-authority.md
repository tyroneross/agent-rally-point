<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> | SPDX-License-Identifier: Apache-2.0 -->

# Closeout — Rally Protocol: Claim Authority + Session Identity + Dogfood

Closeout for the build in [`docs/PLAN-protocol-claim-authority-dogfood.md`](PLAN-protocol-claim-authority-dogfood.md),
target design [`docs/PROTOCOL-NORTH-STAR.md`](PROTOCOL-NORTH-STAR.md). Two-host
dogfood: Claude (`claude_code:ts2-01`) + Codex (`codex:claim-authority-01`),
coordinated via Agent Rally Point, run id `rally-protocol-claim-authority`.

## Canonical branch and HEAD

- **`main`** consolidated to the integration tip via fast-forward (no merge commits).
  Integration HEAD `7296da8`; docs commit on top. Pre-collapse `main` (`d5d0542`)
  archived at `archive/bundles/pre-collapse-integration-d5d0542.bundle`.
- Per-agent branches collapsed: `feat/protocol-integration`, `chore/closeout-gate-green`,
  `claude/session-identity` removed after merge. **Not pushed** (origin push is operator-gated).

## Changed files by phase

| Phase | Commit | Owner | Files |
|---|---|---|---|
| Closeout gate green | `1f9c2fb` | Claude | tree-wide `cargo fmt --all`; `rally-protocol` let-chain; `cockpitd`/`cockpit-cli`/`rally-cli` clippy |
| 1 — session identity | `27cca16` | Claude | `crates/rally-cli/src/session_identity.rs` (new), `lib.rs` (whoami) |
| 2 — `from_session_id` on writes | `f16f55c` | Claude | `store.rs` (Fact `Default` + field, 78 sites), `lib.rs` (say stamp) |
| 3 — event envelope validation | `7296da8` | Claude | `event_envelope.rs` (new), `docs/schemas/rally-protocol-events.md`, `lib.rs` (say advisory validate) |
| Claim authority + scopes + leases | `cbe9bf7` | Codex | `claim_authority.rs`, `resource_scope.rs` (new), `store.rs`, `lib.rs` |
| Inject delivery truthfulness | `63837e9` | Codex | `lib.rs` (inject) |
| Dogfood smoke | (in `7296da8`) | Claude | `scripts/protocol-dogfood-smoke.sh` |
| Spec docs | docs commit | Claude | `PROTOCOL-NORTH-STAR.md`, `PLAN-…dogfood.md`, workstream descriptor, README links |

## Passing commands (final, on `main`)

```
cargo fmt --all -- --check            # 0 diffs
cargo clippy --all-targets -- -D warnings   # clean (all 4 crates)
cargo test --all                      # 545 passed; 0 failed
git diff --check                      # clean
RALLY=target/debug/rally bash scripts/protocol-dogfood-smoke.sh   # 5/5 PASS
```

## Dogfood evidence (Rally event ids)

| Proof | Event id |
|---|---|
| Targeted handoff ACK with exact `from_session_id` + `ref_event_id` | `fact_ec5a` (ACK of handoff `fact_8e59`) |
| `delivered != acked` + targeting (which Claude received it) | `scripts/protocol-dogfood-smoke.sh` 5/5 → `fact_142ad` |
| Cross-tool exclusive claim conflict rejected (live) | `fact_ee7d` (Codex before-write `allow:false` on Claude-held file) |
| Claim-enforcement fix verified + acknowledged | `fact_88ed` (X2–X6) → `fact_ec5a` (X3 ACK) |
| `from_session_id` stamped on real `say` | `fact_6bdd` (inc2 evidence) |
| Main consolidated | `fact_cef5` |

Test→criterion traceability: [`docs/TEST-COVERAGE.md`](TEST-COVERAGE.md).

## Skipped / deferred (not silent)

- **Increment 4** — distinct `handoff.accepted`/`handoff.rejected` commands and
  write-path **enforcement** of `event_envelope::authorize` (auth) + `Deduper`
  (idempotency). The invariants are unit-tested; the `say` path validates
  advisorily today (`envelope-incomplete` warnings) but does not yet block.
- **Criterion 8** — stale-session auto-releasable-claim surfacing (needs the
  session registry projection). Tracked to Lane 2 / registry work.
- **Staged `#![allow(dead_code)]`** in `session_identity.rs` / `event_envelope.rs`
  remains until increment 4 consumes the full vocabulary.

## Known follow-up triggers (from north-star "Deferred Capabilities")

| Capability | Trigger |
|---|---|
| Session key signing / challenge-response | multi-user, untrusted, or federated rooms |
| Hybrid logical clocks | cross-machine ordering beyond per-room sequence |
| Federated transport | real-time cross-machine coordination without shared files |
| Policy-enforcement locks | hosts demand hard prevention over warning-only |
| Strict durable transport audit | delivery/read receipts become compliance evidence |

## Remaining to a clean push (ownership)

1. **Codex `claim-authority-01`** — Lane 2 (claim-lease extensions + structured-scope
   coverage) lands and rebases onto `main` `7296da8`.
2. **Codex monitor** — prune merged `codex/injection-reliability` + `codex/main-codex-integration`.
3. **Operator** — `git push origin main` (the one gated action); decide push timing
   (now vs after Lane 2).
