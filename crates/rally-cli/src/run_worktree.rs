// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Per-agent linked-worktree provisioning for `rally run` (Phase 1b).
//!
//! The structural fix for the shared-branch hazard detected by
//! `worktree_guard.rs`: instead of every agent launching with `cwd =
//! <repo root>` on whatever branch happens to be checked out, each agent
//! gets its OWN linked git worktree on its OWN branch.  All agents still
//! share ONE coordination room because Rally resolves the room via the
//! git common-dir — see `git_common_repo_root` in `lib.rs` and the test
//! `linked_git_worktree_uses_common_room` in `tests/user_journey.rs`.
//!
//! # Layout
//! Worktrees live under `<repo-common-dir-parent>/.rally/worktrees/<session-id>/`
//! so they sit beside the existing `.rally/log/` ledger and share the
//! `.<toolname>/` storage convention.  `.rally/worktrees/` is hidden from
//! the user's tracked tree by git's normal ignore of nested worktree
//! folders (linked-worktree `.git` files are non-tracked).
//!
//! # Branch naming
//! `rally/<session-id>`.  Created off the run base (the current HEAD of
//! the canonical checkout — typically `main` or whatever branch the
//! caller had checked out when invoking `rally run`).
//!
//! # Fail-closed
//! `provision()` returns an error whenever it cannot create the worktree
//! — the caller is expected to surface that error rather than silently
//! launching the agent into the shared checkout.  The deliberate
//! opt-out is `--shared` / `--no-worktree` on `rally run`.
//!
//! # Cleanup
//! `cleanup()` removes the worktree directory and, if the per-agent
//! branch is empty (no unmerged commits), the branch.  If the branch
//! has unmerged commits, the worktree is removed but the branch is
//! retained and a `git bundle` is written next to the worktree path
//! before removal so no work is lost.  Always best-effort — a failure
//! to clean up never blocks `rally stop`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{RallyError, Result};

/// Outcome of a successful worktree provisioning.
#[derive(Clone, Debug)]
pub(crate) struct ProvisionedWorktree {
    /// Absolute filesystem path to the linked worktree (agent's `cwd`).
    pub(crate) path: PathBuf,
    /// Per-agent branch name (e.g. `rally/claude-reviewer-01`).
    pub(crate) branch: String,
}

/// Compute the directory under which all per-agent worktrees live for the
/// given coordination-room parent (i.e. the parent of `.rally/` — the
/// canonical-clone root).
pub(crate) fn worktrees_root(repo_root: &Path) -> PathBuf {
    repo_root.join(".rally").join("worktrees")
}

/// Compute the worktree path for a session WITHOUT creating it.
///
/// Used in dry-run mode so the envelope can advertise the planned path
/// without touching the filesystem.
pub(crate) fn planned_worktree_path(repo_root: &Path, session_id: &str) -> PathBuf {
    worktrees_root(repo_root).join(sanitize_session_id(session_id))
}

/// Compute the per-agent branch name for a session.
pub(crate) fn planned_branch_name(session_id: &str) -> String {
    format!("rally/{}", sanitize_session_id(session_id))
}

/// Provision a dedicated linked worktree for an agent session.
///
/// Side effects on success:
/// 1. `.rally/worktrees/` (parent dir) is created.
/// 2. `git worktree add -b <branch> <path> <base>` runs successfully.
///
/// Returns the absolute path of the worktree and the branch it lives on.
/// On failure, returns a `RallyError` describing what went wrong; the
/// caller MUST treat this as fail-closed and surface the error rather
/// than silently launching in the shared checkout.
pub(crate) fn provision(
    repo_root: &Path,
    session_id: &str,
    git_bin: &str,
) -> Result<ProvisionedWorktree> {
    let parent = worktrees_root(repo_root);
    std::fs::create_dir_all(&parent).map_err(|err| {
        RallyError::Message(format!(
            "rally run: could not create worktrees parent {}: {err}",
            parent.display()
        ))
    })?;

    let path = parent.join(sanitize_session_id(session_id));
    if path.exists() {
        return Err(RallyError::Message(format!(
            "rally run: worktree path {} already exists; refusing to clobber. \
Run `git worktree remove --force` against it first, or pick a different session id.",
            path.display()
        )));
    }
    let branch = planned_branch_name(session_id);
    let base = run_base(repo_root, git_bin).unwrap_or_else(|_| "HEAD".to_string());

    // `git worktree add -b <branch> <path> <base>` creates the branch off
    // <base> and checks it out into <path> in one shot. Fails if the
    // branch already exists — that's the safety we want.
    let output = Command::new(git_bin)
        .arg("-C")
        .arg(repo_root)
        .arg("worktree")
        .arg("add")
        .arg("-b")
        .arg(&branch)
        .arg(&path)
        .arg(&base)
        .output()
        .map_err(|err| {
            RallyError::Message(format!(
                "rally run: failed to invoke `{git_bin} worktree add`: {err}"
            ))
        })?;
    if !output.status.success() {
        return Err(RallyError::Message(format!(
            "rally run: `git worktree add` failed (status {}): {}. \
The default is per-agent worktree isolation; pass --shared to opt out.",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(ProvisionedWorktree { path, branch })
}

/// Outcome of a cleanup attempt; informational only.
///
/// Fields are surfaced for inspection by tests and for future logging
/// hooks (e.g. `rally stop --json` could echo them).  The current
/// production callers discard the outcome — cleanup is best-effort and
/// must not block `rally stop`.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) struct CleanupOutcome {
    /// True when the worktree directory was removed (or did not exist).
    pub(crate) worktree_removed: bool,
    /// True when the per-agent branch was deleted (it was fully merged
    /// into the run base or empty). False when the branch was retained
    /// because it carried unmerged commits.
    pub(crate) branch_deleted: bool,
    /// Optional path to a git bundle written before removal when the
    /// branch had unmerged work.
    pub(crate) bundle_path: Option<PathBuf>,
    /// Non-fatal warnings collected during cleanup.
    pub(crate) warnings: Vec<String>,
    /// True when the branch had unmerged work AND the bundle write failed.
    ///
    /// When this is true the caller MUST NOT count the worktree as reaped:
    /// skipping it preserves the unmerged work until the bundle problem is
    /// resolved.
    pub(crate) bundle_failed: bool,
}

/// Remove a per-agent worktree and its branch (when safe).
///
/// Bundle-before-remove: if the branch carries unmerged commits relative
/// to the run base, this writes `<worktree-path>.bundle` first so no
/// work is lost. Then `git worktree remove --force` removes the
/// worktree directory and (when safe) `git branch -d` removes the
/// branch.
///
/// This function is best-effort: errors are folded into the `warnings`
/// list rather than returned, so a stale leftover worktree never
/// blocks `rally stop` from completing.
pub(crate) fn cleanup(
    repo_root: &Path,
    worktree_path: &Path,
    branch: &str,
    git_bin: &str,
) -> CleanupOutcome {
    let mut warnings = Vec::new();
    let base = run_base(repo_root, git_bin).unwrap_or_else(|_| "HEAD".to_string());

    // 1. If the branch has unmerged commits, bundle before remove.
    //    Safety invariant (f3): if the bundle fails we must NOT remove the
    //    worktree — unmerged work would be permanently lost.  Set
    //    `bundle_failed = true` and return early so the GC caller can skip
    //    this candidate rather than counting it as reaped.
    let mut bundle_path = None;
    let bundle_failed: bool;
    let has_unmerged = branch_has_unmerged(repo_root, branch, &base, git_bin);
    if has_unmerged {
        let bundle = bundle_path_for(worktree_path);
        let bundle_result = Command::new(git_bin)
            .arg("-C")
            .arg(repo_root)
            .arg("bundle")
            .arg("create")
            .arg(&bundle)
            .arg(branch)
            .output();
        match bundle_result {
            Ok(out) if out.status.success() => {
                bundle_path = Some(bundle);
                bundle_failed = false;
            }
            Ok(out) => {
                let msg = format!(
                    "rally stop: bundle write for branch {branch} failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                warnings.push(msg);
                // Return immediately — do NOT remove unmerged work without a bundle.
                return CleanupOutcome {
                    worktree_removed: false,
                    branch_deleted: false,
                    bundle_path: None,
                    warnings,
                    bundle_failed: true,
                };
            }
            Err(err) => {
                warnings.push(format!("rally stop: could not invoke git bundle: {err}"));
                return CleanupOutcome {
                    worktree_removed: false,
                    branch_deleted: false,
                    bundle_path: None,
                    warnings,
                    bundle_failed: true,
                };
            }
        }
    } else {
        bundle_failed = false;
    }

    // 2. Remove the worktree directory.
    let mut worktree_removed = false;
    if worktree_path.exists() {
        let remove = Command::new(git_bin)
            .arg("-C")
            .arg(repo_root)
            .arg("worktree")
            .arg("remove")
            .arg("--force")
            .arg(worktree_path)
            .output();
        match remove {
            Ok(out) if out.status.success() => worktree_removed = true,
            Ok(out) => {
                warnings.push(format!(
                    "rally stop: `git worktree remove --force {}` failed: {}",
                    worktree_path.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
                // Last-resort fallback: rm -rf the directory and try a prune.
                if std::fs::remove_dir_all(worktree_path).is_ok() {
                    worktree_removed = true;
                    let _ = Command::new(git_bin)
                        .arg("-C")
                        .arg(repo_root)
                        .arg("worktree")
                        .arg("prune")
                        .output();
                }
            }
            Err(err) => warnings.push(format!(
                "rally stop: could not invoke git worktree remove: {err}"
            )),
        }
    } else {
        // Already gone — nothing to do.
        worktree_removed = true;
    }

    // 3. Delete the branch if it's empty (-d is safe; refuses on unmerged).
    let mut branch_deleted = false;
    if !has_unmerged {
        let delete = Command::new(git_bin)
            .arg("-C")
            .arg(repo_root)
            .arg("branch")
            .arg("-d")
            .arg(branch)
            .output();
        match delete {
            Ok(out) if out.status.success() => branch_deleted = true,
            Ok(out) => {
                // Not fatal — the branch may already be gone, or git may
                // disagree about its mergedness.  We retain it and warn.
                let stderr = String::from_utf8_lossy(&out.stderr);
                if stderr.contains("not found") || stderr.contains("did not match") {
                    branch_deleted = true; // already absent.
                } else {
                    warnings.push(format!(
                        "rally stop: `git branch -d {branch}` failed: {}",
                        stderr.trim()
                    ));
                }
            }
            Err(err) => warnings.push(format!("rally stop: could not invoke git branch -d: {err}")),
        }
    }

    CleanupOutcome {
        worktree_removed,
        branch_deleted,
        bundle_path,
        warnings,
        bundle_failed,
    }
}

/// Sanitize a session id for use as a path / branch component.
///
/// Keeps alphanumerics, `-`, `_`, and `.`; replaces everything else
/// with `-`. Defensive: session ids are already constrained upstream,
/// but a worktree path landing on disk deserves an explicit filter.
fn sanitize_session_id(session_id: &str) -> String {
    session_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

/// The git ref the run worktree branches off of.
///
/// We resolve in order: the canonical checkout's current HEAD branch,
/// then `main`, then `master`, then literal `HEAD`.  Using the
/// canonical checkout's HEAD lets a developer who has already moved
/// their checkout to a feature branch run a child agent off that
/// branch rather than off `main`.
fn run_base(repo_root: &Path, git_bin: &str) -> Result<String> {
    let head = git_output(repo_root, git_bin, &["symbolic-ref", "--short", "HEAD"]);
    if let Ok(value) = head {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    for candidate in ["main", "master"] {
        if git_output(
            repo_root,
            git_bin,
            &["rev-parse", "--verify", "--quiet", candidate],
        )
        .is_ok()
        {
            return Ok(candidate.to_string());
        }
    }
    Ok("HEAD".to_string())
}

fn branch_has_unmerged(repo_root: &Path, branch: &str, base: &str, git_bin: &str) -> bool {
    // `git rev-list <base>..<branch>` lists commits on branch not on base.
    // Empty output → branch is fully merged into base → safe to delete.
    let range = format!("{base}..{branch}");
    match git_output(repo_root, git_bin, &["rev-list", "--count", &range]) {
        Ok(stdout) => stdout.trim() != "0",
        Err(_) => {
            // If we can't compute mergedness, treat as unmerged (conservative
            // — never destroys work).
            true
        }
    }
}

fn bundle_path_for(worktree_path: &Path) -> PathBuf {
    let mut path = worktree_path.as_os_str().to_owned();
    path.push(".bundle");
    PathBuf::from(path)
}

fn git_output(repo_root: &Path, git_bin: &str, args: &[&str]) -> Result<String> {
    let owned_args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    let output = Command::new(git_bin)
        .arg("-C")
        .arg(repo_root)
        .args(&owned_args)
        .output()
        .map_err(|err| RallyError::Message(format!("invoke {git_bin}: {err}")))?;
    if !output.status.success() {
        return Err(RallyError::Message(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("rally-runwt-{label}-{nanos}"));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    fn init_test_repo(root: &Path) {
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
        run(&["config", "user.email", "rally@example.test"]);
        run(&["config", "user.name", "Rally Test"]);
        run(&["commit", "--allow-empty", "-m", "initial"]);
    }

    #[test]
    fn worktrees_root_lives_under_dot_rally() {
        let repo = tmp_dir("layout");
        let got = worktrees_root(&repo);
        assert_eq!(got, repo.join(".rally").join("worktrees"));
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn planned_branch_name_uses_rally_prefix() {
        assert_eq!(
            planned_branch_name("claude-reviewer-01"),
            "rally/claude-reviewer-01"
        );
    }

    #[test]
    fn sanitize_replaces_unsafe_chars() {
        assert_eq!(sanitize_session_id("ab cd/ef"), "ab-cd-ef");
        assert_eq!(sanitize_session_id("claude_01.test"), "claude_01.test");
    }

    #[test]
    fn provision_creates_linked_worktree_on_new_branch() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let repo = tmp_dir("provision-creates");
        init_test_repo(&repo);
        let pw = provision(&repo, "claude-reviewer-01", "git").expect("provision");
        assert!(pw.path.exists(), "worktree dir must exist");
        assert!(
            pw.path.join(".git").exists(),
            "worktree must carry a .git pointer"
        );
        assert_eq!(pw.branch, "rally/claude-reviewer-01");

        // The worktree's HEAD must be the new branch.
        let head = git_output(&pw.path, "git", &["symbolic-ref", "--short", "HEAD"])
            .expect("symbolic-ref");
        assert_eq!(head.trim(), "rally/claude-reviewer-01");

        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn provision_fails_when_path_exists() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let repo = tmp_dir("provision-clobber");
        init_test_repo(&repo);
        // Pre-populate the would-be worktree path so provision() must refuse.
        let path = planned_worktree_path(&repo, "claude-reviewer-01");
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("placeholder"), b"x").unwrap();

        let err = provision(&repo, "claude-reviewer-01", "git").expect_err("must refuse clobber");
        assert!(
            err.to_string().contains("already exists"),
            "expected refusal message; got: {err}"
        );

        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn cleanup_removes_empty_worktree_and_branch() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let repo = tmp_dir("cleanup-empty");
        init_test_repo(&repo);
        let pw = provision(&repo, "claude-reviewer-01", "git").expect("provision");
        let outcome = cleanup(&repo, &pw.path, &pw.branch, "git");

        assert!(outcome.worktree_removed);
        assert!(outcome.branch_deleted);
        assert!(outcome.bundle_path.is_none(), "no bundle for empty branch");
        assert!(!pw.path.exists(), "worktree dir must be gone after cleanup");
        // Branch must no longer be present.
        let exists = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["rev-parse", "--verify", "--quiet", &pw.branch])
            .output()
            .unwrap();
        assert!(
            !exists.status.success(),
            "branch should be deleted after cleanup of empty branch"
        );

        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn cleanup_bundles_and_retains_unmerged_branch() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let repo = tmp_dir("cleanup-unmerged");
        init_test_repo(&repo);
        let pw = provision(&repo, "claude-reviewer-01", "git").expect("provision");

        // Add a commit on the per-agent branch to make it unmerged.
        fs::write(pw.path.join("note.txt"), b"work in progress").unwrap();
        let run = |cwd: &Path, args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{} {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&pw.path, &["add", "note.txt"]);
        run(&pw.path, &["commit", "-m", "wip"]);

        let outcome = cleanup(&repo, &pw.path, &pw.branch, "git");

        assert!(outcome.worktree_removed);
        assert!(
            !outcome.branch_deleted,
            "unmerged branch must be retained, not deleted"
        );
        assert!(
            outcome.bundle_path.is_some(),
            "must bundle before remove when work is unmerged"
        );
        let bundle = outcome.bundle_path.unwrap();
        assert!(bundle.exists(), "bundle file must exist on disk");

        // Branch must still be present.
        let exists = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["rev-parse", "--verify", "--quiet", &pw.branch])
            .output()
            .unwrap();
        assert!(
            exists.status.success(),
            "unmerged branch should still be present after cleanup"
        );

        fs::remove_dir_all(&repo).ok();
    }
}
