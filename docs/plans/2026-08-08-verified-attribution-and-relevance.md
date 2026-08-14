<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Plan: zero-friction verified attribution and principal-aware relevance

> **Governing outcome:** Rally should automatically prove which local principal authored an
> agent's coordination fact, use that proof to group and rank relevant work across Claude and
> Codex sessions, and preserve today's open, repo-local coordination path when proof is absent.

<!-- checklist
Item 1 — Auth guard: This adds authenticated attribution, not authorization. Verified, unverified, invalid, and system-derived facts remain readable; existing claim/lead/write authority is unchanged. Threat-model artifact: docs/security/TRUST-MODEL.md, updated in C1.
Item 2 — External APIs: N/A: no external API. Local Git user.name is optional display metadata only; no account, cloud identity provider, or network call.
Item 3 — Rate-limit criterion: N/A: no paid or network API.
Item 4 — Discoverability: zero-prompt first-write creation; rally whoami --json exposes principal_id, attribution_state, and profile_path; rally room --principal self exposes the first principal-aware work view; RALLY.md and generated onboarding text name both.
Item 5 — Server/client boundary: signing occurs in the rally-cli client before Direct/Routed dispatch so rallyd never substitutes its own identity; verification uses the same rally-protocol-shaped canonical payload in either store mode.
Item 6 — Concurrency: first-use key creation is atomic under a user-profile file lock; room writes remain serialized by the existing store mutation lock; two simultaneous first writers must converge on one principal key.
Item 7 — Observability: room output derives verified|unverified|invalid|system attribution; whoami reports local profile readiness; dogfood records same-principal/different-session and different-principal/same-room evidence.
Item 8 — Input validation: bounded hex fields, exact Ed25519 lengths, single-line display metadata, fixed attestation schema/algorithm, principal fingerprint equality, repo/engagement binding, and invalid-signature classification without panic.
Item 9 — Stable ID traceability: U-01 -> F-01 -> D-01/D-02 -> T-01/T-02/T-03; U-02 -> F-02 -> D-03 -> T-05/T-06; U-03 -> F-03/F-04 -> D-04 -> T-08/T-09/T-10.
Item 10 — JSON spec object: present in ## Spec Object (JSON).
Item 11 — Blocking-and-novel question gate: zero open questions. Residual uncertainty is labelled [ASSUMED:] and has an in-chunk measurement or compatibility test.
Item 12 — Low-reversibility ADRs: ADR-01 additive fact attestation; ADR-02 local principal key/profile; ADR-03 attribution never becomes an authority gate in this plan; ADR-04 principal permanence with derived activity decay.
Item 13 — Analytical lens: QFD for user-need-to-capability mapping; DSM for schema/signing/view/relevance ordering; Pugh for per-fact signatures vs session-only proof vs self-asserted metadata.
Item 14 — Handoff document: docs/plans/2026-08-08-verified-attribution-and-relevance.handoff.md.
Item 15 — Synthesis dimensions: N/A: no UI surface.
Item 16 — Risk reason: C1 security boundary + persistence contract + user trust claim; C2 user trust claim; C3 user trust claim. Exact per-chunk values appear in the work table.
Item 17 — UI input/output contract: N/A: no UI surface.
Item 18 — Dispatch tier per work item: C1=opus because a cryptographic persistence contract must be frozen; C2=sonnet because it is a bounded query/projection change; C3=sonnet because it reuses existing deterministic ranking and liveness inputs. Escalate only on failed invariants, not elapsed time.
Item 19 — Env-var manifest: N/A: no new external service. No required env var is introduced; an emergency opt-out is deliberately excluded from v1 to avoid an untested second mode.
Item 20 — Capability gap map: present in ## Capability Gap Map.
Item 21 — Single-shot build guardrails: present in ## Single-Shot Build Guardrails.
Item 22 — Read-before-edit map: present in ## Read-Before-Edit Map.
-->

```yaml
plan_id: 2026-08-08-verified-attribution-and-relevance
as_of_head: d44ef90e221d381c343b5679a0eea87578afcc91
operation: plan_only
modifies_api: true
scope_auditor_status: pending_execute_gate
plan_critic_status: local_adversarial_review_only
parallel_skipped_reason: "Codex delegation is disabled for this turn; deterministic plan verification and a documented local adversarial review replace, but do not impersonate, an independent plan critic. Execute still requires the normal independent boundary audit."
groundwork_disposition: "design lens used; full 11-artifact emitter skipped because no unresolved product-definition fork justified a second canonical spec surface"
```

## Goal

Give one developer or a small team of up to 20 people a zero-friction, locally verified
identity that follows their Claude, Codex, Cursor, and other Rally sessions. Every
agent-authored coordination fact should automatically carry a stable principal, an exact session,
and a cryptographic attestation bound to the repository and engagement. Rally should then answer
"what is my work?" and prioritize updates relevant to that principal without adding accounts,
passwords, invitations, organization policy, key rotation, or an authorization gate.

The first vertical slice must work for one user before it claims team value: one local profile,
two host agents, one shared principal, two distinct sessions, signed facts, transparent
verification state, no prompts, and no failed write when identity creation is unavailable.

## Product boundary

This plan adds **verified attribution**. It does not add access control.

| Question | This plan answers | This plan does not claim |
|---|---|---|
| Who produced this fact? | A principal holding the named local Ed25519 key, through the recorded Rally session | The principal is a legally verified person |
| Can two developers be distinguished? | Yes; separately generated keys produce distinct principal IDs in the same repo room | That either developer is an approved organization member |
| Can one developer's Claude and Codex work be grouped? | Yes; the shared user profile signs both while session and tool IDs remain distinct | That all machines owned by that person are linked automatically |
| Does a bad or missing signature block coordination? | No; it changes attribution state and relevance eligibility only | That unsigned facts are safe or authoritative |
| Does identity expire? | No; the principal remains stable while session and relevance signals decay | Revocation, rotation, recovery, or membership lifecycle |

This is the thin part of Option 4 that strengthens Option 1. A future host adapter can translate
Claude/Codex native messages into Rally's canonical envelope without losing principal, agent,
session, repository, fact-kind, scope, or causal provenance. Native transport discovery and full
message conversion are a separate plan; an executor/judge remains outside Rally core and can
consume this ledger from Easy Terminal or Cockpit.

### Relationship to the federated-codec direction

Rally already has 14 versioned command schemas under `docs/schemas/`. Those schemas and the fact
model are the internal protocol; MCP, A2A, Claude `SendMessage`, Codex app-server, tmux, and future
transports are edge codecs. This plan freezes one rule that every later codec must inherit:

**A codec maps external identities into a stable Rally principal/session. It never makes the
external protocol's identifier the canonical identity.**

Capability and backend vocabulary belongs in a mutable runtime/session descriptor, not in an
append-only coordination fact. A runtime field can be renamed with a version/serde alias; a durable
field creates a lifetime compatibility burden. Accordingly, C1 signs fact kind, scope, session,
principal, repository, engagement, and causal content, but does not add `source_protocol`,
`backend`, `wake_signal`, MCP, A2A, or host-native capability names to `Fact`.

If this plan is accepted, it deliberately advances verified attribution ahead of the "later —
authenticated identity" placement in `docs/POSITION-federated-coordination-plane.md`. The product
trigger changed: verified continuity has value for one user and is no longer waiting for a second
writer. The position document's warning still holds for **authorization**; this plan does not
enforce membership or permissions.

The adjacent sequence remains:

1. C1 may land now because it is an internal write-path capability, not a codec.
2. The native Rust hook/hot-path work in the position document should reclaim end-to-end latency
   before codec #1 ships.
3. A future codec plan must prove the canonical model is a lossless superset with round-trip
   conformance tests and must ship its first real consumer with the codec contract.

## Grounded current state

### OBSERVED at `d44ef90e`

- `store::Fact` has additive `from_session_id` but no `principal_id` or attestation.
- `session_identity::ProtocolSessionIdentity` already models `principal_id`, but
  `current_protocol_session` always passes `None`.
- `event_envelope::EventEnvelope` already names `principal_id` and `auth_context`, but the module
  is staged and does not authenticate a write.
- `docs/security/TRUST-MODEL.md` states that facts are unauthenticated and `tool` is self-supplied.
- `docs/PROTOCOL-NORTH-STAR.md` defers signing until multi-user/untrusted/federated rooms.
- Fourteen `agent-rally.command.*.v1.json` schemas already define Rally's command surface. No Rally
  MCP server exists in this repo; hooks + skills + CLI currently provide automatic enforcement,
  model instruction, and tool/API access respectively.
- The workspace already uses `dryoc` and `rand` in `cockpitd`; `rally-cli` does not depend on
  them. Cockpit crypto includes relay encryption and challenge-response that this slice does not
  need.
- Archived commit `caffc92` already proved additive `Fact.principal_id`, replay compatibility,
  session-key reconstruction, and compiler-guided constructor updates. Commits `1400f12` and
  `fdaff32` extended that branch into authority semantics; the branch was archived rather than
  reconciled with later `store.rs` changes.
- Legacy commit `1bbacfd` implemented Ed25519 identity and sign-on-write in the pre-current
  architecture. Commit `0d5024b` removed that entire implementation during the Rust product-line
  replacement; its concept is evidence, not mergeable code.
- Build Loop memory contains open item `AGEN-IDENTITY-kwja72n0cgmx9` and explicitly records signed
  writers as a deferred protocol change. The capability was considered; it was deferred and later
  stranded by architecture churn, not rejected as valueless.

### DECIDED for this build

- A repository room is the collaboration boundary for v1. No organization object or cross-org
  policy is introduced.
- A principal ID is the fingerprint form of a locally generated public key:
  `principal:ed25519:<64 lowercase hex characters>`.
- Git `user.name` may populate a signed display label. It remains self-declared metadata, not proof
  of a human name. Git email is not written to the repo ledger.
- Identity creation is automatic on the first agent-authored write. Read-only `whoami` remains
  read-only: before first write it reports `uninitialized`; after first write it reports the saved
  principal.
- Identity failure is fail-open for coordination: the fact is appended without an attestation and
  derives as `unverified`. No command prompts or blocks on account setup.
- Existing claim, lead, release, and write-authority semantics do not consult signature state in
  this plan. Invalid or unsigned facts remain coordination inputs exactly as today.
- Principal identity is persistent. Session liveness, claim leases, and notification relevance
  decay independently using existing Rally evidence. No hard principal expiry exists.
- External protocol IDs and capability/backend names stay out of `Fact`; the session/runtime
  registry owns those aliases and codec capabilities.
- Same user on a second device receives a second principal until an explicit device-linking design
  is built. That limitation is visible, not silently inferred away.

### ASSUMED, with named checks

- [ASSUMED:] One OS account maps to one Rally user profile for the initial audience. T-06 simulates
  multiple users with isolated config roots; shared-OS-account profile selection is out of scope.
- [ASSUMED:] A 32-byte seed file at `~/.config/rally/identity/v1/principal.json`, mode `0600`, is the
  least-friction portable store for macOS/Linux. T-04 proves permissions and atomic convergence;
  ADR-02 states the same-UID theft limitation.
- [ASSUMED:] Verifying only candidate/emitted facts keeps room latency within the existing command
  budget. Q-03 measures this before C3 can close; full-ledger eager verification is prohibited.

## Approach lenses

### Pugh: identity proof choice

| Candidate | Zero friction | Portable verification | Exact fact integrity | Complexity | Decision |
|---|---:|---:|---:|---:|---|
| Self-asserted Git/repo/user text | High | Low | None | Low | Reject: useful label, not authentication |
| Sign only session registration | High | Medium | Low; any writer can replay `from_session_id` | Medium | Reject: does not prove each write |
| Per-fact Ed25519 attestation | High after first write | High | High for the signed payload | Medium | **Choose** |
| Account service + OAuth/membership | Low | High | High | High | Defer: adds a gate and organization surface before need |

### QFD: need to feature

| User need | Feature response |
|---|---|
| No setup burden | C1 auto-creates one local profile on first write and fails open |
| Same person's agents stay connected | C1 stamps one principal plus distinct tool/session IDs |
| Multiple developers can coordinate | C1 produces distinct, locally verifiable principals in one repo |
| See what matters to me | C2 adds verified principal filtering and work grouping |
| Avoid stale noise without losing history | C3 derives relevance decay; principal/history remain durable |
| Preserve speed and reliability | C1 signs before Direct/Routed dispatch; C2/C3 verify lazily and cache only as disposable projection data |

### DSM: dependency order

`C1 signed attribution contract -> C2 principal work view -> C3 relevance and decay`.

C2 cannot reliably group work before C1 distinguishes verified from merely asserted principals.
C3 cannot rank principal relevance before C2 provides the identity-aware query and projection.

## Locked decisions and ADRs

### ADR-01 — Additive per-fact attestation contract

**Decision:** add optional `principal_id` and optional `attestation` to `Fact`. `attestation` contains
only `schema`, `algorithm`, `public_key_hex`, and `signature_hex`. The signing payload is a versioned,
whitespace-free JSON object with lexicographically sorted object keys:

```text
{
  "context": {"engagement": <room_id>, "repo_id": <stable repo id>},
  "fact": <all Fact fields except attestation, with seq normalized to 0>
}
```

The public-key hex must derive exactly to `principal_id`. Arrays retain order. Verification derives
state; no writer-supplied `verified` boolean is persisted.

**Alternatives rejected:** session-only proof permits replayed session IDs; evidence-string encoding
is smaller but destroys typed protocol interoperability; signing final JSONL bytes couples identity
to store-assigned sequence and physical serialization.

**Rollback:** readers ignore both optional fields; writers can cease emitting them without a data
migration. Historical signatures remain independently checkable.

### ADR-02 — One automatic local principal profile

**Decision:** store a random 32-byte Ed25519 seed and display metadata at
`~/.config/rally/identity/v1/principal.json`. Create it under a user-profile file lock with a
temporary file, `0600` permissions, file sync, atomic rename, and directory sync where supported.
Two concurrent first writers must read the same winning profile.

Use the narrow Ed25519 signing/verification primitives already available through the workspace's
`dryoc` dependency, but implement a small `rally-cli` identity module. Do not import `cockpitd` or
its relay encryption, token, challenge, or key-wrapping model into the CLI hot path.

**Security limit:** this proves possession of a local key. It does not defend against another
process running as the same OS user, private-key theft, or a user who deliberately shares a key.

**Rollback:** remove the profile reader/writer and dependency from `rally-cli`; the profile file is
left inert for recoverability unless the user explicitly deletes it.

### ADR-03 — Attribution informs views, never authority in this plan

**Decision:** `verified`, `unverified`, and `invalid` affect labels, `--principal self`, and C3
ranking. They do not change whether a claim conflicts, a lead action succeeds, a handoff resolves,
or a fact replays.

**Why:** the user wants coordination continuity and tracking without admission burden. Promoting
signatures into authorization would require membership, revocation, recovery, key rotation, and
explicit policy that are intentionally absent.

**Falsifier:** any C1-C3 test that blocks a write or changes claim/lead projection solely because
the signature is absent or invalid invalidates this ADR.

### ADR-04 — Persistent principal, decaying relevance

**Decision:** never expire a principal. Derive activity/relevance from the latest verified fact,
existing session liveness, explicit target/ref relationships, scope overlap, and dependency markers.
Claims continue to use their own lease/renewal/reaper contract. Relevance may rank an item lower; it
may not erase history or authorize reclaim.

**Rollback:** remove the derived ranking fields/filter. Durable identity and facts remain intact.

## Scope

### In scope

- Automatic local principal profile and Ed25519 fact attestation.
- Additive `Fact` schema fields with old-row and old-binary compatibility.
- Signing before the Direct/Routed store split and verification against repo/engagement context.
- `whoami` principal state plus a principal-aware room/history view.
- Principal-aware relevance that reuses scope, target/ref, dependency, state, and liveness evidence.
- Explicit verified/unverified/invalid/system-derived presentation.
- Threat model, protocol, onboarding, and dogfood evidence updates.

### Out of scope

- Accounts, passwords, OAuth, email verification, invitations, or a hosted identity service.
- Organization membership, cross-organization policy, access control, or per-kind permissions.
- Key rotation, revocation lists, recovery, escrow, passphrases, or automatic multi-device linking.
- Treating signatures as claim/lead/write authorization.
- Native Claude/Codex adapter discovery, message redirection, or protocol codecs.
- An MCP server or A2A/ACP conformance implementation. A future codec plan owns lossless round-trip
  tests and must not use its protocol-specific ID as the Rally principal.
- A Rally executor, steering agent, judge, or LLM auditor. Easy Terminal/Cockpit may observe the
  derived outputs later without changing Rally's facilitator boundary.
- Eager verification of the full historical ledger on every command.

## Capability Gap Map

| Capability/workflow | Current source of truth | Target behavior | Gap | Build action | Owned files/contracts | Validation |
|---|---|---|---|---|---|---|
| Principal model | `session_identity.rs` has optional `principal_id`; `current_protocol_session` passes `None` | One stable principal automatically follows local agent sessions | No profile source or write wiring | Add `principal_identity.rs`; feed profile ID into session and facts | `session_identity.rs`, `lib.rs`, new module | T-01, T-04, T-05 |
| Fact proof | `store::Fact` has `from_session_id` only | Agent-authored facts carry verifiable, repo-bound attestations | Tool/session fields remain self-asserted | Add typed attestation, canonical payload, client-side signing and reader verification | `store.rs`, `store_wire.rs`, `rally-protocol` docs/schema as required | T-02, T-03, T-07 |
| Direct/routed parity | `RoomStore` dispatches to Direct or rallyd | Originating client signs before either branch | Signing inside Direct would misattribute routed writes | Prepare once at the dispatcher/client boundary; daemon only persists | `store.rs`, `store_client.rs`, `rallyd_core.rs`, parity tests | T-07 |
| Identity visibility | `whoami` shows protocol session with null principal | Shows uninitialized/ready/unavailable plus principal ID and label | No operator-visible proof state | Extend whoami payload and command schema | `lib.rs`, JSON contract tests, schemas | T-01, T-05 |
| My work/history | `RoomQuery` filters tool/role/path/event/thread | `--principal self|<id>` returns verified authored/target-relevant facts across tools | Tool IDs fragment one person's history | Add principal filter and derived work summary without changing default room output | `cli.rs`, `store.rs`, `lib.rs` | T-08, T-09 |
| Relevance/decay | `next` ranks target/recency; liveness and leases exist separately | Same-principal, scope/dependency, state, and recency improve ordering; identity never expires | No principal continuity; stale noise competes with live work | Add bounded derived score using existing facts/liveness; no new durable event | `lib.rs`, focused ranking tests | T-10, Q-03 |
| Trust documentation | Trust model says single-operator and unsigned facts | Precisely states verified-attribution guarantee and its same-UID limits | Product promise would otherwise outrun implementation | Update trust model, north star trigger, protocol schema, onboarding | docs named in C1/C3 | T-11 |
| Codec-neutral identity | External transports currently expose host/tool/session-shaped IDs | Every codec resolves to the stable Rally principal/session above the transport | A second codec would multiply one agent into multiple identities | Keep codec/backend vocabulary out of Fact; document future identity-mapping conformance | Fact schema, session/runtime descriptor boundary | T-13 schema-negative control |

## Activation map

| Capability | Producer/registration | Runtime consumer | Operator surface | Activation proof |
|---|---|---|---|---|
| Local principal | first agent-authored append loads/creates profile | session identity + attestation signer | `rally whoami --json` | T-01/T-04: first write creates once; whoami reads without mutation |
| Fact attestation | RoomStore client dispatcher prepares fact | Direct store or routed rallyd persists unchanged | fact JSON + derived attribution state | T-02/T-07: same bytes/verification in both modes |
| Verification | room/query projection verifies candidate facts | principal filter and relevance ranker | `rally room --principal ... --json` | T-03/T-08: tamper invalid; unsigned legacy visible but excluded from verified-self grouping |
| Relevance decay | existing facts, liveness, scopes, targets, refs | `next` candidate scorer | `rally next --json` reason/evidence | T-10: fake-clock ordering; claim authority unchanged |

No hook is required to create the identity. Existing hooks call mutating Rally commands and receive
the same automatic path. Generated onboarding text changes only after the CLI behavior and schemas
are green, preventing documentation-only activation.

## Depends-on / reads-from contracts

| Contract | Read by | Status | Required evidence before edit |
|---|---|---|---|
| `Fact` serialization and legacy defaults | C1 | OBSERVED | Read `store.rs` Fact/LedgerLine/append paths; enumerate every literal with compiler errors |
| Direct/Routed store split | C1 | OBSERVED | Read `RoomStore::append_*`, `store_client`, `rallyd_core`; run daemon parity baseline |
| Stable repo and engagement IDs | C1 | OBSERVED | Read `resolve_repo_id`, `room_id`, engagement persistence; fixture clone check |
| Protocol session derivation | C1 | OBSERVED | Read all of `session_identity.rs` and current `whoami` integration |
| Existing authority semantics | C1-C3 | OBSERVED | Read `write_authority.rs`, claim and lead tests; preserve exact behavior |
| Room filtering/composition | C2 | OBSERVED | Read `RoomQuery`, `filtered`, response budget code, JSON schema tests |
| `next` scoring and liveness | C3 | OBSERVED | Read candidate scorer, `liveness.rs`, decay/claim renewal/reaper contracts |
| Archived identity implementation | C1 | HISTORICAL | Inspect `caffc92..fdaff32`; reuse tests/ideas only, never merge or rebase blindly |

## Work table: three bounded commits

### C1 — Automatic signed attribution vertical slice

```yaml
dispatch_tier: opus
risk_reason: security boundary
```

**Owns after an isolated Build Loop worktree is created from a green, peer-integrated base:**
`Cargo.toml`, `Cargo.lock`, `crates/rally-cli/Cargo.toml`, new
`crates/rally-cli/src/principal_identity.rs`, `crates/rally-cli/src/session_identity.rs`,
`crates/rally-cli/src/store.rs`, the minimum Direct/Routed wire files the compiler proves necessary,
`crates/rally-cli/src/lib.rs`, `crates/rally-cli/tests/verified_attribution.rs`, JSON contract/schema
fixtures, `docs/security/TRUST-MODEL.md`, and `docs/PROTOCOL-NORTH-STAR.md`.

Build the additive `Fact` fields, profile lock/atomic creation, deterministic canonical payload,
sign/verify functions, client-side prepare seam, and `whoami` projection as one vertical commit.
Use `dryoc`/`rand` directly in `rally-cli`; do not extract cockpit relay crypto. Every existing
literal gets explicit `None` or a shared constructor only where the constructor already improves
clarity. Do not introduce a generic event rewrite.

**Dogfood gate C1-D:**

1. Two processes with one isolated config root enter a fixture as `claude_code:01` and `codex:01`.
2. Both facts verify to one principal and two different `from_session_id` values.
3. A second isolated config root writes to the same room and produces a second principal.
4. An old unsigned fixture row still replays and remains visible as `unverified`.
5. A copied/tampered signed row derives `invalid` and does not crash room projection.
6. An unwritable profile root still permits an unsigned write.

**Close condition:** T-01 through T-07 and F-01/F-02/Q-01/Q-02 pass. The commit is not described as
"authentication complete"; it is "verified local attribution."

### C2 — Verified principal work view

```yaml
dispatch_tier: sonnet
risk_reason: user trust claim
```

Add `--principal self|<principal-id>` to the existing room query rather than a new account or
dashboard command. `self` reads the local profile without creating it. A fact matches verified-self
only when its signature validates in the current repo/engagement. Unverified and invalid facts stay
visible in the normal room but do not masquerade as the verified principal.

Return a bounded derived work summary grouped by principal, tool/session, scope, latest state,
target/ref, and outcome. Preserve the existing room response budget and omission accounting.

**Dogfood gate C2-D:** use the C1-built binary for real Claude and Codex writes in a dedicated Rally
engagement. Confirm one `--principal self` query shows both agents' claim/status/artifact chain and
does not include a deliberately unsigned look-alike principal.

**Close condition:** T-08/T-09 and F-03/Q-03 pass; default `rally room` JSON remains backward
compatible except for additive fields.

### C3 — Principal-aware relevance with adaptive decay

```yaml
dispatch_tier: sonnet
risk_reason: user trust claim
```

Extend the existing `next` candidate score, not the durable ledger. Use this precedence:

1. exact session/tool target;
2. verified principal target/ownership continuity;
3. scope overlap with the principal's active claims;
4. causal/dependency/ref adjacency;
5. active work state and recency;
6. quiet/stale relevance decay from existing liveness signals.

Emit score reasons in JSON so agents can audit why an item rose. A decayed item remains queryable;
identity does not expire; claim reclaim logic does not consume the relevance score.

Update onboarding so agents need no new ceremony: existing `enter`, `say`, `status`, `next`, and
`room` calls carry identity automatically. Update the trust/north-star language from "signing only
when multi-user" to "signing pays off immediately for continuity; stronger membership/authz waits
for an explicit trigger."

**Dogfood gate C3-D:** run one mixed-host engagement long enough to produce active, quiet, waiting,
and done work. Compare ranked `next` output to the raw room, record latency, and require every top
result to cite a deterministic reason. Easy Terminal/Cockpit integration is observation-only and
not required for this commit.

**Close condition:** T-10/T-11 and F-04/Q-03/Q-04 pass; an independent auditor verifies that no
signature state reached claim, lead, release, or reaper authority.

## Parallelism

The implementation is intentionally serial: C1 freezes the persisted proof contract; C2 consumes
it; C3 consumes C2's projection. Within C1, mechanical constructor updates and independent fixture
creation may be prepared in parallel only after the attestation type and signing bytes are frozen.

`parallel_skipped_reason: C1-C3 share a low-reversibility identity contract, and parallel feature
implementation would multiply schema and projection rework. The current turn also cannot dispatch
an independent plan critic under the active Codex delegation gate.`

Implementation must use an isolated Build Loop worktree. The canonical checkout is currently dirty
and `lib.rs`/`store.rs` have peer-owned changes; C1 begins only from the accepted integrated base,
not from this shared working tree and not by rebasing the archived identity branch.

## Single-Shot Build Guardrails

| Guardrail | Prevents | Evidence/test |
|---|---|---|
| Sign in the originating client before Direct/Routed dispatch | rallyd being recorded as the user | T-07 routed parity |
| Bind signature to repo ID and engagement | cross-repo or cross-engagement replay being labelled verified | T-03 |
| Normalize `seq=0`; omit only attestation from canonical bytes | store-assigned sequence creating a signing loop | T-02 golden vector |
| Derive verification; never persist `verified: true` from the writer | self-asserted trust | T-03 malformed/tamper cases |
| Do not use Git email/name as principal ID | spoofable metadata becoming identity | T-05 same-name/different-key fixture |
| Profile creation is atomic and mode `0600` | split identity on concurrent first use or world-readable seed | T-04 |
| Identity failure degrades to unsigned write | account-style friction or coordination outage | T-04 unwritable-root control |
| Verification does not affect authority | accidental access-control product expansion | T-07 plus claim/lead suite |
| Verify candidates/emitted facts, not the full ledger by default | room/next latency growing with all history | Q-03 measurement and instrumentation |
| Preserve unsigned legacy rows and old-reader decode | migration/cutover requirement | T-02/T-07 |
| Reuse liveness and claim leases; relevance has no reclaim power | a fifth, contradictory expiry engine | T-10 |
| Never place seed/private key in repo, ledger, logs, errors, or test snapshots | secret disclosure | T-12 secret canary grep |
| Keep protocol/backend capability names out of Fact | permanent per-codec aliases and identity fragmentation | T-13 negative schema/golden control |
| Measure at process and hook/write boundaries | Rust startup gains being hidden by wrappers or new crypto work | Q-06 retained baseline and raw timing output |

## Read-Before-Edit Map

| Chunk/work item | Read first | Why it matters | Edit after |
|---|---|---|---|
| C1 fact contract | `store.rs` Fact, LedgerLine, all `append_*`; `store_wire.rs`; `store_client.rs`; `rallyd_core.rs` | Finds the one pre-dispatch seam and every serialization path | Fact/attestation fields and prepare/verify seam |
| C1 session/profile | all `session_identity.rs`; `lib.rs` `current_protocol_session`/`command_whoami`; `hooks_config.rs` config-path convention | Preserves endpoint/session semantics and read-only whoami | new `principal_identity.rs`, session/whoami wiring |
| C1 crypto | `crates/cockpitd/src/crypto.rs`; workspace dependencies; `git show 1bbacfd`; `git show caffc92` | Reuses proven primitives/lessons without importing relay scope or stale code | minimal rally-cli dependency/module |
| C1 trust | `docs/security/TRUST-MODEL.md`; `docs/PROTOCOL-NORTH-STAR.md`; `write_authority.rs`; `lead_seat_authz.rs` | Prevents attribution claims from becoming authz claims | threat/protocol docs and regression controls |
| C2 principal view | `RoomQuery`, `filtered`, room composition/budget code; JSON envelope/schema tests | Keeps filtering and byte ceilings correct | CLI arg, query, derived work summary |
| C3 relevance | `command_next` scorer; `liveness.rs`; `decay.rs`; claim renewal/reaper tests | Reuses one liveness truth and protects reclaim semantics | score/reason projection and focused tests |
| All chunks | `git status`, `rally room --json`, `rally whoami --tool <id> --json`, active claims | Avoids shared-checkout and identity ambiguity | only isolated claimed files |

## F-Criteria (functional)

| ID | Pass condition | Grader |
|---|---|---|
| F-01 | First agent-authored write creates or reuses one local principal with no prompt and attaches a valid attestation | T-01/T-04 |
| F-02 | Signature proves the exact fact plus stable repo/engagement context; tampering or replay changes state to invalid | T-02/T-03 |
| F-03 | One principal query groups work across Claude/Codex tool and session IDs while excluding unsigned impersonation | T-08/T-09 |
| F-04 | `next` ranks exact target, same-principal continuity, scope/dependency relevance, state, and decay with explicit reasons | T-10 |
| F-05 | A second isolated user profile coordinates in the same room with a distinct verified principal and no membership setup | T-06 |

## Q-Criteria (quality)

| ID | Pass condition | Grader |
|---|---|---|
| Q-01 | No account/password prompt; unwritable profile cannot prevent a coordination append | T-04 |
| Q-02 | Legacy unsigned ledgers and current old-reader fixtures remain readable; default authority behavior is unchanged | T-02/T-07 |
| Q-03 | On a fixed 1,000-fact fixture, candidate-only verification adds no more than 10% or 2 ms, whichever is larger, to median `room`/`next` latency across five quiesced-host runs; noisy runs are inconclusive, not PASS | performance harness + retained raw output |
| Q-04 | Principal identity persists indefinitely; only derived relevance/session state decays; no identity expiry timestamp exists | schema review + T-10 |
| Q-05 | No seed/private key bytes appear in canonical JSONL, CLI JSON, error text, or snapshots | T-12 |
| Q-06 | C1 records a fresh 10-run median baseline for `rally version`, one agent-authored write, and the host hook path before editing; after C1, cold start and unsigned-read paths regress by no more than 10% or 2 ms, whichever is larger, and signed append adds no more than Q-03's bound. No PASS is inherited from the working-position measurements | retained raw timing output + T-14 smoke |

## Test matrix

| ID | Test | Mutation-sensitive failure it must catch |
|---|---|---|
| T-01 | Same config root, Claude + Codex writes | removing profile reuse produces two principal IDs |
| T-02 | Golden canonical bytes/signature plus legacy round-trip | changing a signed field or canonical ordering breaks verification; old row still decodes |
| T-03 | Tamper subject/scope/session/repo/engagement/public key/signature independently | any mutation still labelled verified |
| T-04 | Concurrent first-use, mode `0600`, simulated unwritable config | split keys, readable secret, or blocked append |
| T-05 | Same Git display name with two keys; changed display name with one key | name becomes proof or principal changes with presentation metadata |
| T-06 | Two isolated config roots write one repo | users collapse to one principal or require membership setup |
| T-07 | Direct and rallyd-routed append/read; existing claim/lead impersonation regression stays unchanged | daemon signs as itself, drops attestation, or signature becomes authority |
| T-08 | `room --principal self` over mixed verified, unsigned, and invalid facts | unsigned look-alike enters verified-self work |
| T-09 | One principal's claim -> status -> handoff -> artifact across two tools | history fragments at the tool/session boundary or loses causal links |
| T-10 | Fake-clock ranking of active/quiet/stale/done facts with scope/ref/dependency controls | identity expires, stale work disappears, or relevance authorizes reclaim |
| T-11 | Generated schemas/onboarding/trust docs match CLI output and guarantees | docs promise authorization, real-person proof, or multi-device linking |
| T-12 | Seed canary scan across JSONL, CLI output, errors, snapshots, and test artifacts | any private material escapes profile storage |
| T-13 | Negative schema/golden assertion: `Fact` contains no protocol/backend/capability identifier; two external aliases map to one Rally principal in a fixture resolver | codec vocabulary becomes durable identity or one agent fragments by transport |
| T-14 | Before/after 10-run process-boundary timing for version, unsigned read, signed append, and applicable host hook | crypto/dependency work spends the Rust cold-start advantage outside unit-level timing |

### Required commands during Execute

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

Run the existing repository gate on the integrated isolated worktree. A green focused test set does
not substitute for the full workspace gate or the independent boundary audit.

## Dogfood ladder

| Stage | Environment | What Rally itself proves | Promotion rule |
|---|---|---|---|
| D0 baseline | current installed binary, read-only | room size, current unsigned behavior, current `whoami` principal null | retain JSON evidence before C1 |
| D1 fixture | temp repo + isolated config root | first-write identity, tamper detection, fail-open, old-row replay | all C1 tests green |
| D2 dual host | one config root, Claude + Codex fixture sessions | same principal, distinct session/tool, routed/direct parity | T-01/T-07 green |
| D3 multi-user simulation | two isolated config roots, one fixture repo | distinct principals with zero membership setup | T-06 green |
| D4 real Rally canary | dedicated engagement in this repo | existing agents can read signed and unsigned rows together | no room/check/next regression; artifact recorded |
| D5 relevance | mixed live/quiet/waiting/done canary | principal view and score reasons improve signal without hiding history | user review + T-08/T-10 + Q-03 |

Dogfood never uses the canonical repo room as the first cryptographic test. D4 happens only after
fixture, routed, tamper, compatibility, and secret-leak controls pass. Every stage posts a Rally
artifact with the binary build ID, commit, commands, and result; delivery is not counted as receipt
without a target-authored ACK where a handoff is involved.

D0 also captures process-boundary latency. Measurements in
`docs/POSITION-federated-coordination-plane.md` are useful prior evidence, but they are not the C1
baseline: the live binary, ledger size, hook implementation, and machine load can drift. The build
retains its own commands and raw samples so a later performance claim is reproducible.

## Local adversarial review

An independent plan critic could not be dispatched because the active Codex environment prohibits
subagent delegation for this turn. This self-review is recorded as a limitation, not presented as
independent verification.

| Attack/question | Plan response | Residual |
|---|---|---|
| Another same-UID process steals the seed | Threat model says not defended; profile mode limits accidental exposure | Same-UID malicious process can impersonate |
| Writer copies a valid fact into another repo | Signed context includes stable repo ID + engagement | A deliberate full-room clone preserving IDs may preserve validity by design |
| Writer claims another `principal_id` | Fingerprint must match public key and signature | Unsigned look-alike remains visible as unverified |
| Key is deleted | New writes form a new principal; old history remains verifiable | No recovery/linking in v1 |
| Two first writes race | Profile lock + atomic publish converge on one file | Crashed creator must release OS lock; filesystem failure falls open unsigned |
| rallyd signs the request | Signing occurs before dispatch; routed parity test checks exact attestation | Compromised client key remains trusted attribution |
| Verification slows a large room | Verify candidate/emitted facts; no eager historical scan | Full forensic verification needs a later explicit command |
| Signature accidentally becomes authorization | ADR-03 and unchanged impersonation regression make that a test failure | Future authz requires a separate plan and threat model |
| Multiple organizations arrive | Repo remains the v1 boundary | Membership, isolation, invitation, revocation, and per-org policy are future work |

## Multi-user now versus multi-organization later

**Multiple users now** requires no server: each OS user gets a distinct key, the repo room carries
their public attestations, and principal-aware views group only cryptographically matching work.
This is sufficient for individual developers and small teams that already share repository access.

**Multiple organizations later** is a different product boundary. It needs an organization ID,
membership/invitation proof, repository-to-organization binding, isolation, revocation, recovery,
and policy about which principals may perform which operations. Adding `organization_id` text today
would not provide any of those guarantees, so this plan does not create a decorative field that
looks authoritative.

## Open Questions

None. Device linking, revocation, organization membership, native host adapters, and an optional
auditor are explicit future scopes, not unresolved choices that block F-01 through F-05.

## Spec Object (JSON)

```json
{
  "needs": [
    {"id": "U-01", "priority": "P0", "statement": "A developer's agents coordinate under one zero-friction, verifiable local principal", "features": ["F-01", "F-02"], "tests": ["T-01", "T-02", "T-03", "T-04"]},
    {"id": "U-02", "priority": "P0", "statement": "Multiple developers can distinguish and resume their own work in one repo without accounts or organization setup", "features": ["F-03", "F-05"], "tests": ["T-05", "T-06", "T-08", "T-09"]},
    {"id": "U-03", "priority": "P0", "statement": "Agents receive the most relevant coordination state quickly without losing durable history", "features": ["F-03", "F-04"], "tests": ["T-08", "T-09", "T-10"]}
  ],
  "features": [
    {"id": "F-01", "priority": "P0", "title": "Automatic local principal profile and signed fact attribution", "chunk": "C1", "adrs": ["A-01", "A-02", "A-03"], "data": ["D-01", "D-02"], "tests": ["T-01", "T-02", "T-03", "T-04", "T-13", "T-14"]},
    {"id": "F-02", "priority": "P0", "title": "Direct/routed verification parity and explicit attribution state", "chunk": "C1", "adrs": ["A-01", "A-03"], "data": ["D-02", "D-03"], "tests": ["T-02", "T-03", "T-07"]},
    {"id": "F-03", "priority": "P0", "title": "Verified principal work/history view across tools and sessions", "chunk": "C2", "adrs": ["A-03"], "data": ["D-03", "D-04"], "tests": ["T-08", "T-09"]},
    {"id": "F-04", "priority": "P0", "title": "Principal-aware relevance with adaptive decay and explicit score reasons", "chunk": "C3", "adrs": ["A-04"], "data": ["D-04"], "tests": ["T-10"]},
    {"id": "F-05", "priority": "P0", "title": "Two zero-setup users in one repo remain distinct", "chunk": "C1", "adrs": ["A-02", "A-03"], "data": ["D-01", "D-03"], "tests": ["T-05", "T-06"]}
  ],
  "data": [
    {"id": "D-01", "contract": "Local principal profile: schema, 32-byte Ed25519 seed, public-key-derived principal ID, optional display label; private file mode 0600", "tests": ["T-01", "T-04", "T-05", "T-06", "T-12"]},
    {"id": "D-02", "contract": "Fact attribution: optional principal_id plus optional v1 Ed25519 attestation over canonical fact and repo/engagement context", "tests": ["T-02", "T-03", "T-07"]},
    {"id": "D-03", "contract": "Derived attribution state is verified, unverified, invalid, or system; it is never writer supplied and never an authority input in this plan", "tests": ["T-03", "T-07", "T-08"]},
    {"id": "D-04", "contract": "Principal-aware work/relevance projection groups by verified principal, preserves tool/session precision, and decays ranking without expiring identity or history", "tests": ["T-08", "T-09", "T-10"]}
  ],
  "tests": [
    {"id": "T-01", "check": "One config root plus Claude and Codex yields one principal and distinct sessions", "grader": "verified_attribution integration test"},
    {"id": "T-02", "check": "Canonical signing vector is deterministic; legacy row round-trips", "grader": "unit plus golden test"},
    {"id": "T-03", "check": "Every signed field/context mutation derives invalid", "grader": "table-driven adversarial test"},
    {"id": "T-04", "check": "Concurrent first use converges, file is 0600, and profile failure permits unsigned append", "grader": "integration test"},
    {"id": "T-05", "check": "Display metadata is not identity", "grader": "unit/integration test"},
    {"id": "T-06", "check": "Two config roots in one repo yield two verified principals without membership setup", "grader": "integration test"},
    {"id": "T-07", "check": "Direct and routed modes preserve attestation while claim/lead authority is unchanged", "grader": "daemon parity and authority regressions"},
    {"id": "T-08", "check": "Principal self query excludes unsigned/invalid impersonation but normal room retains it", "grader": "room query integration test"},
    {"id": "T-09", "check": "Cross-tool claim/status/handoff/artifact history groups under one verified principal", "grader": "user journey"},
    {"id": "T-10", "check": "Fake-clock relevance ordering uses target, principal, scope/ref/dependency, state and decay without hiding history or changing reclaim", "grader": "ranking and claim lifecycle tests"},
    {"id": "T-11", "check": "CLI schemas, generated onboarding, trust model and north star state the same guarantee", "grader": "schema/docs parity review"},
    {"id": "T-12", "check": "Private seed canary absent from ledger, JSON, errors and snapshots", "grader": "secret-leak test"},
    {"id": "T-13", "check": "Fact schema contains no codec/backend identity vocabulary and multiple external aliases resolve above the codec to one Rally principal", "grader": "negative schema golden plus fixture resolver"},
    {"id": "T-14", "check": "Fresh process-boundary before/after timing retains the Rust cold-start and hook/write latency budget", "grader": "retained 10-run raw samples"}
  ],
  "adrs": [
    {"id": "A-01", "title": "Additive per-fact Ed25519 attestation bound to repo and engagement"},
    {"id": "A-02", "title": "One automatic local principal profile with atomic 0600 storage"},
    {"id": "A-03", "title": "Attribution informs views and never authority in this plan"},
    {"id": "A-04", "title": "Persistent principal with derived session and relevance decay"}
  ]
}
```

## Acceptance and execution gate

The plan is ready for user acceptance when deterministic checklist and plan verification have no
blockers. Execute may begin only after:

1. current peer-owned `lib.rs`/`store.rs` work is integrated or explicitly excluded from the base;
2. an isolated Build Loop worktree is provisioned;
3. the implementer re-reads live Rally claims and the files in the read-before-edit map;
4. the scope-auditor reviews the public Fact/CLI schema changes; and
5. the user accepts this product boundary: verified attribution now, authorization and
   organizations later.
