// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! `rally hook batch`: one process for a lifecycle hook's read set.
//!
//! The contract the shell wrapper relies on: every op's framed output is the
//! exact stdout a separate `rally <op>` invocation prints (minus its trailing
//! newline), ops run in order, `--gate-hooks` stops after a disabled `hooks
//! status`, and anything outside the hook-sized allowlist is refused.

#[path = "../../test-support/temp_root.rs"]
mod temp_root;
use temp_root::TempRoot;

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_rally");

struct Room {
    repo: PathBuf,
    home: PathBuf,
    _root: TempRoot,
}

impl Room {
    fn new(name: &str) -> Self {
        let base = TempRoot::new(&format!("hook-batch-{name}"));
        let repo = base.join("repo");
        let home = base.join("home");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join(".rally")).unwrap();
        fs::create_dir_all(&home).unwrap();
        let room = Self {
            repo,
            home,
            _root: base,
        };
        let seeded = room.run(&["enter", "--tool", "codex:peer", "--json"], &[]);
        assert!(seeded.status.success(), "seed enter: {seeded:?}");
        let claim = room.run(
            &[
                "say",
                "claim",
                "--tool",
                "codex:peer",
                "--path",
                "src/a.rs",
                "--subject",
                "peer editing a",
                "--json",
            ],
            &[],
        );
        assert!(claim.status.success(), "seed claim: {claim:?}");
        room
    }

    fn run(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .current_dir(&self.repo)
            .env("HOME", &self.home)
            .env("RALLY_GLOBAL_INDEX", "1")
            .env_remove("RALLY_HOOKS")
            .env_remove("RALLY_HOOK_TIMEOUT_MS")
            .env_remove("RALLY_TOOL_ID")
            .env_remove("RALLY_SESSION_ID");
        for (key, value) in envs {
            cmd.env(key, value);
        }
        cmd.output().expect("spawn rally")
    }

    fn cleanup(self) {}
}

/// Split `RALLY-HOOK-BATCH/1 rc:len ...\n<outputs>` into (rc, output) pairs.
fn split(stdout: &[u8]) -> Vec<(u8, String)> {
    let newline = stdout
        .iter()
        .position(|b| *b == b'\n')
        .expect("header line");
    let header = std::str::from_utf8(&stdout[..newline]).unwrap();
    let mut items = header.split(' ');
    assert_eq!(items.next(), Some("RALLY-HOOK-BATCH/1"), "header: {header}");
    let mut body = &stdout[newline + 1..];
    // write_line appends one newline after the whole frame.
    assert_eq!(body.last(), Some(&b'\n'));
    body = &body[..body.len() - 1];
    let mut out = Vec::new();
    let mut offset = 0;
    for item in items {
        let (rc, len) = item.split_once(':').expect("rc:len");
        let len: usize = len.parse().unwrap();
        out.push((
            rc.parse().unwrap(),
            String::from_utf8(body[offset..offset + len].to_vec()).unwrap(),
        ));
        offset += len;
    }
    assert_eq!(offset, body.len(), "lengths must cover the body exactly");
    out
}

fn batch_args(ops: &[&[&str]], gate: bool) -> Vec<String> {
    let mut args = vec!["hook".to_string(), "batch".to_string()];
    if gate {
        args.push("--gate-hooks".to_string());
    }
    args.push("--counts".to_string());
    args.push(
        ops.iter()
            .map(|op| op.len().to_string())
            .collect::<Vec<_>>()
            .join(","),
    );
    args.push("--".to_string());
    for op in ops {
        args.extend(op.iter().map(|s| s.to_string()));
    }
    args
}

#[test]
fn batched_reads_are_byte_identical_to_separate_invocations() {
    let room = Room::new("parity");
    let ops: [&[&str]; 4] = [
        &["hooks", "status", "--json", "--timeout-ms", "2000"],
        &["room", "--json", "--timeout-ms", "2000"],
        &[
            "next",
            "--tool",
            "claude_code:me",
            "--audit",
            "--json",
            "--timeout-ms",
            "2000",
        ],
        &[
            "check",
            "before-complete",
            "--tool",
            "codex:peer",
            "--strict",
            "--json",
        ],
    ];
    let args = batch_args(&ops, true);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    // A trailing aggregate budget is what the wrapper's guard appends.
    let mut with_budget = arg_refs.clone();
    with_budget.extend(["--timeout-ms", "8000"]);
    // Room/next text carries second-resolution ages, so a batch and the
    // separate calls can straddle a clock tick under load. Byte parity must
    // hold on at least one of three back-to-back attempts; a real drift
    // (different render path) fails all three.
    let mut last_drift = String::new();
    let mut results = Vec::new();
    for _attempt in 0..3 {
        let batch = room.run(&with_budget, &[]);
        assert!(batch.status.success(), "batch: {batch:?}");
        results = split(&batch.stdout);
        assert_eq!(results.len(), ops.len());
        last_drift.clear();
        for (op, (rc, output)) in ops.iter().zip(&results) {
            let separate = room.run(op, &[]);
            let expected = String::from_utf8(separate.stdout).unwrap();
            if *rc != separate.status.code().unwrap() as u8
                || output != expected.trim_end_matches('\n')
            {
                last_drift = format!(
                    "`rally {}` drifted:\nbatched={output}\nseparate={expected}",
                    op.join(" ")
                );
                break;
            }
        }
        if last_drift.is_empty() {
            break;
        }
    }
    assert!(last_drift.is_empty(), "{last_drift}");
    // The peer's claim is visible through the batched room read.
    let room_json: Value = serde_json::from_str(&results[1].1).unwrap();
    assert!(room_json.to_string().contains("src/a.rs"));
    room.cleanup();
}

#[test]
fn gate_hooks_skips_everything_after_a_disabled_hooks_status() {
    let room = Room::new("gate");
    let ops: [&[&str]; 3] = [
        &["hooks", "status", "--json"],
        &["room", "--json"],
        &[
            "status",
            "post",
            "--tool",
            "claude_code:me",
            "--state",
            "idle",
            "--json",
        ],
    ];
    let args = batch_args(&ops, true);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = room.run(&arg_refs, &[("RALLY_HOOKS", "off")]);
    assert!(out.status.success(), "{out:?}");
    let results = split(&out.stdout);
    assert_eq!(results[0].0, 0);
    assert!(
        results[0].1.contains("\"enabled\": false"),
        "{}",
        results[0].1
    );
    assert_eq!((results[1].0, results[1].1.as_str()), (125, ""));
    assert_eq!((results[2].0, results[2].1.as_str()), (125, ""));
    // The skipped heartbeat must not have reached the ledger.
    let status = room.run(&["status", "read", "--json"], &[]);
    assert!(
        !String::from_utf8_lossy(&status.stdout).contains("claude_code:me"),
        "a gated batch posted a heartbeat"
    );

    // Without the gate the same batch runs every op, heartbeat last.
    let ungated = batch_args(&ops, false);
    let ungated_refs: Vec<&str> = ungated.iter().map(String::as_str).collect();
    let out = room.run(&ungated_refs, &[("RALLY_HOOKS", "off")]);
    let results = split(&out.stdout);
    assert!(results.iter().all(|(rc, _)| *rc == 0), "{results:?}");
    let status = room.run(&["status", "read", "--json"], &[]);
    assert!(String::from_utf8_lossy(&status.stdout).contains("claude_code:me"));
    room.cleanup();
}

#[test]
fn non_hook_ops_and_malformed_counts_are_refused() {
    let room = Room::new("refuse");
    for args in [
        // Not on the allowlist: long-running or claim-writing commands.
        vec!["hook", "batch", "--counts", "2", "--", "watch", "--json"],
        vec![
            "hook", "batch", "--counts", "3", "--", "say", "claim", "--json",
        ],
        // Counts that overrun the argv, and junk after the op list.
        vec!["hook", "batch", "--counts", "4", "--", "room", "--json"],
        vec![
            "hook", "batch", "--counts", "2", "--", "room", "--json", "extra",
        ],
        vec!["hook", "batch", "--counts", "0", "--"],
    ] {
        let out = room.run(&args, &[]);
        assert_eq!(out.status.code(), Some(2), "{args:?} -> {out:?}");
        assert!(out.stdout.is_empty(), "{args:?} printed {:?}", out.stdout);
    }
    room.cleanup();
}

#[test]
fn capabilities_advertise_batch_for_the_wrapper_probe() {
    let room = Room::new("caps");
    let out = room.run(&["hook", "capabilities", "--json"], &[]);
    let text = String::from_utf8(out.stdout).unwrap();
    // The wrapper matches this exact pretty spelling.
    assert!(text.contains("\"batch\": {"), "{text}");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["data"]["hook"]["batch"]["contract"], 1);
    room.cleanup();
}
