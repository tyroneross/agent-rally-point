// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Adversarial control for the fixture git-identity isolation guard.
//!
//! Closes the register entry for the 2026-07-10 identity leak: 64 commits
//! authored `Rally Test <rally@example.test>` landed in the REAL
//! agent-rally-point checkout because a fixture ran
//! `git -C <root> config user.email ...` against a root that resolved into
//! (or sat inside) the real repo. `git config`'s default `--local` scope
//! writes to whatever repo `-C <root>` discovers by walking up from `root` —
//! so any fixture that ever calls that subcommand is one bad root away from
//! repeating the leak, no matter how carefully the root is normally chosen.
//!
//! These tests prove, independently, the two controls that close it for
//! good:
//! 1. `assert_fixture_root` panics BEFORE git ever runs, for any path
//!    outside the process temp dir — the impossible-state guard.
//! 2. `fixture_git` never writes to any git config file, PERIOD — even when
//!    pointed at a repo that already carries its own identity config. This
//!    is tested against a throwaway decoy repo (never the real checkout),
//!    independently of guard #1, because guard #1 permits any temp-dir path
//!    through — the decoy lives in the temp dir on purpose, so this test
//!    isolates "does `fixture_git` itself ever write config" from "does the
//!    guard reject a bad root".

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

mod common;
use common::test_git_fixture::{FIXTURE_EMAIL, assert_fixture_root, fixture_git};

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
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
    let out = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["config", "--local", "--get", "user.email"])
        .output()
        .expect("git config --get");
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
        let out = Command::new("git")
            .arg("-C")
            .arg(&decoy)
            .args(args)
            .output()
            .expect("git invocation");
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
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let decoy = tmp_dir("envleak-decoy");
    let scratch = tmp_dir("envleak-scratch");

    let setup = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&decoy)
            .args(args)
            .output()
            .expect("git invocation");
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
    let decoy_head_before = {
        let out = Command::new("git")
            .arg("-C")
            .arg(&decoy)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git invocation");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    // Reproduce the hostile condition: GIT_DIR (and friends) exported at the
    // decoy, exactly as git would inject them into a hook's environment.
    // SAFETY: single-threaded test process; set before any fixture_git call
    // and removed immediately after.
    unsafe {
        std::env::set_var("GIT_DIR", decoy.join(".git"));
        std::env::set_var("GIT_WORK_TREE", &decoy);
    }

    let result = std::panic::catch_unwind(|| {
        fixture_git(&scratch, &["init", "-q", "-b", "main"]);
        fixture_git(&scratch, &["commit", "--allow-empty", "-m", "scratch commit"]);
        fixture_git(&scratch, &["rev-parse", "HEAD"])
    });

    unsafe {
        std::env::remove_var("GIT_DIR");
        std::env::remove_var("GIT_WORK_TREE");
    }

    let scratch_head = result.expect("fixture_git must work under a hostile GIT_DIR");

    // The decoy must be untouched: no config write, and no new commit.
    assert_eq!(
        config_before,
        fs::read(&decoy_config).expect("read decoy config after"),
        "an inherited GIT_DIR must not let a fixture write into another repo's config"
    );
    let decoy_head_after = {
        let out = Command::new("git")
            .arg("-C")
            .arg(&decoy)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git invocation");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
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
