// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Shared-branch / worktree hazard detector.
//!
//! Detects the coordination hazard described in dynamic-workflows/COORDINATION.md
//! rule 5: an agent is working in the canonical shared checkout while it is on a
//! non-`main` branch and at least one peer is active.  In that situation, any
//! commit lands on the peer's branch — a silent misdirect that actually occurred
//! in production.
//!
//! The correct pattern is: each agent works from its OWN `git worktree` off
//! `main`.  Linked worktrees are never flagged here.
//!
//! # Never blocks
//! Per the rally charter this module only returns an advisory string.  The caller
//! decides what to do (warn + record a durable risk fact).

use std::fs;
use std::path::Path;

/// Returns `true` when `repo_root/.git` is a **file** (linked worktree pointer)
/// rather than a directory (canonical clone).
///
/// A linked worktree created by `git worktree add` has a `.git` that is a
/// plain text file containing `gitdir: /path/to/main/.git/worktrees/<name>`.
/// A canonical clone has `.git` as a directory.
///
/// No `git` subprocess is used — files are read directly.
pub(crate) fn is_linked_worktree(repo_root: &Path) -> bool {
    repo_root
        .join(".git")
        .metadata()
        .map(|m| m.is_file())
        .unwrap_or(false)
}

/// Parse the current branch name from `repo_root/.git/HEAD`.
///
/// Returns `Some(branch_name)` for a symbolic ref (`ref: refs/heads/<name>`).
/// Returns `None` for detached HEAD, missing file, or parse failure.
///
/// No `git` subprocess is used — the file is read directly.
pub(crate) fn current_branch(repo_root: &Path) -> Option<String> {
    let head_path = repo_root.join(".git").join("HEAD");
    let content = fs::read_to_string(&head_path).ok()?;
    let trimmed = content.trim();
    // Symbolic ref form: "ref: refs/heads/<branch>"
    let branch = trimmed.strip_prefix("ref: refs/heads/")?;
    let branch = branch.trim();
    if branch.is_empty() {
        None
    } else {
        Some(branch.to_string())
    }
}

/// Detect the shared-branch / worktree coordination hazard.
///
/// The hazard fires when **all** of the following hold:
/// 1. This is NOT a linked worktree (i.e. this is the canonical shared clone).
/// 2. The current branch is not `main` or `master` (a named feature/patch branch).
/// 3. At least one OTHER active peer is present (`active_peer_count >= 1`), meaning
///    there are ≥ 2 active tools in the room in total.
///
/// Linked worktrees on their own branch are the CORRECT pattern and are never
/// flagged.  A solo agent on a feature branch (no peers) is also not flagged —
/// the hazard is about commits silently landing on a peer's branch.
///
/// Returns `Some(warning_message)` when the hazard fires, `None` otherwise.
pub(crate) fn detect_shared_branch_hazard(
    _repo_root: &Path,
    is_linked: bool,
    current_branch: Option<&str>,
    active_peer_count: usize,
) -> Option<String> {
    // Condition (a): must be canonical clone, not a linked worktree.
    if is_linked {
        return None;
    }

    // Condition (b): branch must be non-main.
    let branch = match current_branch {
        None => return None, // detached HEAD — can't determine, skip.
        Some(b) if b == "main" || b == "master" => return None,
        Some(b) => b,
    };

    // Condition (c): at least one peer is active.
    if active_peer_count == 0 {
        return None;
    }

    Some(format!(
        "shared-branch-hazard: canonical checkout is on branch '{branch}' with {active_peer_count} active peer(s) \
— a commit here lands on a peer's branch. Check in, and work from your own `git worktree` off main."
    ))
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
        let p = std::env::temp_dir().join(format!("rally-wg-{label}-{nanos}"));
        fs::create_dir_all(&p).unwrap();
        p
    }

    // --- is_linked_worktree ---

    #[test]
    fn linked_worktree_dot_git_is_file() {
        let root = tmp_dir("linked-git-file");
        // Write a .git FILE (linked worktree pattern).
        fs::write(
            root.join(".git"),
            "gitdir: /some/canonical/.git/worktrees/feat\n",
        )
        .unwrap();
        assert!(
            is_linked_worktree(&root),
            "directory with a .git FILE must be detected as linked worktree"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn canonical_clone_dot_git_is_dir() {
        let root = tmp_dir("canonical-git-dir");
        // Write a .git DIRECTORY (canonical clone pattern).
        fs::create_dir_all(root.join(".git")).unwrap();
        assert!(
            !is_linked_worktree(&root),
            "directory with a .git DIR must NOT be detected as linked worktree"
        );
        fs::remove_dir_all(&root).ok();
    }

    // --- current_branch ---

    #[test]
    fn current_branch_returns_branch_name() {
        let root = tmp_dir("branch-parse");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(
            root.join(".git").join("HEAD"),
            "ref: refs/heads/feat/my-feature\n",
        )
        .unwrap();
        assert_eq!(
            current_branch(&root).as_deref(),
            Some("feat/my-feature"),
            "branch name must be parsed from ref: refs/heads/<name>"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn current_branch_returns_none_on_detached_head() {
        let root = tmp_dir("detached-head");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(
            root.join(".git").join("HEAD"),
            "abc1234def5678abc1234def5678abc1234def56\n",
        )
        .unwrap();
        assert!(
            current_branch(&root).is_none(),
            "detached HEAD must return None"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn current_branch_returns_none_on_missing_head() {
        let root = tmp_dir("missing-head");
        fs::create_dir_all(root.join(".git")).unwrap();
        // No HEAD file written.
        assert!(
            current_branch(&root).is_none(),
            "missing HEAD file must return None"
        );
        fs::remove_dir_all(&root).ok();
    }

    // --- detect_shared_branch_hazard ---

    /// Hazard fires: canonical clone, non-main branch, 1 peer.
    #[test]
    fn hazard_fires_on_canonical_non_main_with_peer() {
        let dummy = tmp_dir("hazard-fires");
        let result = detect_shared_branch_hazard(
            &dummy,
            false,               // canonical clone (not linked)
            Some("feat/danger"), // non-main branch
            1,                   // 1 active peer
        );
        assert!(
            result.is_some(),
            "hazard must fire: canonical + non-main + peer"
        );
        let msg = result.unwrap();
        assert!(
            msg.contains("shared-branch-hazard"),
            "message must contain 'shared-branch-hazard'; got: {msg}"
        );
        assert!(
            msg.contains("feat/danger"),
            "message must name the branch; got: {msg}"
        );
        assert!(
            msg.contains("1 active peer"),
            "message must name the peer count; got: {msg}"
        );
        fs::remove_dir_all(&dummy).ok();
    }

    /// Hazard fires: multiple peers escalates the count in the message.
    #[test]
    fn hazard_fires_with_multiple_peers() {
        let dummy = tmp_dir("hazard-multi-peer");
        let result = detect_shared_branch_hazard(&dummy, false, Some("fix/bug-42"), 3);
        assert!(result.is_some(), "hazard must fire with 3 peers");
        let msg = result.unwrap();
        assert!(
            msg.contains("3 active peer"),
            "peer count must appear in message; got: {msg}"
        );
        fs::remove_dir_all(&dummy).ok();
    }

    /// No false positive: on `main`, canonical + peers must NOT fire.
    #[test]
    fn no_hazard_on_main_branch() {
        let dummy = tmp_dir("no-hazard-main");
        let result = detect_shared_branch_hazard(
            &dummy,
            false,        // canonical clone
            Some("main"), // main branch
            2,            // peers present
        );
        assert!(
            result.is_none(),
            "hazard must NOT fire when branch is 'main'"
        );
        fs::remove_dir_all(&dummy).ok();
    }

    /// No false positive: on `master`.
    #[test]
    fn no_hazard_on_master_branch() {
        let dummy = tmp_dir("no-hazard-master");
        let result = detect_shared_branch_hazard(&dummy, false, Some("master"), 2);
        assert!(
            result.is_none(),
            "hazard must NOT fire when branch is 'master'"
        );
        fs::remove_dir_all(&dummy).ok();
    }

    /// No false positive: linked worktree on non-main branch with peers.
    #[test]
    fn no_hazard_on_linked_worktree() {
        let dummy = tmp_dir("no-hazard-linked");
        let result = detect_shared_branch_hazard(
            &dummy,
            true,                // linked worktree — CORRECT pattern
            Some("feat/danger"), // non-main branch
            5,                   // peers present
        );
        assert!(
            result.is_none(),
            "hazard must NOT fire for a linked worktree (that's the correct pattern)"
        );
        fs::remove_dir_all(&dummy).ok();
    }

    /// No false positive: detached HEAD (None branch) must not fire.
    #[test]
    fn no_hazard_on_detached_head() {
        let dummy = tmp_dir("no-hazard-detached");
        let result = detect_shared_branch_hazard(
            &dummy, false, // canonical clone
            None,  // detached HEAD
            3,
        );
        assert!(
            result.is_none(),
            "hazard must NOT fire on detached HEAD (branch unknown)"
        );
        fs::remove_dir_all(&dummy).ok();
    }

    /// No false positive: solo agent (no peers) on non-main branch.
    #[test]
    fn no_hazard_when_solo() {
        let dummy = tmp_dir("no-hazard-solo");
        let result = detect_shared_branch_hazard(
            &dummy,
            false,
            Some("feat/solo-work"),
            0, // no peers
        );
        assert!(
            result.is_none(),
            "hazard must NOT fire when there are no active peers (solo agent)"
        );
        fs::remove_dir_all(&dummy).ok();
    }
}
