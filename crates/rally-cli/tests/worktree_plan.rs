// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

mod common;
mod support;
use common::test_git_fixture::fixture_git;
use support::rally_cmd::rally_command;
use support::temp_root::TempRoot;

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    _root: TempRoot,
}

impl Fixture {
    fn new() -> Self {
        let root = TempRoot::new("rally-worktree-plan");
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        fixture_git(&repo, &["init", "-q", "-b", "main"]);
        fixture_git(&repo, &["commit", "--allow-empty", "-m", "initial"]);
        Self {
            root: root.canonicalize().unwrap(),
            repo: repo.canonicalize().unwrap(),
            _root: root,
        }
    }

    fn worktree(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        fixture_git(
            &self.repo,
            &[
                "worktree",
                "add",
                "-b",
                name,
                path.to_str().unwrap(),
                "HEAD",
            ],
        );
        path
    }

    fn plan(&self) -> Value {
        self.plan_from(&self.repo)
    }

    fn plan_from(&self, dir: &Path) -> Value {
        let output = rally_command()
            .args(["worktree", "plan", "--json"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn branch<'a>(plan: &'a Value, name: &str) -> &'a Value {
    plan["data"]["worktree_plan"]["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .unwrap()
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// A `tmux` stand-in placed first on PATH, so closeout's liveness probe is
/// decided by the fixture, not by whatever tmux server this machine runs.
/// `live` lists the given session names; empty `live` answers as tmux does
/// with no server, which proves the session's process is gone.
fn fake_tmux_path(fixture: &Fixture, name: &str, live: &[&str]) -> std::ffi::OsString {
    use std::os::unix::fs::PermissionsExt;
    let dir = fixture.root.join(name);
    fs::create_dir_all(&dir).unwrap();
    let script = if live.is_empty() {
        "#!/bin/sh\necho 'no server running on /nonexistent' >&2\nexit 1\n".to_string()
    } else {
        let lines: String = live
            .iter()
            .map(|session| format!("echo '{session} @1 %1'\n"))
            .collect();
        format!("#!/bin/sh\n{lines}")
    };
    let bin = dir.join("tmux");
    fs::write(&bin, script).unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![dir];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    std::env::join_paths(paths).unwrap()
}

fn managed_run(fixture: &Fixture, name: &str, extra: &[&str]) -> Value {
    let run = rally_command()
        .args([
            "run",
            "codex",
            "--json",
            "--name",
            name,
            "--backend",
            "tmux",
            "--tmux-bin",
            "/usr/bin/true",
        ])
        .args(extra)
        .env("HOME", &fixture.root)
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let run: Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(run["command"], "run", "setup must finish: {run}");
    run["data"]["run"]["session"].clone()
}

fn closeout_rows(fixture: &Fixture, path: &std::ffi::OsStr) -> Vec<Value> {
    let closeout = rally_command()
        .args(["worktree", "closeout", "--json"])
        .env("HOME", &fixture.root)
        .env("PATH", path)
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    assert!(
        closeout.status.success(),
        "{}",
        String::from_utf8_lossy(&closeout.stderr)
    );
    let closeout: Value = serde_json::from_slice(&closeout.stdout).unwrap();
    closeout["data"]["worktree_closeout"]["worktrees"]
        .as_array()
        .unwrap()
        .clone()
}

fn rally_files(fixture: &Fixture) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn files(dir: &Path, out: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_symlink() {
                continue;
            }
            if path.is_dir() {
                // A managed worktree is a Git checkout, not Rally state.
                if path.file_name().is_some_and(|n| n == "worktrees") {
                    continue;
                }
                files(&path, out);
            } else {
                out.insert(path.clone(), fs::read(path).unwrap());
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    files(&fixture.repo.join(".rally"), &mut out);
    out
}

/// A detached worktree has no branch, and a session started without a
/// worktree has none either. That shared absence is not a branch match: the
/// detached tree is nobody's, however many branchless sessions are live.
#[test]
fn closeout_does_not_assign_a_detached_worktree_to_a_branchless_session() {
    if !git_available() {
        return;
    }
    let fixture = Fixture::new();
    let session = managed_run(&fixture, "shared", &["--no-worktree"]);
    assert!(
        session["branch"].is_null(),
        "fixture needs a session with no branch: {session}"
    );
    let detached = fixture.root.join("detached");
    fixture_git(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "--detach",
            detached.to_str().unwrap(),
            "HEAD",
        ],
    );
    let path = fake_tmux_path(
        &fixture,
        "tmux-live",
        &[session["target"].as_str().unwrap()],
    );
    let rows = closeout_rows(&fixture, &path);
    let row = rows
        .iter()
        .find(|row| row["path"] == detached.to_str().unwrap())
        .unwrap();
    assert_eq!(row["branch_state"], "detached");
    assert_eq!(row["ownership"]["availability"], "current", "{row}");
    assert_eq!(row["ownership"]["status"], "no_live_owner_observed");
    assert!(row["ownership"]["session_id"].is_null());
}

/// A session whose process is gone never wrote its stopped record. Closeout
/// must stop reporting it as the owner, and must not write that record itself.
#[test]
fn closeout_ignores_a_session_whose_process_is_gone_without_writing() {
    if !git_available() {
        return;
    }
    let fixture = Fixture::new();
    let session = managed_run(&fixture, "gone", &[]);
    let worktree = session["worktree_path"].as_str().unwrap().to_string();
    let session_id = session["session_id"].as_str().unwrap().to_string();
    let target = session["target"].as_str().unwrap().to_string();
    let find = |rows: &[Value]| {
        rows.iter()
            .find(|row| row["path"] == worktree.as_str())
            .unwrap()
            .clone()
    };

    let live = find(&closeout_rows(
        &fixture,
        &fake_tmux_path(&fixture, "tmux-live", &[&target]),
    ));
    assert_eq!(live["ownership"]["status"], "live_owner", "{live}");
    assert_eq!(live["ownership"]["session_id"], session_id.as_str());

    let before = rally_files(&fixture);
    let gone = find(&closeout_rows(
        &fixture,
        &fake_tmux_path(&fixture, "tmux-gone", &[]),
    ));
    assert_eq!(before, rally_files(&fixture), "closeout must not write");
    assert!(gone["ownership"]["session_id"].is_null(), "{gone}");
    assert_eq!(gone["ownership"]["status"], "no_live_owner_observed");
    assert_eq!(gone["ownership"]["availability"], "current");
    assert_eq!(
        gone["ownership"]["process_gone_session_ids"],
        serde_json::json!([session_id])
    );
    assert_eq!(
        live["ownership"]["process_gone_session_ids"],
        serde_json::json!([])
    );
}

#[test]
fn closeout_reports_unavailable_ownership_without_creating_a_room() {
    if !git_available() {
        return;
    }
    let fixture = Fixture::new();
    let feature = fixture.worktree("feature");
    let output = rally_command()
        .args(["worktree", "closeout", "--json"])
        .current_dir(&feature)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["schema"], "agent-rally.command.worktree-closeout.v1");
    let payload = &result["data"]["worktree_closeout"];
    assert_eq!(payload["schema_version"], 1);
    assert_eq!(payload["sources"]["rally"], "unavailable");
    let row = payload["worktrees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["path"] == feature.to_str().unwrap())
        .unwrap();
    assert_eq!(row["ownership"]["status"], "unknown");
    assert_eq!(row["ownership"]["availability"], "unavailable");
    assert_eq!(row["merge_target"]["status"], "unknown");
    assert_eq!(row["disposition"]["action"], "retain");
    assert!(
        !fixture.repo.join(".rally").exists(),
        "read-only closeout must not create a Rally room"
    );
}

#[test]
fn closeout_does_not_mutate_an_existing_room() {
    if !git_available() {
        return;
    }
    let fixture = Fixture::new();
    let entered = rally_command()
        .args(["enter", "--tool", "test:closeout-owner", "--json"])
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    assert!(
        entered.status.success(),
        "{}",
        String::from_utf8_lossy(&entered.stderr)
    );
    let rally_dir = fixture.repo.join(".rally");
    assert!(rally_dir.join("log").exists());
    fn files(dir: &Path, out: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files(&path, out);
            } else {
                out.insert(path.clone(), fs::read(path).unwrap());
            }
        }
    }
    let mut before = std::collections::BTreeMap::new();
    files(&rally_dir, &mut before);
    let output = rally_command()
        .args(["worktree", "closeout", "--json"])
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        result["data"]["worktree_closeout"]["sources"]["rally"],
        "current"
    );
    let mut after = std::collections::BTreeMap::new();
    files(&rally_dir, &mut after);
    assert_eq!(before, after, "closeout must not mutate Rally files");
}

#[test]
fn closeout_marks_moved_managed_worktree_ambiguous_at_both_paths() {
    if !git_available() {
        return;
    }
    let fixture = Fixture::new();
    let run = rally_command()
        .args([
            "run",
            "codex",
            "--json",
            "--name",
            "moving",
            "--backend",
            "tmux",
            "--tmux-bin",
            "/usr/bin/true",
        ])
        .env("HOME", &fixture.root)
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let run: Value = serde_json::from_slice(&run.stdout).unwrap();
    // This suite tests worktree ownership, not the production watchdog. A
    // committed-timeout receipt is not evidence that fixture setup finished.
    assert_eq!(run["command"], "run", "setup must finish: {run}");
    let old_path = PathBuf::from(
        run["data"]["run"]["session"]["worktree_path"]
            .as_str()
            .unwrap(),
    );
    let branch = run["data"]["run"]["session"]["branch"].as_str().unwrap();
    let moved_path = fixture.root.join("moved-managed");
    fixture_git(
        &fixture.repo,
        &[
            "worktree",
            "move",
            old_path.to_str().unwrap(),
            moved_path.to_str().unwrap(),
        ],
    );
    // The session is live: a moved worktree is ambiguous only while its
    // session still runs (a session whose process is gone is set aside).
    let target = run["data"]["run"]["session"]["target"].as_str().unwrap();
    let rows = closeout_rows(&fixture, &fake_tmux_path(&fixture, "tmux-live", &[target]));
    let old = rows
        .iter()
        .find(|row| row["path"] == old_path.to_str().unwrap())
        .unwrap();
    let moved = rows
        .iter()
        .find(|row| row["path"] == moved_path.to_str().unwrap())
        .unwrap();
    assert_eq!(old["ownership"]["availability"], "ambiguous");
    assert_eq!(moved["ownership"]["availability"], "ambiguous");
    assert_eq!(moved["branch"], format!("refs/heads/{branch}"));
    assert_eq!(old["disposition"]["action"], "retain");
    assert_eq!(moved["disposition"]["action"], "retain");
}

#[test]
fn plans_every_branch_and_worktree_without_writing_state() {
    if !git_available() {
        return;
    }
    let fixture = Fixture::new();
    let feature = fixture.worktree("feature");
    fs::write(feature.join("feature.txt"), "feature\n").unwrap();
    fixture_git(&feature, &["add", "feature.txt"]);
    fixture_git(&feature, &["commit", "-m", "feature"]);
    fixture_git(&fixture.repo, &["branch", "unattached"]);
    let detached = fixture.root.join("detached");
    fixture_git(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "--detach",
            detached.to_str().unwrap(),
            "HEAD",
        ],
    );
    fixture_git(
        &fixture.repo,
        &["update-ref", "refs/remotes/origin/remote-only", "HEAD"],
    );

    let first = fixture.plan();
    let report = &first["data"]["worktree_plan"];
    assert_eq!(first["schema"], "agent-rally.command.worktree-plan.v1");
    assert_eq!(report["base"], "main");
    assert_eq!(report["branches"].as_array().unwrap().len(), 3);
    assert_eq!(report["worktrees"].as_array().unwrap().len(), 3);
    assert_eq!(branch(&first, "feature")["relation"], "merge_candidate");
    assert_eq!(branch(&first, "feature")["next_action"], "merge_into_base");
    fixture_git(
        &fixture.repo,
        &["worktree", "lock", feature.to_str().unwrap()],
    );
    let locked = fixture.plan();
    assert_eq!(branch(&locked, "feature")["next_action"], "merge_into_base");
    assert!(
        locked["data"]["worktree_plan"]["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["path"] == feature.to_str().unwrap() && row["locked"] == true)
    );
    fixture_git(
        &fixture.repo,
        &["worktree", "unlock", feature.to_str().unwrap()],
    );
    assert_eq!(branch(&first, "unattached")["relation"], "in_sync");
    assert!(
        report["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["branch"].is_null())
    );
    assert!(
        report["remote_branches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "origin/remote-only")
    );
    assert_eq!(
        first,
        fixture.plan(),
        "unchanged Git state must yield the same plan"
    );
    assert_eq!(first, fixture.plan_from(&feature));
    assert!(!fixture.repo.join(".rally").exists());

    let other = fixture.root.join("other");
    fs::create_dir_all(&other).unwrap();
    fixture_git(&other, &["init", "-q", "-b", "main"]);
    let inherited_git_dir = rally_command()
        .args(["worktree", "plan", "--json"])
        .current_dir(&fixture.repo)
        .env("GIT_DIR", other.join(".git"))
        .output()
        .unwrap();
    assert!(inherited_git_dir.status.success());
    assert_eq!(
        first,
        serde_json::from_slice::<Value>(&inherited_git_dir.stdout).unwrap()
    );

    fs::write(fixture.repo.join("uncommitted.txt"), "keep this work").unwrap();
    let dirty = fixture.plan();
    assert_eq!(dirty["data"]["worktree_plan"]["base_status"], "dirty");
    assert_eq!(branch(&dirty, "main")["next_action"], "none");
    assert!(
        branch(&dirty, "main")["blockers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(branch(&dirty, "feature")["next_action"], "resolve_blockers");
    assert!(
        branch(&dirty, "feature")["blockers"]
            .as_array()
            .unwrap()
            .contains(&Value::from("base_worktree_not_clean"))
    );

    let sync = fixture.root.join("sync");
    fixture_git(
        &fixture.repo,
        &["worktree", "add", sync.to_str().unwrap(), "unattached"],
    );
    fs::write(sync.join("uncommitted.txt"), "keep this too").unwrap();
    let dirty_sync = fixture.plan();
    assert_eq!(branch(&dirty_sync, "unattached")["relation"], "in_sync");
    assert_eq!(
        branch(&dirty_sync, "unattached")["worktree_status"],
        "dirty"
    );
    assert_eq!(branch(&dirty_sync, "unattached")["next_action"], "none");
    assert!(
        branch(&dirty_sync, "unattached")["blockers"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    fixture_git(&fixture.repo, &["add", "uncommitted.txt"]);
    fixture_git(&fixture.repo, &["commit", "-m", "main update"]);
    let diverged = fixture.plan();
    assert_eq!(branch(&diverged, "feature")["relation"], "diverged");
    assert_eq!(
        branch(&diverged, "feature")["next_action"],
        "reconcile_divergence"
    );

    fs::remove_dir_all(&detached).unwrap();
    let missing = fixture.plan();
    let missing_tree = missing["data"]["worktree_plan"]["worktrees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["path"] == detached.to_str().unwrap())
        .unwrap();
    assert_eq!(missing_tree["status"], "unavailable");
    assert_eq!(missing_tree["prunable"], true);
}

#[test]
fn tracks_upstream_changes_and_rejects_an_unknown_base() {
    if !git_available() {
        return;
    }
    let fixture = Fixture::new();
    let feature = fixture.worktree("feature");
    fixture_git(
        &fixture.repo,
        &["remote", "add", "origin", fixture.repo.to_str().unwrap()],
    );
    fixture_git(
        &fixture.repo,
        &["update-ref", "refs/remotes/origin/feature", "HEAD"],
    );
    fixture_git(
        &fixture.repo,
        &["branch", "--set-upstream-to=origin/feature", "feature"],
    );
    fixture_git(&feature, &["commit", "--allow-empty", "-m", "local work"]);
    let plan = fixture.plan();
    assert_eq!(branch(&plan, "feature")["upstream_state"], "present");
    assert_eq!(branch(&plan, "feature")["ahead_of_upstream"], 1);
    assert_eq!(branch(&plan, "feature")["upstream_action"], "push_branch");

    fixture_git(
        &fixture.repo,
        &["update-ref", "-d", "refs/remotes/origin/feature"],
    );
    let gone = fixture.plan();
    assert_eq!(branch(&gone, "feature")["upstream_state"], "gone");
    assert_eq!(
        branch(&gone, "feature")["upstream_action"],
        "repair_upstream"
    );

    fixture_git(
        &fixture.repo,
        &["branch", "--set-upstream-to=main", "feature"],
    );
    let local_upstream = fixture.plan();
    assert_eq!(branch(&local_upstream, "feature")["upstream_kind"], "local");
    assert_eq!(
        branch(&local_upstream, "feature")["upstream_action"],
        "merge_into_upstream"
    );

    let output = rally_command()
        .args(["worktree", "plan", "--base", "missing", "--json"])
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    assert!(!output.status.success());
}
