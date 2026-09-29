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
//! `cleanup()` retains dirty or unmerged worktrees for explicit review.
//! It only removes clean worktrees whose branch is merged into a known target.

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
    /// The ref selected at creation, independent of the caller's later HEAD.
    pub(crate) merge_target_ref: Option<String>,
    pub(crate) merge_target_commit: String,
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
    let merge_target_ref = (base != "HEAD").then(|| base.clone());
    let merge_target_commit = git_output(repo_root, git_bin, &["rev-parse", &base])?;

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
    Ok(ProvisionedWorktree {
        path,
        branch,
        merge_target_ref,
        merge_target_commit: merge_target_commit.trim().to_string(),
    })
}

/// Outcome of a cleanup attempt; informational only.
///
/// Fields are surfaced for inspection by tests and for future logging
/// hooks (e.g. `rally stop --json` could echo them).  The current
/// production callers discard the outcome — cleanup is best-effort and
/// must not block `rally stop`.
#[derive(Clone, Debug)]
pub(crate) struct CleanupOutcome {
    /// True when the worktree directory was removed (or did not exist).
    pub(crate) worktree_removed: bool,
    /// True when the per-agent branch was deleted (it was fully merged
    /// into the run base or empty). False when the branch was retained
    /// because it carried unmerged commits.
    pub(crate) branch_deleted: bool,
    /// Optional safety bundle. Implicit cleanup now retains unmerged source,
    /// so this is always `None` and remains only for disposition compatibility.
    pub(crate) bundle_path: Option<PathBuf>,
    /// Non-fatal warnings collected during cleanup.
    pub(crate) warnings: Vec<String>,
    pub(crate) merged: Option<bool>,
    pub(crate) dirty: Option<bool>,
    pub(crate) reason: &'static str,
}

/// Remove a per-agent worktree and its branch (when safe).
///
/// Dirty or unmerged worktrees retain their checkout and recovery path.
/// Then non-forcing `git worktree remove` performs Git's own final dirtiness
/// check before removing the worktree directory, and (when safe) `git branch
/// -d` removes the branch.
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
    let base = run_base(repo_root, git_bin)
        .ok()
        .filter(|base| base != "HEAD");
    cleanup_against(repo_root, worktree_path, branch, base.as_deref(), git_bin)
}

/// Implicit lifecycle cleanup uses the recorded creation target. Legacy sessions
/// without that ref retain their source rather than guessing from a moving HEAD.
pub(crate) fn cleanup_against(
    repo_root: &Path,
    worktree_path: &Path,
    branch: &str,
    target_ref: Option<&str>,
    git_bin: &str,
) -> CleanupOutcome {
    let mut warnings = Vec::new();

    // A missing directory may still be registered as a prunable Git worktree.
    // Implicit cleanup never claims it was removed or deletes its branch.
    if !worktree_path.exists() {
        warnings.push(format!("rally stop: worktree path {} is missing; inspect Git registration before explicit prune", worktree_path.display()));
        return CleanupOutcome {
            worktree_removed: false,
            branch_deleted: false,
            bundle_path: None,
            warnings,
            merged: None,
            dirty: None,
            reason: "path_missing_registration_unverified",
        };
    }

    // Never force-remove a dirty worktree. A bundle protects commits, but it
    // cannot preserve modified or untracked files. If status cannot be read,
    // retain the worktree as the conservative recovery path.
    //
    // Gitignored files are also retained: agents keep local memory, run state
    // and env files there, and non-forcing `git worktree remove` deletes them
    // without complaint. They are listed for review, never removed.
    if worktree_path.exists() {
        let status = Command::new(git_bin)
            .arg("-C")
            .arg(worktree_path)
            .args(["status", "--porcelain", "--untracked-files=all", "--ignored"])
            .output();
        let ignored_only = |stdout: &[u8]| {
            let text = String::from_utf8_lossy(stdout);
            let mut lines = text.lines().filter(|line| !line.is_empty()).peekable();
            lines.peek().is_some() && lines.all(|line| line.starts_with("!! "))
        };
        match status {
            Ok(out) if out.status.success() && out.stdout.is_empty() => {}
            Ok(out) if out.status.success() && ignored_only(&out.stdout) => {
                let text = String::from_utf8_lossy(&out.stdout);
                let ignored: Vec<&str> = text
                    .lines()
                    .filter_map(|line| line.strip_prefix("!! "))
                    .collect();
                let sample = ignored.iter().take(5).copied().collect::<Vec<_>>().join(", ");
                let more = ignored.len().saturating_sub(5);
                warnings.push(format!(
                    "rally stop: retained worktree {} because it holds {} gitignored path(s) for review: {sample}{}",
                    worktree_path.display(),
                    ignored.len(),
                    if more > 0 { format!(" (+{more} more)") } else { String::new() }
                ));
                return CleanupOutcome {
                    worktree_removed: false,
                    branch_deleted: false,
                    bundle_path: None,
                    warnings,
                    merged: None,
                    dirty: Some(false),
                    reason: "ignored_files",
                };
            }
            Ok(out) if out.status.success() => {
                warnings.push(format!(
                    "rally stop: retained dirty worktree for recovery at {}",
                    worktree_path.display()
                ));
                return CleanupOutcome {
                    worktree_removed: false,
                    branch_deleted: false,
                    bundle_path: None,
                    warnings,
                    merged: None,
                    dirty: Some(true),
                    reason: "dirty",
                };
            }
            Ok(out) => {
                warnings.push(format!(
                    "rally stop: could not inspect worktree {}; retained it for recovery: {}",
                    worktree_path.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
                return CleanupOutcome {
                    worktree_removed: false,
                    branch_deleted: false,
                    bundle_path: None,
                    warnings,
                    merged: None,
                    dirty: None,
                    reason: "inspection_failed",
                };
            }
            Err(err) => {
                warnings.push(format!(
                    "rally stop: could not inspect worktree {}; retained it for recovery: {err}",
                    worktree_path.display()
                ));
                return CleanupOutcome {
                    worktree_removed: false,
                    branch_deleted: false,
                    bundle_path: None,
                    warnings,
                    merged: None,
                    dirty: None,
                    reason: "inspection_failed",
                };
            }
        }
    }
    let Some(base) = target_ref else {
        warnings.push(format!(
            "rally stop: no recorded merge target for {}; retained it for review",
            worktree_path.display()
        ));
        return CleanupOutcome {
            worktree_removed: false,
            branch_deleted: false,
            bundle_path: None,
            warnings,
            merged: None,
            dirty: Some(false),
            reason: "merge_target_unknown",
        };
    };
    match branch_has_unmerged(repo_root, branch, base, git_bin) {
        Some(false) => {}
        Some(true) => {
            warnings.push(format!("rally stop: retained unmerged worktree for review at {} (branch {branch}, target {base})", worktree_path.display()));
            return CleanupOutcome {
                worktree_removed: false,
                branch_deleted: false,
                bundle_path: None,
                warnings,
                merged: Some(false),
                dirty: Some(false),
                reason: "unmerged",
            };
        }
        None => {
            warnings.push(format!(
                "rally stop: could not verify merge status of {branch} against {base}; retained {}",
                worktree_path.display()
            ));
            return CleanupOutcome {
                worktree_removed: false,
                branch_deleted: false,
                bundle_path: None,
                warnings,
                merged: None,
                dirty: Some(false),
                reason: "merge_status_unknown",
            };
        }
    }

    // 2. Remove the worktree directory without --force. Git performs the
    //    final dirtiness check, closing the gap between our diagnostic preflight
    //    above and the removal attempt. A refusal retains the recovery path;
    //    never fall back to remove_dir_all because it cannot protect a writer
    //    racing with cleanup.
    let mut worktree_removed = false;
    if worktree_path.exists() {
        let remove = Command::new(git_bin)
            .arg("-C")
            .arg(repo_root)
            .arg("worktree")
            .arg("remove")
            .arg(worktree_path)
            .output();
        match remove {
            Ok(out) if out.status.success() => worktree_removed = true,
            Ok(out) => {
                warnings.push(format!(
                    "rally stop: `git worktree remove {}` refused cleanup; retained it for recovery: {}",
                    worktree_path.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            Err(err) => warnings.push(format!(
                "rally stop: could not invoke git worktree remove: {err}"
            )),
        }
    }

    // 3. Delete the branch if it's empty (-d is safe; refuses on unmerged).
    let mut branch_deleted = false;
    if worktree_removed {
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
        bundle_path: None,
        warnings,
        merged: Some(true),
        dirty: Some(false),
        reason: if worktree_removed {
            "removed"
        } else {
            "remove_failed"
        },
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

fn branch_has_unmerged(repo_root: &Path, branch: &str, base: &str, git_bin: &str) -> Option<bool> {
    // `git rev-list <base>..<branch>` lists commits on branch not on base.
    // Empty output → branch is fully merged into base → safe to delete.
    let range = format!("{base}..{branch}");
    match git_output(repo_root, git_bin, &["rev-list", "--count", &range]) {
        Ok(stdout) => stdout.trim().parse::<u64>().ok().map(|count| count > 0),
        Err(_) => None,
    }
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
        use crate::test_git_fixture::fixture_git;
        fixture_git(root, &["init", "-q", "-b", "main"]);
        fixture_git(root, &["commit", "--allow-empty", "-m", "initial"]);
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
    fn cleanup_retains_clean_unmerged_worktree_and_branch() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let repo = tmp_dir("cleanup-unmerged");
        init_test_repo(&repo);
        let pw = provision(&repo, "claude-reviewer-01", "git").expect("provision");

        // Add a commit on the per-agent branch to make it unmerged.
        fs::write(pw.path.join("note.txt"), b"work in progress").unwrap();
        crate::test_git_fixture::fixture_git(&pw.path, &["add", "note.txt"]);
        crate::test_git_fixture::fixture_git(&pw.path, &["commit", "-m", "wip"]);

        let outcome = cleanup(&repo, &pw.path, &pw.branch, "git");

        assert!(!outcome.worktree_removed);
        assert!(
            !outcome.branch_deleted,
            "unmerged branch must be retained, not deleted"
        );
        assert!(
            outcome.bundle_path.is_none(),
            "retained checkout needs no bundle"
        );
        assert!(pw.path.exists(), "unmerged checkout must remain available");

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

    #[test]
    fn cleanup_retains_worktree_holding_gitignored_files() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let repo = tmp_dir("cleanup-ignored");
        init_test_repo(&repo);
        fs::write(repo.join(".gitignore"), b".env\nmemory/\n").unwrap();
        crate::test_git_fixture::fixture_git(&repo, &["add", ".gitignore"]);
        crate::test_git_fixture::fixture_git(&repo, &["commit", "-m", "ignore rules"]);
        let pw = provision(&repo, "ignored-worker-01", "git").expect("provision");

        // Merged (empty) branch and a porcelain-clean tree: the only thing
        // standing between this worktree and removal is its ignored files.
        fs::write(pw.path.join(".env"), b"SECRET=1").unwrap();
        fs::create_dir_all(pw.path.join("memory")).unwrap();
        fs::write(pw.path.join("memory/notes.md"), b"agent memory").unwrap();
        let outcome = cleanup(&repo, &pw.path, &pw.branch, "git");

        assert!(!outcome.worktree_removed);
        assert!(!outcome.branch_deleted);
        assert_eq!(outcome.reason, "ignored_files");
        assert_eq!(fs::read(pw.path.join(".env")).unwrap(), b"SECRET=1");
        assert_eq!(
            fs::read(pw.path.join("memory/notes.md")).unwrap(),
            b"agent memory"
        );
        assert!(
            outcome.warnings.iter().any(|w| w.contains(".env")),
            "warning must list ignored paths for review: {:?}",
            outcome.warnings
        );

        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn cleanup_retains_dirty_worktree_and_uncommitted_files() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let repo = tmp_dir("cleanup-dirty");
        init_test_repo(&repo);
        fs::write(repo.join("tracked.txt"), b"base").unwrap();
        crate::test_git_fixture::fixture_git(&repo, &["add", "tracked.txt"]);
        crate::test_git_fixture::fixture_git(&repo, &["commit", "-m", "tracked base"]);
        let pw = provision(&repo, "dirty-worker-01", "git").expect("provision");

        fs::write(pw.path.join("tracked.txt"), b"modified").unwrap();
        fs::write(pw.path.join("untracked.txt"), b"uncommitted").unwrap();
        let outcome = cleanup(&repo, &pw.path, &pw.branch, "git");

        assert!(!outcome.worktree_removed);
        assert!(!outcome.branch_deleted);
        assert!(pw.path.exists(), "dirty worktree must remain recoverable");
        assert_eq!(fs::read(pw.path.join("tracked.txt")).unwrap(), b"modified");
        assert_eq!(
            fs::read(pw.path.join("untracked.txt")).unwrap(),
            b"uncommitted"
        );
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.contains(pw.path.to_string_lossy().as_ref())),
            "warning must include recovery path: {:?}",
            outcome.warnings
        );

        // Return the fixture to a clean state so the test can remove it.
        fs::write(pw.path.join("tracked.txt"), b"base").unwrap();
        fs::remove_file(pw.path.join("untracked.txt")).unwrap();
        let final_cleanup = cleanup(&repo, &pw.path, &pw.branch, "git");
        assert!(final_cleanup.worktree_removed);
        fs::remove_dir_all(&repo).ok();
    }
}
