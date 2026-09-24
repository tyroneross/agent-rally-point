// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! A read-only Git snapshot for planning branch updates and merges.
//! Git owns branch/worktree truth; Rally does not maintain a second ledger.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use serde::Serialize;

#[derive(Debug, Serialize)]
pub(crate) struct Plan {
    pub base: String,
    pub base_head: String,
    pub base_worktree: Option<String>,
    pub base_status: Option<String>,
    pub branches: Vec<Branch>,
    pub remote_branches: Vec<RemoteBranch>,
    pub worktrees: Vec<Worktree>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Branch {
    pub name: String,
    pub head: String,
    pub worktree: Option<String>,
    pub worktree_status: Option<String>,
    pub ahead_of_base: u64,
    pub behind_base: u64,
    pub unique_patch_commits: Option<u64>,
    pub relation: &'static str,
    pub upstream: Option<String>,
    pub upstream_kind: &'static str,
    pub upstream_state: &'static str,
    pub ahead_of_upstream: Option<u64>,
    pub behind_upstream: Option<u64>,
    pub upstream_action: &'static str,
    pub next_action: &'static str,
    pub blockers: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RemoteBranch {
    pub name: String,
    pub head: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct Worktree {
    pub path: String,
    pub head: String,
    pub branch: Option<String>,
    pub status: String,
    pub locked: bool,
    pub prunable: bool,
}

#[derive(Debug)]
struct RefRow {
    head: String,
    upstream: Option<String>,
}

pub(crate) fn build(repo: &Path, requested_base: Option<&str>) -> Result<Plan, String> {
    let local = refs(repo, "refs/heads")?;
    let remote = refs(repo, "refs/remotes")?;
    let base = select_base(repo, requested_base, &local)?;
    let base_ref = format!("refs/heads/{base}");
    let base_head = local[&base_ref].head.clone();
    let mut worktrees = list_worktrees(repo)?;
    worktrees.sort_by(|a, b| a.path.cmp(&b.path));
    let by_branch: BTreeMap<String, &Worktree> = worktrees
        .iter()
        .filter_map(|wt| wt.branch.as_ref().map(|branch| (branch.clone(), wt)))
        .collect();
    let base_tree = by_branch.get(&base_ref).copied();
    let base_status = base_tree.map(|wt| wt.status.clone());
    let base_worktree = base_tree.map(|wt| wt.path.clone());

    let existing_refs: BTreeSet<_> = local.keys().chain(remote.keys()).cloned().collect();
    let mut branches = Vec::with_capacity(local.len());
    for (full_ref, row) in &local {
        let name = full_ref.trim_start_matches("refs/heads/").to_string();
        let tree = by_branch.get(full_ref).copied();
        let (behind_base, ahead_of_base) = if full_ref == &base_ref {
            (0, 0)
        } else {
            counts(repo, &base_ref, full_ref)?
        };
        // Patch equivalence is advisory: merge commits can still carry topology
        // or conflict decisions that a patch count does not capture.
        let unique_patch_commits = if ahead_of_base > 0 {
            Some(count_unique_patches(repo, &base_ref, full_ref)?)
        } else {
            None
        };
        let relation = relation(
            full_ref == &base_ref,
            behind_base,
            ahead_of_base,
            unique_patch_commits,
        );
        let upstream = row.upstream.clone();
        let upstream_kind = match upstream.as_deref() {
            Some(value) if value.starts_with("refs/remotes/") => "remote_tracking",
            Some(value) if value.starts_with("refs/heads/") => "local",
            Some(_) => "other",
            None => "none",
        };
        let upstream_state = match upstream.as_deref() {
            None => "none",
            Some(value) if existing_refs.contains(value) => "present",
            Some(_) => "gone",
        };
        let (behind_upstream, ahead_of_upstream) = match upstream.as_deref() {
            Some(value) if upstream_state == "present" => {
                let (behind, ahead) = counts(repo, value, full_ref)?;
                (Some(behind), Some(ahead))
            }
            _ => (None, None),
        };
        let upstream_action = upstream_action(
            upstream_state,
            upstream_kind,
            behind_upstream,
            ahead_of_upstream,
        );
        let mut blockers = Vec::new();
        // A dirty checkout is inventory, not a blocker, when no base-relative
        // update or merge is being recommended.
        if !matches!(relation, "base" | "in_sync") && tree.is_some_and(|wt| wt.status != "clean") {
            blockers.push("source_worktree_not_clean");
        }
        if matches!(
            relation,
            "merge_candidate" | "diverged" | "review_patch_equivalent"
        ) {
            match base_tree {
                None => blockers.push("base_not_checked_out"),
                Some(wt) if wt.status != "clean" => blockers.push("base_worktree_not_clean"),
                _ => {}
            }
        }
        let next_action = action(relation, &blockers);
        branches.push(Branch {
            name,
            head: row.head.clone(),
            worktree: tree.map(|wt| wt.path.clone()),
            worktree_status: tree.map(|wt| wt.status.clone()),
            ahead_of_base,
            behind_base,
            unique_patch_commits,
            relation,
            upstream: upstream.map(|value| short_ref(&value)),
            upstream_kind,
            upstream_state,
            ahead_of_upstream,
            behind_upstream,
            upstream_action,
            next_action,
            blockers,
        });
    }
    let remote_branches = remote
        .into_iter()
        .filter(|(name, _)| !name.ends_with("/HEAD"))
        .map(|(name, row)| RemoteBranch {
            name: short_ref(&name),
            head: row.head,
        })
        .collect();
    Ok(Plan {
        base,
        base_head,
        base_worktree,
        base_status,
        branches,
        remote_branches,
        worktrees,
    })
}

fn relation(is_base: bool, behind: u64, ahead: u64, unique: Option<u64>) -> &'static str {
    if is_base {
        "base"
    } else if ahead == 0 && behind == 0 {
        "in_sync"
    } else if ahead == 0 {
        "integrated"
    } else if unique == Some(0) {
        "review_patch_equivalent"
    } else if behind == 0 {
        "merge_candidate"
    } else {
        "diverged"
    }
}

fn action(relation: &str, blockers: &[&str]) -> &'static str {
    if !blockers.is_empty() {
        return "resolve_blockers";
    }
    match relation {
        "base" | "in_sync" => "none",
        "integrated" => "update_or_close_branch",
        "merge_candidate" => "merge_into_base",
        "review_patch_equivalent" => "review_equivalence",
        _ => "reconcile_divergence",
    }
}

fn upstream_action(
    state: &str,
    kind: &str,
    behind: Option<u64>,
    ahead: Option<u64>,
) -> &'static str {
    match (state, kind, behind, ahead) {
        ("gone", _, _, _) => "repair_upstream",
        ("present", _, Some(0), Some(0)) => "none",
        ("present", "remote_tracking", Some(0), Some(_)) => "push_branch",
        ("present", "local", Some(0), Some(_)) => "merge_into_upstream",
        ("present", _, Some(0), Some(_)) => "review_upstream",
        ("present", _, Some(_), Some(0)) => "update_from_upstream",
        ("present", _, Some(_), Some(_)) => "reconcile_upstream",
        _ => "none",
    }
}

fn select_base(
    repo: &Path,
    requested: Option<&str>,
    local: &BTreeMap<String, RefRow>,
) -> Result<String, String> {
    if let Some(base) = requested {
        let full = format!("refs/heads/{base}");
        return local
            .contains_key(&full)
            .then(|| base.to_string())
            .ok_or_else(|| format!("local base branch {base:?} does not exist"));
    }
    if let Ok(value) = git(
        repo,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    ) {
        let remote_head = String::from_utf8(value).map_err(|e| e.to_string())?;
        if let Some(name) = remote_head.trim().strip_prefix("refs/remotes/origin/")
            && local.contains_key(&format!("refs/heads/{name}"))
        {
            return Ok(name.to_string());
        }
    }
    for name in ["main", "master", "trunk"] {
        if local.contains_key(&format!("refs/heads/{name}")) {
            return Ok(name.to_string());
        }
    }
    Err("cannot determine a local base branch; pass --base BRANCH".into())
}

fn refs(repo: &Path, prefix: &str) -> Result<BTreeMap<String, RefRow>, String> {
    let bytes = git(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(upstream)",
            prefix,
        ],
    )?;
    let text = String::from_utf8(bytes).map_err(|e| format!("non-UTF-8 Git ref output: {e}"))?;
    let mut result = BTreeMap::new();
    for line in text.lines() {
        let fields: Vec<_> = line.split('\0').collect();
        if fields.len() != 3 || fields[0].is_empty() || fields[1].is_empty() {
            return Err("malformed Git for-each-ref output".into());
        }
        result.insert(
            fields[0].to_string(),
            RefRow {
                head: fields[1].to_string(),
                upstream: (!fields[2].is_empty()).then(|| fields[2].to_string()),
            },
        );
    }
    Ok(result)
}

fn list_worktrees(repo: &Path) -> Result<Vec<Worktree>, String> {
    let bytes = git(repo, &["worktree", "list", "--porcelain", "-z"])?;
    let text = String::from_utf8(bytes).map_err(|e| format!("non-UTF-8 worktree path: {e}"))?;
    let mut result = Vec::new();
    let mut path = None;
    let mut head = None;
    let mut branch = None;
    let mut locked = false;
    let mut prunable = false;
    for field in text.split('\0') {
        if field.is_empty() {
            if path.is_some() || head.is_some() || branch.is_some() {
                let path = path
                    .take()
                    .ok_or("malformed Git worktree entry: missing path")?;
                let head = head
                    .take()
                    .ok_or("malformed Git worktree entry: missing HEAD")?;
                let status = if !Path::new(&path).exists() {
                    "unavailable".to_string()
                } else {
                    match git(
                        Path::new(&path),
                        &["status", "--porcelain=v1", "-z", "--untracked-files=normal"],
                    ) {
                        Ok(bytes) if bytes.is_empty() => "clean".to_string(),
                        Ok(_) => "dirty".to_string(),
                        Err(_) => "unknown".to_string(),
                    }
                };
                result.push(Worktree {
                    path,
                    head,
                    branch: branch.take(),
                    status,
                    locked,
                    prunable,
                });
            }
            locked = false;
            prunable = false;
            continue;
        }
        if let Some(value) = field.strip_prefix("worktree ") {
            path = Some(value.to_string());
        } else if let Some(value) = field.strip_prefix("HEAD ") {
            head = Some(value.to_string());
        } else if let Some(value) = field.strip_prefix("branch ") {
            branch = Some(value.to_string());
        } else if field.starts_with("locked") {
            locked = true;
        } else if field.starts_with("prunable") {
            prunable = true;
        }
    }
    Ok(result)
}

fn counts(repo: &Path, left: &str, right: &str) -> Result<(u64, u64), String> {
    let range = format!("{left}...{right}");
    let bytes = git(repo, &["rev-list", "--left-right", "--count", &range])?;
    let text = String::from_utf8(bytes).map_err(|e| e.to_string())?;
    let mut values = text.split_whitespace().map(str::parse::<u64>);
    match (values.next(), values.next(), values.next()) {
        (Some(Ok(left)), Some(Ok(right)), None) => Ok((left, right)),
        _ => Err("malformed Git rev-list count".into()),
    }
}

fn count_unique_patches(repo: &Path, base: &str, branch: &str) -> Result<u64, String> {
    let range = format!("{base}...{branch}");
    let bytes = git(
        repo,
        &[
            "rev-list",
            "--cherry-pick",
            "--right-only",
            "--no-merges",
            "--count",
            &range,
        ],
    )?;
    String::from_utf8(bytes)
        .map_err(|e| e.to_string())?
        .trim()
        .parse::<u64>()
        .map_err(|e| format!("malformed Git unique-patch count: {e}"))
}

fn short_ref(value: &str) -> String {
    value
        .strip_prefix("refs/remotes/")
        .or_else(|| value.strip_prefix("refs/heads/"))
        .unwrap_or(value)
        .to_string()
}

fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut command = Command::new("git");
    command
        .args(["-c", "core.fsmonitor=false", "-C"])
        .arg(repo)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
    // Hook-inherited Git variables can override -C and inspect another repo.
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_PREFIX",
        "GIT_OBJECT_DIRECTORY",
        "GIT_COMMON_DIR",
        "GIT_QUARANTINE_PATH",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CEILING_DIRECTORIES",
        "GIT_NAMESPACE",
    ] {
        command.env_remove(key);
    }
    let output = command
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}
