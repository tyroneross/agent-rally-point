// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Single source of truth for git identity used by throwaway fixture repos
//! in this crate's tests.
//!
//! # The defect this closes
//! On 2026-07-10, 70 commits landed in the REAL agent-rally-point checkout
//! authored `Rally Test <rally@example.test>`. Fixture helpers ran
//! `git -C <root> config user.email rally@example.test`, and `git config`
//! (default `--local`) wrote into the real repository.
//!
//! `-C <root>` looks like it makes that impossible. It does not. Git injects
//! `GIT_DIR`, `GIT_WORK_TREE` and `GIT_INDEX_FILE` into a hook's environment,
//! and **those override `-C`**. The pre-push gate ran `cargo test` without
//! clearing them, so a fixture's `git -C <scratch>` resolved against the real
//! checkout. The local override then beat the correct global identity for
//! every later commit. (RC-064, mechanism corrected 2026-08-05 by an
//! independent forensic pass; the earlier "fixture root drifted into a real
//! checkout" reading was a real but DIFFERENT hazard, and was not what
//! happened here.)
//!
//! Two independent controls follow from that, and the distinction matters:
//!   1. [`fixture_git`] writes no config at all, so there is nothing to
//!      misroute no matter what the environment says. This is what actually
//!      closes the incident.
//!   2. It also clears the repo-scoping env vars, so a fixture's `init` and
//!      `commit` cannot be redirected either — the hook was fixed at
//!      `6616b711`, but a fixture that depends on its caller having cleaned
//!      the environment is waiting for the next caller who forgets.
//!
//! [`assert_fixture_root`] is a THIRD, weaker control against a different
//! hazard. It would not have caught the July 10 incident: those fixture roots
//! were correct.
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
        // THE actual July-10 mechanism (RC-064, corrected 2026-08-05): git
        // injects these into a hook's environment, and they OVERRIDE `-C`.
        // The pre-push gate ran `cargo test` without clearing them, so
        // `git -C <scratch>` in a fixture resolved against the REAL repo.
        // `.githooks/pre-push` was fixed at `6616b711`, but a fixture that
        // depends on its caller having cleared the environment is a fixture
        // waiting for the next caller who forgets. Clearing them HERE makes
        // the fixture correct no matter who spawns it -- including `cargo
        // test` run by hand inside a hook, which no gate covers.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_PREFIX")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_QUARANTINE_PATH")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_CEILING_DIRECTORIES")
        .env_remove("GIT_NAMESPACE")
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
