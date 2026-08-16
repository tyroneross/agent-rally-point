# Handoff — C6 room message contract (implementer pointers)

Plan: `docs/plans/2026-08-16-c6-room-message-contract.md` (read the sections named per chunk; do not re-derive).
Worktree: `/Users/tyroneross/dev/git-folder/agent-rally-point/.build-loop/worktrees/c6-20260816`, branch `bl/c6-room-message-2026-08-16`.
Hard constraints (all chunks): no version bump; CHANGELOG heading untouched; no tag/push; `git add` explicit paths only; never touch README.md, config/host-integrations.json, scripts/*, .github/workflows/release.yml, docs/RELEASING.md (peer claim `codex:release-cleanup-c5f8ebd7`); never edit inside the two `UNTRUSTED-DATA BOUNDARY` blocks; never touch `crates/rally-cli/src/hook_runtime.rs`.

## Chunk K — knob (Rust). Read plan §"Chunk details / K", ADR-01; satisfy T-06, T-07.
- FIRST: `git status && git diff crates/rally-cli/src/hooks_config.rs crates/rally-cli/src/cli.rs crates/rally-cli/src/lib.rs crates/rally-cli/tests/cli_guardrails.rs` — this work is already in flight on disk. Adopt it; finish gaps (a) status text line, (b) `envelope_hooks` re-pin.
- Verify: CP1 commands in the plan.

## Chunk S — shell composer. Read plan §"The message contract", §"Templates", §"Command selection", §"Chunk details / S", ADR-02; satisfy F-01…F-04, F-06, F-07 (tests T-01…T-05, T-08, T-09 are written by chunk T against this spec — run them).
- Only file: `hooks/rally-coordination-hook.sh`, outside both sanitizer blocks. Verbose path = untouched legacy code; brief branch = new composer in renderer 2; renderer 1 adds the `brief` object; line 1916 gains `RALLY_NEXT_JSON`; hooks_meta gains the room_detail line + env-first export.
- Before handing over: `bash tests/hooks/test_sanitizer_block_parity.sh`; grep guards listed in S(6); every worked render in the plan reproduces byte-for-byte from the matching stub.

## Chunk T — tests. Read plan §"Chunk details / T" (fixture table G-a…G-o, mutation table m1…m8, env-pin list), §"Templates" (expected strings).
- New: `tests/hooks/test_room_message_contract.sh`, `crates/rally-cli/tests/hook_room_message.rs`. Edits (env only): `tests/hooks/test_context_sanitization.sh` (1 line), `tests/hooks/test_rally_coordination_hook.sh` (4 invocation sites), `crates/rally-cli/tests/hook_projection_parity.rs` (1 `.env` line).
- The verbose oracle is `git show ce3d7e9:hooks/rally-coordination-hook.sh`; never hand-write verbose expectations.
- Run each mutation once and record which case went red.

## Chunk D — docs. Read plan §"Chunk details / D"; satisfy F-08.
- `CHANGELOG.md` entry goes UNDER the existing `## v0.2.5 - 2026-08-15` heading (heading + release-date note byte-unchanged).

## Gates at the end (orchestrator)
CP1 → CP2 → CP3 as listed in the plan; RC-073 `reaper_scale` is a KNOWN pre-existing red in `scripts/run-quality-gate.sh` — report it verbatim, never mask it. Merge step for CHANGELOG and the peer-claim coordination: plan §"Host surfaces and the peer claim".
