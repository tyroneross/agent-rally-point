// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct Workspace {
    cwd: PathBuf,
    home: PathBuf,
    /// When true, passes RALLY_GLOBAL_INDEX=1 to every command (opt-in for
    /// tests that exercise cross-repo status).
    global_index: bool,
    workspace_root: Option<PathBuf>,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let cwd = temp_path(&format!("{name}-cwd"));
        let home = temp_path(&format!("{name}-home"));
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(cwd.join(".git")).unwrap();
        Self {
            cwd,
            home,
            global_index: false,
            workspace_root: None,
        }
    }

    /// Create a workspace sharing an existing home directory.
    fn new_with_home(name: &str, home: PathBuf) -> Self {
        let cwd = temp_path(&format!("{name}-cwd"));
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(cwd.join(".git")).unwrap();
        Self {
            cwd,
            home,
            global_index: false,
            workspace_root: None,
        }
    }

    /// Create a workspace under an explicit parent directory while sharing HOME.
    fn new_with_home_in(name: &str, parent: &Path, home: PathBuf) -> Self {
        let cwd = parent.join(format!("{name}-cwd"));
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(cwd.join(".git")).unwrap();
        Self {
            cwd,
            home,
            global_index: false,
            workspace_root: None,
        }
    }

    /// Enable RALLY_GLOBAL_INDEX=1 for all commands run through this workspace.
    fn with_global_index(mut self) -> Self {
        self.global_index = true;
        self
    }

    /// Bound cross-repo status to a specific workspace root.
    fn with_workspace_root(mut self, workspace_root: PathBuf) -> Self {
        self.workspace_root = Some(workspace_root);
        self
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.output(args);
        assert!(
            output.status.success(),
            "command {:?} failed\nstdout: {}\nstderr: {}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn output(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rally"));
        cmd.current_dir(&self.cwd).env("HOME", &self.home);
        if self.global_index {
            cmd.env("RALLY_GLOBAL_INDEX", "1");
        }
        if let Some(workspace_root) = &self.workspace_root {
            cmd.env("RALLY_WORKSPACE_ROOT", workspace_root);
        }
        cmd.args(args).output().unwrap()
    }

    fn cleanup(self) {
        fs::remove_dir_all(&self.cwd).ok();
        fs::remove_dir_all(&self.home).ok();
    }
}

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn json_error(output: &Output) -> Value {
    assert_eq!(output.status.code(), Some(2));
    serde_json::from_slice(&output.stderr).unwrap()
}

#[test]
fn subcommand_help_handles_positionals_without_panicking() {
    let workspace = Workspace::new("rally-subcommand-help-positionals");

    for command in [
        "say", "check", "locate", "run", "inject", "attach", "capture", "stop",
    ] {
        let output = workspace.output(&[command, "--help"]);
        assert!(
            output.status.success(),
            "{command} --help failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("Usage"),
            "{command} --help did not print usage"
        );
    }

    workspace.cleanup();
}

#[test]
fn top_level_help_and_docs_advertise_ptyd_backend() {
    let workspace = Workspace::new("rally-help-ptyd-backend");

    let output = workspace.output(&["--help"]);
    assert!(
        output.status.success(),
        "rally --help failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(
        help.contains("--backend <auto|tmux|cmux|ptyd>"),
        "top-level help must list every supported run backend; help:\n{help}"
    );

    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("rally-cli crate should live under <repo>/crates/rally-cli");
    let readme = fs::read_to_string(repo.join("README.md")).expect("README.md");
    assert!(
        readme.contains("rally run --backend <auto|tmux|cmux|ptyd>"),
        "README must document ptyd and auto as supported run backends"
    );
    let rally = fs::read_to_string(repo.join("RALLY.md")).expect("RALLY.md");
    assert!(
        rally.contains("Run backends are `auto`, `tmux`, `cmux`, and `ptyd`."),
        "RALLY.md must document the supported run backends"
    );

    workspace.cleanup();
}

#[test]
fn hooks_command_toggles_repo_config_and_prompt_mode() {
    let workspace = Workspace::new("rally-hooks-config");

    let initial = workspace.json(&["hooks", "status", "--json"]);
    assert_eq!(initial["data"]["hooks"]["enabled"], true);
    assert_eq!(initial["data"]["hooks"]["prompt"], "once");

    let off = workspace.json(&["hooks", "off", "--scope", "repo", "--json"]);
    assert_eq!(off["data"]["hooks"]["scope"], "repo");
    assert_eq!(off["data"]["hooks"]["enabled"], false);

    let prompt = workspace.json(&["hooks", "prompt", "--off", "--json"]);
    assert_eq!(prompt["data"]["hooks"]["scope"], "repo");
    assert_eq!(prompt["data"]["hooks"]["prompt"], "off");

    let after = workspace.json(&["hooks", "status", "--json"]);
    assert_eq!(after["data"]["hooks"]["enabled"], false);
    assert_eq!(after["data"]["hooks"]["enabled_source"], "repo");
    assert_eq!(after["data"]["hooks"]["prompt"], "off");
    assert_eq!(after["data"]["hooks"]["prompt_source"], "repo");

    let config = fs::read_to_string(workspace.cwd.join(".rally/config.json")).unwrap();
    assert!(
        config.contains("\"enabled\": false") && config.contains("\"prompt\": \"off\""),
        "repo hook config did not persist expected fields:\n{config}"
    );

    workspace.cleanup();
}

#[test]
fn bounded_numeric_flags_reject_out_of_range_values() {
    let workspace = Workspace::new("rally-bounded-numeric-flags");

    let high = workspace.output(&["next", "--json", "--tool", "codex", "--limit", "999"]);
    let body = json_error(&high);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("--limit must be between 1 and 20")
    );

    let low = workspace.output(&["capture", "missing-session", "--json", "--lines", "0"]);
    let body = json_error(&low);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("--lines must be between 1 and 2000")
    );

    workspace.cleanup();
}

#[test]
fn aliased_flags_reject_both_names_instead_of_dropping_one() {
    let workspace = Workspace::new("rally-aliased-flags");

    let target = workspace.output(&[
        "say",
        "handoff",
        "--json",
        "--tool",
        "codex",
        "--target",
        "claude",
        "--to",
        "other",
        "--subject",
        "ambiguous target",
    ]);
    let body = json_error(&target);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("cannot use --target and --to together")
    );

    let handoff = workspace.output(&[
        "inject",
        "some-session",
        "--json",
        "--handoff",
        "fact_a",
        "--ref",
        "fact_b",
    ]);
    let body = json_error(&handoff);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("cannot use --handoff and --ref together")
    );

    workspace.cleanup();
}

/// `rally status --help` and `rally status --global --help` must exit 0 (no
/// bpaf positional-rightmost panic). `status` has no positionals so it must
/// never panic, but we assert the specific exit codes as a regression guard.
#[test]
fn status_help_flags_exit_zero() {
    let workspace = Workspace::new("rally-status-help");

    let out_help = workspace.output(&["status", "--help"]);
    assert!(
        out_help.status.success(),
        "rally status --help exited non-zero ({})\nstdout: {}\nstderr: {}",
        out_help.status,
        String::from_utf8_lossy(&out_help.stdout),
        String::from_utf8_lossy(&out_help.stderr)
    );

    let out_global_help = workspace.output(&["status", "--global", "--help"]);
    assert!(
        out_global_help.status.success(),
        "rally status --global --help exited non-zero ({})\nstdout: {}\nstderr: {}",
        out_global_help.status,
        String::from_utf8_lossy(&out_global_help.stdout),
        String::from_utf8_lossy(&out_global_help.stderr)
    );

    workspace.cleanup();
}

/// Core acceptance test for Component C (central rollup).
///
/// - Sets up two isolated repo rooms (repo_a and repo_b) sharing one HOME.
/// - Enters each room as a distinct tool and claims work in each.
/// - Runs `rally status --global --json` from repo_a.
/// - Asserts both repos appear in the output with correct `lead` and
///   `open_claims`.
/// - Asserts repo A's claims are never attributed to repo B.
/// - Asserts NO new fact was written to either ledger after the status read
///   (fact count is unchanged).
#[test]
fn status_global_aggregates_two_repos_without_writing_facts() {
    let home = temp_path("rally-status-global-home");
    fs::create_dir_all(&home).unwrap();

    // --- Repo A ---
    // B17: global index is opt-in; set RALLY_GLOBAL_INDEX=1 so enter writes to
    // the cross-repo index and status --global can see both repos.
    let repo_a =
        Workspace::new_with_home("rally-status-global-repo-a", home.clone()).with_global_index();
    // Enter as tool_a (first enter → lead)
    let enter_a = repo_a.json(&["enter", "--tool", "tool_a", "--json"]);
    assert_eq!(
        enter_a["data"]["enter"]["tool"], "tool_a",
        "repo_a enter failed"
    );
    // Add two open claims in repo_a
    repo_a.json(&[
        "say",
        "claim",
        "--tool",
        "tool_a",
        "--subject",
        "work alpha-1",
        "--json",
    ]);
    repo_a.json(&[
        "say",
        "claim",
        "--tool",
        "tool_a",
        "--subject",
        "work alpha-2",
        "--json",
    ]);

    // --- Repo B ---
    let repo_b =
        Workspace::new_with_home("rally-status-global-repo-b", home.clone()).with_global_index();
    // Enter as tool_b (first enter → lead)
    let enter_b = repo_b.json(&["enter", "--tool", "tool_b", "--json"]);
    assert_eq!(
        enter_b["data"]["enter"]["tool"], "tool_b",
        "repo_b enter failed"
    );
    // Add one open claim in repo_b
    repo_b.json(&[
        "say",
        "claim",
        "--tool",
        "tool_b",
        "--subject",
        "work beta-1",
        "--json",
    ]);

    // Snapshot fact counts before reading status.
    let room_a_before = repo_a.json(&["room", "--json"]);
    let room_b_before = repo_b.json(&["room", "--json"]);
    let claims_a_before = room_a_before["data"]["room"]["active_claims"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0);
    let claims_b_before = room_b_before["data"]["room"]["active_claims"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0);
    assert_eq!(
        claims_a_before, 2,
        "repo_a should have 2 open claims before status"
    );
    assert_eq!(
        claims_b_before, 1,
        "repo_b should have 1 open claim before status"
    );

    // Run `rally status --global --json` from repo_a's cwd.
    let status = repo_a.json(&["status", "--global", "--json"]);
    assert_eq!(status["ok"], true);
    assert_eq!(status["command"], "status");

    let repos = status["data"]["status"]["repos"]
        .as_array()
        .expect("data.status.repos must be an array");

    // Both repos must appear.
    assert!(
        repos.len() >= 2,
        "expected at least 2 repos in status, got {}: {repos:#?}",
        repos.len()
    );

    // Use canonical paths for comparison — on macOS /tmp is a symlink to
    // /private/tmp, so the registry stores the resolved path.
    let repo_a_canonical = fs::canonicalize(&repo_a.cwd)
        .unwrap_or_else(|_| repo_a.cwd.clone())
        .to_string_lossy()
        .to_string();
    let repo_b_canonical = fs::canonicalize(&repo_b.cwd)
        .unwrap_or_else(|_| repo_b.cwd.clone())
        .to_string_lossy()
        .to_string();

    let entry_a = repos
        .iter()
        .find(|r| {
            r["repo"]
                .as_str()
                .map(|p| p == repo_a_canonical)
                .unwrap_or(false)
        })
        .unwrap_or_else(|| panic!("repo_a ({repo_a_canonical}) not found in status: {repos:#?}"));

    let entry_b = repos
        .iter()
        .find(|r| {
            r["repo"]
                .as_str()
                .map(|p| p == repo_b_canonical)
                .unwrap_or(false)
        })
        .unwrap_or_else(|| panic!("repo_b ({repo_b_canonical}) not found in status: {repos:#?}"));

    assert!(
        entry_a["workspace_root"].as_str().is_some(),
        "repo_a status must include workspace_root: {entry_a:#?}"
    );
    assert!(
        entry_a["workspace_id"].as_str().is_some(),
        "repo_a status must include workspace_id: {entry_a:#?}"
    );

    // repo_a: lead=tool_a, open_claims=2
    assert_eq!(
        entry_a["lead"], "tool_a",
        "repo_a lead must be tool_a; got: {entry_a:#?}"
    );
    assert_eq!(
        entry_a["open_claims"], 2,
        "repo_a open_claims must be 2; got: {entry_a:#?}"
    );

    // repo_b: lead=tool_b, open_claims=1
    assert_eq!(
        entry_b["lead"], "tool_b",
        "repo_b lead must be tool_b; got: {entry_b:#?}"
    );
    assert_eq!(
        entry_b["open_claims"], 1,
        "repo_b open_claims must be 1; got: {entry_b:#?}"
    );

    // Cross-repo isolation: repo_b must not show repo_a's claim count.
    assert_ne!(
        entry_b["open_claims"], 2,
        "repo_b incorrectly shows repo_a's claim count (cross-repo leakage)"
    );

    // Verify NO new facts written to either ledger by the status command.
    let room_a_after = repo_a.json(&["room", "--json"]);
    let room_b_after = repo_b.json(&["room", "--json"]);
    let claims_a_after = room_a_after["data"]["room"]["active_claims"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0);
    let claims_b_after = room_b_after["data"]["room"]["active_claims"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0);
    assert_eq!(
        claims_a_before, claims_a_after,
        "status wrote a fact to repo_a (claim count changed)"
    );
    assert_eq!(
        claims_b_before, claims_b_after,
        "status wrote a fact to repo_b (claim count changed)"
    );

    // Cleanup.
    repo_a.cleanup();
    repo_b.cleanup();
    fs::remove_dir_all(&home).ok();
}

#[test]
fn status_global_filters_to_current_workspace_boundary() {
    let home = temp_path("rally-status-global-filter-home");
    let workspace_a_root = temp_path("rally-status-global-workspace-a");
    let workspace_b_root = temp_path("rally-status-global-workspace-b");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&workspace_a_root).unwrap();
    fs::create_dir_all(&workspace_b_root).unwrap();

    let repo_a = Workspace::new_with_home_in("repo-a", &workspace_a_root, home.clone())
        .with_global_index()
        .with_workspace_root(workspace_a_root.clone());
    let repo_b = Workspace::new_with_home_in("repo-b", &workspace_b_root, home.clone())
        .with_global_index()
        .with_workspace_root(workspace_b_root.clone());

    repo_a.json(&["enter", "--tool", "tool_a", "--json"]);
    repo_b.json(&["enter", "--tool", "tool_b", "--json"]);

    let index_path = home.join(".agent-rally-point/rooms/v1/index.json");
    let index: Value = serde_json::from_slice(&fs::read(&index_path).unwrap()).unwrap();
    assert_eq!(
        index["rooms"].as_array().map(Vec::len),
        Some(2),
        "global pointer index should know both repos before status filtering"
    );

    let status = repo_a.json(&["status", "--global", "--json"]);
    let repos = status["data"]["status"]["repos"]
        .as_array()
        .expect("data.status.repos must be an array");

    let repo_a_canonical = fs::canonicalize(&repo_a.cwd)
        .unwrap_or_else(|_| repo_a.cwd.clone())
        .to_string_lossy()
        .to_string();
    let repo_b_canonical = fs::canonicalize(&repo_b.cwd)
        .unwrap_or_else(|_| repo_b.cwd.clone())
        .to_string_lossy()
        .to_string();
    let workspace_a_canonical = fs::canonicalize(&workspace_a_root)
        .unwrap_or_else(|_| workspace_a_root.clone())
        .to_string_lossy()
        .to_string();

    assert!(
        repos
            .iter()
            .any(|r| r["repo"].as_str() == Some(repo_a_canonical.as_str())),
        "current workspace repo should be visible: {repos:#?}"
    );
    assert!(
        !repos
            .iter()
            .any(|r| r["repo"].as_str() == Some(repo_b_canonical.as_str())),
        "other workspace repo must be hidden from workspace-scoped status: {repos:#?}"
    );
    assert_eq!(
        status["data"]["status"]["workspace_root"].as_str(),
        Some(workspace_a_canonical.as_str()),
        "status should report the workspace boundary used for filtering"
    );

    repo_a.cleanup();
    repo_b.cleanup();
    fs::remove_dir_all(&workspace_a_root).ok();
    fs::remove_dir_all(&workspace_b_root).ok();
    fs::remove_dir_all(&home).ok();
}

// ── DX: the first command a new user types ──────────────────────────────────

/// `rally --version` used to fail with "unknown Rally command --version",
/// naming neither the working form nor `--help`. An error that does not say
/// what to do next costs a round trip in someone's first minute with the tool.
///
/// Asserts the aliases work AND that `version` itself still does — a fix that
/// silently moved the behaviour would pass a one-sided test.
#[test]
fn version_flag_aliases_resolve_to_the_version_command() {
    let canonical = Command::new(env!("CARGO_BIN_EXE_rally"))
        .arg("version")
        .output()
        .expect("run rally version");
    assert!(
        canonical.status.success(),
        "`rally version` must keep working: {}",
        String::from_utf8_lossy(&canonical.stderr)
    );
    let expected = String::from_utf8_lossy(&canonical.stdout)
        .trim()
        .to_string();
    assert!(
        expected.starts_with("rally "),
        "expected a version line, got: {expected:?}"
    );

    for flag in ["--version", "-V"] {
        let out = Command::new(env!("CARGO_BIN_EXE_rally"))
            .arg(flag)
            .output()
            .unwrap_or_else(|e| panic!("run rally {flag}: {e}"));
        assert!(
            out.status.success(),
            "`rally {flag}` must exit 0, got {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            expected,
            "`rally {flag}` must print exactly what `rally version` prints"
        );
    }
}

/// An unknown command must point somewhere. Naming the bad token alone leaves
/// the user guessing at the command list.
#[test]
fn unknown_command_error_names_help() {
    let out = Command::new(env!("CARGO_BIN_EXE_rally"))
        .arg("frobnicate")
        .output()
        .expect("run rally frobnicate");
    assert!(!out.status.success(), "an unknown command must not exit 0");
    let msg = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        msg.contains("frobnicate"),
        "the error must name the offending token: {msg:?}"
    );
    assert!(
        msg.contains("--help"),
        "the error must tell the user how to find the command list: {msg:?}"
    );
}
