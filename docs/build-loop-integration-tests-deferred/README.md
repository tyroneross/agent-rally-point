# Retired build-loop integration fixtures

The deferred Python integration tests that used to live here were copied from
`build-loop/scripts/app_pulse/` during the v0.0.1 extraction. They were not
general-purpose tests for `agent-rally-point`; they validated build-loop's
specific integration with the old channel.

Those files have been removed from this repository. Greenfield Rally work is
verified through the Rust suite, and any build-loop-specific Python integration
coverage belongs back in build-loop itself.

Current acceptance path:

```bash
cargo test
cargo clippy --all-targets -- -D warnings
git diff --check
```
