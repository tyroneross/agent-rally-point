// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
//! The ONE way an integration test spawns the `rally` binary.
//!
//! # Why this module exists
//!
//! `DEFAULT_WATCHDOG_TIMEOUT_MS` (`crates/rally-cli/src/lib.rs`) is 3000ms.
//! That value is CORRECT for production: rally is invoked synchronously from
//! agent write-hooks, and an unbounded hook can wedge a session (2026-05-30,
//! four `before-write` hooks in uninterruptible kernel wait for 7h45m). It is
//! not being changed here and must not be changed for tests.
//!
//! It is the wrong value for a test-spawned command. An integration test
//! starts real daemon children, does real filesystem work, and runs on a
//! machine that may be compiling Rust in four other lanes. Three seconds of
//! wall clock is a coin flip there, not a bound.
//!
//! Before this module existed there was no place to say so. Every test file
//! built its own `Command::new(env!("CARGO_BIN_EXE_rally"))`, so the only way
//! to correct the budget was per file, by remembering to. Fifteen files
//! remembered and carry `--timeout-ms` / `RALLY_HOOK_TIMEOUT_MS` workarounds;
//! `referenced_handoff_targeting.rs` did not, and it is the file that flaked
//! (measured 1 failing run in 20 in isolation, 2 of 8 full-suite runs).
//! Knowledge that spreads by copy-paste stops at the first file that does not
//! copy. This module is that knowledge as a mechanism instead.
//!
//! # Contract
//!
//! Every integration test spawns rally through [`rally_command`]. A test that
//! calls `Command::new(env!("CARGO_BIN_EXE_rally"))` directly silently
//! inherits the production budget again; `scripts/lint_sibling_asymmetry.py`
//! flags that omission.

use std::process::Command;

/// Wall-clock watchdog budget for a test-spawned `rally` invocation, in ms.
///
/// Sized against two failure modes, not against a benchmark:
///
/// * **Too low** and a loaded machine turns a passing test red at random.
///   Ten times the production budget clears every observed slow window
///   (the worst measured single-command wall clock under four parallel
///   Rust compiles was well under a second; 3000ms was tripped by
///   scheduling latency, not by work).
/// * **Too high** and a genuinely wedged command hangs the suite instead of
///   failing it. 30s is still a bound a human will wait through.
///
/// Must stay inside `MIN_WATCHDOG_TIMEOUT_MS..=MAX_WATCHDOG_TIMEOUT_MS`
/// (100..60_000) or the CLI clamps it and this constant stops meaning what
/// it says. `budget_is_inside_the_cli_clamp_band` asserts that.
pub const TEST_WATCHDOG_TIMEOUT_MS: u64 = 30_000;

/// Path to the `rally` binary under test.
pub fn rally_bin() -> &'static str {
    env!("CARGO_BIN_EXE_rally")
}

/// Build a `rally` [`Command`] carrying the test watchdog budget.
///
/// This is the choke point. Callers still set their own cwd, `HOME`, env and
/// args; the budget is the one thing they no longer get to forget.
///
/// The budget travels as `RALLY_HOOK_TIMEOUT_MS` rather than `--timeout-ms`
/// so it applies to every invocation without the caller having to thread a
/// flag through an arg slice it builds elsewhere.
pub fn rally_command() -> Command {
    let mut cmd = Command::new(rally_bin());
    cmd.env(
        "RALLY_HOOK_TIMEOUT_MS",
        TEST_WATCHDOG_TIMEOUT_MS.to_string(),
    );
    cmd
}

/// Build a `rally` [`Command`] with NO watchdog override.
///
/// `resolve_watchdog_timeout` lets an explicit override win for EVERY command
/// and clamps it to 60s. That is correct for ordinary verbs and wrong for the
/// two whose own budget must be allowed to exceed the hook band:
///
/// * `inject --timeout-seconds N` for N above ~55, which sizes its watchdog
///   from the ACK wait plus headroom.
/// * A test that is deliberately exercising the production default itself
///   (`watchdog_timeout.rs` and its siblings — there the watchdog IS the
///   subject under test, and an ambient override would erase the thing being
///   measured).
///
/// Reach for [`rally_command`] unless you are one of those. If you are, say
/// which in a comment at the call site.
pub fn rally_command_unbudgeted() -> Command {
    Command::new(rally_bin())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CLI clamps an out-of-band override into `100..=60_000`
    /// (`MIN_WATCHDOG_TIMEOUT_MS`/`MAX_WATCHDOG_TIMEOUT_MS`). A budget outside
    /// that band would be silently rewritten, so this constant would no longer
    /// describe what tests actually run under.
    #[test]
    fn budget_is_inside_the_cli_clamp_band() {
        assert!(
            (100..=60_000).contains(&TEST_WATCHDOG_TIMEOUT_MS),
            "TEST_WATCHDOG_TIMEOUT_MS={TEST_WATCHDOG_TIMEOUT_MS} is outside the CLI clamp band \
             100..=60000 and would be silently rewritten"
        );
    }

    /// The whole point is that tests do not run at the production budget.
    #[test]
    fn budget_is_not_the_production_default() {
        assert_ne!(
            TEST_WATCHDOG_TIMEOUT_MS, 3000,
            "test budget must differ from DEFAULT_WATCHDOG_TIMEOUT_MS; if it does not, \
             this module is ceremony and the flake is back"
        );
    }

    #[test]
    fn budgeted_command_carries_the_override_and_unbudgeted_does_not() {
        let budgeted = rally_command();
        let carried = budgeted
            .get_envs()
            .find(|(k, _)| *k == "RALLY_HOOK_TIMEOUT_MS")
            .and_then(|(_, v)| v)
            .map(|v| v.to_string_lossy().to_string());
        assert_eq!(carried.as_deref(), Some("30000"));

        let unbudgeted = rally_command_unbudgeted();
        assert!(
            unbudgeted
                .get_envs()
                .all(|(k, _)| k != "RALLY_HOOK_TIMEOUT_MS"),
            "rally_command_unbudgeted must not set a budget"
        );
    }
}
