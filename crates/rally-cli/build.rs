// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Build script for rally-cli.
//!
//! Emits `RALLY_BUILD_ID` = `<CARGO_PKG_VERSION>+<git-short-hash>[-dirty]`.
//! Falls back to `<version>+nogit` when git is unavailable or the working
//! tree is not a git repo. Never panics — a degraded stamp is still useful.
//!
//! Worktree-safe rebuild triggers: a LINKED worktree's `.git` is a file (not
//! a directory) pointing at `<common-dir>/worktrees/<name>`, so `HEAD` and
//! the index live in that per-worktree gitdir while `packed-refs` and the
//! branch's own ref file live in the COMMON dir shared across worktrees.
//! Hardcoding `.git/HEAD` / `.git/packed-refs` (the old approach) is simply
//! wrong from inside a worktree. We resolve every watched path via
//! `git rev-parse --git-path` / `--git-common-dir` instead, so the rebuild
//! trigger is correct from ANY worktree, not just a plain checkout.
//!
//! Dirty-suffix scoping: computed from `git status --porcelain`, but Rally's
//! OWN runtime coordination state is excluded (`.rally/`, `.build-loop/`,
//! `.wrangler/`) — a live coordination session (claims, ledger writes,
//! build-loop working-state) must never make every binary built in this
//! checkout report as `-dirty`; only build-relevant source changes should.
//!
//! `.agents/` is deliberately NOT excluded (f3, 2026-07-09): unlike the
//! three dirs above, `.agents/` is not exclusively Rally runtime scratch —
//! it also holds `.agents/plugins/marketplace.json`, a TRACKED, shipped
//! release manifest that gets version-bumped in real commits. Excluding
//! `.agents/` wholesale hid an uncommitted edit to that manifest from the
//! `-dirty` signal. If a genuine Rally-runtime-only subdirectory under
//! `.agents/` is added later, exclude that SPECIFIC subdir path here —
//! never the whole `.agents/` prefix.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let version = env!("CARGO_PKG_VERSION");

    // Best-effort: any watch path we fail to resolve is simply skipped. A
    // missed rebuild trigger just means a stale build id in a rare edge
    // case, never a build failure.
    for path in git_watch_paths() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-changed=build.rs");

    let hash = git_short_hash().unwrap_or_else(|| "nogit".to_string());
    let dirty_suffix = if hash != "nogit" && has_relevant_dirty_state() {
        "-dirty"
    } else {
        ""
    };

    let build_id = format!("{version}+{hash}{dirty_suffix}");
    println!("cargo:rustc-env=RALLY_BUILD_ID={build_id}");
}

/// Rally runtime/coordination state that must never affect the build id.
/// Paths are repo-root-relative, matching `git status --porcelain` output.
/// `.agents/` is intentionally absent — see the module doc comment (f3).
const DIRTY_EXCLUDED_PREFIXES: [&str; 3] = [".rally/", ".build-loop/", ".wrangler/"];

/// The git-internal paths whose change should trigger a rebuild: `HEAD`
/// (per-worktree), `packed-refs` (common dir), the index (per-worktree), and
/// the loose ref file HEAD's symbolic target points at (the branch tip), if
/// any. Resolution is best-effort — any step that fails is silently skipped.
fn git_watch_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Some(p) = git_path("HEAD") {
        paths.push(p);
    }
    if let Some(p) = git_path("packed-refs") {
        paths.push(p);
    }
    if let Some(p) = git_path("index") {
        paths.push(p);
    }
    if let (Some(common_dir), Some(target)) = (git_common_dir(), symbolic_head_target()) {
        paths.push(common_dir.join(target));
    }

    paths
}

/// `git rev-parse --git-path <arg>` — resolves to the correct absolute path
/// for the CURRENT worktree (per-worktree files like `HEAD`/`index`) or the
/// common dir (shared files like `packed-refs`), whichever `<arg>` actually
/// lives in. `None` on any failure.
fn git_path(arg: &str) -> Option<PathBuf> {
    run_git_trimmed(["rev-parse", "--git-path", arg]).map(PathBuf::from)
}

/// `git rev-parse --git-common-dir` — the dir shared across all worktrees,
/// where `refs/`, `packed-refs`, and object storage actually live.
fn git_common_dir() -> Option<PathBuf> {
    run_git_trimmed(["rev-parse", "--git-common-dir"]).map(PathBuf::from)
}

/// The ref HEAD currently points at (e.g. `refs/heads/main`), relative to
/// the common dir. `None` for a detached HEAD — nothing extra to watch, the
/// per-worktree `HEAD` file already covers that case directly.
fn symbolic_head_target() -> Option<PathBuf> {
    run_git_trimmed(["symbolic-ref", "-q", "HEAD"]).map(PathBuf::from)
}

/// `git rev-parse --short HEAD`.
fn git_short_hash() -> Option<String> {
    run_git_trimmed(["rev-parse", "--short", "HEAD"])
}

/// Whether the working tree has build-relevant uncommitted changes.
fn has_relevant_dirty_state() -> bool {
    let Some(porcelain) = run_git_raw(["status", "--porcelain"]) else {
        return false;
    };
    porcelain.lines().any(is_build_relevant_status_line)
}

/// `git status --porcelain` lines look like `XY path` or, for renames,
/// `XY old -> new`. Returns true when the line touches at least one path
/// OUTSIDE the excluded Rally-runtime prefixes (i.e. it should count toward
/// `-dirty`).
fn is_build_relevant_status_line(line: &str) -> bool {
    // Byte offset 3 is the standard `XY ` status prefix; `.get` (not
    // indexing) keeps this panic-free even on a malformed/short line.
    let path = line.get(3..).unwrap_or("").trim();
    let candidates: Vec<&str> = match path.split_once(" -> ") {
        Some((old, new)) => vec![old, new],
        None => vec![path],
    };
    !candidates.iter().all(|p| {
        DIRTY_EXCLUDED_PREFIXES
            .iter()
            .any(|prefix| p.starts_with(prefix))
    })
}

/// Run `git <args>`, returning trimmed stdout on success or `None` on any
/// failure (git not found, non-zero exit, empty/non-UTF8 output).
fn run_git_trimmed<I, S>(args: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let raw = run_git_raw(args)?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Run `git <args>`, returning raw (untrimmed) stdout on success or `None`
/// on failure. Untrimmed because line-oriented output (`status --porcelain`)
/// must preserve per-line structure.
fn run_git_raw<I, S>(args: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// f3 (2026-07-09) regression guard: `.agents/plugins/marketplace.json` is a
/// TRACKED, shipped release manifest — an edit to it must count as
/// build-relevant dirt, unlike genuine Rally-runtime scratch under
/// `.rally/`, `.build-loop/`, or `.wrangler/`.
///
/// NOTE: `cargo test` does not execute a crate's `build.rs` as a normal test
/// target (build scripts have no `[[test]]` entry and are not part of
/// `--lib`/`--bins` test discovery), so this module is exercised via
/// `rustc --test build.rs` directly rather than `cargo test -p rally-cli`.
/// It is kept here (rather than a separate `tests/` file) because
/// `is_build_relevant_status_line` and `DIRTY_EXCLUDED_PREFIXES` are private
/// to this file.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_marketplace_json_edit_is_build_relevant() {
        assert!(
            is_build_relevant_status_line(" M .agents/plugins/marketplace.json"),
            ".agents/plugins/marketplace.json is a tracked release manifest — \
             an edit to it must trigger -dirty, not be silently excluded"
        );
    }

    #[test]
    fn rally_runtime_state_is_excluded_from_dirty() {
        for line in [
            " M .rally/log/room.jsonl",
            " M .build-loop/working-state/current.json",
            " M .wrangler/state/foo.json",
        ] {
            assert!(
                !is_build_relevant_status_line(line),
                "Rally/build-loop/wrangler runtime scratch must stay excluded: {line}"
            );
        }
    }

    #[test]
    fn source_file_edit_is_build_relevant() {
        assert!(is_build_relevant_status_line(
            " M crates/rally-cli/src/lib.rs"
        ));
    }

    #[test]
    fn rename_counts_if_either_side_is_build_relevant() {
        // `git status --porcelain` rename lines: `XY old -> new`.
        assert!(is_build_relevant_status_line(
            "R  .rally/scratch.jsonl -> crates/rally-cli/src/new.rs"
        ));
        assert!(!is_build_relevant_status_line(
            "R  .rally/old.jsonl -> .rally/new.jsonl"
        ));
    }
}
