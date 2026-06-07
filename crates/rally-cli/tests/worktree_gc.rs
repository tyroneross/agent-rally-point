// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `rally worktree gc`.
//!
//! All tests operate on ephemeral git repos in the OS temp dir — they never
//! touch the live agent-rally-point checkout.
//!
//! # Conventions
//! * `init_repo(root)` — bare minimum init: empty commit on `main`.
//! * `make_rally_worktree(repo, session_id)` — provisions a worktree via
//!   `run_worktree::provision`, paralleling what `rally run` does.
//! * `make_rally_worktree_with_commit(...)` — adds one commit so the branch
//!   is unmerged.
//! * All tests gate on `git_available()` so they skip cleanly in sandboxed
//!   CI that has no git binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tmp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("rally-gctest-{label}-{nanos}"));
    fs::create_dir_all(&p).unwrap();
    // Canonicalize so macOS /var → /private/var symlink is resolved and paths
    // match what `git worktree list --porcelain` returns.
    p.canonicalize().unwrap_or(p)
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Initialise a bare-minimum git repo: `git init -b main` + initial empty commit.
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
    run(&["config", "user.email", "gc-test@example.test"]);
    run(&["config", "user.name", "GC Test"]);
    run(&["commit", "--allow-empty", "-m", "initial"]);
}

/// Provision a rally-managed worktree (`rally/<session-id>`) using the same
/// `run_worktree::provision` call that `rally run` uses — so tests exercise
/// the real code path.
///
/// Returns the CANONICAL path (symlink-resolved) so assertions match what
/// `git worktree list --porcelain` returns on macOS (/private/var vs /var).
fn make_rally_worktree(repo: &Path, session_id: &str) -> PathBuf {
    fs::create_dir_all(repo.join(".rally").join("worktrees")).unwrap();
    let wt_path = repo.join(".rally").join("worktrees").join(session_id);
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "add", "-b"])
        .arg(format!("rally/{session_id}"))
        .arg(&wt_path)
        .arg("HEAD")
        .output()
        .expect("git worktree add");
    assert!(
        out.status.success(),
        "git worktree add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Canonicalize to match what git returns in --porcelain output.
    wt_path.canonicalize().unwrap_or(wt_path)
}

/// Add one file-commit in the worktree's branch so it is unmerged relative to main.
fn add_commit_in_worktree(wt_path: &Path) {
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(wt_path)
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    fs::write(wt_path.join("wip.txt"), b"work in progress").unwrap();
    run(&["add", "wip.txt"]);
    run(&["commit", "-m", "wip"]);
}

/// Merge a branch into `main` in the canonical repo.
fn merge_branch_into_main(repo: &Path, branch: &str) {
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    // Switch canonical to main first (it should already be there), then merge.
    // The canonical repo was never checked out to any branch in the worktree
    // tests — HEAD stays on main.
    run(&["merge", "--no-ff", branch, "-m", &format!("Merge {branch}")]);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A merged worktree (branch fully merged into main) must be listed as a
/// reap candidate on dry-run and must actually be removed on `--apply`.
#[test]
fn merged_worktree_is_reaped_on_apply_listed_on_dry_run() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let repo = tmp_dir("merged-reap");
    init_repo(&repo);
    let wt = make_rally_worktree(&repo, "claude-protocol-claude-01");
    // Add a commit, then merge it so the branch is fully merged.
    add_commit_in_worktree(&wt);
    merge_branch_into_main(&repo, "rally/claude-protocol-claude-01");

    // --- dry-run: worktree must still exist, candidate must be listed ---
    let report = rally_cli::worktree_gc::run_gc(rally_cli::worktree_gc::GcConfig {
        repo_root: repo.clone(),
        apply: false,
        ttl_secs: 24 * 3600,
        now_ts: None,
        presence_facts: vec![],
        git_bin: "git".to_string(),
        backend_liveness_probe: None,
    });
    assert!(report.is_ok(), "dry-run must not error: {:?}", report);
    let report = report.unwrap();
    assert!(
        report.candidates.iter().any(|c| c.worktree_path == wt),
        "merged worktree must appear as candidate in dry-run; candidates={:?}",
        report.candidates
    );
    // Dry-run: nothing removed.
    assert!(
        report.reaped.is_empty(),
        "dry-run must not reap anything; reaped={:?}",
        report.reaped
    );
    assert!(wt.exists(), "dry-run must leave worktree intact");

    // --- apply: worktree is removed ---
    let report = rally_cli::worktree_gc::run_gc(rally_cli::worktree_gc::GcConfig {
        repo_root: repo.clone(),
        apply: true,
        ttl_secs: 24 * 3600,
        now_ts: None,
        presence_facts: vec![],
        git_bin: "git".to_string(),
        backend_liveness_probe: None,
    });
    assert!(report.is_ok(), "apply must not error: {:?}", report);
    let report = report.unwrap();
    assert!(
        !report.reaped.is_empty(),
        "apply must have reaped at least one worktree"
    );
    assert!(!wt.exists(), "worktree directory must be gone after --apply");

    fs::remove_dir_all(&repo).ok();
}

/// An unmerged worktree whose owning agent has LIVE presence (fresh heartbeat)
/// must NEVER be reaped, even with `--apply`.
#[test]
fn unmerged_with_live_owner_is_never_reaped() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let repo = tmp_dir("live-owner");
    init_repo(&repo);
    let wt = make_rally_worktree(&repo, "claude-review-01");
    add_commit_in_worktree(&wt); // make it unmerged

    // Simulate a FRESH heartbeat for the owner tool derived from this branch.
    // Branch = rally/claude-review-01 → agent prefix = claude.
    // We supply a presence fact for tool "claude" with a timestamp that is
    // within the default TTL (24h).
    let now_ts = "2026-06-07T12:00:00Z";
    let fresh_ts = "2026-06-07T11:58:00Z"; // 2 minutes ago — live

    let facts = vec![make_presence_fact("claude", 1, "state=idle", fresh_ts)];

    let report = rally_cli::worktree_gc::run_gc(rally_cli::worktree_gc::GcConfig {
        repo_root: repo.clone(),
        apply: true,
        ttl_secs: 24 * 3600,
        now_ts: Some(now_ts.to_string()),
        presence_facts: facts,
        git_bin: "git".to_string(),
        backend_liveness_probe: None,
    });
    assert!(report.is_ok(), "must not error: {:?}", report);
    let report = report.unwrap();

    assert!(
        report.reaped.is_empty(),
        "live-owner unmerged worktree must never be reaped; reaped={:?}",
        report.reaped
    );
    assert!(wt.exists(), "worktree must still exist");

    // Confirm the candidate was skipped and the skip reason mentions liveness.
    let skipped = report
        .skipped
        .iter()
        .find(|s| s.worktree_path == wt);
    assert!(
        skipped.is_some(),
        "unmerged+live must appear in skipped list"
    );
    let skip_reason = &skipped.unwrap().reason;
    assert!(
        skip_reason.to_lowercase().contains("live"),
        "skip reason must mention live owner; got: {skip_reason}"
    );

    fs::remove_dir_all(&repo).ok();
}

/// An unmerged worktree whose owning agent has STALE presence (last heartbeat
/// older than TTL) must be bundled then reaped.
#[test]
fn unmerged_with_stale_owner_is_bundled_then_reaped() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let repo = tmp_dir("stale-owner");
    init_repo(&repo);
    let wt = make_rally_worktree(&repo, "codex-claim-authority-01");
    add_commit_in_worktree(&wt); // make it unmerged

    // Stale heartbeat: 48 hours ago, TTL = 24h → stale.
    let now_ts = "2026-06-07T12:00:00Z";
    let stale_ts = "2026-06-05T12:00:00Z"; // 2 days ago

    let facts = vec![make_presence_fact("codex", 1, "state=idle", stale_ts)];

    // Backend probe: returns true (= backend is DEAD) so the f2 gate lets the
    // reap proceed for this stale unmerged worktree.
    let dead_probe: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(|_| true);

    let report = rally_cli::worktree_gc::run_gc(rally_cli::worktree_gc::GcConfig {
        repo_root: repo.clone(),
        apply: true,
        ttl_secs: 24 * 3600,
        now_ts: Some(now_ts.to_string()),
        presence_facts: facts,
        git_bin: "git".to_string(),
        backend_liveness_probe: Some(dead_probe),
    });
    assert!(report.is_ok(), "must not error: {:?}", report);
    let report = report.unwrap();

    assert!(
        !report.reaped.is_empty(),
        "stale-owner unmerged worktree must be reaped; reaped={:?}",
        report.reaped
    );
    assert!(!wt.exists(), "worktree directory must be gone");

    // The branch was unmerged, so cleanup() must have produced a bundle.
    assert!(
        !report.bundles.is_empty(),
        "a bundle must have been created for the unmerged work; bundles={:?}",
        report.bundles
    );

    fs::remove_dir_all(&repo).ok();
}

/// The default branch (main) and the current worktree (cwd) must never be
/// touched regardless of flags.
#[test]
fn default_branch_and_cwd_worktree_are_never_reaped() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let repo = tmp_dir("never-reap");
    init_repo(&repo);

    // No rally-managed worktrees at all.
    let report = rally_cli::worktree_gc::run_gc(rally_cli::worktree_gc::GcConfig {
        repo_root: repo.clone(),
        apply: true,
        ttl_secs: 24 * 3600,
        now_ts: None,
        presence_facts: vec![],
        git_bin: "git".to_string(),
        backend_liveness_probe: None,
    });
    assert!(report.is_ok());
    let report = report.unwrap();
    assert!(
        report.reaped.is_empty(),
        "must not reap the canonical checkout or default branch; reaped={:?}",
        report.reaped
    );

    fs::remove_dir_all(&repo).ok();
}

/// Dry-run must make absolutely zero filesystem or git changes.
/// After dry-run, the worktree directory must still be present.
#[test]
fn dry_run_makes_no_filesystem_changes() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let repo = tmp_dir("dry-run-noop");
    init_repo(&repo);
    let wt = make_rally_worktree(&repo, "claude-x-01");
    // Merged branch → would be a reap candidate.
    add_commit_in_worktree(&wt);
    merge_branch_into_main(&repo, "rally/claude-x-01");

    // Snapshot branch list before.
    let branches_before = list_branches(&repo);

    let report = rally_cli::worktree_gc::run_gc(rally_cli::worktree_gc::GcConfig {
        repo_root: repo.clone(),
        apply: false, // DRY-RUN
        ttl_secs: 24 * 3600,
        now_ts: None,
        presence_facts: vec![],
        git_bin: "git".to_string(),
        backend_liveness_probe: None,
    });
    assert!(report.is_ok());
    let report = report.unwrap();
    assert!(
        report.reaped.is_empty(),
        "dry-run must not reap; reaped={:?}",
        report.reaped
    );

    // Worktree dir must still exist.
    assert!(wt.exists(), "dry-run must not remove the worktree directory");

    // Branch list must be unchanged.
    let branches_after = list_branches(&repo);
    assert_eq!(
        branches_before, branches_after,
        "dry-run must not modify any branches"
    );

    fs::remove_dir_all(&repo).ok();
}

/// `--ttl` boundary: a worktree whose owner's last heartbeat is older than
/// the TTL is stale and reapable; one that is newer is live and must be kept.
///
/// Uses distinct agent-type prefixes (claude vs codex) so the owner-prefix
/// extraction unambiguously maps each session to one tool.
#[test]
fn ttl_boundary_respected() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let repo = tmp_dir("ttl-boundary");
    init_repo(&repo);
    // Session IDs: agent prefix is the first `-`-delimited token.
    // "claude-task-01" → prefix "claude" (stale).
    // "codex-task-01"  → prefix "codex"  (live).
    let wt_stale = make_rally_worktree(&repo, "claude-task-01");
    let wt_live = make_rally_worktree(&repo, "codex-task-01");
    add_commit_in_worktree(&wt_stale);
    add_commit_in_worktree(&wt_live);

    let now_ts = "2026-06-07T12:00:00Z";
    // TTL = 1 hour (3600s).
    // claude: 2 hours old → stale → reaped.
    // codex:  30 minutes old → live → kept.
    let stale_ts = "2026-06-07T10:00:00Z"; // 2h ago
    let live_ts = "2026-06-07T11:30:00Z";  // 30m ago

    let facts = vec![
        make_presence_fact("claude", 1, "state=idle", stale_ts),
        make_presence_fact("codex", 2, "state=idle", live_ts),
    ];

    // Backend probe: returns true (dead) so the f2 gate lets the stale
    // claude worktree reap. The live codex worktree never reaches the probe
    // (it's skipped earlier at the live-owner check).
    let dead_probe: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(|_| true);

    let report = rally_cli::worktree_gc::run_gc(rally_cli::worktree_gc::GcConfig {
        repo_root: repo.clone(),
        apply: true,
        ttl_secs: 3600, // 1 hour TTL
        now_ts: Some(now_ts.to_string()),
        presence_facts: facts,
        git_bin: "git".to_string(),
        backend_liveness_probe: Some(dead_probe),
    });
    assert!(report.is_ok(), "must not error: {:?}", report);
    let report = report.unwrap();

    // claude's worktree is stale + unmerged → reaped.
    assert!(
        report.reaped.iter().any(|r| r.worktree_path == wt_stale),
        "stale worktree must be reaped; reaped={:?}",
        report.reaped
    );
    // codex's worktree is live → kept.
    assert!(
        report.reaped.iter().all(|r| r.worktree_path != wt_live),
        "live worktree must NOT be reaped; reaped={:?}",
        report.reaped
    );
    assert!(wt_live.exists(), "live worktree directory must still exist");

    fs::remove_dir_all(&repo).ok();
}

// ---------------------------------------------------------------------------
// f1 — live_owner_hyphen_underscore_mismatch_not_reaped
// ---------------------------------------------------------------------------

/// f1 safety fix: branch `rally/claude-code-task-01` derives owner prefix
/// `claude-code` (hyphens).  The tool registered in the room is
/// `claude_code:01` (underscores).  Before the fix, none of the three
/// predicates in `is_owner_live` matched, so a LIVE owner was falsely
/// treated as absent/stale → the worktree was reaped.  After normalizing
/// both sides (replace '-' → '_'), the match succeeds → not reaped.
#[test]
fn live_owner_hyphen_underscore_mismatch_not_reaped() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let repo = tmp_dir("f1-hyphen-underscore");
    init_repo(&repo);
    // Session id uses the multi-word hyphenated form — derive_owner_prefix
    // returns "claude-code" (hyphenated).
    let wt = make_rally_worktree(&repo, "claude-code-task-01");
    add_commit_in_worktree(&wt); // unmerged

    let now_ts = "2026-06-07T12:00:00Z";
    let fresh_ts = "2026-06-07T11:58:00Z"; // 2 min ago — within any TTL

    // Tool name uses underscores, as registered by `rally run claude`
    // (AgentSpec.tool = "claude_code").  This is the mismatch f1 fixes.
    let facts = vec![make_presence_fact("claude_code:01", 1, "state=idle", fresh_ts)];

    let report = rally_cli::worktree_gc::run_gc(rally_cli::worktree_gc::GcConfig {
        repo_root: repo.clone(),
        apply: true,
        ttl_secs: 24 * 3600,
        now_ts: Some(now_ts.to_string()),
        presence_facts: facts,
        git_bin: "git".to_string(),
        backend_liveness_probe: None,
    });
    assert!(report.is_ok(), "must not error: {:?}", report);
    let report = report.unwrap();

    // The owner is LIVE — the worktree must NOT be reaped.
    assert!(
        report.reaped.is_empty(),
        "f1: live claude_code:01 must protect rally/claude-code-task-01 from reaping; \
         reaped={:?}",
        report.reaped
    );
    assert!(wt.exists(), "worktree directory must still exist");

    // Confirm it landed in skipped with a live-related reason.
    let skipped = report.skipped.iter().find(|s| s.worktree_path == wt);
    assert!(
        skipped.is_some(),
        "f1: worktree must appear in skipped list; skipped={:?}",
        report.skipped
    );
    let reason = &skipped.unwrap().reason;
    assert!(
        reason.to_lowercase().contains("live"),
        "f1: skip reason must mention live owner; got: {reason}"
    );

    fs::remove_dir_all(&repo).ok();
}

// ---------------------------------------------------------------------------
// f2 — backend_live_stale_by_ttl_not_reaped
// ---------------------------------------------------------------------------

/// f2 safety fix: a worktree whose owner is TTL-stale (no recent heartbeat)
/// but whose backing tmux/cmux session is still live according to the backend
/// probe must NOT be reaped.  The probe returning `false` (backend alive)
/// causes the GC to skip the worktree.
#[test]
fn backend_live_stale_by_ttl_not_reaped() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let repo = tmp_dir("f2-backend-live");
    init_repo(&repo);
    let wt = make_rally_worktree(&repo, "codex-long-run-01");
    add_commit_in_worktree(&wt); // unmerged

    // Agent is stale by TTL — no heartbeat for 48 h.
    let now_ts = "2026-06-07T12:00:00Z";
    let stale_ts = "2026-06-05T12:00:00Z";
    let facts = vec![make_presence_fact("codex", 1, "state=idle", stale_ts)];

    // Backend probe: returns false (= backend IS alive, NOT dead) for this
    // session so the GC must skip it.
    let probe: Arc<dyn Fn(&str) -> bool + Send + Sync> =
        Arc::new(|session_id: &str| -> bool {
            // `false` = backend is live (session exists); reaper must skip.
            assert!(
                session_id == "codex-long-run-01",
                "probe called with unexpected session_id: {session_id}"
            );
            false // backend alive → NOT dead
        });

    let report = rally_cli::worktree_gc::run_gc(rally_cli::worktree_gc::GcConfig {
        repo_root: repo.clone(),
        apply: true,
        ttl_secs: 24 * 3600,
        now_ts: Some(now_ts.to_string()),
        presence_facts: facts,
        git_bin: "git".to_string(),
        backend_liveness_probe: Some(probe),
    });
    assert!(report.is_ok(), "must not error: {:?}", report);
    let report = report.unwrap();

    assert!(
        report.reaped.is_empty(),
        "f2: backend-live worktree must not be reaped even when TTL-stale; \
         reaped={:?}",
        report.reaped
    );
    assert!(wt.exists(), "worktree directory must still exist");

    let skipped = report.skipped.iter().find(|s| s.worktree_path == wt);
    assert!(
        skipped.is_some(),
        "f2: worktree must appear in skipped list; skipped={:?}",
        report.skipped
    );
    let reason = &skipped.unwrap().reason;
    assert!(
        reason.to_lowercase().contains("live"),
        "f2: skip reason must mention live backend; got: {reason}"
    );

    fs::remove_dir_all(&repo).ok();
}

// ---------------------------------------------------------------------------
// f3 — bundle_failure_skips_unmerged_worktree
// ---------------------------------------------------------------------------

/// f3 safety fix: when `git bundle` fails (e.g. a fake git that always fails),
/// the worktree must NOT be removed and must appear in `skipped`, not `reaped`,
/// with a warning surfaced.  This prevents silent data loss when the safety
/// bundle cannot be written.
#[test]
fn bundle_failure_skips_unmerged_worktree() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let repo = tmp_dir("f3-bundle-fail");
    init_repo(&repo);
    let wt = make_rally_worktree(&repo, "claude-bundle-fail-01");
    add_commit_in_worktree(&wt); // unmerged — would require a bundle

    // Write a fake `git` that fails every `bundle create` but succeeds for
    // all other git commands (by delegating to the real git).
    let fake_git_dir = tmp_dir("fake-git-bin");
    let fake_git = fake_git_dir.join("git");
    let real_git = Command::new("which")
        .arg("git")
        .output()
        .expect("which git")
        .stdout;
    let real_git_path = String::from_utf8_lossy(&real_git).trim().to_string();
    // Shell wrapper: intercept `git bundle create` and exit 1; delegate rest.
    let script = format!(
        "#!/bin/sh\nif echo \"$*\" | grep -q 'bundle create'; then\n  echo 'fake git: bundle create disabled' >&2\n  exit 1\nfi\nexec {real_git_path} \"$@\"\n"
    );
    fs::write(&fake_git, script.as_bytes()).unwrap();
    // Make executable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let stale_ts = "2026-06-05T12:00:00Z"; // 2 days ago — stale
    let now_ts = "2026-06-07T12:00:00Z";
    let facts = vec![make_presence_fact("claude", 1, "state=idle", stale_ts)];

    // Backend probe: returns true (dead) so f2 lets the stale worktree
    // proceed to cleanup(), where the fake git's bundle-fail triggers f3.
    let dead_probe: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(|_| true);

    let report = rally_cli::worktree_gc::run_gc(rally_cli::worktree_gc::GcConfig {
        repo_root: repo.clone(),
        apply: true,
        ttl_secs: 24 * 3600,
        now_ts: Some(now_ts.to_string()),
        presence_facts: facts,
        git_bin: fake_git.to_string_lossy().to_string(),
        backend_liveness_probe: Some(dead_probe),
    });
    assert!(report.is_ok(), "must not error: {:?}", report);
    let report = report.unwrap();

    // Worktree must still exist on disk (not removed).
    assert!(
        wt.exists(),
        "f3: worktree must still exist when bundle fails; path={:?}",
        wt
    );
    // Must NOT appear in reaped.
    assert!(
        report.reaped.iter().all(|r| r.worktree_path != wt),
        "f3: worktree with failed bundle must not be counted as reaped; reaped={:?}",
        report.reaped
    );
    // Must appear in skipped.
    let skipped = report.skipped.iter().find(|s| s.worktree_path == wt);
    assert!(
        skipped.is_some(),
        "f3: worktree must appear in skipped list; skipped={:?}",
        report.skipped
    );
    // A warning must be emitted.
    assert!(
        !report.warnings.is_empty(),
        "f3: a warning must be emitted when bundle fails; warnings={:?}",
        report.warnings
    );

    fs::remove_dir_all(&repo).ok();
    fs::remove_dir_all(&fake_git_dir).ok();
}

// ---------------------------------------------------------------------------
// f4 — gc_from_linked_worktree_protects_cwd
// ---------------------------------------------------------------------------

/// f4 safety fix: when `rally worktree gc` runs with `repo_root` pointing at
/// the canonical checkout but the PROCESS CWD is a linked worktree, that
/// linked worktree must be protected (never reaped).  The original code only
/// resolved `--show-toplevel` via `-C repo_root`, so a linked worktree that
/// happened to be the process cwd was not protected.
///
/// We simulate this by setting the process cwd via `std::env::set_current_dir`
/// to the linked worktree and then invoking gc.
#[test]
fn gc_from_linked_worktree_protects_cwd() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let repo = tmp_dir("f4-cwd-protect");
    init_repo(&repo);
    // Create the linked worktree that will be "our" cwd.
    let linked_wt = make_rally_worktree(&repo, "claude-linked-cwd-01");
    add_commit_in_worktree(&linked_wt); // give it an unmerged commit so it's a candidate

    // Change the process cwd to the linked worktree.
    let original_cwd = std::env::current_dir().expect("current_dir");
    std::env::set_current_dir(&linked_wt).expect("set_current_dir to linked worktree");

    let now_ts = "2026-06-07T12:00:00Z";
    let stale_ts = "2026-06-05T12:00:00Z"; // owner stale — would reap without f4
    let facts = vec![make_presence_fact("claude", 1, "state=idle", stale_ts)];

    let report = rally_cli::worktree_gc::run_gc(rally_cli::worktree_gc::GcConfig {
        repo_root: repo.clone(),
        apply: true,
        ttl_secs: 24 * 3600,
        now_ts: Some(now_ts.to_string()),
        presence_facts: facts,
        git_bin: "git".to_string(),
        backend_liveness_probe: None,
    });

    // Restore cwd unconditionally.
    std::env::set_current_dir(&original_cwd).ok();

    assert!(report.is_ok(), "must not error: {:?}", report);
    let report = report.unwrap();

    // The linked worktree (our cwd) must NOT be reaped.
    assert!(
        report.reaped.iter().all(|r| r.worktree_path != linked_wt),
        "f4: linked worktree that is the process cwd must not be reaped; reaped={:?}",
        report.reaped
    );
    assert!(
        linked_wt.exists(),
        "f4: cwd linked worktree must still exist after gc"
    );

    // It must appear in skipped with a protection reason.
    let skipped = report.skipped.iter().find(|s| s.worktree_path == linked_wt);
    assert!(
        skipped.is_some(),
        "f4: cwd worktree must appear in skipped list; skipped={:?}",
        report.skipped
    );
    let reason = &skipped.unwrap().reason;
    assert!(
        reason.to_lowercase().contains("current"),
        "f4: skip reason must mention 'current' worktree; got: {reason}"
    );

    fs::remove_dir_all(&repo).ok();
}

// ---------------------------------------------------------------------------
// Helpers used by tests
// ---------------------------------------------------------------------------

fn make_presence_fact(
    tool: &str,
    seq: i64,
    subject: &str,
    created_at: &str,
) -> rally_cli::worktree_gc::PresenceFact {
    rally_cli::worktree_gc::PresenceFact {
        tool: tool.to_string(),
        seq,
        subject: subject.to_string(),
        created_at: created_at.to_string(),
    }
}

fn list_branches(repo: &Path) -> Vec<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["branch", "--format=%(refname:short)"])
        .output()
        .expect("git branch");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .tap_mut(|v| v.sort())
}

trait TapMut {
    fn tap_mut(self, f: impl FnOnce(&mut Self)) -> Self;
}

impl<T> TapMut for T {
    fn tap_mut(mut self, f: impl FnOnce(&mut Self)) -> Self {
        f(&mut self);
        self
    }
}
