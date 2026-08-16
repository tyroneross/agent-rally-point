// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Rust room/check projection parity with the shipped SessionStart renderer.
//!
//! The shell hook used to reinterpret two canonical Rust decisions: it hid an
//! `idle` squad even when adaptive liveness kept that squad as Unknown, and it
//! hid an active claim when `lease_expires_at` parsed in the past. The real
//! before-write gate still surfaced both claims, so the prompt could omit the
//! exact ownership fact that later stopped or warned on a write.
//!
//! This test runs the real CLI and the checked-in hook. It does not copy the JS
//! predicates into Rust: Rust creates the room/check projections, then the
//! production shell renderer consumes that room through `RALLY_BIN`.

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const VIEWER: &str = "codex:hook-viewer";
const LIVE_OWNER: &str = "live-owner";
const UNKNOWN_OWNER: &str = "unknown-owner";
const CLOSED_OWNER: &str = "closed-owner";
const LIVE_PATH: &str = "src/live.rs";
const UNKNOWN_PATH: &str = "src/unknown.rs";
const CLOSED_PATH: &str = "src/closed.rs";
const EXPIRED_LEASE: &str = "lease_expires_at:2000-01-01T00:00:00Z";

struct Workspace {
    cwd: PathBuf,
    home: PathBuf,
    dedupe: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("hook-projection-parity-{suffix}"));
        let cwd = base.join("repo");
        let home = base.join("home");
        let dedupe = base.join("dedupe");
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&dedupe).unwrap();
        Self { cwd, home, dedupe }
    }

    fn output(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_GLOBAL_INDEX", "1")
            .env("RALLY_HOOK_TIMEOUT_MS", "30000")
            .env("RALLY_SESSION_ID", "hook-projection-parity-session")
            .args(args)
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.output(args);
        assert!(
            out.status.success(),
            "cmd {args:?} failed\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
            panic!(
                "cmd {args:?} emitted invalid JSON ({error})\nstdout={}\nstderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            )
        })
    }

    fn claim(&self, tool: &str, path: &str) -> String {
        self.json(&[
            "say",
            "claim",
            "--tool",
            tool,
            "--path",
            path,
            "--subject",
            "hook parity fixture",
            "--json",
        ])["data"]["say"]["fact"]["event_id"]
            .as_str()
            .expect("claim returns event_id")
            .to_string()
    }

    fn release(&self, tool: &str, claim_id: &str) {
        self.json(&[
            "say",
            "release",
            "--tool",
            tool,
            "--ref",
            claim_id,
            "--subject",
            "hook parity fixture complete",
            "--json",
        ]);
    }

    fn segments(&self) -> Vec<PathBuf> {
        fs::read_dir(self.cwd.join(".rally/log"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect()
    }

    /// Backdate every Unknown-owner fact beyond the default adaptive window,
    /// leaving inject/code-progress signals absent. Rust therefore classifies
    /// the owner Unknown and keeps its `idle` squad visible. Both open claims
    /// receive a parseably expired lease without being released.
    fn make_unknown_and_expire_open_claims(&self) {
        let old = (chrono::Utc::now() - chrono::Duration::hours(2))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let mut expired_claims = 0;

        for segment in self.segments() {
            let text = fs::read_to_string(&segment).unwrap();
            let mut rewritten = String::with_capacity(text.len());
            for line in text.lines().filter(|line| !line.trim().is_empty()) {
                let mut record: Value = serde_json::from_str(line).unwrap();
                let tool = record["payload"]["tool"].as_str().map(str::to_string);
                let is_unknown_owner = tool.as_deref() == Some(UNKNOWN_OWNER);
                let is_open_fixture_claim = record["payload"]["kind"] == "claim"
                    && matches!(tool.as_deref(), Some(LIVE_OWNER) | Some(UNKNOWN_OWNER));

                if is_unknown_owner {
                    record["payload"]["created_at"] = Value::String(old.clone());
                    record["occurred_at"] = Value::String(old.clone());
                }

                if is_open_fixture_claim {
                    let evidence = record["payload"]["evidence"]
                        .as_array_mut()
                        .expect("claim evidence is an array");
                    let lease = evidence
                        .iter_mut()
                        .find(|item| {
                            item.as_str()
                                .is_some_and(|text| text.starts_with("lease_expires_at:"))
                        })
                        .expect("claim carries lease evidence");
                    *lease = Value::String(EXPIRED_LEASE.to_string());
                    expired_claims += 1;
                }

                rewritten.push_str(&serde_json::to_string(&record).unwrap());
                rewritten.push('\n');
            }
            fs::write(segment, rewritten).unwrap();
        }

        assert_eq!(expired_claims, 2, "both open claims were rewritten");
        self.drop_projection_cache();
    }

    fn drop_projection_cache(&self) {
        let rally = self.cwd.join(".rally");
        for name in [
            "facts.db",
            "facts.db-wal",
            "facts.db-shm",
            ".reconcile-cache.json",
            "snapshot.cache.json",
        ] {
            fs::remove_file(rally.join(name)).ok();
        }
    }

    fn check_names_claim(&self, path: &str, claim_id: &str) -> bool {
        let check = self.json(&[
            "check",
            "before-write",
            "--tool",
            VIEWER,
            "--path",
            path,
            "--json",
        ]);
        check["data"]["check"]["findings"]
            .as_array()
            .expect("check findings are an array")
            .iter()
            .any(|finding| finding["fact_id"].as_str() == Some(claim_id))
    }

    fn render_start_prompt(&self) -> String {
        Command::new("node")
            .arg("--version")
            .output()
            .expect("node is required to exercise the shipped hook renderer");

        let out = Command::new(hook_script())
            .args(["start", VIEWER])
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_BIN", env!("CARGO_BIN_EXE_rally"))
            .env("RALLY_GLOBAL_INDEX", "1")
            .env("RALLY_HOOK_TIMEOUT_MS", "30000")
            .env("RALLY_HOOK_PROMPT", "always")
            // C6: this test pins the VERBOSE roster projection (every active claim
            // scope and both owners). The default room-detail is `brief`, which renders
            // a short single-line message instead, so the pin must name the mode it
            // grades. Not an expectation change — the assertions are untouched.
            .env("RALLY_HOOK_ROOM_DETAIL", "verbose")
            .env("RALLY_SESSION_ID", "hook-projection-parity-session")
            .env("RALLY_HOOK_DEDUPE_DIR", &self.dedupe)
            .env_remove("RALLY_HOOKS")
            .output()
            .expect("spawn shipped coordination hook");
        assert!(
            out.status.success(),
            "hook failed\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        let envelope: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
            panic!(
                "hook emitted invalid JSON ({error})\nstdout={}\nstderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            )
        });
        envelope["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap_or_else(|| panic!("hook omitted SessionStart context: {envelope}"))
            .to_string()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if let Some(base) = self.cwd.parent() {
            fs::remove_dir_all(base).ok();
        }
    }
}

fn hook_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hooks/rally-coordination-hook.sh")
}

fn active_claim_ids(room: &Value) -> Vec<&str> {
    room["data"]["room"]["active_claims"]
        .as_array()
        .expect("room active_claims are an array")
        .iter()
        .filter_map(|claim| claim["event_id"].as_str())
        .collect()
}

fn squad_status<'a>(room: &'a Value, tool: &str) -> Option<&'a str> {
    room["data"]["room"]["squads"]
        .as_array()
        .expect("room squads are an array")
        .iter()
        .find(|squad| squad["tool"].as_str() == Some(tool))
        .and_then(|squad| squad["status"].as_str())
}

#[test]
fn session_prompt_and_before_write_follow_the_same_rust_claim_projection() {
    let workspace = Workspace::new();
    let live_claim = workspace.claim(LIVE_OWNER, LIVE_PATH);
    let unknown_claim = workspace.claim(UNKNOWN_OWNER, UNKNOWN_PATH);
    let closed_claim = workspace.claim(CLOSED_OWNER, CLOSED_PATH);
    workspace.release(CLOSED_OWNER, &closed_claim);
    workspace.make_unknown_and_expire_open_claims();

    let room = workspace.json(&["room", "--json"]);
    let active = active_claim_ids(&room);
    assert!(active.contains(&live_claim.as_str()));
    assert!(active.contains(&unknown_claim.as_str()));
    assert!(!active.contains(&closed_claim.as_str()));
    for claim_id in [&live_claim, &unknown_claim] {
        let claim = room["data"]["room"]["active_claims"]
            .as_array()
            .unwrap()
            .iter()
            .find(|claim| claim["event_id"].as_str() == Some(claim_id.as_str()))
            .unwrap();
        assert!(
            claim["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item.as_str() == Some(EXPIRED_LEASE)),
            "lease expiry alone must not remove active claim {claim_id}"
        );
    }
    assert_eq!(squad_status(&room, LIVE_OWNER), Some("active"));
    assert_eq!(
        squad_status(&room, UNKNOWN_OWNER),
        Some("idle"),
        "an idle-but-Unknown squad must remain in Rust's default room projection"
    );

    let cases = [
        (LIVE_PATH, &live_claim, true),
        (UNKNOWN_PATH, &unknown_claim, true),
        (CLOSED_PATH, &closed_claim, false),
    ];
    let checks: Vec<bool> = cases
        .iter()
        .map(|(path, claim_id, _)| workspace.check_names_claim(path, claim_id))
        .collect();
    let prompt = workspace.render_start_prompt();

    assert!(
        prompt.contains(LIVE_OWNER) && prompt.contains(UNKNOWN_OWNER),
        "Rust-visible Live and Unknown owners must both be rendered: {prompt}"
    );

    for ((path, claim_id, expected_active), check_visible) in cases.iter().zip(checks) {
        let scope = format!("file:{path}");
        let prompt_visible = prompt.contains(&scope);
        let room_visible = active.contains(&claim_id.as_str());
        assert_eq!(
            room_visible, *expected_active,
            "room projection for {scope}"
        );
        assert_eq!(
            check_visible, room_visible,
            "before-write and active_claims disagree for {scope}"
        );
        assert_eq!(
            prompt_visible, check_visible,
            "shipped renderer and before-write disagree for {scope}; prompt={prompt}"
        );
    }
}
