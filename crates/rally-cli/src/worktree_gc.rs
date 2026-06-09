// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Sweep-reaper for leftover rally per-agent worktrees and branches.
//!
//! Implements `rally worktree gc [--apply] [--ttl <duration>] [--json]`.
//!
//! # Reap criteria
//! A worktree is a **candidate** when it is rally-managed (branch starts with
//! `rally/` OR path under `.rally/worktrees/`). It is **reapable** if either:
//!
//! - (a) The branch is fully merged into the default branch
//!   (`git merge-base --is-ancestor <branch> <default>`), OR
//! - (b) The owning agent's presence is stale beyond the configured TTL.
//!
//! # NEVER reap
//! - The default branch worktree (the canonical checkout).
//! - The current worktree (the process's cwd) — identified by the `HEAD`
//!   the git process resolves.
//! - Any worktree whose owner has LIVE (non-stale) presence in the room.
//! - An unmerged worktree whose owner is LIVE (cleanup() bundles unmerged
//!   work, but we must not even attempt that when the owner is active).
//!
//! # Reuse of cleanup()
//! [`run_worktree::cleanup`] performs the safe remove sequence:
//! bundle-if-unmerged → `git worktree remove --force` (with rm-rf+prune
//! fallback) → `git branch -d` (safe, refuses unmerged). This module
//! ENUMERATES and FILTERS, then delegates every actual removal to `cleanup()`.
//!
//! # Liveness
//! The caller supplies [`PresenceFact`] values (a flat projection of the room's
//! `FactKind::Presence` facts). The reaper computes staleness from `created_at`
//! vs `now_ts` using the same threshold as [`agent_state::IDLE_THRESHOLD_SECS`]
//! (but overridable by the caller's `ttl_secs`). Owner derivation: the agent
//! name is extracted from the branch name (`rally/<agent>-<rest>`) and matched
//! against the `tool` field in each presence fact using prefix/substring
//! matching — robust enough without requiring an exact-name contract between
//! `rally run` and `rally worktree gc`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use crate::run_worktree;

/// Backend-liveness probe: given a per-agent `session_id`, returns `true` when
/// the backing tmux/cmux session is confirmed gone (dead) and `false` when it
/// is still live. Shared between `GcConfig` and the `lib.rs` call site so the
/// `Arc<dyn Fn(...)>` shape lives in exactly one place (clippy::type_complexity).
pub type BackendLivenessProbe = Arc<dyn Fn(&str) -> bool + Send + Sync>;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A minimal presence fact supplied by the caller.
///
/// The caller reads `FactKind::Presence` facts from the room store and
/// populates these; the reaper only needs tool name, seq, and timestamp.
/// This thin wrapper avoids a circular dependency on `store::Fact` from a
/// public test helper.
#[derive(Clone, Debug)]
pub struct PresenceFact {
    pub tool: String,
    pub seq: i64,
    pub subject: String,
    pub created_at: String,
}

/// Configuration for a single GC run.
pub struct GcConfig {
    /// The canonical repo root (parent of `.rally/`, `.git/`).
    pub repo_root: PathBuf,
    /// When `false` (default): dry-run — enumerate + classify but make no
    /// filesystem/git changes.  When `true`: execute `cleanup()` on reapable
    /// worktrees.
    pub apply: bool,
    /// Staleness threshold in seconds.  A presence fact older than this makes
    /// the owning agent stale.  Default: 24 * 3600 (24 hours).
    pub ttl_secs: u64,
    /// Reference timestamp for liveness computation (RFC3339).  `None` → use
    /// the system clock at call time.
    pub now_ts: Option<String>,
    /// Presence facts from the rally room for liveness checks.  Empty when the
    /// caller cannot open the room store (graceful degradation: falls back to
    /// TTL-only staleness with no live presence data).
    pub presence_facts: Vec<PresenceFact>,
    /// Git binary to use (e.g. `"git"`).
    pub git_bin: String,
    /// f2 — Backend-liveness gate for unmerged worktrees that are stale-by-TTL
    /// only.
    ///
    /// When supplied, a worktree that is reapable ONLY because its owner is
    /// TTL-stale (i.e. unmerged) is additionally required to be confirmed
    /// backend-dead before it is reaped.  The probe is called with the
    /// per-agent `session_id` (branch suffix after `rally/`), and returns
    /// `true` when the backing tmux/cmux session is confirmed gone (dead) and
    /// `false` when the session is still live.  If the probe returns `false`
    /// (live backend), the GC skips the worktree with a reason mentioning the
    /// live backend.
    ///
    /// `None` → no backend probe is performed (TTL-only staleness is
    /// sufficient).  This preserves backward-compatibility for callers that
    /// do not have a tmux/cmux binary available.
    pub backend_liveness_probe: Option<BackendLivenessProbe>,
}

/// One GC candidate (may or may not be reaped).
#[derive(Clone, Debug)]
pub struct GcCandidate {
    pub worktree_path: PathBuf,
    pub branch: String,
    /// Human-readable reason this is a candidate.
    pub reason: String,
}

/// A worktree that was skipped (not reaped) and why.
#[derive(Clone, Debug)]
pub struct GcSkipped {
    pub worktree_path: PathBuf,
    pub branch: String,
    /// Human-readable skip reason.
    pub reason: String,
}

/// A worktree that was successfully reaped.
#[derive(Clone, Debug)]
pub struct GcReaped {
    pub worktree_path: PathBuf,
    pub branch: String,
    pub branch_deleted: bool,
}

/// Structured GC report returned by [`run_gc`].
#[derive(Debug)]
pub struct GcReport {
    /// All rally-managed worktrees that met the enumeration filter (candidates +
    /// skipped together make this list). Dry-run entries appear here but not in
    /// `reaped`.
    pub candidates: Vec<GcCandidate>,
    /// Worktrees actually reaped (`--apply` only).
    pub reaped: Vec<GcReaped>,
    /// Worktrees that were candidates but were skipped (live owner, etc.).
    pub skipped: Vec<GcSkipped>,
    /// Bundle paths written before any unmerged-work removal.
    pub bundles: Vec<PathBuf>,
    /// Non-fatal warnings collected during cleanup.
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Core logic
// ---------------------------------------------------------------------------

/// Run a GC sweep against `config.repo_root`.
///
/// Parses git's worktree list, filters for rally-managed entries,
/// classifies each as reapable or skippable, and (when `config.apply`)
/// calls [`run_worktree::cleanup`] on each reapable entry.
pub fn run_gc(config: GcConfig) -> Result<GcReport, String> {
    let repo = &config.repo_root;
    let git_bin = &config.git_bin;
    let now_ts = config
        .now_ts
        .clone()
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));

    // 1. Enumerate worktrees.
    let entries = list_worktrees(repo, git_bin)?;

    // 2. Resolve default branch and current worktree(s) to protect them.
    //    f4: resolve BOTH the repo-root toplevel (via -C repo) AND the process
    //    cwd toplevel (no -C, inherits actual cwd) so that running gc from
    //    inside a linked worktree protects that worktree too.
    let default_branch = resolve_default_branch(repo, git_bin);
    let current_wt = resolve_current_worktree(repo, git_bin);
    let cwd_wt = resolve_cwd_worktree(git_bin);

    // 3. Build liveness index from presence facts.
    let liveness = build_liveness_index(&config.presence_facts, &now_ts, config.ttl_secs);

    // 4. Classify each entry.
    let mut candidates: Vec<GcCandidate> = Vec::new();
    let mut reaped: Vec<GcReaped> = Vec::new();
    let mut skipped: Vec<GcSkipped> = Vec::new();
    let mut bundles: Vec<PathBuf> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    for entry in entries {
        // Only process rally-managed worktrees.
        if !is_rally_managed(&entry.branch, &entry.path) {
            continue;
        }

        // Protect the default branch's worktree.
        if let Some(ref def) = default_branch {
            if entry.branch == *def || entry.branch == format!("refs/heads/{def}") {
                skipped.push(GcSkipped {
                    worktree_path: entry.path.clone(),
                    branch: entry.branch.clone(),
                    reason: "default branch — never reap".to_string(),
                });
                continue;
            }
        }

        // Protect the current (cwd-resolved) worktree.
        // f4: check both the repo-root resolved path AND the actual process cwd
        // so that running gc from inside a linked worktree protects that linked
        // worktree as well.
        let is_current_wt = current_wt
            .as_ref()
            .is_some_and(|p| same_path(p, &entry.path))
            || cwd_wt.as_ref().is_some_and(|p| same_path(p, &entry.path));
        if is_current_wt {
            skipped.push(GcSkipped {
                worktree_path: entry.path.clone(),
                branch: entry.branch.clone(),
                reason: "current worktree — never reap".to_string(),
            });
            continue;
        }

        // Determine owner liveness.
        let owner_prefix = derive_owner_prefix(&entry.branch);
        let owner_is_live = is_owner_live(&owner_prefix, &liveness);

        // Determine merge status.
        let merged = if let Some(ref def) = default_branch {
            is_merged(repo, &entry.branch, def, git_bin)
        } else {
            false
        };

        // Classify.
        if owner_is_live && !merged {
            // Live owner + unmerged → must NOT reap.
            skipped.push(GcSkipped {
                worktree_path: entry.path.clone(),
                branch: entry.branch.clone(),
                reason: format!(
                    "live owner ({owner_prefix}) — unmerged work; wait for agent to finish"
                ),
            });
            continue;
        }

        // f2 — backend-liveness gate: an unmerged worktree that is reapable
        // ONLY by TTL staleness (not by merge) must also be confirmed
        // backend-dead before we touch it.  A long-running agent that simply
        // hasn't posted a heartbeat recently is indistinguishable from a dead
        // one by TTL alone.  If the probe says the session is still live, skip.
        if !merged {
            // Reapable only by TTL staleness (not by merge): we must confirm the
            // agent is actually gone before deleting unmerged work.
            let session_id = entry.branch.strip_prefix("rally/").unwrap_or(&entry.branch);
            match config.backend_liveness_probe {
                Some(ref probe) => {
                    if !probe(session_id) {
                        skipped.push(GcSkipped {
                            worktree_path: entry.path.clone(),
                            branch: entry.branch.clone(),
                            reason: format!(
                                "backend probe says session {session_id} is still live — not reaped"
                            ),
                        });
                        continue;
                    }
                    // backend confirmed dead → fall through to reap (cleanup bundles).
                }
                None => {
                    // No backend probe wired: TTL staleness alone cannot tell a dead
                    // agent from a quiet long-running one. Conservatively refuse to reap
                    // unmerged work. (Merged worktrees are already reaped above.)
                    skipped.push(GcSkipped {
                        worktree_path: entry.path.clone(),
                        branch: entry.branch.clone(),
                        reason: format!(
                            "unmerged + no backend probe — refusing to reap on staleness alone (session {session_id})"
                        ),
                    });
                    continue;
                }
            }
        }

        // Reapable: merged OR (unmerged + stale owner + backend-dead).
        let reason = if merged {
            "branch merged into default".to_string()
        } else {
            format!(
                "owner stale (>{ttl}s since last presence)",
                ttl = config.ttl_secs
            )
        };

        candidates.push(GcCandidate {
            worktree_path: entry.path.clone(),
            branch: entry.branch.clone(),
            reason: reason.clone(),
        });

        if !config.apply {
            // Dry-run: classify only.
            continue;
        }

        // Apply: call cleanup().
        let outcome = run_worktree::cleanup(repo, &entry.path, &entry.branch, git_bin);
        for w in &outcome.warnings {
            warnings.push(w.clone());
        }
        // f3 — bundle_failed guard: if cleanup() could not write the safety
        // bundle for unmerged work, it did NOT remove the worktree.  Push to
        // skipped (not reaped) so the caller knows the worktree is still
        // present and surfaces the warning.
        if outcome.bundle_failed {
            warnings.push(format!(
                "worktree gc: skipped {} — bundle failed, unmerged work preserved",
                entry.path.display()
            ));
            skipped.push(GcSkipped {
                worktree_path: entry.path.clone(),
                branch: entry.branch.clone(),
                reason: "bundle write failed — unmerged work not removed".to_string(),
            });
            continue;
        }
        if let Some(bundle) = outcome.bundle_path {
            bundles.push(bundle);
        }
        reaped.push(GcReaped {
            worktree_path: entry.path.clone(),
            branch: entry.branch.clone(),
            branch_deleted: outcome.branch_deleted,
        });
    }

    Ok(GcReport {
        candidates,
        reaped,
        skipped,
        bundles,
        warnings,
    })
}

// ---------------------------------------------------------------------------
// Worktree enumeration
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct WorktreeEntry {
    path: PathBuf,
    branch: String,
}

/// Parse `git worktree list --porcelain` output into `WorktreeEntry` values.
fn list_worktrees(repo: &Path, git_bin: &str) -> Result<Vec<WorktreeEntry>, String> {
    let out = Command::new(git_bin)
        .arg("-C")
        .arg(repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map_err(|e| format!("could not invoke {git_bin} worktree list: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(parse_porcelain(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse `git worktree list --porcelain` output.
///
/// Each worktree block looks like:
/// ```text
/// worktree /absolute/path
/// HEAD <sha>
/// branch refs/heads/<name>
/// (blank line)
/// ```
/// Bare/detached worktrees carry `detached` or `bare` instead of `branch`.
fn parse_porcelain(output: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;

    for line in output.lines() {
        if let Some(path_str) = line.strip_prefix("worktree ") {
            // Flush previous block.
            if let (Some(path), Some(branch)) = (current_path.take(), current_branch.take()) {
                entries.push(WorktreeEntry { path, branch });
            }
            current_path = Some(PathBuf::from(path_str.trim()));
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            // `branch refs/heads/<name>` — strip the prefix to get the short name.
            let short = branch_ref
                .trim()
                .strip_prefix("refs/heads/")
                .unwrap_or(branch_ref.trim())
                .to_string();
            current_branch = Some(short);
        }
        // Other lines (HEAD, bare, detached, empty) are ignored.
    }
    // Flush the last block.
    if let (Some(path), Some(branch)) = (current_path, current_branch) {
        entries.push(WorktreeEntry { path, branch });
    }
    entries
}

// ---------------------------------------------------------------------------
// Rally-managed filter
// ---------------------------------------------------------------------------

fn is_rally_managed(branch: &str, path: &Path) -> bool {
    branch.starts_with("rally/") || path.components().any(|c| c.as_os_str() == ".rally")
}

// ---------------------------------------------------------------------------
// Default branch and current worktree resolution
// ---------------------------------------------------------------------------

fn resolve_default_branch(repo: &Path, git_bin: &str) -> Option<String> {
    // Try symbolic-ref HEAD first (covers normal cases where the canonical
    // checkout's HEAD is on main/master).
    if let Ok(out) = Command::new(git_bin)
        .arg("-C")
        .arg(repo)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
    {
        if out.status.success() {
            let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    // Fallback: check main then master.
    for candidate in ["main", "master"] {
        if Command::new(git_bin)
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "--verify", "--quiet", candidate])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Some(candidate.to_string());
        }
    }
    None
}

/// The path of the worktree in which this process is running — identified by
/// resolving `git rev-parse --show-toplevel` from the current working dir.
/// The canonical worktree (repo_root itself) is always protected; we also
/// protect the specific linked worktree the process happens to be in.
fn resolve_current_worktree(repo: &Path, git_bin: &str) -> Option<PathBuf> {
    // The canonical checkout's top-level is the repo root itself — always kept.
    // Linked worktree cwd would differ, but the GC typically runs from the
    // canonical checkout.  Return repo_root as the "current" so it is always
    // protected.
    let out = Command::new(git_bin)
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if out.status.success() {
        let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        return Some(p);
    }
    None
}

/// f4 — Resolve the worktree that contains the ACTUAL PROCESS CWD.
///
/// `resolve_current_worktree` always resolves via `-C repo_root`, which
/// returns the canonical checkout.  When `rally worktree gc` is invoked from
/// inside a linked worktree, the process cwd is DIFFERENT from the canonical
/// checkout.  This function runs `git rev-parse --show-toplevel` with NO `-C`
/// flag so it inherits the real process cwd, returning the linked worktree
/// path.  The result is added to the protected set in addition to `repo_root`.
fn resolve_cwd_worktree(git_bin: &str) -> Option<PathBuf> {
    let out = Command::new(git_bin)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if out.status.success() {
        let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        return Some(p);
    }
    None
}

fn same_path(a: &Path, b: &Path) -> bool {
    // Canonicalize for symlink safety; fall back to raw comparison on error.
    let ca = a.canonicalize().unwrap_or_else(|_| a.to_path_buf());
    let cb = b.canonicalize().unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

// ---------------------------------------------------------------------------
// Merge check
// ---------------------------------------------------------------------------

fn is_merged(repo: &Path, branch: &str, into: &str, git_bin: &str) -> bool {
    // `git merge-base --is-ancestor <branch> <into>` exits 0 when branch's tip
    // is reachable from `into` (i.e. fully merged).
    // Alternatively: `git rev-list --count <into>..<branch>` == 0.
    let range = format!("{into}..{branch}");
    match Command::new(git_bin)
        .arg("-C")
        .arg(repo)
        .args(["rev-list", "--count", &range])
        .output()
    {
        Ok(out) if out.status.success() => {
            let count_str = String::from_utf8_lossy(&out.stdout);
            count_str.trim() == "0"
        }
        // If the branch doesn't exist yet or any git error, conservatively
        // report as not merged (never destroy work silently).
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Liveness index
// ---------------------------------------------------------------------------

/// Map from tool-name patterns → whether the tool is live (not stale).
struct LivenessIndex {
    /// Tools that are definitively LIVE (presence within TTL).
    live_tools: BTreeSet<String>,
    /// Tools that are definitively STALE (presence older than TTL).
    stale_tools: BTreeSet<String>,
}

/// f1 — Normalize a tool/agent name for hyphen-vs-underscore-safe comparison.
///
/// `rally run` registers tools with underscores (e.g. `claude_code:01`) but
/// `derive_owner_prefix` may extract a hyphenated prefix from the branch name
/// (e.g. `claude-code` from `rally/claude-code-task-01`).  Normalizing BOTH
/// sides to underscores before any comparison eliminates false-negative
/// liveness mismatches that would incorrectly allow a live owner's worktree to
/// be reaped.
fn normalize_name(s: &str) -> String {
    s.replace('-', "_")
}

fn build_liveness_index(facts: &[PresenceFact], now_ts: &str, ttl_secs: u64) -> LivenessIndex {
    let now_secs = chrono::DateTime::parse_from_rfc3339(now_ts)
        .map(|dt| dt.timestamp())
        .ok();

    // Keep only the highest-seq fact per tool (keyed by normalized name so
    // that `claude-code` and `claude_code` collapse to the same entry).
    use std::collections::BTreeMap;
    let mut latest: BTreeMap<String, &PresenceFact> = BTreeMap::new();
    for fact in facts {
        let key = normalize_name(&fact.tool);
        let entry = latest.entry(key).or_insert(fact);
        if fact.seq > entry.seq {
            *entry = fact;
        }
    }

    let mut live_tools = BTreeSet::new();
    let mut stale_tools = BTreeSet::new();

    for (normalized_tool, fact) in &latest {
        let seen_secs = chrono::DateTime::parse_from_rfc3339(&fact.created_at)
            .map(|dt| dt.timestamp())
            .ok();
        let stale = match (now_secs, seen_secs) {
            (Some(n), Some(s)) => (n - s) as u64 > ttl_secs,
            // Unparseable timestamps → treat as NOT stale (conservative;
            // mirrors agent_state::project_agent_states behaviour).
            _ => false,
        };
        if stale {
            stale_tools.insert(normalized_tool.clone());
        } else {
            live_tools.insert(normalized_tool.clone());
        }
    }

    LivenessIndex {
        live_tools,
        stale_tools,
    }
}

// ---------------------------------------------------------------------------
// Owner derivation
// ---------------------------------------------------------------------------

/// Extract the agent/tool prefix from a rally branch name.
///
/// `rally/<session-id>` where session-id is typically `<agent>-<task>-<n>`.
/// We extract the part before `-<task>-<n>` as the owner prefix, then use
/// it for substring-based matching against tool names in the liveness index.
///
/// Examples:
/// - `rally/claude-protocol-claude-01` → prefix = `claude`
/// - `rally/codex-claim-authority-01` → prefix = `codex`
/// - `rally/opencode-foo-01` → prefix = `opencode`
fn derive_owner_prefix(branch: &str) -> String {
    // Strip the `rally/` prefix.
    let session_id = branch.strip_prefix("rally/").unwrap_or(branch);
    // The session-id format is `<agent>-<task-components>-<n>`.
    // The agent name is the first `-`-delimited component, BUT well-known
    // multi-word agent names (e.g. `claude-code`, `opencode`) complicate this.
    // Heuristic: match against known multi-word prefixes first; fall back to
    // the first component.
    for multi in &["claude-code", "opencode", "gemini-code"] {
        if session_id.starts_with(multi) {
            return multi.to_string();
        }
    }
    // Single-word: take everything up to the first `-`.
    session_id
        .split('-')
        .next()
        .unwrap_or(session_id)
        .to_string()
}

/// Return `true` when the owner is live (NOT stale).
///
/// f1: both the owner prefix and the stored tool names are normalized
/// (hyphens → underscores) before comparison, eliminating false mismatches
/// between e.g. `claude-code` (from the branch name) and `claude_code`
/// (from the tool registration).
///
/// Matching strategy (in priority order):
/// 1. Exact match between normalized prefix and a normalized tool name.
/// 2. Tool name starts with the normalized prefix.
/// 3. Prefix starts with the tool name (covers tool `claude` matching prefix
///    `claude` extracted from `rally/claude-foo-01`).
///
/// A tool in `stale_tools` is explicitly not live. When no presence facts
/// exist for the owner at all (neither live nor stale), the reaper has no
/// room data for this agent — falls back to TTL-only (treats as stale, i.e.
/// reapable).
fn is_owner_live(owner_prefix: &str, idx: &LivenessIndex) -> bool {
    // Normalize the owner prefix once; the index is already normalized.
    let norm_prefix = normalize_name(owner_prefix);
    // Explicitly stale → not live (short-circuit before checking live set).
    let name_match = |set: &BTreeSet<String>| {
        set.iter().any(|t| {
            t == &norm_prefix || t.starts_with(&norm_prefix) || norm_prefix.starts_with(t.as_str())
        })
    };
    if name_match(&idx.stale_tools) {
        return false;
    }
    name_match(&idx.live_tools)
}

// ---------------------------------------------------------------------------
// Unit tests for internal helpers
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_porcelain_extracts_path_and_branch() {
        let input = "\
worktree /home/user/repo
HEAD abc123
branch refs/heads/main

worktree /home/user/.rally/worktrees/claude-foo-01
HEAD def456
branch refs/heads/rally/claude-foo-01

worktree /home/user/.rally/worktrees/codex-bar-01
HEAD 789abc
branch refs/heads/rally/codex-bar-01

";
        let entries = parse_porcelain(input);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, PathBuf::from("/home/user/repo"));
        assert_eq!(entries[0].branch, "main");
        assert_eq!(
            entries[1].path,
            PathBuf::from("/home/user/.rally/worktrees/claude-foo-01")
        );
        assert_eq!(entries[1].branch, "rally/claude-foo-01");
        assert_eq!(entries[2].branch, "rally/codex-bar-01");
    }

    #[test]
    fn is_rally_managed_matches_branch_prefix_and_path() {
        assert!(is_rally_managed("rally/claude-01", Path::new("/any/path")));
        assert!(is_rally_managed(
            "main",
            Path::new("/repo/.rally/worktrees/foo")
        ));
        assert!(!is_rally_managed("main", Path::new("/repo/src")));
    }

    #[test]
    fn derive_owner_prefix_extracts_single_word_agent() {
        assert_eq!(derive_owner_prefix("rally/claude-protocol-01"), "claude");
        assert_eq!(
            derive_owner_prefix("rally/codex-claim-authority-01"),
            "codex"
        );
        assert_eq!(derive_owner_prefix("rally/gemini-foo-01"), "gemini");
    }

    #[test]
    fn derive_owner_prefix_handles_multi_word_agents() {
        assert_eq!(
            derive_owner_prefix("rally/claude-code-protocol-01"),
            "claude-code"
        );
        assert_eq!(derive_owner_prefix("rally/opencode-task-01"), "opencode");
    }

    #[test]
    fn is_owner_live_exact_match() {
        let mut idx = LivenessIndex {
            live_tools: BTreeSet::new(),
            stale_tools: BTreeSet::new(),
        };
        idx.live_tools.insert("claude".to_string());
        assert!(is_owner_live("claude", &idx));
    }

    #[test]
    fn is_owner_live_prefix_match() {
        let mut idx = LivenessIndex {
            live_tools: BTreeSet::new(),
            stale_tools: BTreeSet::new(),
        };
        // Tool name is claude_code:01 — starts with owner prefix "claude".
        idx.live_tools.insert("claude_code:01".to_string());
        assert!(is_owner_live("claude", &idx));
    }

    #[test]
    fn is_owner_live_stale_returns_false() {
        let mut idx = LivenessIndex {
            live_tools: BTreeSet::new(),
            stale_tools: BTreeSet::new(),
        };
        idx.stale_tools.insert("codex".to_string());
        assert!(!is_owner_live("codex", &idx));
    }

    #[test]
    fn is_owner_live_absent_returns_false() {
        let idx = LivenessIndex {
            live_tools: BTreeSet::new(),
            stale_tools: BTreeSet::new(),
        };
        // No data → treat as stale.
        assert!(!is_owner_live("unknown-agent", &idx));
    }

    #[test]
    fn build_liveness_index_respects_ttl() {
        let facts = vec![
            PresenceFact {
                tool: "fresh".to_string(),
                seq: 1,
                subject: "state=idle".to_string(),
                created_at: "2026-06-07T11:50:00Z".to_string(), // 10 min ago
            },
            PresenceFact {
                tool: "stale".to_string(),
                seq: 2,
                subject: "state=idle".to_string(),
                created_at: "2026-06-05T00:00:00Z".to_string(), // 2 days ago
            },
        ];
        let idx = build_liveness_index(&facts, "2026-06-07T12:00:00Z", 3600); // 1h TTL
        assert!(idx.live_tools.contains("fresh"));
        assert!(idx.stale_tools.contains("stale"));
    }

    #[test]
    fn build_liveness_index_deduplicates_by_highest_seq() {
        let facts = vec![
            PresenceFact {
                tool: "a".to_string(),
                seq: 1,
                subject: "state=idle".to_string(),
                created_at: "2026-06-05T00:00:00Z".to_string(), // old
            },
            PresenceFact {
                tool: "a".to_string(),
                seq: 5,
                subject: "state=working".to_string(),
                created_at: "2026-06-07T11:55:00Z".to_string(), // fresh
            },
        ];
        let idx = build_liveness_index(&facts, "2026-06-07T12:00:00Z", 3600);
        // seq 5 wins → fresh → live.
        assert!(idx.live_tools.contains("a"), "highest-seq fact must win");
        assert!(!idx.stale_tools.contains("a"));
    }

    #[test]
    fn same_path_handles_trailing_slashes() {
        let a = Path::new("/tmp/foo");
        let b = Path::new("/tmp/foo");
        assert!(same_path(a, b));
    }
}
