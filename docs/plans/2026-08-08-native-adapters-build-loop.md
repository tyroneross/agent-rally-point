---
id: native-adapters-build-loop
dimension: runtime
severity: high
architecture_impact: true
modifies_api: true
scope_auditor_status: self-review-complete-independent-review-pending
risk_reason: runtime protocol
dispatch_tier: frontier
files_touched:
  - crates/rally-cli/src/cli.rs
  - crates/rally-cli/src/lib.rs
  - crates/rally-cli/src/hook_runtime.rs
  - crates/rally-cli/tests/native_hook.rs
  - hooks/rally-coordination-hook.sh
---

# Native adapters Build Loop

<!-- checklist
Item 1 — Auth guard: N/A: this slice adds local coordination attribution, not authentication or authorization; verified principals remain Phase 2.
Item 2 — External APIs: N/A: no external API call is introduced; all work is local CLI, filesystem, and existing Rally ledger behavior.
Item 3 — Rate-limit criterion: N/A: no paid or external API call.
Item 4 — Discoverability: N/A: CLI/backend only; `rally hook --help` and top-level help expose the command.
Item 5 — Server/client boundary: N/A: one local Rust CLI process and host stdin/stdout; host codecs are isolated in `hook_runtime.rs`.
Item 6 — Concurrency: existing RoomStore mutation lock and claim-authority index remain authoritative; same-owner checks read the index and fallback to a locked authoritative snapshot only on missing/corrupt index.
Item 7 — Observability: retain durable working status and claim facts; emit a bounded stderr diagnostic on fail-open internal check failure; timing test reports p95.
Item 8 — Input validation: parse stdin as bounded typed fields from JSON, normalize paths, reject unsupported phases and hosts at CLI entry, and fail open on malformed/empty host envelopes.
Item 9 — Stable ID traceability: U-01 -> F-01 -> D-01/D-02 -> T-01/T-02/T-03; U-02 -> F-02 -> D-03 -> T-04/T-05.
Item 10 — JSON spec object: present in `Spec Object (JSON)` below with linked needs, features, data points, tests, and ADRs.
Item 11 — Blocking-and-novel question gate: N/A: no open questions; direct host registration is explicitly deferred as reversible Phase 1B scope.
Item 12 — Low-reversibility ADRs: N/A: the change is additive and feature-detected with legacy fallback; ADR-01 records the reversible native-command boundary.
Item 13 — Analytical lens: DSM for command/wrapper/store dependencies plus QFD for speed, accuracy, and reliability acceptance mapping.
Item 14 — Handoff document: `docs/plans/2026-08-08-native-adapters-build-loop.handoff.md` links F-01/F-02 to ADR-01 and T-01..T-05.
Item 15 — Synthesis dimensions: N/A: no UI surface.
Item 16 — Risk reason: runtime protocol — host stdin/stdout response envelopes change behind a capability-detected fallback.
Item 17 — UI input/output contract: N/A: no UI surface.
Item 18 — Dispatch tier per work item: frontier for the cross-host runtime seam and consequence analysis; deterministic validators and benchmarks run as scripts.
Item 19 — Env-var manifest: N/A: no new external service; new environment switches are optional local controls with defaults.
Item 20 — Capability gap map: present below for native transaction, identity stability, compatibility, and latency.
Item 21 — Single-shot build guardrails: present below with host-envelope, fail-open, durability, and scope controls.
Item 22 — Read-before-edit map: present below with exact CLI, check, wrapper, test, and protocol sources.
-->

**Run:** `bl-20260808T072127Z-codex-788004`

**Branch:** `bl/run-788004`
**Status:** Phase 1A implemented and locally verified

## Outcome

Rally will use native host capabilities as accelerators while keeping one internal identity, durable ledger, claim model, and receipt model. The delivery order is dependency-driven: make coordination cheap, establish stable attribution, then connect native transports.

## Architecture direction

| Layer | Rally owns | Host owns |
|---|---|---|
| Coordination model | identity mapping, claims, dependencies, intent, state, receipts, history | none |
| Transport selection | capability negotiation, downgrade policy, delivery result | native messaging/session APIs |
| Execution | no agent execution | sessions, subagents, resume, worktrees |
| Durability | append-only facts and reproducible projections | disposable transcripts/session state |

Every codec maps a host address into a stable Rally agent/session identity. A codec may lose no required field silently; it must either carry the field, preserve it in the Rally envelope, or return a typed downgrade/refusal.

## Architecture note

The new boundary is one additive CLI transaction: host JSON enters
`rally hook`, Rally performs coordination through its existing authorities, and
one host-specific JSON envelope leaves. Rally continues to own identity,
claims, durable facts, and ACK semantics; the host owns only invocation and
response interpretation. ADR-01 selects this boundary over either duplicating
coordination in shell or introducing an abstract adapter framework before two
native consumers exist. Capability detection makes rollback immediate.

## Locked decisions

- Analytical lens: DSM for dependency sequencing and QFD for mapping speed, accuracy, and reliability to tests.
- ADR-01: add one native Rust transaction behind capability detection; keep the existing shell path as the compatibility fallback.
- Default remains advisory. Strict blocking is opt-in and only emitted for hosts whose hook contract supports denial.
- Codex receives visible context but no fabricated Claude permission fields.
- No principal authorization, external service, new runtime dependency, or direct host configuration change lands in Phase 1A.

## Approach Lenses

- Clean-sheet best approach: each supported host invokes the installed Rally binary directly through one versioned host-codec contract; Rally reads a bounded coordination projection and emits one typed response.
- Current-constraints approach: the shipped project hooks already enter through a security-hardened shell wrapper and users may have older installed binaries, so replace only the expensive transaction while retaining capability detection and fallback.
- Bridge/backcast: prove the native transaction and host parity first, then move supported host registrations directly to it in Phase 1B while the wrapper remains the compatibility path for old binaries and other phases.
- Recommendation: ship the constrained bridge now because it removes Node and repeated processes without coupling correctness to an installer migration. The direct command already proves the clean-sheet latency target.

## Spec Object (JSON)

```json
{
  "needs": [
    {"id": "U-01", "statement": "Every edit receives fast, consistent Rally deconfliction."},
    {"id": "U-02", "statement": "Native acceleration must preserve host-specific safety and durable Rally semantics."}
  ],
  "features": [
    {"id": "F-01", "needIds": ["U-01"], "statement": "One-process native before-write transaction."},
    {"id": "F-02", "needIds": ["U-02"], "statement": "Capability-detected wrapper fallback and host envelope codec."}
  ],
  "dataPoints": [
    {"id": "D-01", "featureIds": ["F-01"], "statement": "Normalized path and current claim projection."},
    {"id": "D-02", "featureIds": ["F-01"], "statement": "Stable host session and tool identity."},
    {"id": "D-03", "featureIds": ["F-02"], "statement": "Host family, allow verdict, strict capability, and advisory message."}
  ],
  "tests": [
    {"id": "T-01", "featureIds": ["F-01"], "statement": "Allowed path auto-claims exactly once."},
    {"id": "T-02", "featureIds": ["F-01"], "statement": "Observer PID preserves identity without host session input."},
    {"id": "T-03", "featureIds": ["F-01"], "statement": "Warm direct p95 stays below 20 ms in the opt-in timing gate."},
    {"id": "T-04", "featureIds": ["F-02"], "statement": "Claude and Codex conflict envelopes preserve their distinct contracts."},
    {"id": "T-05", "featureIds": ["F-02"], "statement": "Wrapper uses native mode without Node and falls back for older binaries."}
  ],
  "adrs": [
    {"id": "ADR-01", "featureIds": ["F-01", "F-02"], "decision": "Native CLI transaction with capability-detected legacy fallback", "rollback": "Set RALLY_NATIVE_HOOK=off or use an older binary."}
  ]
}
```

## Scope

Phase 1A owns the native `before-write` command, host envelope codec, stable
session fallback, compatibility routing, tests, and measured evidence.

### Out of scope

- Direct host configuration against the binary; retained for Phase 1B after missing-binary behavior is proven.
- Verified human principals and signatures; Phase 2.
- Codex or Claude native message delivery; Phases 3 and 4.
- Organization accounts, authorization policy, key rotation, and cross-org controls.

## One-commit table

| # | Commit subject | Files owned | Depends on | dispatch_tier |
|---|---|---|---|---|
| 1 | `feat(hooks): add native before-write transaction` | CLI parser/runtime, wrapper, integration tests, plan/handoff | existing check, claim authority, and host hook contracts | `frontier` — cross-host runtime contract with safety consequences |

## Depends-on (reads-from)

- `host stdin JSON: tool_input.file_path / notebook_path and session_id variants` — verified by existing shell fixtures and T-01/T-04 fixtures.
- `.rally/config.json hooks.enabled` through `hooks_config::resolve` — verified; written by `rally hooks on|off` and covered by shell configuration tests.
- `.rally/active-claims.json` through `claim_authority::read_index` — verified; maintained by the existing claim-authority append path and exercised by T-01.
- `.rally` durable facts and bounded snapshot cache through `command_check` — verified; written by `RoomStore` and covered by projection/cache tests.
- `RALLY_OBSERVER_PID` — verified; exported by `hooks/rally-coordination-hook.sh` before native dispatch and exercised by T-02.
- `RALLY_HOOK_STRICT`, `RALLY_NATIVE_HOOK`, and dedupe controls — verified; optional environment inputs with explicit defaults in the wrapper/runtime.

## Activation Map

- Native before-write transaction — trigger: Claude/Codex/Cursor/Gemini `PreToolUse` registration invokes `hooks/rally-coordination-hook.sh before-write <host>`, whose capability branch invokes `rally hook before-write <host>` — verified-live: yes (T-05 executes the shipped wrapper into the real native binary).
- Legacy compatibility fallback — trigger: the same wrapper call sees no exact native help marker, `RALLY_NATIVE_HOOK=off`, or a failed native invocation and continues into the established shell/Node path — verified-live: yes (existing shell suite uses old-command stubs; 39 cases pass).
- Native command direct call — trigger: `rally hook before-write <host>` CLI dispatch from a supported host registration or manual fixture — verified-live: yes (T-01 through T-04 execute the real binary).

## Capability Gap Map

| Capability/Workflow | Current source of truth | Target behavior | Gap | Build action | Owned files/contracts | Validation |
|---|---|---|---|---|---|---|
| Before-write transaction | `hooks/rally-coordination-hook.sh` | One Rust process coordinates and renders | Shell launches multiple Rally and Node processes | Add `rally hook before-write` | `cli.rs`, `lib.rs`, `hook_runtime.rs` | T-01, T-03 |
| Host envelope codec | wrapper Node renderers and host tests | Claude/Codex/Gemini/Cursor output from typed Rust functions | Logic duplicated in shell-embedded scripts | Centralize parser/renderer | `hook_runtime.rs` | T-04 plus sanitizer tests |
| Session stability | wrapper `RALLY_OBSERVER_PID` and host input | Repeated edits map to one tool identity | Native fallback used child PID | Prefer explicit/session/env/observer signals | `hook_runtime.rs` | T-02 |
| Compatibility | wrapper legacy path | New binaries accelerate; old binaries continue | No native capability probe | Cache a positive capability marker and fall back on failure | `rally-coordination-hook.sh` | T-05 and shell contracts |

## Single-Shot Build Guardrails

| Guardrail | Prevents | Evidence/test |
|---|---|---|
| Use canonical `command_check`, `command_status_post`, and `command_say` authorities | Semantic fork between hooks and CLI | T-01 plus projection parity tests |
| Default advisory; block only in strict mode on denial-capable hosts | Unexpected edit denial or fabricated host fields | T-04 |
| Treat peer-controlled strings as quoted, flattened data | Prompt/context injection through coordination facts | `test_context_sanitization.sh` |
| Native failure falls through to the existing wrapper path | Silent loss of coordination on binary skew | T-05 plus wrapper contract tests |
| Change only `before-write` in Phase 1A | Regression in start, idle, and after-write | existing 39-case shell suite |

## Read-Before-Edit Map

| Chunk/Work item | Read first | Why it matters | Edit after |
|---|---|---|---|
| Native CLI surface | `crates/rally-cli/src/cli.rs`, command dispatch/help in `lib.rs` | Preserve parser and help registration invariants | `cli.rs`, `lib.rs` |
| Coordination transaction | `command_check`, `command_status_post`, `command_say`, `claim_authority.rs` | Reuse canonical concurrency and persistence authorities | `lib.rs`, `hook_runtime.rs` |
| Host envelopes | `hooks/rally-coordination-hook.sh`, `tests/hooks/test_rally_coordination_hook.sh`, `test_context_sanitization.sh` | Preserve exact host contracts and untrusted-data boundary | `hook_runtime.rs`, wrapper |
| Performance | `check_perf_failclosed.rs`, snapshot cache and claim index paths | Avoid replacing one replay with another hidden slow path | `native_hook.rs`, hot-path code only |

## F-Criteria

| Criterion | Pass condition | Grader |
|---|---|---|
| F-01 deconfliction | Foreign live claim surfaces a visible warning; strict Claude denies | T-04 |
| F-01 claim | Allowed unclaimed path creates one durable claim | T-01 |
| F-01 identity | Missing host session still produces one stable owner | T-02 |
| F-01 speed | Direct warm p95 below 20 ms | T-03 with `RALLY_TIMING_TESTS=1` |
| F-02 compatibility | No Node on supported native path; older binaries retain legacy path | T-05 and shell suites |

## Q-Criteria

| Criterion | Pass condition | Grader |
|---|---|---|
| Formatting | No Rust formatting diff | `cargo fmt --all -- --check` |
| Static analysis | No warning across all rally-cli targets | `cargo clippy -p rally-cli --all-targets -- -D warnings` |
| Regression | Complete rally-cli suite passes | `cargo test -p rally-cli` |
| Security boundary | Host context remains sanitized | `tests/hooks/test_context_sanitization.sh` |

## Risks

| Risk | Likelihood | Mitigation |
|---|---|---|
| Installed binary predates native command | Medium | Exact help probe and legacy fallback |
| Host omits session id | Medium | Reuse stable observer process identity |
| Wrapper still exceeds direct 20 ms gate | Certain in Phase 1A | Measure separately; direct registration is Phase 1B |
| Claim index missing or corrupt | Low | Fall back to authoritative snapshot rather than duplicate claims |

## Delivery sequence

### Phase 1A — native before-write vertical slice (this run)

- [x] Add `rally hook before-write <host>`.
- [x] Parse Claude/Codex/Gemini/Cursor-compatible path and session fields in Rust.
- [x] Execute current working-state, check, and auto-claim semantics in one process.
- [x] Emit the correct host envelope with default advisory and opt-in strict behavior.
- [x] Switch only the existing before-write wrapper path.
- [x] Prove behavior with integration fixtures.
- [x] Measure warm p50/p95 against the legacy wrapper.

Acceptance:

- No Node process runs on the before-write path.
- A foreign live claim still surfaces a visible stop-level advisory.
- Default mode stays advisory; strict mode blocks only on a stop-level check.
- An allowed unclaimed path is claimed once; subsequent writes do not duplicate the claim.
- Missing/invalid stdin stays fail-open and returns a valid empty or advisory host envelope.
- The measured latency improves versus the current wrapper; any remaining distance to 20 ms is tied to a named store operation and a next test.

Measured on the same hermetic local room with a release binary:

| Path | p50 | p95 | Result |
|---|---:|---:|---|
| Legacy shell + Node wrapper | 427.82 ms | 581.16 ms | baseline |
| Compatibility wrapper + native command | 32.85 ms | 48.69 ms | about 92% lower p50 |
| Direct warm native command | 3.90 ms | 4.11 ms | passes the `<20 ms` product gate |

The compatibility wrapper remains above 20 ms because it retains shell startup,
binary safety checks, watchdog dispatch, and legacy capability fallback. The
native transaction itself is below the gate. Direct host registration is the
next incremental optimization; the compatibility wrapper remains the fallback
for older binaries and non-native phases.

### Phase 1B — direct registration and storage safeguards

- [ ] Register supported hosts directly against `rally hook before-write` after validating binary discovery and missing-binary behavior.
- [x] Keep check on the existing bounded snapshot-cache fast path rather than replaying the ledger on warm calls.
- [x] Make normal same-owner claim checks read the active claim index and fall back to the authoritative projection only when missing or corrupt.
- [ ] Update the active claim index incrementally after verified append; rebuild only on missing/corrupt/stale index.
- [ ] Add a release/debug rebuild command and corruption tests.
- [x] Enforce direct warm `before-write` p95 `<20 ms` on a representative room fixture through an opt-in timing gate.

### Phase 2 — verified principal attribution

- [ ] Auto-create a local Ed25519 principal on first write with no prompt.
- [ ] Add optional `principal_id` and attestation fields to facts and command envelopes.
- [ ] Bind signatures to repository, engagement, fact payload, and authoring session.
- [ ] Verify on read and expose `verified`, `unsigned_legacy`, and `invalid` states.
- [ ] Add `whoami` principal output and `room --principal self` relevance filtering.
- [ ] Keep authentication informational: no authorization, org account, rotation, revocation, or cross-org policy in this phase.

### Phase 3 — Codex native adapter

- [ ] Generate/bind against the supported Codex app-server schema.
- [ ] Map Rally delivery intent to live interrupt when supported and to resume/queued prompt otherwise.
- [ ] Preserve Rally event id, sender principal, target session, requested ACK, fallback, and delivery result.
- [ ] Keep tmux/ptyd as explicit fallbacks, not implicit success.

### Phase 4 — Claude native adapter

- [ ] Detect `ListAgents`/`SendMessage` capability at runtime.
- [ ] Map Claude agent/session handles into the same Rally identity space.
- [ ] Treat inbox-write success as delivery, not as Rally's positive ACK.
- [ ] Fall back explicitly when the native surface is unavailable or changes.

### Phase 5 — mixed-host dogfood

- [ ] Claude sends a claimed-work handoff to Codex through Rally.
- [ ] Codex receives through its native adapter, writes a target-authored ACK, completes work, and emits a receipt.
- [ ] Claude observes the receipt without polling the full ledger.
- [ ] Capture delivery rate, latency, downgrade, duplicate, and identity-fragmentation metrics.

### Phase 6 — codec SDK and MCP

- [ ] Extract the proven internal adapter seam into one Rust trait and conformance suite.
- [ ] Add round-trip tests for at least two host vocabularies.
- [ ] Expose Rally read/write/tool discovery through MCP; permission_tier: T3 for local repo coordination writes with no external-network authority. Do not describe MCP as push delivery.
- [ ] Refuse required-field loss instead of silently degrading.

## Testing strategy

- Pure parsers/renderers: table tests and hostile-string/property fixtures.
- CLI contracts: hermetic integration tests with real binaries and temp rooms.
- Storage: corruption, concurrent writers, restart/rebuild, legacy ledger, and daemon/direct parity.
- Identity: tamper, replay across repo/engagement, legacy unsigned facts, key-permission, and concurrent first-use tests.
- Native adapters: generated-schema fixtures, capability downgrade matrix, idempotent delivery ids, ACK timeout, and fallback tests.
- Dogfood: one committed mixed-host scenario with Rally event ids as the trace spine.

## Phase 1A verification

- `cargo fmt --all -- --check`
- `cargo clippy -p rally-cli --all-targets -- -D warnings`
- `cargo test -p rally-cli` — green on the confirmation run: 578 library tests passed, 1 ignored, plus all integration suites. An initial default-parallel run exposed one unrelated store memoization test failure; the test passed four isolated repetitions and the complete confirmation run.
- `cargo test -p rally-cli --test native_hook` — 9 passed.
- `RALLY_TIMING_TESTS=1 cargo test -p rally-cli --test native_hook warm_native_hook_has_an_opt_in_twenty_millisecond_gate -- --nocapture` — 4.48 ms p95 on the final focused run.
- `tests/hooks/test_rally_coordination_hook.sh` — 39 passed.
- `tests/hooks/test_context_sanitization.sh` — 12 passed.
- `tests/hooks/test_node_absence_advisory.sh` — 4 passed.
- Existing hook projection, fail-open watchdog, and performance contracts — 8 passed.

## Complexity controls

- Ship one working vertical slice before extracting an interface.
- Add no new runtime dependency unless the standard library/current workspace cannot meet the contract.
- Keep every schema change additive and legacy-readable.
- Measure latency and token cost at each boundary.
- Do not mark transport success as task acknowledgment.
