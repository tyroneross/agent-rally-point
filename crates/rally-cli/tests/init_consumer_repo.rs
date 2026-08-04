// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `rally init` running in a **consumer repo** — any
//! repo that isn't agent-rally-point itself.
//!
//! Before this fix, `rally init` hardcoded five doc paths specific to
//! agent-rally-point's own documentation set (`RALLY.md`,
//! `dynamic-workflows/COORDINATION.md`, `dynamic-workflows/PROTOCOL.md`,
//! `docs/ORCHESTRATION.md`, `docs/ANY-AGENT-ONBOARDING.md`) and hard-errored
//! if any of them was missing from the target worktree — which is every
//! repo except this one. Worse, `.rally/` was created *before* that check,
//! so a failed `rally init` left a partially-initialised `.rally/` behind.
//!
//! These tests prove: pointer docs are optional and selectively recorded,
//! and a genuine init failure leaves no `.rally/` directory behind.
//!
//! # Conventions
//! Mirrors `worktree_gc.rs`'s `tmp_dir`/`init_repo` helpers and
//! `claims_refresh.rs`'s `env!("CARGO_BIN_EXE_rally")` binary-invocation
//! idiom — all tests operate on ephemeral git repos in the OS temp dir and
//! never touch the live agent-rally-point checkout.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tmp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("rally-init-consumer-{label}-{nanos}"));
    fs::create_dir_all(&p).unwrap();
    p.canonicalize().unwrap_or(p)
}

/// Initialise a bare-minimum git repo: `git init -b main` + initial empty
/// commit. A consumer repo — deliberately carries none of
/// agent-rally-point's own doc pointers.
fn init_repo(root: &Path) {
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("git invocation");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "init-consumer-test@example.test"]);
    run(&["config", "user.name", "Init Consumer Test"]);
    run(&["commit", "--allow-empty", "-m", "initial"]);
}

fn run_rally(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rally"))
        .current_dir(root)
        .args(args)
        .output()
        .expect("rally invocation")
}

fn manifest_json(root: &Path) -> Value {
    let manifest_path = root.join(".rally").join("manifest.json");
    let raw = fs::read_to_string(&manifest_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest_path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse manifest.json: {e}"))
}

/// The five pointer-doc manifest keys agent-rally-point knows about.
const ALL_POINTER_LABELS: &[&str] = &[
    "guide",
    "doctrine",
    "protocol",
    "board",
    "any_agent_onboarding",
];

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `rally init` in a repo with NONE of agent-rally-point's five pointer docs
/// must exit 0 and produce a usable `.rally/manifest.json`. Before the fix
/// this hard-errored on the first missing doc (`docs.guide`).
#[test]
fn init_succeeds_in_a_repo_with_none_of_the_pointer_docs() {
    let root = tmp_dir("none");
    init_repo(&root);

    let out = run_rally(&root, &["init", "--json"]);
    assert!(
        out.status.success(),
        "rally init must succeed in a repo with no pointer docs\nstderr: {}\nstdout: {}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );

    let manifest = manifest_json(&root);
    assert!(
        manifest.get("repo").and_then(Value::as_str).is_some(),
        "manifest must carry repo: {manifest}"
    );
    assert_eq!(manifest["schema"], "agent-rally.manifest.v1");
    assert!(
        manifest.get("ledger").and_then(Value::as_str).is_some(),
        "manifest must carry ledger: {manifest}"
    );
    assert_eq!(manifest["room_cmd"], "rally room");
    assert_eq!(manifest["init_cmd"], "rally init");
    assert!(
        manifest.get("pointer_markers").is_some(),
        "manifest must carry pointer_markers: {manifest}"
    );

    fs::remove_dir_all(&root).ok();
}

/// The manifest's `docs` object must NOT contain a key whose pointer target
/// does not resolve. A consumer repo with none of the five docs must produce
/// an empty (or fully-absent-keyed) `docs` object.
#[test]
fn init_omits_pointer_docs_that_do_not_resolve() {
    let root = tmp_dir("omit-all");
    init_repo(&root);

    let out = run_rally(&root, &["init", "--json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let manifest = manifest_json(&root);
    let docs = manifest
        .get("docs")
        .unwrap_or_else(|| panic!("manifest must carry a docs object: {manifest}"));
    for label in ALL_POINTER_LABELS {
        assert!(
            docs.get(*label).is_none(),
            "docs.{label} must not appear when {label}'s target does not resolve; docs={docs}"
        );
    }
    assert_eq!(manifest["pointer_docs_resolved"], 0);
    assert_eq!(manifest["pointer_docs_omitted"], 5);

    fs::remove_dir_all(&root).ok();
}

/// A consumer repo that DOES carry a subset of the pointer docs must have
/// exactly that subset recorded in the manifest — proving the fix is
/// selective (only-what-resolves) rather than "drop all docs".
#[test]
fn init_keeps_every_pointer_doc_that_does_resolve() {
    let root = tmp_dir("partial");
    init_repo(&root);
    fs::write(root.join("RALLY.md"), "# RALLY.md\nstub\n").unwrap();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(
        root.join("docs").join("ORCHESTRATION.md"),
        "# ORCHESTRATION.md\nstub\n",
    )
    .unwrap();

    let out = run_rally(&root, &["init", "--json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let manifest = manifest_json(&root);
    let docs = manifest["docs"].clone();
    assert_eq!(docs["guide"], "RALLY.md", "docs={docs}");
    assert_eq!(docs["board"], "docs/ORCHESTRATION.md", "docs={docs}");
    for label in ["doctrine", "protocol", "any_agent_onboarding"] {
        assert!(
            docs.get(label).is_none(),
            "docs.{label} must be absent (its target was never created); docs={docs}"
        );
    }
    assert_eq!(manifest["pointer_docs_resolved"], 2);
    assert_eq!(manifest["pointer_docs_omitted"], 3);

    fs::remove_dir_all(&root).ok();
}

/// If `rally init` fails partway through — after `.rally/` has already been
/// created for the manifest write, but before the pointer-doc step
/// completes — it must not leave `.rally/` behind. We force a reproducible
/// failure by pre-creating `CLAUDE.md` as a DIRECTORY (not a file): the
/// manifest write succeeds first (docs are optional, so nothing there
/// fails), then the pointer-doc upsert step tries to `read_to_string` a
/// directory and errors. This is deterministic on any OS/user (unlike a
/// permissions-based failure, which silently no-ops when run as root).
#[test]
fn failed_init_leaves_no_rally_directory() {
    let root = tmp_dir("forced-failure");
    init_repo(&root);
    // Force upsert_pointer_in_doc("CLAUDE.md") to fail: it's a directory,
    // not a file, so `fs::read_to_string` on it returns an I/O error.
    fs::create_dir_all(root.join("CLAUDE.md")).unwrap();

    let out = run_rally(&root, &["init", "--json"]);
    assert!(
        !out.status.success(),
        "rally init must fail when CLAUDE.md is a directory\nstdout: {}",
        String::from_utf8_lossy(&out.stdout),
    );

    assert!(
        !root.join(".rally").exists(),
        "a failed rally init must not leave `.rally/` behind"
    );

    fs::remove_dir_all(&root).ok();
}

/// Sanity: agent-rally-point's own repo layout (all five docs present) still
/// gets full-fidelity manifest entries — this fix must not regress the
/// happy path. Simulated here rather than run against the live checkout
/// (never touch the live `.rally/`): a scratch repo seeded with all five
/// docs must behave identically to agent-rally-point's own tree.
#[test]
fn init_records_all_five_docs_when_all_five_are_present() {
    let root = tmp_dir("full-fidelity");
    init_repo(&root);
    fs::write(root.join("RALLY.md"), "# RALLY.md\n").unwrap();
    fs::create_dir_all(root.join("dynamic-workflows")).unwrap();
    fs::write(
        root.join("dynamic-workflows").join("COORDINATION.md"),
        "# COORDINATION.md\n",
    )
    .unwrap();
    fs::write(
        root.join("dynamic-workflows").join("PROTOCOL.md"),
        "# PROTOCOL.md\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(
        root.join("docs").join("ORCHESTRATION.md"),
        "# ORCHESTRATION.md\n",
    )
    .unwrap();
    fs::write(
        root.join("docs").join("ANY-AGENT-ONBOARDING.md"),
        "# ANY-AGENT-ONBOARDING.md\n",
    )
    .unwrap();

    let out = run_rally(&root, &["init", "--json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let manifest = manifest_json(&root);
    let docs = manifest["docs"].clone();
    assert_eq!(docs["guide"], "RALLY.md");
    assert_eq!(docs["doctrine"], "dynamic-workflows/COORDINATION.md");
    assert_eq!(docs["protocol"], "dynamic-workflows/PROTOCOL.md");
    assert_eq!(docs["board"], "docs/ORCHESTRATION.md");
    assert_eq!(docs["any_agent_onboarding"], "docs/ANY-AGENT-ONBOARDING.md");
    assert_eq!(manifest["pointer_docs_resolved"], 5);
    assert_eq!(manifest["pointer_docs_omitted"], 0);

    fs::remove_dir_all(&root).ok();
}
