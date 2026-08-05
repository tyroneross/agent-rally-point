// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Single source of truth for git identity used by throwaway fixture repos
//! in this crate's tests.
//!
//! # The defect this closes
//! On 2026-07-10, 64 commits landed in the REAL agent-rally-point checkout
//! authored `Rally Test <rally@example.test>`. The mechanism: fixture helpers
//! ran `git -C <root> config user.email rally@example.test`. `git config`
//! (default `--local`) discovers the enclosing repo by walking up from
//! `-C <root>`. If `root` ever resolved to — or sat inside — the real
//! checkout (or was a linked worktree, where `--local` writes to the MAIN
//! repo's `.git/config`), the fixture silently wrote a local identity
//! override into the real repo that beat the correct global identity for
//! every later commit.
//!
//! Contributor PR #11 (May 2026) fixed this exact defect CLASS for
//! `core.hooksPath` in tmp-repo fixtures. It reappeared in July on a path
//! that fix did not cover — a point-fix on one call site is not a durable
//! control. This module is the durable control: every fixture in this
//! crate routes through [`fixture_git`], which never writes to any config
//! file (identity is supplied per-invocation via `-c` and env vars), and
//! every fixture root is asserted to live inside the process temp dir
//! BEFORE git ever runs.
//!
//! Shared with integration tests (`tests/*.rs`) via `tests/common/mod.rs`,
//! which pulls this file in with `#[path = "../../src/test_git_fixture.rs"]`
//! rather than duplicating it — one implementation, one place to fix next
//! time.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Identity every fixture repo commits under. Never written to any config
/// file — see module docs. Uses the RFC 2606 reserved `.invalid` TLD so it
/// can never be a routable address, and so it reads as distinct from the
/// `@example.test` string that already sits in 64 historical commits (a
/// leak detector can tell a NEW leak from the known-historical ones).
pub const FIXTURE_NAME: &str = "Rally Fixture";
pub const FIXTURE_EMAIL: &str = "fixture@rally.invalid";

/// Panics unless `root` resolves to a path inside the process temp dir.
///
/// This is the impossible-state guard: a fixture whose root escaped to a
/// real checkout dies here, BEFORE git ever runs against it.
///
/// Both `root` and `std::env::temp_dir()` are canonicalized before
/// comparison (macOS's `/var` -> `/private/var` symlink would otherwise
/// produce false failures). If a path does not exist yet, the nearest
/// existing ancestor is canonicalized instead and the non-existent tail is
/// re-appended, so the guard also works for a root a caller is about to
/// create.
pub fn assert_fixture_root(root: &Path) {
    let expected = canonicalize_best_effort(&std::env::temp_dir());
    let actual = canonicalize_best_effort(root);
    assert!(
        actual.starts_with(&expected),
        "fixture root {actual:?} is OUTSIDE the expected temp dir {expected:?} — \
         a test fixture tried to operate on a path outside the process temp \
         directory. This guard exists specifically to stop a fixture from \
         writing git identity (or any other git config) into a REAL checkout; \
         see the 64-commit `rally@example.test` identity leak from 2026-07-10 \
         that this module's callers close for good."
    );
}

/// Canonicalize `path`, falling back to canonicalizing the nearest existing
/// ancestor and re-appending the non-existent tail when `path` itself does
/// not exist yet.
fn canonicalize_best_effort(path: &Path) -> PathBuf {
    if let Ok(canon) = path.canonicalize() {
        return canon;
    }
    let mut existing = path;
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while let Some(parent) = existing.parent() {
        if let Some(name) = existing.file_name() {
            tail.push(name.to_os_string());
        }
        existing = parent;
        if let Ok(canon) = existing.canonicalize() {
            let mut result = canon;
            for part in tail.iter().rev() {
                result.push(part);
            }
            return result;
        }
    }
    // Nothing on the path exists (shouldn't happen in practice — even
    // `std::env::temp_dir()` and filesystem roots exist) — fall back to the
    // uncanonicalized path rather than panicking here; `assert_fixture_root`
    // still does its job on a best-effort basis.
    path.to_path_buf()
}

/// Runs `git -C <root> <args>` with identity supplied PER INVOCATION.
/// Writes NOTHING to any git config file. Asserts the fixture root is
/// inside the process temp dir first, then asserts the command succeeded.
/// Returns trimmed stdout.
pub fn fixture_git(root: &Path, args: &[&str]) -> String {
    assert_fixture_root(root);
    let out = Command::new("git")
        .arg("-c")
        .arg(format!("user.name={FIXTURE_NAME}"))
        .arg("-c")
        .arg(format!("user.email={FIXTURE_EMAIL}"))
        .arg("-C")
        .arg(root)
        .args(args)
        // Belt and braces: `-c` above covers anything reading config;
        // these env vars cover commit-creation call paths directly.
        .env("GIT_AUTHOR_NAME", FIXTURE_NAME)
        .env("GIT_AUTHOR_EMAIL", FIXTURE_EMAIL)
        .env("GIT_COMMITTER_NAME", FIXTURE_NAME)
        .env("GIT_COMMITTER_EMAIL", FIXTURE_EMAIL)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git invocation");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}
