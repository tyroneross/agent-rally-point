// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Adversarial control for the fixture git-identity isolation guard.
//!
//! Closes the register entry for the 2026-07-10 identity leak: 64 commits on `main`
//! authored `Rally Test <rally@example.test>` landed in the REAL
//! agent-rally-point checkout because Git exported repository-scoping
//! variables such as `GIT_DIR`, `GIT_WORK_TREE`, and `GIT_INDEX_FILE` into a
//! hook. Those variables override `git -C <root>`, so a fixture's local-config
//! command targeted the real checkout even though its scratch root was valid.
//!
//! These tests prove the independent controls that close it:
//! 1. `fixture_git` clears inherited repository-scoping variables before
//!    every child Git command.
//! 2. `fixture_git` never writes identity into a Git config file, even when
//!    pointed at a repo that already carries its own identity config.
//! 3. `assert_fixture_root` rejects paths outside the process temp dir as
//!    defense in depth against a separate bad-root hazard.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

mod common;
use common::test_git_fixture::{
    FIXTURE_EMAIL, GIT_REPOSITORY_SCOPE_ENV_VARS, assert_fixture_root, fixture_git,
    fixture_git_command,
};

const HOSTILE_ENV_CHILD: &str = "RALLY_FIXTURE_GIT_HOSTILE_ENV_CHILD";
const HOSTILE_ENV_SCRATCH: &str = "RALLY_FIXTURE_GIT_HOSTILE_ENV_SCRATCH";
const HOSTILE_ENV_HEAD_OUT: &str = "RALLY_FIXTURE_GIT_HOSTILE_ENV_HEAD_OUT";
const HOSTILE_SUITE_CHILD: &str = "RALLY_FIXTURE_GIT_HOSTILE_SUITE_CHILD";

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Runs a raw repo-scoped Git command without fixture identity overrides.
/// Decoy setup needs the repository's own local identity, but it must still
/// be immune to hostile hook variables that override `git -C <root>`.
fn repo_git(root: &std::path::Path, args: &[&str]) -> Output {
    let mut command = fixture_git_command(root);
    command.args(args);
    command.output().expect("git invocation")
}

fn apply_hostile_git_scope(command: &mut Command, decoy: &std::path::Path) {
    let git_dir = decoy.join(".git");
    let index = git_dir.join("index");
    let objects = git_dir.join("objects");
    let ceiling = decoy.parent().unwrap_or(decoy);
    for var in GIT_REPOSITORY_SCOPE_ENV_VARS {
        match *var {
            "GIT_DIR" | "GIT_COMMON_DIR" => command.env(var, &git_dir),
            "GIT_WORK_TREE" => command.env(var, decoy),
            "GIT_INDEX_FILE" => command.env(var, &index),
            "GIT_OBJECT_DIRECTORY" | "GIT_QUARANTINE_PATH" | "GIT_ALTERNATE_OBJECT_DIRECTORIES" => {
                command.env(var, &objects)
            }
            "GIT_CEILING_DIRECTORIES" => command.env(var, ceiling),
            "GIT_PREFIX" => command.env(var, "hostile-prefix/"),
            "GIT_NAMESPACE" => command.env(var, "rally-hostile"),
            _ => unreachable!("all repository-scope variables are mapped"),
        };
    }
}

fn tmp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("rally-fixture-isolation-{label}-{nanos}"));
    fs::create_dir_all(&p).unwrap();
    p
}

/// A path that is outside the process temp dir on every platform this builds
/// for — used ONLY as a probe to prove the guard rejects it. Never mutated; no
/// git command is ever run against it in this file.
///
/// This deliberately does NOT use `CARGO_MANIFEST_DIR`. The pre-push gate builds
/// in a detached worktree under `$TMPDIR`, so there the crate root IS inside the
/// temp dir and the guard correctly does not fire — a `should_panic` test keyed
/// on the manifest dir passes locally and fails in the gate. The assertion under
/// test is "outside temp is rejected", so the probe must be outside temp by
/// construction rather than by assumption about where the checkout lives.
fn outside_tempdir_probe() -> PathBuf {
    PathBuf::from("/rally-fixture-guard-probe-never-created")
}

#[test]
#[should_panic(expected = "is OUTSIDE the expected temp dir")]
fn fixture_root_outside_tempdir_panics() {
    assert_fixture_root(&outside_tempdir_probe());
}

#[test]
#[should_panic(expected = "is OUTSIDE the expected temp dir")]
fn fixture_git_rejects_nested_path_outside_tempdir() {
    // A nested path proves the guard rejects by prefix, not by exact string
    // match on one known root.
    //
    // Scope limit, stated because the guard's name overpromises: this control
    // is "outside the process temp dir is rejected", NOT "the real checkout is
    // rejected". Those coincide only while the checkout lives outside `$TMPDIR`.
    // In the pre-push gate — and in any CI or dev setup that builds under a temp
    // path — the checkout IS inside the temp dir and this guard cannot fire. The
    // unconditional control is `fixture_git_writes_no_identity_config` below:
    // the fixture writes no config anywhere, so a bad root is inert regardless
    // of where it points. This guard is defense in depth, not the boundary.
    let nested = outside_tempdir_probe().join("src");
    assert_fixture_root(&nested);
}

#[test]
fn fixture_git_rejects_config_subcommand() {
    let root = tmp_dir("reject-config");
    let result = std::panic::catch_unwind(|| {
        fixture_git(
            &root,
            &["config", "user.email", "must-not-write@rally.invalid"],
        );
    });
    assert!(
        result.is_err(),
        "fixture_git must make config writes unavailable through its normal API"
    );
    fs::remove_dir_all(&root).ok();
}

#[test]
fn fixture_git_writes_no_identity_config() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let root = tmp_dir("no-identity-config");
    fixture_git(&root, &["init", "-q", "-b", "main"]);
    fixture_git(&root, &["commit", "--allow-empty", "-m", "initial"]);

    let author_email = fixture_git(&root, &["log", "-1", "--format=%ae"]);
    let committer_email = fixture_git(&root, &["log", "-1", "--format=%ce"]);
    assert_eq!(
        author_email, FIXTURE_EMAIL,
        "commit author email must be the fixture identity"
    );
    assert_eq!(
        committer_email, FIXTURE_EMAIL,
        "commit committer email must be the fixture identity"
    );

    // Nothing was ever written to local config: `git config --local --get
    // user.email` must exit non-zero (git's convention for "key not set").
    let out = repo_git(&root, &["config", "--local", "--get", "user.email"]);
    assert!(
        !out.status.success(),
        "user.email must NOT be present in local config after fixture_git commits; got: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // Byte-level check of the config file itself: neither key string appears.
    let config_bytes = fs::read(root.join(".git").join("config")).expect("read .git/config");
    let config_text = String::from_utf8_lossy(&config_bytes);
    assert!(
        !config_text.contains("user.email") && !config_text.contains("user.name"),
        "fixture_git must never write user.email/user.name into .git/config; got:\n{config_text}"
    );

    fs::remove_dir_all(&root).ok();
}

/// The load-bearing proof: run `fixture_git` against a root that ALREADY
/// carries its own (decoy) identity config — mimicking a real checkout that
/// has a human's `user.email`/`user.name` set locally — and assert the
/// decoy's `.git/config` is byte-identical before and after. This proves the
/// identity-write elimination independently of `assert_fixture_root`'s
/// temp-dir guard: the decoy root IS inside the temp dir (so the guard lets
/// it through on purpose), which isolates "does `fixture_git` ever write
/// config" as its own, separately-tested claim.
#[test]
fn fixture_git_leaves_decoy_real_repo_config_untouched() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let decoy = tmp_dir("decoy-real-repo");

    // Build the decoy with its OWN local identity config, directly via
    // `git config` (deliberately NOT via fixture_git) — simulating a real
    // repo a human configured by hand. This is exactly what the pre-fix bug
    // would have clobbered had `-C` resolved here instead of the intended
    // fixture root.
    let setup = |args: &[&str]| {
        let out = repo_git(&decoy, args);
        assert!(
            out.status.success(),
            "decoy setup git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    setup(&["init", "-q", "-b", "main"]);
    setup(&["config", "user.email", "decoy-human@rossl.example"]);
    setup(&["config", "user.name", "Decoy Human"]);
    setup(&["commit", "--allow-empty", "-m", "decoy initial"]);

    let config_path = decoy.join(".git").join("config");
    let before = fs::read(&config_path).expect("read decoy .git/config before");

    // Run fixture_git against the decoy root — same `-C` discovery path that
    // caused the original leak, except now routed through the shared helper.
    fixture_git(&decoy, &["commit", "--allow-empty", "-m", "fixture commit"]);

    let after = fs::read(&config_path).expect("read decoy .git/config after");
    assert_eq!(
        before, after,
        "fixture_git must leave a pre-existing repo's .git/config byte-identical"
    );

    // And the commit fixture_git just made carries the FIXTURE identity, not
    // the decoy's own configured identity — proving the `-c`/env overrides
    // win even against a repo with its own local config already set.
    let author_email = fixture_git(&decoy, &["log", "-1", "--format=%ae"]);
    assert_eq!(
        author_email, FIXTURE_EMAIL,
        "fixture_git's commit must carry FIXTURE_EMAIL even though the decoy \
         repo has its own configured identity"
    );

    fs::remove_dir_all(&decoy).ok();
}

/// The regression test for RC-064's ACTUAL mechanism, corrected 2026-08-05.
///
/// The original diagnosis — "a fixture whose root drifted into a real
/// checkout" — was wrong. All three July-10 fixtures already passed an
/// explicit `git -C <scratch>`. What defeated `-C` was environment
/// inheritance: git injects `GIT_DIR`, `GIT_WORK_TREE` and `GIT_INDEX_FILE`
/// into a hook's environment, **those override `-C`**, and the pre-push gate
/// ran `cargo test` without clearing them. So `git -C <scratch> config`
/// resolved against the REAL repository.
///
/// `.githooks/pre-push` was fixed at `6616b711` by clearing the scope vars in
/// its gate subshells. That closes the path the gate controls. It does NOT
/// close the fixture itself, which stays vulnerable to any future caller who
/// forgets — a hand-run `cargo test` inside a hook, a new gate script, a
/// different host. `fixture_git` therefore clears the vars itself, and this
/// test is what proves it: it reproduces the exact hostile condition by
/// exporting `GIT_DIR` at a decoy repo, and asserts the write lands in the
/// fixture and never in the decoy.
///
/// Note what this test does NOT rely on: `assert_fixture_root` cannot help
/// here. Both roots are legitimately inside the temp dir. The temp-dir guard
/// would have let the original incident through untouched.
#[test]
fn fixture_git_ignores_inherited_git_dir_pointing_at_another_repo() {
    // The parent test launches this exact test in a child process with the
    // hostile Git variables set only on that child. Keeping the ambient
    // environment process-local prevents a concurrent libtest thread from
    // observing the reproduction setup.
    if std::env::var_os(HOSTILE_ENV_CHILD).is_some() {
        let scratch =
            PathBuf::from(std::env::var_os(HOSTILE_ENV_SCRATCH).expect("child scratch path"));
        let head_out =
            PathBuf::from(std::env::var_os(HOSTILE_ENV_HEAD_OUT).expect("child HEAD output path"));
        fixture_git(&scratch, &["init", "-q", "-b", "main"]);
        fixture_git(
            &scratch,
            &["commit", "--allow-empty", "-m", "scratch commit"],
        );
        let scratch_head = fixture_git(&scratch, &["rev-parse", "HEAD"]);
        fs::write(head_out, scratch_head).expect("write child HEAD output");
        return;
    }

    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let decoy = tmp_dir("envleak-decoy");
    let scratch = tmp_dir("envleak-scratch");

    let setup = |args: &[&str]| {
        let out = repo_git(&decoy, args);
        assert!(
            out.status.success(),
            "decoy setup git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    setup(&["init", "-q", "-b", "main"]);
    setup(&["config", "user.email", "decoy-human@rossl.example"]);
    setup(&["config", "user.name", "Decoy Human"]);
    setup(&["commit", "--allow-empty", "-m", "decoy initial"]);

    let decoy_config = decoy.join(".git").join("config");
    let config_before = fs::read(&decoy_config).expect("read decoy config before");
    let decoy_head_before =
        String::from_utf8_lossy(&repo_git(&decoy, &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();

    // Reproduce the hostile condition in a child test process. Environment
    // variables configured on Command affect only the child, so sibling tests
    // in this process cannot observe GIT_DIR while they run raw Git commands.
    let head_out = scratch.join("child-head.txt");
    let mut child_command = Command::new(std::env::current_exe().expect("current test binary"));
    child_command
        .arg("--exact")
        .arg("fixture_git_ignores_inherited_git_dir_pointing_at_another_repo")
        .arg("--test-threads=1")
        .env(HOSTILE_ENV_CHILD, "1")
        .env(HOSTILE_ENV_SCRATCH, &scratch)
        .env(HOSTILE_ENV_HEAD_OUT, &head_out);
    apply_hostile_git_scope(&mut child_command, &decoy);
    let child = child_command
        .output()
        .expect("launch hostile-environment child test");
    assert!(
        child.status.success(),
        "fixture_git must work under a hostile Git environment; stdout: {} stderr: {}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    let scratch_head = fs::read_to_string(&head_out)
        .expect("read child HEAD output")
        .trim()
        .to_string();

    // The decoy must be untouched: no config write, and no new commit.
    assert_eq!(
        config_before,
        fs::read(&decoy_config).expect("read decoy config after"),
        "an inherited GIT_DIR must not let a fixture write into another repo's config"
    );
    let decoy_head_after =
        String::from_utf8_lossy(&repo_git(&decoy, &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();
    assert_eq!(
        decoy_head_before, decoy_head_after,
        "an inherited GIT_DIR must not let a fixture commit into another repo"
    );

    // And the fixture's own repo really did get the commit.
    assert!(
        !scratch_head.is_empty(),
        "the fixture repo must have its own HEAD"
    );
    assert_ne!(
        scratch_head, decoy_head_after,
        "the fixture commit must live in the fixture repo, not the decoy"
    );

    fs::remove_dir_all(&decoy).ok();
    fs::remove_dir_all(&scratch).ok();
}

/// Launch the complete integration-test binary with every repository-scoping
/// variable pointed at an external decoy. This catches future raw Git setup
/// commands that bypass the shared sanitized command boundary.
#[test]
fn fixture_test_binary_is_hermetic_under_inherited_git_scope() {
    if std::env::var_os(HOSTILE_SUITE_CHILD).is_some() {
        return;
    }
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }

    let decoy = tmp_dir("whole-suite-decoy");
    let setup = |args: &[&str]| {
        let out = repo_git(&decoy, args);
        assert!(
            out.status.success(),
            "whole-suite decoy setup git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    setup(&["init", "-q", "-b", "main"]);
    setup(&["config", "user.email", "decoy-human@rossl.example"]);
    setup(&["config", "user.name", "Decoy Human"]);
    setup(&["commit", "--allow-empty", "-m", "decoy initial"]);

    let config_path = decoy.join(".git/config");
    let index_path = decoy.join(".git/index");
    let config_before = fs::read(&config_path).expect("read whole-suite decoy config before");
    let index_before = fs::read(&index_path).expect("read whole-suite decoy index before");
    let head_before = repo_git(&decoy, &["rev-parse", "HEAD"]).stdout;

    let mut child_command = Command::new(std::env::current_exe().expect("current test binary"));
    child_command
        .arg("--test-threads=8")
        .env(HOSTILE_SUITE_CHILD, "1");
    apply_hostile_git_scope(&mut child_command, &decoy);
    let child = child_command
        .output()
        .expect("launch full hostile-environment fixture test binary");
    assert!(
        child.status.success(),
        "the complete fixture test binary must pass under hostile Git scope; stdout: {} stderr: {}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );

    assert_eq!(
        config_before,
        fs::read(&config_path).expect("read whole-suite decoy config after"),
        "the hostile whole-suite run must not alter the decoy config"
    );
    assert_eq!(
        index_before,
        fs::read(&index_path).expect("read whole-suite decoy index after"),
        "the hostile whole-suite run must not alter the decoy index"
    );
    assert_eq!(
        head_before,
        repo_git(&decoy, &["rev-parse", "HEAD"]).stdout,
        "the hostile whole-suite run must not move the decoy HEAD"
    );

    fs::remove_dir_all(&decoy).ok();
}
