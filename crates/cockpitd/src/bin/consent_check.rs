// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Conformance-check binary for `cockpitd::consent`.
//!
//! `consent.rs` is a library module with no command-line surface, so the
//! language-neutral conformance runner
//! (`<build-loop checkout>/scripts/consent_conformance.py`) had no way to
//! drive it — the runner's `RustAdapter` seam existed but raised
//! `NotImplementedError`. This binary IS that seam's target: a thin CLI that
//! does nothing but parse `--key`/`--json` and call into `consent.rs`. No
//! consent logic is duplicated here.
//!
//! Usage:
//!     consent_check --key build-loop:codex --json
//!
//! Env contract (identical to the Python reference,
//! `scripts/cli_dispatch_consent.py`, and to `consent::check`'s own contract):
//!   - `AGENT_CONSENT_SELFTEST` + `AGENT_CONSENT_STORE_PATH` redirect the
//!     store to a throwaway path — honored by `consent::check` itself via
//!     `consent::store_path()`, so this binary does not touch either var.
//!   - `AGENT_DISPATCH_DEPTH` feeds the depth guard — again read by
//!     `consent::check` from the real process environment; this binary passes
//!     no override, exactly like the crate's own production entry point.
//!
//! Exit codes mirror the contract (`references/cli-dispatch-consent-contract.md`
//! "## Exit codes") via `ConsentVerdict::exit_code()`: 0 allowed, 1 must-ask,
//! 2 denied (also depth-exceeded and unknown-agent-type — the Rust verdict
//! type does not currently carry a fifth code for those), 3 chain broken.

use std::process::ExitCode;

fn print_usage_and_exit(msg: &str) -> ExitCode {
    eprintln!("consent_check: {msg}");
    eprintln!("usage: consent_check --key <product:vendor> [--json]");
    // 2 is "denied" in the contract's exit-code table; a malformed invocation
    // is not a chain-broken (3) or must-ask (1) case, so it is folded into
    // the same "refuse" bucket as denied/depth-exceeded rather than
    // introducing a fifth code this binary would own alone.
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut key: Option<String> = None;
    let mut json = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--key" => {
                i += 1;
                match args.get(i) {
                    Some(v) => key = Some(v.clone()),
                    None => return print_usage_and_exit("--key requires a value"),
                }
            }
            "--json" => json = true,
            other => return print_usage_and_exit(&format!("unrecognized argument {other:?}")),
        }
        i += 1;
    }

    let key = match key {
        Some(k) if !k.is_empty() => k,
        _ => return print_usage_and_exit("--key <product:vendor> is required"),
    };

    // Real production entry point — reads the real store path (subject to
    // the SELFTEST/STORE_PATH override baked into consent::store_path()) and
    // the real AGENT_DISPATCH_DEPTH. No logic from consent.rs is re-derived
    // here.
    let verdict = cockpitd::consent::check(&key);
    let exit = verdict.exit_code();

    if json {
        let out = serde_json::json!({
            "allowed": verdict.allowed,
            "reason": verdict.reason,
            "reason_code": format!("{:?}", verdict.reason_code),
            "key": verdict.key,
            "exit": exit,
        });
        println!("{}", serde_json::to_string(&out).expect("verdict always serializes"));
    } else {
        println!("{}: {}", verdict.key, verdict.reason);
    }

    ExitCode::from(exit as u8)
}
