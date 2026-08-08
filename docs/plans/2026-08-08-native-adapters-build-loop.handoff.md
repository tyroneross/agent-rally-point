# Native before-write implementation handoff

## Scope

Review the Phase 1A native `before-write` vertical slice. Do not extend the
review into verified principals or native message delivery.

## Implementation pointers

- When reviewing F-01, read ADR-01 in
  `docs/plans/2026-08-08-native-adapters-build-loop.md`, inspect
  `command_hook` in `crates/rally-cli/src/lib.rs`, and satisfy T-01, T-02, and
  T-03 in `crates/rally-cli/tests/native_hook.rs`.
- When reviewing F-02, compare the codec in
  `crates/rally-cli/src/hook_runtime.rs` against the legacy renderer in
  `hooks/rally-coordination-hook.sh`, then satisfy T-04 and T-05.
- Treat `command_check`, `command_status_post`, `command_say`, and the active
  claim index as authorities. Report any duplicated coordination semantics.

## Adversarial review questions

1. Can malformed host JSON, unknown hosts, or peer-controlled claim text escape
   the fail-open and untrusted-data contracts?
2. Can two edits from one host fragment into different Rally owners when the
   host omits a session id?
3. Can strict mode deny a Codex edit even though Codex has no supported deny
   response field?
4. Can a missing, old, or failing Rally binary suppress the legacy fallback?
5. Can a repeated write create duplicate durable claims or invalidate the
   snapshot cache on every edit?

## Required evidence

- `cargo test -p rally-cli --test native_hook`
- `RALLY_TIMING_TESTS=1 cargo test -p rally-cli --test native_hook warm_native_hook_has_an_opt_in_twenty_millisecond_gate -- --nocapture`
- `tests/hooks/test_rally_coordination_hook.sh`
- `tests/hooks/test_context_sanitization.sh`
- `cargo clippy -p rally-cli --all-targets -- -D warnings`
