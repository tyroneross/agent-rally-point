<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Builder handoff: verified attribution and principal-aware relevance

## Outcome and boundary

Build zero-prompt, locally verified principal attribution for Rally facts, then use that proof for
cross-agent work views and relevance ranking. Do not build accounts, organization membership,
authorization, key lifecycle, native host adapters, or an executor/judge.

Rally identity is internal-first and sits above codecs. Do not persist MCP/A2A/ACP/SendMessage,
backend, wake-signal, or other protocol capability vocabulary in `Fact`; those aliases belong in a
runtime/session descriptor. A later codec maps into the Rally principal/session and proves
losslessness through its own conformance suite.

Canonical plan:
`docs/plans/2026-08-08-verified-attribution-and-relevance.md`.

Base requirement: create an isolated Build Loop worktree from a green, peer-integrated commit. The
canonical checkout was dirty and had peer-owned `lib.rs`/`store.rs` work when this handoff was
written. Do not implement in that shared tree. Do not rebase or merge
`archive/identity-wiring-20260703`; inspect its tests and concepts only.

## Implementation sequence

### When implementing F-01/F-02/F-05 in C1

Read ADR-01, ADR-02, and ADR-03. Satisfy T-01 through T-07 and T-12.

1. Read all of `session_identity.rs`; the `Fact`, `LedgerLine`, Direct/Routed dispatcher, and all
   `append_*` paths in `store.rs`; `store_client.rs`; `rallyd_core.rs`; `write_authority.rs`; and
   the whoami path in `lib.rs`.
2. Baseline direct/routed and authority tests before editing.
3. Add optional typed `principal_id` and attestation fields once. Let compiler errors enumerate
   constructor sites; do not use a blind text rewrite.
4. Add a minimal `principal_identity.rs` using workspace `dryoc`/`rand`: atomic locked profile
   creation, public-key-derived principal ID, canonical payload, sign, and verify.
5. Sign in the originating client before Direct/Routed dispatch. Normalize `seq=0`; bind repo ID
   and engagement; omit only attestation from signing bytes.
6. Keep failed identity creation non-blocking: append unsigned and derive `unverified`.
7. Extend `whoami` without violating its read-only contract. It may report `uninitialized`; only a
   mutating command creates the profile.
8. Update `docs/security/TRUST-MODEL.md` in the same commit. State same-UID/private-key limits and
   explicitly deny authorization claims.
9. Capture a fresh 10-run process-boundary baseline before editing and repeat it after C1. Measure
   cold start, an unsigned read, a signed append, and the applicable host hook; do not substitute a
   micro-benchmark or inherit the working-position document's numbers.

Required adversarial controls: concurrent first use, field-by-field tampering, repo/engagement
replay, same-name/different-key, different-tool/same-key, unwritable profile, routed parity,
unchanged claim/lead behavior, private-seed canary absence, no codec vocabulary in the Fact schema,
and no material cold-start/hook/write latency regression.

### When implementing F-03 in C2

Read ADR-03. Satisfy T-08 and T-09.

1. Read `RoomQuery`, `filtered`, room composition/budget logic, CLI parsing, and JSON schema tests.
2. Add `--principal self|<id>` to the existing room surface. `self` reads but never creates the
   profile.
3. Match verified principal work only after signature verification in the current context.
4. Keep unsigned/invalid facts visible in the default room; they cannot masquerade as verified
   self.
5. Group work without erasing tool/session precision, scope, causal refs, state, or outcome.
6. Preserve response byte ceilings and omission accounting.

Dogfood with one config root, real Claude and Codex writes, and one unsigned look-alike. The
principal view must show both real agents and exclude the look-alike; default room must retain all.

### When implementing F-04 in C3

Read ADR-04. Satisfy T-10 and T-11.

1. Read the current `next` candidate scorer, `liveness.rs`, `decay.rs`, claim renewal, and reaper
   tests.
2. Extend only the derived score: exact target, verified principal continuity, scope overlap,
   dependency/ref adjacency, state/recency, and existing liveness decay.
3. Emit deterministic score reasons.
4. Do not persist a new relevance event, expire a principal, hide history, or feed relevance into
   claim/reaper authority.
5. Update generated onboarding, RALLY.md, protocol schema docs, trust model, and north-star signing
   trigger only after behavior is green.

Dogfood in a dedicated engagement with active, quiet, waiting, and done facts. Retain raw latency
and ranking output. Observation in Easy Terminal/Cockpit is a later integration, not a C3 blocker.

## Mandatory invariants

- `principal_id` proves a key, not a real-world name.
- Git `user.name` is display metadata only; Git email never enters the ledger.
- Principal identity persists; session/relevance signals decay.
- Missing or invalid proof never blocks a Rally write in C1-C3.
- Existing claim, lead, release, resolve, and reaper behavior is byte/decision compatible.
- Private seed material appears only in the mode-0600 local profile.
- Candidate-only/lazy verification protects room and `next` latency.
- External protocol aliases resolve to Rally identity above the codec and never enter durable facts.
- Rally remains the host-neutral facilitator. Native transport and an optional auditor consume the
  protocol later; they do not enter this build.

## Verification

Run focused controls after each commit and the full gate on the integrated isolated worktree:

```bash
cargo fmt --all -- --check
cargo clippy -p rally-cli --all-targets -- -D warnings
cargo test -p rally-cli --lib principal_identity
cargo test -p rally-cli --test verified_attribution
cargo test -p rally-cli --test write_authority_daemon_parity
cargo test -p rally-cli --test lead_seat_authz
cargo test -p rally-cli --test json_envelope_contract
cargo test --workspace
git diff --check
```

Before closeout, run an independent scope audit of the Fact and CLI schema changes and an
independent adversarial audit of the attribution-versus-authority boundary. The planning turn's
local adversarial review is not a substitute.
