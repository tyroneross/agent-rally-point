// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Shared test-fixture plumbing for rally-cli integration tests.
//!
//! Pulls in `src/test_git_fixture.rs` by path rather than duplicating it —
//! integration-test crates (built with `--test`) already have `cfg(test)`
//! set, so the source module's `#[cfg(test)]` gate is satisfied here too.
//! This keeps identity-fixture logic to ONE implementation shared by the
//! unit tests in `lib.rs`/`run_worktree.rs` and the integration tests in
//! this directory.
#[path = "../../src/test_git_fixture.rs"]
pub mod test_git_fixture;
