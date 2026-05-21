# Deferred build-loop integration tests

These two test files were copied verbatim from `build-loop/scripts/app_pulse/`
during the v0.0.1 extraction (sprint 1). They are NOT general-purpose
tests for `agent-rally-point` — they validate build-loop's specific
integration with the channel:

- `test_orchestrator_contract.py` — verifies `agents/build-orchestrator.md`
  in build-loop's repo documents the App Pulse surfacing block.
- `test_cross_tool.py` — Stage 3 cross-tool validation that loads the
  channel modules from build-loop's canonical install vs a hermetic copy.

Both belong in build-loop's own test suite, not in the standalone
package. They are kept here for reference until build-loop's cutover
(sprint 3) at which point they should move back into build-loop's
integration tests.

For now: not collected by pytest.
