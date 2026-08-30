// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Ten golden tests for `rally hook before-write`, per
//! The native before-write contract's C2 table with
//! the binding plan-critic revisions (R2, R4, R5, R7, R8) folded in.
//!
//! Exactly ten `#[test]` fns. The operator's contract is FEW AND FLAWLESS: a
//! test that flakes is worse than no test, so timing-sensitive assertions are
//! relative (R8) or driven by deterministic debug-only seams
//! (`RALLY_TEST_HOOK_FORCE_DEADLINE`) rather than a sleep+race wherever one
//! sufficed.
//!
//! Several sub-cases exercise the SHELL's early-exec/probe branch
//! (`hooks/rally-coordination-hook.sh`'s C3 chunk). At the time this file was
//! authored C1 had landed but C3 had not, so those sub-cases are expected RED
//! until C3 lands (documented per sub-case below and in the implementer
//! report) rather than weakened to pass — the same posture the plan takes for
//! T-09(ii)'s confirmed live SEC-001 bypass.

use serde_json::{Value, json};
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Once;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BIN: &str = env!("CARGO_BIN_EXE_rally");

fn hook_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hooks/rally-coordination-hook.sh")
}

/// Pays the macOS Gatekeeper/codesign first-exec tax on the freshly rebuilt
/// debug binary ONCE, outside every timed assertion — the harness-side analog
/// of `install_stub` in `tests/hooks/test_rally_coordination_hook.sh` (a cold
/// first fire measured at 2172ms against a 400ms watchdog; warm fires at
/// 432ms). `std::sync::Once` makes this safe regardless of `cargo test`'s
/// thread-per-test scheduling.
static WARM: Once = Once::new();
fn warm_up() {
    WARM.call_once(|| {
        let _ = Command::new(BIN)
            .arg("version")
            .arg("--json")
            .env("HOME", std::env::temp_dir())
            .env("RALLY_GLOBAL_INDEX", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

/// `${RALLY_BIN//[^A-Za-z0-9._-]/_}` (C3's probe marker filename), ported so
/// tests can compute the marker path a probe would write without depending on
/// the shell.
fn sanitize_bin_path(raw: &str) -> String {
    raw.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn sorted_keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .map(|obj| obj.keys().cloned().collect())
        .unwrap_or_default();
    keys.sort();
    keys
}

/// In-file fixture: a scratch repo + scratch HOME, per
/// `tests/json_envelope_contract.rs`'s `Workspace` pattern.
struct Fixture {
    repo: PathBuf,
    home: PathBuf,
}

impl Fixture {
    /// A normal fixture: `.git` (so `repo_root()` resolves) and `.rally` (so
    /// `hook_runtime::resolve_root` finds a room without needing `rally init`).
    fn new(name: &str) -> Self {
        let fx = Self::bare(name);
        fs::create_dir_all(fx.repo.join(".git")).unwrap();
        fs::create_dir_all(fx.repo.join(".rally")).unwrap();
        fx
    }

    /// T-08(b): a directory with no `.rally` anywhere up the chain.
    fn bare(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let repo = std::env::temp_dir().join(format!("native-hook-{name}-{nanos}-repo"));
        let home = std::env::temp_dir().join(format!("native-hook-{name}-{nanos}-home"));
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&home).unwrap();
        Self { repo, home }
    }

    fn cleanup(self) {
        let _ = fs::remove_dir_all(&self.repo);
        let _ = fs::remove_dir_all(&self.home);
    }

    fn run_bin(&self, args: &[&str]) -> std::process::Output {
        self.run_bin_stdin(args, "", &[])
    }

    fn run_bin_stdin(
        &self,
        args: &[&str],
        stdin_text: &str,
        extra_envs: &[(&str, &str)],
    ) -> std::process::Output {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .current_dir(&self.repo)
            .env("HOME", &self.home)
            .env("RALLY_GLOBAL_INDEX", "1")
            // Ambient CI identity vars outrank RALLY_SESSION_ID in
            // EndpointInputs::from_env; strip so session assertions hold on
            // GitHub Actions (same class as 72aed10 / write_authority parity).
            .env_remove("GITHUB_ACTIONS")
            .env_remove("GITHUB_RUN_ID")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in extra_envs {
            cmd.env(key, value);
        }
        let mut child = cmd.spawn().expect("spawn rally");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin_text.as_bytes())
            .expect("write stdin");
        child.wait_with_output().expect("wait rally")
    }

    fn run_shell(
        &self,
        phase: &str,
        tool: &str,
        stdin_text: &str,
        extra_envs: &[(&str, &str)],
    ) -> std::process::Output {
        let mut cmd = Command::new("bash");
        cmd.arg(hook_script())
            .arg(phase)
            .arg(tool)
            .current_dir(&self.repo)
            .env("HOME", &self.home)
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("RALLY_GLOBAL_INDEX", "1")
            .env("RALLY_HOOKS", "")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in extra_envs {
            cmd.env(key, value);
        }
        let mut child = cmd.spawn().expect("spawn hook");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin_text.as_bytes())
            .expect("write stdin");
        child.wait_with_output().expect("wait hook")
    }

    fn seed_peer_claim(&self, path: &str) {
        let enter = self.run_bin(&["enter", "--tool", "codex:peer", "--json"]);
        assert!(
            enter.status.success(),
            "seed enter failed: {}",
            String::from_utf8_lossy(&enter.stderr)
        );
        let claim = self.run_bin(&[
            "say",
            "claim",
            "--tool",
            "codex:peer",
            "--path",
            path,
            "--subject",
            "peer",
            "--json",
        ]);
        assert!(
            claim.status.success(),
            "seed claim failed: {}",
            String::from_utf8_lossy(&claim.stderr)
        );
    }

    fn active_claims(&self) -> Vec<Value> {
        let out = self.run_bin(&["room", "--json"]);
        let body: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "room --json not JSON: {e}\nstdout={}\nstderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        body["data"]["room"]["active_claims"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fn log_dir(&self) -> PathBuf {
        self.repo.join(".rally").join("log")
    }

    fn ledger_lines(&self) -> usize {
        let Ok(entries) = fs::read_dir(self.log_dir()) else {
            return 0;
        };
        entries
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
            .map(|entry| {
                fs::read_to_string(entry.path())
                    .unwrap_or_default()
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .count()
            })
            .sum()
    }

    /// Ledger rows are `{seq, occurred_at, event_type, payload}` on disk
    /// (`LedgerLine`, store.rs:2344) — `event_type` carries the fact `kind`
    /// and `payload` carries the rest of the `Fact` struct.
    fn ledger_facts(&self) -> Vec<Value> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(self.log_dir()) else {
            return out;
        };
        for entry in entries.flatten() {
            if entry.path().extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            let Ok(text) = fs::read_to_string(entry.path()) else {
                continue;
            };
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(value) = serde_json::from_str::<Value>(line) {
                    out.push(value);
                }
            }
        }
        out
    }

    fn rally_listing(&self) -> Vec<(String, u64)> {
        let base = self.repo.join(".rally");
        let mut out = Vec::new();
        fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, u64)>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(base, &path, out);
                } else if let Ok(meta) = entry.metadata() {
                    let rel = path
                        .strip_prefix(base)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .to_string();
                    out.push((rel, meta.len()));
                }
            }
        }
        walk(&base, &base, &mut out);
        out.sort();
        out
    }

    fn marker_path(&self, name: &str) -> PathBuf {
        self.repo.join(".rally").join(".hook-seen").join(name)
    }
}

fn write_executable(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

// ---------------------------------------------------------------------------
// T-01
// ---------------------------------------------------------------------------

/// The claim/presence read-back and the `native` probe marker are what make
/// this test real — `{}` alone is also emitted for pure-read, disabled,
/// suppressed and malformed fires. R9 hardening: stderr must stay empty and
/// no mutation-abort marker may exist on a clean fire.
///
/// The probe-marker assertion requires C3's shell exec/probe branch, which
/// had not landed in this worktree when this test was authored; see the
/// implementer report for its observed status.
/// The probe marker is two lines: the verdict, then the binary's identity
/// (`size:fractional-mtime`) that the verdict was computed against. The
/// identity line is what invalidates the cache when the binary changes — bash
/// 3.2's `-nt` compares whole seconds and could not see a same-second rebuild.
/// Tests care about the verdict, so read line one.
fn probe_verdict(path: &std::path::Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn claude_unclaimed_allows_and_autoclaims() {
    warm_up();
    let fx = Fixture::new("t01");
    let stdin = json!({"tool_name":"Write","tool_input":{"file_path":"src/new.rs"}}).to_string();
    let out = fx.run_shell(
        "before-write",
        "claude_code",
        &stdin,
        &[
            ("RALLY_BIN", BIN),
            ("RALLY_SESSION_ID", "sess-one"),
            ("RALLY_HOOK_MS_BUDGET_SCALE", "4"),
        ],
    );
    assert!(
        out.status.success(),
        "exit code {:?}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.trim_end(), "{}", "stdout: {stdout}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.trim().is_empty(), "stderr must be empty: {stderr}");

    let claims = fx.active_claims();
    let claim = claims
        .iter()
        .find(|c| c["tool"] == "claude_code:sess-one")
        .unwrap_or_else(|| panic!("no claim for claude_code:sess-one in {claims:#?}"));
    assert!(
        claim["scope"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s == "file:src/new.rs"),
        "claim scope: {:?}",
        claim["scope"]
    );

    // ensure_presence's own "agent presence: <tool>" heartbeat and
    // post_working_status's "state=working | file=... | intent=..." fact are
    // BOTH kind=presence for this tool; the working-status one is the one the
    // golden names.
    let facts = fx.ledger_facts();
    let presence = facts.iter().find(|f| {
        f["event_type"] == "presence"
            && f["payload"]["tool"] == "claude_code:sess-one"
            && f["payload"]["subject"]
                .as_str()
                .unwrap_or("")
                .contains("working")
    });
    let presence =
        presence.unwrap_or_else(|| panic!("no working-status presence fact in {facts:#?}"));
    let subject = presence["payload"]["subject"].as_str().unwrap_or("");
    assert!(subject.contains("working"), "subject: {subject}");
    assert!(subject.contains("src/new.rs"), "subject: {subject}");

    assert!(
        !fx.marker_path("sess-one.mutation-abort.seen").exists(),
        "a clean fire must not leave a mutation-abort marker"
    );

    let probe = fx.marker_path(&format!("native-probe.{}.seen", sanitize_bin_path(BIN)));
    assert_eq!(
        probe_verdict(&probe),
        "native",
        "probe marker missing/mismatched at {probe:?} (requires C3's shell probe/exec branch)"
    );

    fx.cleanup();
}

// ---------------------------------------------------------------------------
// T-02
// ---------------------------------------------------------------------------

#[test]
fn claude_peer_claim_is_high_severity_advisory() {
    warm_up();
    let fx = Fixture::new("t02");
    fx.seed_peer_claim("src/shared.rs");
    let stdin = json!({"tool_name":"Write","tool_input":{"file_path":"src/shared.rs"}}).to_string();
    let out = fx.run_shell(
        "before-write",
        "claude_code",
        &stdin,
        &[
            ("RALLY_BIN", BIN),
            ("RALLY_SESSION_ID", "sess-two"),
            ("RALLY_HOOK_MS_BUDGET_SCALE", "4"),
        ],
    );
    assert!(
        out.status.success(),
        "exit code {:?}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let body: Value = serde_json::from_str(stdout.trim_end())
        .unwrap_or_else(|e| panic!("not JSON: {e}: {stdout}"));

    assert_eq!(body["hookSpecificOutput"]["permissionDecision"], "allow");
    assert_eq!(body["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    let reason = body["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap_or_else(|| panic!("no permissionDecisionReason: {body:#}"));
    assert_eq!(body["systemMessage"].as_str().unwrap_or(""), reason);
    assert!(reason.contains("HIGH-SEVERITY"), "{reason}");
    assert!(
        reason.contains("advisory \u{2014} not blocking"),
        "{reason}"
    );
    assert!(
        reason.starts_with("UNTRUSTED LEDGER DATA FOLLOWS"),
        "{reason}"
    );

    let claims = fx.active_claims();
    assert!(
        !claims.iter().any(|c| c["tool"] == "claude_code:sess-two"),
        "a conflicting write must not auto-claim: {claims:#?}"
    );

    fx.cleanup();
}

// ---------------------------------------------------------------------------
// T-03
// ---------------------------------------------------------------------------

#[test]
fn claude_strict_denies() {
    warm_up();
    let fx = Fixture::new("t03");
    fx.seed_peer_claim("src/shared.rs");
    let stdin = json!({"tool_name":"Write","tool_input":{"file_path":"src/shared.rs"}}).to_string();
    let out = fx.run_shell(
        "before-write",
        "claude_code",
        &stdin,
        &[
            ("RALLY_BIN", BIN),
            ("RALLY_SESSION_ID", "sess-three"),
            ("RALLY_HOOK_STRICT", "1"),
            ("RALLY_HOOK_MS_BUDGET_SCALE", "4"),
        ],
    );
    assert!(
        out.status.success(),
        "exit code {:?}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let body: Value = serde_json::from_str(stdout.trim_end())
        .unwrap_or_else(|e| panic!("not JSON: {e}: {stdout}"));

    assert_eq!(body["hookSpecificOutput"]["permissionDecision"], "deny");
    let reason = body["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap_or_else(|| panic!("no permissionDecisionReason: {body:#}"));
    assert!(reason.contains("STRICT MODE \u{2014} BLOCKING"), "{reason}");
    assert!(
        body.get("systemMessage").is_none(),
        "strict deny must drop systemMessage: {body:#}"
    );

    fx.cleanup();
}

// ---------------------------------------------------------------------------
// T-04 (R4: extended to cursor + gemini, default and strict, exact key sets)
// ---------------------------------------------------------------------------

#[test]
fn codex_conflict_never_carries_permission_decision() {
    warm_up();
    let fx = Fixture::new("t04");
    fx.seed_peer_claim("src/shared.rs");
    let apply_patch =
        json!({"tool_name":"apply_patch","tool_input":{"command":"*** Update File: src/shared.rs"}})
            .to_string();
    let write_env =
        json!({"tool_name":"Write","tool_input":{"file_path":"src/shared.rs"}}).to_string();

    // Codex: exactly one key, both modes.
    for strict in [false, true] {
        let session = if strict {
            "sess-codex-two"
        } else {
            "sess-codex-one"
        };
        let mut envs = vec![
            ("RALLY_BIN", BIN),
            ("RALLY_SESSION_ID", session),
            ("RALLY_HOOK_MS_BUDGET_SCALE", "4"),
        ];
        if strict {
            envs.push(("RALLY_HOOK_STRICT", "1"));
        }
        let out = fx.run_shell("before-write", "codex", &apply_patch, &envs);
        assert!(
            out.status.success(),
            "codex strict={strict} exit {:?}\nstderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let body: Value = serde_json::from_str(stdout.trim_end())
            .unwrap_or_else(|e| panic!("codex strict={strict} not JSON: {e}: {stdout}"));
        let keys = sorted_keys(&body);
        assert_eq!(
            keys,
            vec!["systemMessage".to_string()],
            "codex strict={strict} keys={keys:?}"
        );
        let msg = body["systemMessage"].as_str().unwrap_or("");
        assert!(
            msg.contains("HIGH-SEVERITY"),
            "codex strict={strict}: {msg}"
        );
    }

    // Cursor: {permission, agent_message} in both modes; only the value flips.
    for strict in [false, true] {
        let session = if strict {
            "sess-cursor-two"
        } else {
            "sess-cursor-one"
        };
        let mut envs = vec![
            ("RALLY_BIN", BIN),
            ("RALLY_SESSION_ID", session),
            ("RALLY_HOOK_MS_BUDGET_SCALE", "4"),
        ];
        if strict {
            envs.push(("RALLY_HOOK_STRICT", "1"));
        }
        let out = fx.run_shell("before-write", "cursor", &write_env, &envs);
        assert!(
            out.status.success(),
            "cursor strict={strict} exit {:?}\nstderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let body: Value = serde_json::from_str(stdout.trim_end())
            .unwrap_or_else(|e| panic!("cursor strict={strict} not JSON: {e}: {stdout}"));
        let keys = sorted_keys(&body);
        assert_eq!(
            keys,
            vec!["agent_message".to_string(), "permission".to_string()],
            "cursor strict={strict} keys={keys:?}"
        );
        assert_eq!(
            body["permission"],
            if strict { "deny" } else { "allow" },
            "cursor strict={strict}"
        );
    }

    // Gemini default: one top-level key, additionalContext shape.
    {
        let out = fx.run_shell(
            "before-write",
            "gemini",
            &write_env,
            &[
                ("RALLY_BIN", BIN),
                ("RALLY_SESSION_ID", "sess-gemini-one"),
                ("RALLY_HOOK_MS_BUDGET_SCALE", "4"),
            ],
        );
        assert!(
            out.status.success(),
            "gemini default exit {:?}\nstderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let body: Value = serde_json::from_str(stdout.trim_end())
            .unwrap_or_else(|e| panic!("gemini default not JSON: {e}: {stdout}"));
        let keys = sorted_keys(&body);
        assert_eq!(
            keys,
            vec!["hookSpecificOutput".to_string()],
            "gemini default keys={keys:?}"
        );
        let inner_keys = sorted_keys(&body["hookSpecificOutput"]);
        assert_eq!(
            inner_keys,
            vec!["additionalContext".to_string(), "hookEventName".to_string()],
            "gemini default inner keys={inner_keys:?}"
        );
        assert_eq!(body["hookSpecificOutput"]["hookEventName"], "BeforeTool");
    }

    // Gemini strict: decision/reason, no hookSpecificOutput.
    {
        let out = fx.run_shell(
            "before-write",
            "gemini",
            &write_env,
            &[
                ("RALLY_BIN", BIN),
                ("RALLY_SESSION_ID", "sess-gemini-two"),
                ("RALLY_HOOK_STRICT", "1"),
                ("RALLY_HOOK_MS_BUDGET_SCALE", "4"),
            ],
        );
        assert!(
            out.status.success(),
            "gemini strict exit {:?}\nstderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let body: Value = serde_json::from_str(stdout.trim_end())
            .unwrap_or_else(|e| panic!("gemini strict not JSON: {e}: {stdout}"));
        let keys = sorted_keys(&body);
        assert_eq!(
            keys,
            vec!["decision".to_string(), "reason".to_string()],
            "gemini strict keys={keys:?}"
        );
        assert_eq!(body["decision"], "deny");
    }

    fx.cleanup();
}

// ---------------------------------------------------------------------------
// T-05 (R8: relative timing bound, not an absolute ms ceiling)
// ---------------------------------------------------------------------------

#[test]
fn pure_reads_return_immediately_without_ledger_work() {
    warm_up();
    let fx = Fixture::new("t05");
    let before_lines = fx.ledger_lines();
    let before_listing = fx.rally_listing();

    // Same-run baseline: a `rally version` spawn on this host, this load, this
    // build. `.rally` listing/ledger-line equality are the real falsifiers;
    // this bound only catches a pure read that regressed into doing store
    // work (R8 — an absolute `< 100ms` bound flakes under cargo-test
    // parallelism on a loaded host).
    let baseline_start = Instant::now();
    let _ = fx.run_bin(&["version", "--json"]);
    let baseline = baseline_start.elapsed();
    let bound = baseline * 3 + Duration::from_millis(50);

    let repo_root = fx.repo.to_str().unwrap().to_string();
    let cases: [(&str, Value); 3] = [
        (
            "Read",
            json!({"tool_name":"Read","tool_input":{"file_path":"src/a.rs"}}),
        ),
        (
            "Glob",
            json!({"tool_name":"Glob","tool_input":{"pattern":"**/*.rs"}}),
        ),
        (
            "read_file",
            json!({"tool_name":"read_file","tool_input":{"path":"src/a.rs"}}),
        ),
    ];
    for (label, envelope) in cases {
        let args = [
            "hook",
            "before-write",
            "--tool",
            "claude_code:read-one",
            "--repo-root",
            repo_root.as_str(),
        ];
        let start = Instant::now();
        let out = fx.run_bin_stdin(&args, &envelope.to_string(), &[]);
        let elapsed = start.elapsed();
        assert!(
            out.status.success(),
            "{label} exit code {:?}",
            out.status.code()
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(stdout.trim_end(), "{}", "{label} stdout: {stdout}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.trim().is_empty(), "{label} stderr: {stderr}");
        assert!(
            elapsed <= bound,
            "{label} elapsed {elapsed:?} exceeds 3x-baseline bound {bound:?} (baseline {baseline:?})"
        );
    }

    assert_eq!(
        fx.ledger_lines(),
        before_lines,
        "pure reads must not write the ledger"
    );
    assert_eq!(
        fx.rally_listing(),
        before_listing,
        "pure reads must not touch .rally on disk"
    );

    fx.cleanup();
}

// ---------------------------------------------------------------------------
// T-06
// ---------------------------------------------------------------------------

#[test]
fn opaque_shell_and_target_cap() {
    warm_up();
    let fx = Fixture::new("t06");
    let repo_root = fx.repo.to_str().unwrap().to_string();
    let before_lines = fx.ledger_lines();

    for (label, envelope) in [
        (
            "Bash",
            json!({"tool_name":"Bash","tool_input":{"command":"rm -rf src; echo x > a.txt"}}),
        ),
        (
            "exec_command",
            json!({"tool_name":"exec_command","tool_input":{"command":"ls"}}),
        ),
    ] {
        let args = [
            "hook",
            "before-write",
            "--tool",
            "codex:shell-one",
            "--repo-root",
            repo_root.as_str(),
        ];
        let out = fx.run_bin_stdin(&args, &envelope.to_string(), &[]);
        assert!(out.status.success(), "{label} exit {:?}", out.status.code());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim_end(),
            "{}",
            "{label}"
        );
        assert_eq!(
            fx.ledger_lines(),
            before_lines,
            "{label} must not write the ledger"
        );
    }

    // 16 targets: one claim, 16 scopes — NOT truncated.
    let sixteen = (0..16)
        .map(|i| format!("*** Add File: src/f{i}.rs"))
        .collect::<Vec<_>>()
        .join("\n");
    let envelope16 = json!({"tool_name":"apply_patch","tool_input":{"command": sixteen}});
    let args16 = [
        "hook",
        "before-write",
        "--tool",
        "codex:cap-one",
        "--repo-root",
        repo_root.as_str(),
    ];
    let out = fx.run_bin_stdin(&args16, &envelope16.to_string(), &[]);
    assert!(
        out.status.success(),
        "16-target exit {:?}",
        out.status.code()
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim_end(), "{}");
    let claims = fx.active_claims();
    let claim = claims
        .iter()
        .find(|c| c["tool"] == "codex:cap-one")
        .unwrap_or_else(|| panic!("16-target claim missing in {claims:#?}"));
    let scopes = claim["scope"].as_array().unwrap();
    assert_eq!(scopes.len(), 16, "scopes: {scopes:#?}");

    // 17 targets: MALFORMED (whole transaction rejected), not truncate-to-16.
    // The malformed-path marker names `classification.session` — the
    // ENVELOPE's own `session_id`, read before `resolve_identity` ever runs —
    // so the session has to travel on stdin, not via `--session-id`.
    let seventeen = (0..17)
        .map(|i| format!("*** Add File: src/g{i}.rs"))
        .collect::<Vec<_>>()
        .join("\n");
    let envelope17 = json!({
        "session_id": "sess-cap17",
        "tool_name":"apply_patch",
        "tool_input":{"command": seventeen},
    });
    let ledger_before_17 = fx.ledger_lines();
    let args17 = [
        "hook",
        "before-write",
        "--tool",
        "codex:cap-one",
        "--repo-root",
        repo_root.as_str(),
    ];
    let out = fx.run_bin_stdin(&args17, &envelope17.to_string(), &[]);
    assert!(
        out.status.success(),
        "17-target exit {:?}",
        out.status.code()
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim_end(), "{}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("exceeds 16 targets"), "stderr: {stderr}");
    assert_eq!(
        fx.ledger_lines(),
        ledger_before_17,
        "malformed 17-target must not write the ledger"
    );
    let marker = fx.marker_path("sess-cap17.native-malformed-apply_patch.seen");
    assert!(marker.exists(), "expected marker at {marker:?}");

    fx.cleanup();
}

// ---------------------------------------------------------------------------
// T-07 (R2, substantially revised)
// ---------------------------------------------------------------------------

#[test]
fn deadline_miss_is_fail_loud_advisory() {
    warm_up();

    // (a) shell path. Prime the real shell probe BEFORE exporting
    // RALLY_TEST_BLOCK_MS (which delays EVERY rally spawn, including the
    // shell's own `hook capabilities` probe). Using the production probe to
    // create the marker avoids duplicating BSD/GNU stat semantics in the test
    // harness and proves the timed fire consumes an actual cache hit.
    {
        let fx = Fixture::new("t07a");
        let probe = fx.marker_path(&format!("native-probe.{}.seen", sanitize_bin_path(BIN)));
        let stat_dir = fx.home.join("stat-bin");
        let stat_calls = fx.home.join("stat-calls.log");
        write_executable(
            &stat_dir.join("stat"),
            "#!/usr/bin/env bash\n\
printf '%s\\n' \"$1\" >> \"$STAT_CALLS\"\n\
case \"$1\" in\n\
  -c) printf '%s\\n' 'stable-file-id' ;;\n\
  -f) printf 'fs-%s\\n' \"$(wc -l < \"$STAT_CALLS\")\" ;;\n\
  *) exit 2 ;;\n\
esac\n",
        );
        let path_env = format!(
            "{}:{}",
            stat_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let stat_calls_env = stat_calls.to_string_lossy().into_owned();
        let prime_stdin =
            json!({"tool_name":"Read","tool_input":{"file_path":"src/slow.rs"}}).to_string();
        let prime = fx.run_shell(
            "before-write",
            "claude_code",
            &prime_stdin,
            &[
                ("RALLY_BIN", BIN),
                ("RALLY_SESSION_ID", "sess-slow-one"),
                ("PATH", &path_env),
                ("STAT_CALLS", &stat_calls_env),
            ],
        );
        assert!(
            prime.status.success(),
            "T-07a probe exit {:?}\nstderr={}",
            prime.status.code(),
            String::from_utf8_lossy(&prime.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&prime.stdout).trim_end(), "{}");
        assert_eq!(
            probe_verdict(&probe),
            "native",
            "T-07a setup must establish a real native cache hit"
        );

        let stdin =
            json!({"tool_name":"Write","tool_input":{"file_path":"src/slow.rs"}}).to_string();
        let start = Instant::now();
        let out = fx.run_shell(
            "before-write",
            "claude_code",
            &stdin,
            &[
                ("RALLY_BIN", BIN),
                ("RALLY_SESSION_ID", "sess-slow-one"),
                ("PATH", &path_env),
                ("STAT_CALLS", &stat_calls_env),
                // Keep a full second between the assertion ceiling and the
                // simulated stall so loaded CI runners cannot blur success
                // (the watchdog fired) with failure (the stall completed).
                ("RALLY_TEST_BLOCK_MS", "3000"),
                ("RALLY_HOOK_TIMEOUT_MS", "300"),
            ],
        );
        let elapsed = start.elapsed();
        assert!(
            out.status.success(),
            "T-07a exit {:?}\nstderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            elapsed < Duration::from_millis(2000),
            "T-07a elapsed {elapsed:?}"
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let trimmed = stdout.trim_end();
        assert_ne!(trimmed, "{}", "T-07a must not be a clean {{}} response");
        let body: Value = serde_json::from_str(trimmed)
            .unwrap_or_else(|e| panic!("T-07a not JSON: {e}: {stdout}"));
        assert!(
            body.get("permissionDecision").is_none() && body.get("decision").is_none(),
            "T-07a must carry no permission verdict: {body:#}"
        );
        assert!(
            trimmed.contains("rally coordination skipped") && trimmed.contains("UNCLAIMED"),
            "T-07a body: {trimmed}"
        );
        assert_eq!(
            probe_verdict(&probe),
            "native",
            "T-07a probe marker must still read native afterward"
        );
        assert_eq!(
            fs::read_to_string(&stat_calls).unwrap_or_default(),
            "-c\n-c\n",
            "T-07a must use stable GNU file identity, never GNU filesystem status"
        );
        assert!(
            !fx.active_claims()
                .iter()
                .any(|c| c["tool"] == "claude_code:sess-slow-one"),
            "no claim may land on an aborted transaction"
        );
        fx.cleanup();
    }

    // (b) binary + OUTER watchdog. The stage-block seam stalls between
    // snapshot and claim append; only the main-thread watchdog can catch this
    // (finding 5: the outer advisory names argv's tool, the inner would name
    // the resolved id — the main thread never sees the envelope).
    {
        let fx = Fixture::new("t07b");
        let stdin =
            json!({"tool_name":"Write","tool_input":{"file_path":"src/slow-b.rs"}}).to_string();
        let start = Instant::now();
        let out = fx.run_bin_stdin(
            &[
                "hook",
                "before-write",
                "--tool",
                "claude_code:slow-two",
                "--repo-root",
                fx.repo.to_str().unwrap(),
                "--timeout-ms",
                "400",
            ],
            &stdin,
            &[("RALLY_TEST_HOOK_STAGE_BLOCK_MS", "800")],
        );
        let elapsed = start.elapsed();
        assert!(
            out.status.success(),
            "T-07b exit {:?}\nstderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            elapsed < Duration::from_millis(1000),
            "T-07b elapsed {elapsed:?}"
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let trimmed = stdout.trim_end();
        assert_ne!(trimmed, "{}");
        let body: Value = serde_json::from_str(trimmed)
            .unwrap_or_else(|e| panic!("T-07b not JSON: {e}: {stdout}"));
        assert!(body.get("permissionDecision").is_none() && body.get("decision").is_none());
        assert!(
            trimmed.contains("rally coordination skipped") && trimmed.contains("UNCLAIMED"),
            "{trimmed}"
        );
        assert!(
            !fx.active_claims()
                .iter()
                .any(|c| c["tool"] == "claude_code:slow-two")
        );
        fx.cleanup();
    }

    // (c) NEW (R2): deterministic inner-Deadline seam. The ONLY sub-case that
    // falsifies the pre-claim `Deadline::exhausted` check — finding 3: the
    // reduced reason renders "auto-claim skipped _budget_", not
    // "auto-claim skipped (budget)" (parens are off the reduce() allowlist).
    {
        let fx = Fixture::new("t07c");
        let stdin =
            json!({"tool_name":"Write","tool_input":{"file_path":"src/slow-c.rs"}}).to_string();
        let out = fx.run_bin_stdin(
            &[
                "hook",
                "before-write",
                "--tool",
                "claude_code:slow-three",
                "--repo-root",
                fx.repo.to_str().unwrap(),
            ],
            &stdin,
            &[("RALLY_TEST_HOOK_FORCE_DEADLINE", "1")],
        );
        assert!(
            out.status.success(),
            "T-07c exit {:?}\nstderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let trimmed = stdout.trim_end();
        let body: Value = serde_json::from_str(trimmed)
            .unwrap_or_else(|e| panic!("T-07c not JSON: {e}: {stdout}"));
        assert!(body.get("permissionDecision").is_none() && body.get("decision").is_none());
        let msg = body["systemMessage"].as_str().unwrap_or("");
        assert!(
            msg.contains("auto-claim skipped _budget_"),
            "T-07c must carry the reduced reason (finding 3): {msg}"
        );
        assert!(
            !fx.active_claims()
                .iter()
                .any(|c| c["tool"] == "claude_code:slow-three")
        );
        fx.cleanup();
    }

    // (d) R4: cursor sub-case, same deterministic seam for reliability.
    {
        let fx = Fixture::new("t07d");
        let stdin =
            json!({"tool_name":"Write","tool_input":{"file_path":"src/slow-d.rs"}}).to_string();
        let out = fx.run_bin_stdin(
            &[
                "hook",
                "before-write",
                "--tool",
                "cursor:slow-four",
                "--repo-root",
                fx.repo.to_str().unwrap(),
            ],
            &stdin,
            &[("RALLY_TEST_HOOK_FORCE_DEADLINE", "1")],
        );
        assert!(
            out.status.success(),
            "T-07d exit {:?}\nstderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let trimmed = stdout.trim_end();
        let body: Value = serde_json::from_str(trimmed)
            .unwrap_or_else(|e| panic!("T-07d not JSON: {e}: {stdout}"));
        let keys = sorted_keys(&body);
        assert_eq!(
            keys,
            vec!["agent_message".to_string(), "permission".to_string()],
            "T-07d keys={keys:?}"
        );
        assert_eq!(body["permission"], "allow");
        assert!(
            body["agent_message"]
                .as_str()
                .unwrap_or("")
                .contains("UNCLAIMED")
        );
        fx.cleanup();
    }
}

// ---------------------------------------------------------------------------
// T-08 (R5: sub-case (d) added; F-2/SEC-001: sub-case (e) added)
// ---------------------------------------------------------------------------

#[test]
fn malformed_no_rally_or_old_binary_fail_open() {
    warm_up();

    // (a) malformed JSON envelope.
    {
        let fx = Fixture::new("t08a");
        let before = fx.ledger_lines();
        let out = fx.run_bin_stdin(
            &[
                "hook",
                "before-write",
                "--tool",
                "claude_code:mal-one",
                "--repo-root",
                fx.repo.to_str().unwrap(),
            ],
            "{not json",
            &[],
        );
        assert!(out.status.success(), "T-08a exit {:?}", out.status.code());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim_end(), "{}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let diag_lines = stderr.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(
            diag_lines, 1,
            "expected exactly one stderr diagnostic: {stderr}"
        );
        assert_eq!(
            fx.ledger_lines(),
            before,
            "malformed envelope must not write the ledger"
        );
        fx.cleanup();
    }

    // (b) no .rally anywhere.
    {
        let fx = Fixture::bare("t08b");
        let out = fx.run_bin_stdin(
            &[
                "hook",
                "before-write",
                "--tool",
                "claude_code:mal-two",
                "--repo-root",
                fx.repo.to_str().unwrap(),
            ],
            &json!({"tool_name":"Write","tool_input":{"file_path":"src/a.rs"}}).to_string(),
            &[],
        );
        assert!(out.status.success(), "T-08b exit {:?}", out.status.code());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim_end(), "{}");
        assert!(
            !fx.repo.join(".rally").exists(),
            "must not create .rally in a non-rally directory"
        );
        fx.cleanup();
    }

    // (c) shell: RALLY_BIN is an old-binary stub lacking `hook`. Requires C3's
    // probe/exec branch to even attempt `hook capabilities`; see report.
    {
        let fx = Fixture::new("t08c");
        let calls = fx.repo.join("CALLS.log");
        let stub_path = fx.home.join("stub").join("rally");
        write_executable(
            &stub_path,
            &format!(
                "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{calls}\"\nif [ \"$1\" = \"hook\" ]; then exit 2; fi\nprintf '%s' '{{}}'\nexit 0\n",
                calls = calls.display()
            ),
        );

        let stdin =
            json!({"tool_name":"Write","tool_input":{"file_path":"src/old.rs"}}).to_string();
        let out = fx.run_shell(
            "before-write",
            "claude_code",
            &stdin,
            &[
                ("RALLY_BIN", stub_path.to_str().unwrap()),
                ("RALLY_SESSION_ID", "sess-old-one"),
                ("RALLY_HOOK_MS_BUDGET_SCALE", "4"),
            ],
        );
        assert!(
            out.status.success(),
            "T-08c exit {:?}\nstderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        let calls_text = fs::read_to_string(&calls).unwrap_or_default();
        let mut lines = calls_text.lines();
        let first = lines.next().unwrap_or("");
        let second = lines.next().unwrap_or("");
        assert!(
            first.starts_with("hook capabilities"),
            "expected first call `hook capabilities`, got {first:?}; CALLS={calls_text}"
        );
        assert!(
            second.starts_with("hooks status"),
            "expected second call `hooks status`, got {second:?}; CALLS={calls_text}"
        );
        let probe = fx.marker_path(&format!(
            "native-probe.{}.seen",
            sanitize_bin_path(stub_path.to_str().unwrap())
        ));
        assert_eq!(
            probe_verdict(&probe),
            "fallback",
            "probe marker at {probe:?}"
        );
        fx.cleanup();
    }

    // (d) R5: hooks disabled for the repo -> {}-or-empty, zero ledger delta.
    // Binary-direct; does not depend on C3.
    {
        let fx = Fixture::new("t08d");
        let off = fx.run_bin(&["hooks", "off", "--scope", "repo", "--json"]);
        assert!(
            off.status.success(),
            "hooks off failed: {}",
            String::from_utf8_lossy(&off.stderr)
        );
        let before = fx.ledger_lines();
        let stdin =
            json!({"tool_name":"Write","tool_input":{"file_path":"src/off.rs"}}).to_string();
        let out = fx.run_bin_stdin(
            &[
                "hook",
                "before-write",
                "--tool",
                "claude_code:off-one",
                "--repo-root",
                fx.repo.to_str().unwrap(),
            ],
            &stdin,
            &[],
        );
        assert!(out.status.success(), "T-08d exit {:?}", out.status.code());
        let stdout = String::from_utf8_lossy(&out.stdout);
        let trimmed = stdout.trim_end();
        assert!(
            trimmed.is_empty() || trimmed == "{}",
            "T-08d stdout: {stdout}"
        );
        assert_eq!(
            fx.ledger_lines(),
            before,
            "disabled hooks must not write the ledger"
        );
        fx.cleanup();
    }

    // (e) F-2 / SEC-001: a REPO-controlled normalize failure is NOT the
    // (a) malformed-envelope case and must NOT share its `{}`.
    //
    // The vector: a hostile repo commits a symlink at an ancestor of a path
    // the victim will edit (`escape/` -> outside the root), so
    // `normalize_targets` refuses the target mid-transaction. `{}` on stdout
    // is byte-identical to sub-case T-01's "checked, no conflict", and stderr
    // is not surfaced to the model on exit 0 -- the agent would edit an
    // unclaimed contested path believing rally had deconflicted it. Assert the
    // fail-loud advisory instead, and assert it still carries NO permission
    // verdict, because advising is not gating.
    {
        let fx = Fixture::new("t08e");
        let outside = std::env::temp_dir().join(format!(
            "native-hook-t08e-outside-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(outside.join("src")).unwrap();
        std::os::unix::fs::symlink(&outside, fx.repo.join("escape")).unwrap();

        let before = fx.ledger_lines();
        let stdin =
            json!({"tool_name":"Write","tool_input":{"file_path":"escape/src/x.rs"}}).to_string();
        let out = fx.run_bin_stdin(
            &[
                "hook",
                "before-write",
                "--tool",
                "claude_code:sec-one",
                "--repo-root",
                fx.repo.to_str().unwrap(),
            ],
            &stdin,
            &[],
        );
        assert!(out.status.success(), "T-08e exit {:?}", out.status.code());
        let stdout = String::from_utf8_lossy(&out.stdout);
        let trimmed = stdout.trim_end();
        assert_ne!(
            trimmed, "{}",
            "T-08e: a symlink-crossing target must not answer with the \
             same `{{}}` a clean deconflicted fire answers with (SEC-001)"
        );
        let body: Value = serde_json::from_str(trimmed)
            .unwrap_or_else(|e| panic!("T-08e not JSON: {e}: {stdout}"));
        assert!(
            body.get("permissionDecision").is_none() && body.get("decision").is_none(),
            "T-08e must carry no permission verdict: {body:#}"
        );
        assert!(
            trimmed.contains("rally coordination skipped") && trimmed.contains("UNCLAIMED"),
            "T-08e body: {trimmed}"
        );
        assert_eq!(
            fx.ledger_lines(),
            before,
            "a refused target must not write the ledger"
        );
        let _ = fs::remove_dir_all(&outside);
        fx.cleanup();
    }
}

// ---------------------------------------------------------------------------
// T-09
// ---------------------------------------------------------------------------

/// R1/finding 6: vector (ii) is a CONFIRMED live SEC-001 bypass on the shell
/// as it stood when this file was authored (hook.sh:670's bare-name
/// `RALLY_BIN="rally"` fallback re-resolves through the same `$PATH` at
/// hook.sh:689 and executes the just-refused in-repo binary). C3 fixes it.
/// This test asserts the CORRECT behavior for all three vectors — (ii) is
/// therefore expected RED until C3 lands; see the implementer report.
#[test]
fn sec001_vectors_refuse_and_fall_back() {
    warm_up();

    let canary_script = |canary: &Path| -> String {
        format!(
            "#!/usr/bin/env bash\nprintf x > \"{}\"\nprintf '%s' '{{}}'\nexit 0\n",
            canary.display()
        )
    };

    // (i) RALLY_BIN is an absolute path INSIDE the scanned repo.
    {
        let fx = Fixture::new("t09i");
        let canary = fx.repo.join("CANARY-i");
        let planted = fx.repo.join("target/debug/rally");
        write_executable(&planted, &canary_script(&canary));
        let stdin = json!({"tool_name":"Write","tool_input":{"file_path":"src/a.rs"}}).to_string();
        let out = fx.run_shell(
            "before-write",
            "claude_code",
            &stdin,
            &[
                ("RALLY_BIN", planted.to_str().unwrap()),
                ("RALLY_SESSION_ID", "sess-sec-one"),
            ],
        );
        assert!(out.status.success(), "T-09i exit {:?}", out.status.code());
        assert!(
            !canary.exists(),
            "T-09i: SEC-001 bypass — the in-repo binary was executed"
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("SEC-001"),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        fx.cleanup();
    }

    // (ii) an in-repo bin/rally on PATH, no ~/.local/bin/rally to fall back
    // to. EXPECTED RED until C3 lands (see doc comment above).
    {
        let fx = Fixture::new("t09ii");
        let canary = fx.repo.join("CANARY-ii");
        let planted = fx.repo.join("bin/rally");
        write_executable(&planted, &canary_script(&canary));
        let path_env = format!(
            "{}:{}",
            planted.parent().unwrap().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let stdin = json!({"tool_name":"Write","tool_input":{"file_path":"src/a.rs"}}).to_string();
        let out = fx.run_shell(
            "before-write",
            "claude_code",
            &stdin,
            &[("PATH", &path_env), ("RALLY_SESSION_ID", "sess-sec-two")],
        );
        assert!(out.status.success(), "T-09ii exit {:?}", out.status.code());
        assert!(
            !canary.exists(),
            "T-09ii: SEC-001 bypass — RALLY_BIN=\"rally\" bare-name fallback re-resolved \
             through $PATH and executed the planted binary (hook.sh:670/689, R1). \
             Expected fixed by C3."
        );
        fx.cleanup();
    }

    // (iii) RALLY_BIN is an outside symlink laundering into the repo.
    {
        let fx = Fixture::new("t09iii");
        let canary = fx.repo.join("CANARY-iii");
        let planted = fx.repo.join("target/debug/rally");
        write_executable(&planted, &canary_script(&canary));
        let outside_dir =
            std::env::temp_dir().join(format!("t09iii-outside-{}", std::process::id()));
        fs::create_dir_all(&outside_dir).unwrap();
        let symlink = outside_dir.join("rally");
        std::os::unix::fs::symlink(&planted, &symlink).unwrap();
        let stdin = json!({"tool_name":"Write","tool_input":{"file_path":"src/a.rs"}}).to_string();
        let out = fx.run_shell(
            "before-write",
            "claude_code",
            &stdin,
            &[
                ("RALLY_BIN", symlink.to_str().unwrap()),
                ("RALLY_SESSION_ID", "sess-sec-three"),
            ],
        );
        assert!(out.status.success(), "T-09iii exit {:?}", out.status.code());
        assert!(
            !canary.exists(),
            "T-09iii: symlink laundering executed the in-repo binary"
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("SEC-001"),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = fs::remove_dir_all(&outside_dir);
        fx.cleanup();
    }
}

// ---------------------------------------------------------------------------
// T-10
// ---------------------------------------------------------------------------

#[test]
fn duplicate_event_runs_once() {
    warm_up();
    let fx = Fixture::new("t10");
    let stdin = json!({"tool_name":"Write","tool_input":{"file_path":"src/dup.rs"}}).to_string();
    let repo_root = fx.repo.to_str().unwrap().to_string();
    let args = [
        "hook",
        "before-write",
        "--tool",
        "claude_code:dup-one",
        "--session-id",
        "sess-dup-one",
        "--repo-root",
        repo_root.as_str(),
    ];

    let before = fx.ledger_lines();
    let out1 = fx.run_bin_stdin(&args, &stdin, &[("RALLY_HOOK_SOURCE", "plugin")]);
    assert!(
        out1.status.success(),
        "first fire exit {:?}",
        out1.status.code()
    );
    let after1 = fx.ledger_lines();
    assert!(
        after1 > before,
        "first fire must run: before={before} after={after1}"
    );

    let out2 = fx.run_bin_stdin(&args, &stdin, &[("RALLY_HOOK_SOURCE", "project")]);
    assert!(
        out2.status.success(),
        "second fire exit {:?}",
        out2.status.code()
    );
    assert_eq!(
        String::from_utf8_lossy(&out2.stdout).trim_end(),
        "{}",
        "second (duplicate) fire must be suppressed"
    );
    let after2 = fx.ledger_lines();
    assert_eq!(after2, after1, "duplicate fire must not write the ledger");

    let out3 = fx.run_bin_stdin(&args, &stdin, &[("RALLY_HOOK_SOURCE", "plugin")]);
    assert!(
        out3.status.success(),
        "third fire exit {:?}",
        out3.status.code()
    );
    let after3 = fx.ledger_lines();
    assert!(
        after3 > after2,
        "third fire (source count exceeds executed) must run again: after2={after2} after3={after3}"
    );

    fx.cleanup();
}
