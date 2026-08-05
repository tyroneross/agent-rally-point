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

/// The real agent-rally-point checkout's `rally-cli` crate root — used ONLY
/// as a probe path to prove the guard rejects it. Never mutated; no git
/// command is ever run against it in this file.
fn real_repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
#[should_panic(expected = "is OUTSIDE the expected temp dir")]
fn fixture_root_outside_tempdir_panics() {
    assert_fixture_root(&real_repo_root());
}

#[test]
#[should_panic(expected = "is OUTSIDE the expected temp dir")]
fn fixture_git_rejects_path_inside_real_checkout() {
    // A path constructed UNDER CARGO_MANIFEST_DIR (not the crate root itself)
    // proves the guard rejects any path inside the real checkout, not merely
    // the one exact root string.
    let inside_real_checkout = real_repo_root().join("src");
    assert_fixture_root(&inside_real_checkout);
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
