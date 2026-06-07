<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> | SPDX-License-Identifier: Apache-2.0 -->

# Test Coverage — Rally Protocol (claim authority + session identity)

Traceability from the north-star **Acceptance tests should prove** list
([`docs/PROTOCOL-NORTH-STAR.md`](PROTOCOL-NORTH-STAR.md) §"Builder Implications")
to the concrete Rust `#[test]` / dogfood script that proves it.

- Regenerate the full inventory: `cargo test --all -- --list` (548 tests defined).
- Last full run on `main`: **`cargo test --all` → 545 passed / 0 failed** (rest `#[ignore]`).
- Gate: `cargo fmt --all -- --check` (0) · `cargo clippy --all-targets -- -D warnings` (clean).

Status legend: ✅ proven · ⚠️ invariant unit-tested, write-path enforcement pending (increment 4) · ◻️ pending (Lane 2 / future).

| # | North-star criterion | Proof | Status |
|---|---|---|---|
| 1 | Two Claude sessions can be distinguished and targeted | `session_identity::tests::two_claude_sessions_are_distinguishable`; `scripts/protocol-dogfood-smoke.sh` (targeting) | ✅ |
| 2 | A delivered handoff is not treated as ACKed | `scripts/protocol-dogfood-smoke.sh` (delivered≠acked, 5/5); dogfood `fact_142ad` | ✅ |
| 3 | ACK requires exact `from_session_id` and `ref_event_id` | `event_envelope::tests::ack_and_resolve_require_ref_event_id_and_causation_id`; live ACK `fact_ec5a` | ✅ |
| 4 | Claims conflict by overlapping structured resource scopes | `resource_scope::tests::resource_scope_conflicts_same_file_exclusive`, `…_conflicts_parent_dir_and_child_file` | ✅ |
| 5 | Exclusive claim acquisition is transactional under concurrent writers | `claim_authority::tests::claim_authority_rejects_conflicting_exclusive_owner`; live cross-tool proof `fact_ee7d` | ✅ |
| 6 | Lease renewal does not append durable facts | `claim_authority::tests::claim_authority_lease_renewal_is_index_only` | ✅ |
| 7 | Claim expiration emits one durable event and frees ownership | `claim_authority::tests::claim_authority_expiry_emits_each_claim_once` | ✅ |
| 8 | Stale sessions surface auto-releasable claims without auto-releasing | (no dedicated test; needs registry stale-session projection) | ◻️ Lane 2 / future |
| 9 | Heartbeat updates do not append durable ledger rows | By construction — presence is mutable registry state; `agent_state.rs` projects over `FactKind::Presence` without durable heartbeat rows | ✅ |
| 10 | Replayed ledger + registry snapshot reconstructs room state | `claim_authority::tests::claim_authority_rebuild_ignores_released_claims`; `store::ledger_tests::legacy_fact_without_from_session_id_still_replays`, `…rotated_engagement_segment_in_archive_still_replays` | ✅ |
| 11 | Duplicate writer retries do not duplicate durable facts | `event_envelope::tests::duplicate_idempotency_key_collapses_to_one` (invariant); `Deduper` not yet wired into the append path | ⚠️ |
| 12 | Authorization prevents an observer releasing another's claim / publishing privileged results | `event_envelope::tests::observer_cannot_release_anothers_claim_but_lead_can`, `…lead_can_transfer_and_agent_can_publish_validation` (invariant); `authorize()` not yet called at the write path | ⚠️ |
| — | The old release-suppression bug stays fixed | `claim_authority::tests::claim_authority_old_release_does_not_suppress_later_claim_same_scope` | ✅ |
| — | `from_session_id` round-trips and legacy rows replay | `store::ledger_tests::fact_from_session_id_round_trips_and_defaults_none`, `…legacy_fact_without_from_session_id_still_replays` | ✅ |

## Module test inventories

- **`session_identity::tests`** (10) — endpoint precedence, pane-restart keeps endpoint / new session, host-only ambiguity, legacy-tool back-fill, charset-safe ids, two-Claudes-distinguishable.
- **`event_envelope::tests`** (12) — per-kind required ids, reply (ACK) ref+causation, claim/handoff id requirements, strict-vs-lenient `from_session_id`, observer/lead authorization, idempotency collapse, legacy-row deserialize, JSON round-trips.
- **`claim_authority::tests`** (6) + **`resource_scope::tests`** (7) — Codex lane: transactional exclusive acquisition, lease renewal/expiry, rebuild-from-ledger, scope canonicalization + parent/child conflict, access-mode policy.
- **`store::ledger_tests`** — `from_session_id` + legacy/rotated replay.

## Gaps (tracked, not silent)

| Gap | Where it lands |
|---|---|
| ⚠️ 11 — write-path idempotency dedup (`Deduper` wiring at append) | Increment 4 |
| ⚠️ 12 — write-path authorization (`authorize()` at `say`) | Increment 4 |
| ◻️ 8 — stale-session auto-release surfacing | Lane 2 / registry work |
| Staged `#![allow(dead_code)]` in `session_identity.rs` / `event_envelope.rs` | Removed when increment-4 consumes the full vocabulary |

The advisory path is live today: `command_say` validates each durable fact's
envelope (`event_envelope::ProtocolEventKind::validate`, Lenient) and surfaces
`envelope-incomplete` `SayWarning`s; increment 4 promotes the ⚠️ items from
unit-tested invariants to enforced write-path behavior.
