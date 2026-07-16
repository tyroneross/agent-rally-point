// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `rally claims-refresh` — the one-shot lane-claim
//! refresh that replaces the 8+ manual `rally say claim` ritual per multi-lane
//! run (retro §11 / easy-terminal enforce-candidate #3).
//!
//! Proven behaviors:
//! - a whole manifest is claimed in ONE call;
//! - re-running RENEWS own claims (no conflict);
//! - a live peer's conflicting claim is REPORTED without blocking the rest of
//!   the manifest (graceful degradation).

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct Workspace {
    cwd: PathBuf,
    home: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let cwd = std::env::temp_dir().join(format!("cr-{name}-{nanos}-cwd"));
        let home = std::env::temp_dir().join(format!("cr-{name}-{nanos}-home"));
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(cwd.join(".git")).unwrap();
        Self { cwd, home }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .args(args)
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "cmd {:?} failed\nstderr: {}\nstdout: {}",
            args,
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout),
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn write_manifest(&self, name: &str, body: &str) {
        fs::write(self.cwd.join(name), body).unwrap();
    }

    fn cleanup(self) {
        fs::remove_dir_all(self.cwd).ok();
        fs::remove_dir_all(self.home).ok();
    }
}

fn claims_refresh_data(v: &Value) -> &Value {
    &v["data"]["claims_refresh"]
}

#[test]
fn claims_refresh_claims_full_manifest_in_one_call() {
    let ws = Workspace::new("full-manifest");
    // Comment + blank lines must be ignored; three real paths claimed.
    ws.write_manifest(
        "lane.manifest",
        "# lane alpha files\nsrc/a.rs\n\nsrc/b.rs\nsrc/c.rs\n",
    );

    let out = ws.json(&[
        "claims-refresh",
        "--tool",
        "claude_code",
        "--lane",
        "alpha",
        "--manifest",
        "lane.manifest",
        "--json",
    ]);
    let d = claims_refresh_data(&out);
    assert_eq!(d["total"], 3, "three claimable paths parsed");
    assert_eq!(
        d["claimed"].as_array().unwrap().len(),
        3,
        "all three claimed"
    );
    assert_eq!(d["conflicts"].as_array().unwrap().len(), 0, "no conflicts");
    assert_eq!(d["lane"], "alpha");

    // Claims landed in the room, tagged with the lane evidence marker.
    let claims = ws.json(&["claims", "--json"]);
    let rows = claims["data"]["claims"]["rows"].as_array().unwrap();
    let scopes: Vec<String> = rows
        .iter()
        .flat_map(|r| r["scope"].as_array().cloned().unwrap_or_default())
        .map(|s| s.as_str().unwrap_or_default().to_string())
        .collect();
    for f in ["file:src/a.rs", "file:src/b.rs", "file:src/c.rs"] {
        assert!(
            scopes.iter().any(|s| s == f),
            "claim landed for {f}: {scopes:?}"
        );
    }
    let has_lane_marker = rows.iter().any(|r| {
        r["evidence"]
            .as_array()
            .map(|ev| ev.iter().any(|e| e.as_str() == Some("lane:alpha")))
            .unwrap_or(false)
    });
    assert!(
        has_lane_marker,
        "at least one claim carries lane:alpha evidence"
    );

    ws.cleanup();
}

#[test]
fn claims_refresh_renews_own_claims_without_conflict() {
    let ws = Workspace::new("renew");
    ws.write_manifest("m", "src/x.rs\nsrc/y.rs\n");
    let args = [
        "claims-refresh",
        "--tool",
        "claude_code",
        "--lane",
        "beta",
        "--manifest",
        "m",
        "--json",
    ];
    // First pass claims; second pass must RENEW (same owner → no conflict).
    let first = ws.json(&args);
    assert_eq!(
        claims_refresh_data(&first)["claimed"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let second = ws.json(&args);
    let d = claims_refresh_data(&second);
    assert_eq!(
        d["claimed"].as_array().unwrap().len(),
        2,
        "renewal re-claims both"
    );
    assert_eq!(
        d["conflicts"].as_array().unwrap().len(),
        0,
        "own-claim renewal never conflicts"
    );
    ws.cleanup();
}

#[test]
fn claims_refresh_reports_live_peer_conflict_without_blocking_rest() {
    let ws = Workspace::new("conflict");
    // A live peer (different tool) claims one file first.
    let peer = ws.run(&[
        "say",
        "claim",
        "--tool",
        "codex",
        "--path",
        "src/shared.rs",
        "--summary",
        "peer holds shared.rs",
    ]);
    assert!(
        peer.status.success(),
        "peer claim: {}",
        String::from_utf8_lossy(&peer.stderr)
    );

    ws.write_manifest("m", "src/shared.rs\nsrc/mine.rs\n");
    let out = ws.json(&[
        "claims-refresh",
        "--tool",
        "claude_code",
        "--lane",
        "gamma",
        "--manifest",
        "m",
        "--json",
    ]);
    let d = claims_refresh_data(&out);
    // shared.rs conflicts (live peer); mine.rs still claimed — graceful degradation.
    let claimed: Vec<&str> = d["claimed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        claimed,
        vec!["src/mine.rs"],
        "only the non-conflicting file claimed"
    );
    let conflicts = d["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1, "exactly one live-peer conflict");
    assert_eq!(conflicts[0]["path"], "src/shared.rs");
    assert!(
        conflicts[0]["owner"].as_str().unwrap().contains("codex"),
        "conflict names the peer owner: {}",
        conflicts[0]["owner"]
    );
    ws.cleanup();
}

#[test]
fn claims_refresh_empty_manifest_errors() {
    let ws = Workspace::new("empty");
    ws.write_manifest("m", "# only comments\n\n");
    let out = ws.run(&[
        "claims-refresh",
        "--tool",
        "claude_code",
        "--lane",
        "delta",
        "--manifest",
        "m",
    ]);
    assert!(
        !out.status.success(),
        "blank/comment-only manifest is a usage error"
    );
    ws.cleanup();
}
