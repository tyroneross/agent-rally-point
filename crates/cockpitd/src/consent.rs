// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! CLI dispatch consent gate — Rally Point's half of a shared, frozen contract.
//!
//! Contract: `<build-loop checkout>/references/cli-dispatch-consent-contract.md`.
//! Reference implementation (must be byte-compatible on hashing): `<build-loop
//! checkout>/scripts/cli_dispatch_consent.py`. This module is the OTHER
//! implementation — different language, different chokepoint (in-process check
//! in `supervisor.rs`, vs. a PreToolUse hook on the Build Loop side) — and both
//! are graded against the same conformance suite. Read the contract before
//! changing anything here.
//!
//! Rally Point is a daemon, not an interactive host: it has no ask surface
//! (`AskUserQuestion`, a Codex approval prompt, …). So this module NEVER
//! records a consent decision — only the operator's own tooling (the Python
//! CLI, or whatever authors `~/.agent-consent/cli-dispatch-consent.json`)
//! writes that file. Rally Point only reads what the operator already decided.
//! Absence of a record is never consent — it means "must ask", and a daemon
//! with no ask surface refuses rather than asking on the operator's behalf.
//!
//! WHAT THIS IS: tamper-EVIDENT, not tamper-proof. The gated process runs as
//! the operator and can write the store. What it cannot do is write it without
//! breaking the hash chain and changing the head hash the operator was last
//! shown. Detection, not prevention — see the contract's "What this gate is,
//! and is not" section.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

// ── Contract constants ──────────────────────────────────────────────────────

/// This implementation's product name in the `"<product>:<vendor>"` key.
const PRODUCT: &str = "rally-point";

/// Modes the wire format allows. Only `"auto"` grants; every other value
/// (including anything not in this list — wrong case, padding, unknown
/// strings) refuses. Exact string match, same as the Python reference.
const MODES: &[&str] = &["once", "ask", "auto", "denied"];

/// Env var incremented by each dispatching process, vendor- and
/// product-neutral by contract design (the cascade it caps crosses products).
const DEPTH_ENV: &str = "AGENT_DISPATCH_DEPTH";
/// Above this depth, refuse regardless of any recorded consent.
const DEPTH_CAP: i64 = 2;

/// Test-only store-path redirection, honored ONLY when `SELFTEST_ENV` is also
/// set — mirrors the Python reference's `PYTEST_CURRENT_TEST` /
/// `AGENT_CONSENT_SELFTEST` gate. As a general override this would be a
/// one-line bypass of everything above it, so it is a two-key handshake, not
/// a single env var.
const TEST_STORE_ENV: &str = "AGENT_CONSENT_STORE_PATH";
const SELFTEST_ENV: &str = "AGENT_CONSENT_SELFTEST";

// ── Public types ─────────────────────────────────────────────────────────────

/// Machine-readable reason a dispatch was allowed or refused.
///
/// Every non-`Allowed` variant is its own branch in `check_with` — the
/// contract's "never-relax rule": a recorded decision may only ever turn a
/// `confirm` into an `auto`, and each way of failing to be a verified,
/// literal `auto` is a separate code path, not a documentation claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasonCode {
    /// Operator recorded `auto` for this key, and the chain verifies.
    Allowed,
    /// No entry for this key was found by replaying the log.
    NoRecord,
    /// A recorded entry exists but its mode is `once` or `ask` — a prior
    /// answer exists, but it grants nothing forward.
    NotAuto,
    /// Operator recorded `denied` for this key.
    Denied,
    /// The hash chain does not verify (tampered, edited in place, or a
    /// broken `seq`/`prev_sha256` link). Treated as no consent.
    ChainBroken,
    /// `AGENT_DISPATCH_DEPTH` is unset-but-invalid, negative, non-integer, or
    /// above the cap. Checked BEFORE the store and overrides any recorded
    /// `auto` — no consent answer the operator gave was an answer about
    /// recursion.
    DepthExceeded,
    /// `agent_type` does not map to a recognized contract vendor. Refuses
    /// rather than defaulting to allow.
    UnknownAgentType,
}

/// The result of a consent check: may this dispatch proceed WITHOUT asking?
///
/// Rally Point has no ask surface, so `allowed: false` always means "do not
/// dispatch" — there is no `needs_prompt` distinction the way the Python CLI
/// exposes one for an interactive host to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentVerdict {
    pub allowed: bool,
    pub reason_code: ReasonCode,
    pub reason: String,
    pub key: String,
}

impl ConsentVerdict {
    /// Mirrors the reference implementation's exit-code convention
    /// (contract §"Exit codes") purely so Rally Point logs and the Python
    /// CLI's own output correlate for anyone debugging across both halves.
    /// Not otherwise used by the gate itself.
    pub fn exit_code(&self) -> i32 {
        match self.reason_code {
            ReasonCode::Allowed => 0,
            ReasonCode::NoRecord | ReasonCode::NotAuto => 1,
            ReasonCode::Denied | ReasonCode::DepthExceeded | ReasonCode::UnknownAgentType => 2,
            ReasonCode::ChainBroken => 3,
        }
    }
}

// ── Public API ───────────────────────────────────────────────────────────────

/// May `key` (already in `"<product>:<vendor>"` form) dispatch right now
/// without asking? Reads the real per-operator store and the real process
/// environment — production entry point.
pub fn check(key: &str) -> ConsentVerdict {
    check_with(key, &store_path(), None)
}

/// Map a Rally Point `agent_type` (as passed to `Supervisor::launch_session`)
/// to a contract vendor, build the `"rally-point:<vendor>"` key, and check it.
///
/// Only `"claude"` and `"codex"` are recognized — the two adapters Rally
/// Point actually implements (`adapter::claude`, `adapter::codex`). Anything
/// else refuses rather than defaulting to allow, per the contract's
/// never-relax rule: an unmapped vendor is not the same as an unconditional
/// grant, and guessing a mapping for a string we don't ship an adapter for
/// would be worse than refusing.
pub fn check_for_agent_type(agent_type: &str) -> ConsentVerdict {
    match key_for_agent_type(agent_type) {
        Some(key) => check(&key),
        None => ConsentVerdict {
            allowed: false,
            reason_code: ReasonCode::UnknownAgentType,
            reason: format!(
                "agent_type {agent_type:?} is not a recognized consent vendor \
                 (rally-point maps only \"claude\" and \"codex\"); refusing \
                 rather than defaulting to allow"
            ),
            key: format!("{PRODUCT}:{agent_type}"),
        },
    }
}

fn key_for_agent_type(agent_type: &str) -> Option<String> {
    match agent_type {
        "claude" => Some(format!("{PRODUCT}:claude")),
        "codex" => Some(format!("{PRODUCT}:codex")),
        _ => None,
    }
}

// ── Core check (path + depth injectable for tests) ─────────────────────────

/// Core logic. `path` and `depth_override` are injectable so tests never
/// touch the operator's real `~/.agent-consent` store or race on the real
/// `AGENT_DISPATCH_DEPTH` process env — production callers go through
/// `check()`/`check_for_agent_type()`, which supply the real store path and
/// `None` (read the real env).
fn check_with(key: &str, path: &Path, depth_override: Option<&str>) -> ConsentVerdict {
    let key_s = key.to_string();

    // Depth is checked FIRST — it overrides any recorded consent, because no
    // answer the operator gave was an answer about recursion.
    let depth = depth_status(depth_override);
    if depth.exceeded {
        return ConsentVerdict {
            allowed: false,
            reason_code: ReasonCode::DepthExceeded,
            reason: depth.reason,
            key: key_s,
        };
    }

    let store = load(path);
    let chain = verify_chain(&store);
    if !chain.ok {
        return ConsentVerdict {
            allowed: false,
            reason_code: ReasonCode::ChainBroken,
            reason: format!(
                "consent log does not verify ({}); treating as no consent",
                chain.reason
            ),
            key: key_s,
        };
    }

    let state = replay(&store);
    let entry = match state.get(&key_s) {
        Some(e) => e,
        None => {
            return ConsentVerdict {
                allowed: false,
                reason_code: ReasonCode::NoRecord,
                reason: format!("no consent recorded for {key_s}; the operator has not been asked"),
                key: key_s,
            };
        }
    };

    let mode = entry.get("mode").and_then(|m| m.as_str()).unwrap_or("");
    let decided_at = entry
        .get("decided_at")
        .and_then(|d| d.as_str())
        .unwrap_or("unknown");

    if mode == "auto" {
        return ConsentVerdict {
            allowed: true,
            reason_code: ReasonCode::Allowed,
            reason: format!("operator set {key_s} to auto on {decided_at}"),
            key: key_s,
        };
    }
    if mode == "denied" {
        return ConsentVerdict {
            allowed: false,
            reason_code: ReasonCode::Denied,
            reason: format!("operator denied {key_s} on {decided_at}"),
            key: key_s,
        };
    }
    // ask / once — a prior answer exists, but it grants nothing forward.
    ConsentVerdict {
        allowed: false,
        reason_code: ReasonCode::NotAuto,
        reason: format!("operator chose {mode:?} for {key_s}; ask again before dispatching"),
        key: key_s,
    }
}

// ── Store path ───────────────────────────────────────────────────────────────

/// Fixed per-operator path under a vendor-neutral directory — see contract
/// §"Store". The env override is honored ONLY when `SELFTEST_ENV` is also
/// set, exactly like the Python reference's test-runner gate.
///
/// Fails CLOSED when `HOME` is unset or empty: returns a path that cannot
/// exist, so `load()` returns an empty store and the caller sees `NoRecord`,
/// never a spurious grant.
fn store_path() -> PathBuf {
    if std::env::var(SELFTEST_ENV).is_ok()
        && let Ok(over) = std::env::var(TEST_STORE_ENV)
    {
        return PathBuf::from(over);
    }
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => Path::new(&home)
            .join(".agent-consent")
            .join("cli-dispatch-consent.json"),
        _ => {
            // No legitimate HOME to read from. Point at a path that cannot
            // exist rather than falling back to a relative/cwd-based guess
            // that could accidentally resolve to something readable.
            PathBuf::from(
                "/nonexistent-agent-consent-home-is-unset/.agent-consent/cli-dispatch-consent.json",
            )
        }
    }
}

// ── Wire format: load / hash / verify / replay ──────────────────────────────

/// The parsed store: just the log array. Entries are kept as raw `Value` —
/// like the Python reference, this module never round-trips through a fixed
/// struct, so an entry with unexpected/extra fields still hashes and chains
/// correctly instead of silently losing data to a struct that doesn't know
/// about them.
#[derive(Debug, Clone, Default)]
struct ConsentStore {
    log: Vec<Value>,
}

/// Never panics or propagates an error. An unreadable, missing, or malformed
/// store is NO consent, not consent — matching the Python reference's `load()`
/// docstring verbatim.
fn load(path: &Path) -> ConsentStore {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(_) => return ConsentStore::default(),
    };
    let parsed: Value = match serde_json::from_str(&data) {
        Ok(v) => v,
        Err(_) => return ConsentStore::default(),
    };
    let log = parsed
        .as_object()
        .and_then(|o| o.get("log"))
        .and_then(|l| l.as_array())
        .cloned();
    match log {
        Some(log) => ConsentStore { log },
        None => ConsentStore::default(),
    }
}

/// `entry_sha256` = SHA-256 over the UTF-8 canonical JSON of the entry with
/// `entry_sha256` removed, keys sorted, separators `(",", ":")`, no trailing
/// newline — contract §"Hashing", byte-for-byte identical to the Python
/// reference's `_canonical` + `entry_hash`.
///
/// `BTreeMap<String, Value>` gives sorted keys "for free" (matches Python's
/// `sort_keys=True`, since both sort lexicographically by the key string),
/// and `serde_json::to_vec` on a map already emits the compact `{"a":1,"b":2}`
/// form with no spaces by default — matching `separators=(",", ":")` with no
/// extra configuration needed. Verified against real Python output in
/// `tests::cross_implementation_digest_matches_python` below.
fn canonical_bytes(entry: &Value) -> Vec<u8> {
    let mut sorted: BTreeMap<String, Value> = BTreeMap::new();
    if let Some(obj) = entry.as_object() {
        for (k, v) in obj {
            if k != "entry_sha256" {
                sorted.insert(k.clone(), v.clone());
            }
        }
    }
    serde_json::to_vec(&sorted).expect("canonical entry map always serializes")
}

fn entry_hash(entry: &Value) -> String {
    let digest = Sha256::digest(canonical_bytes(entry));
    format!("{digest:x}")
}

struct ChainResult {
    ok: bool,
    reason: String,
}

/// Walk every entry: `seq` must equal its index, `prev_sha256` must equal the
/// previous entry's `entry_sha256` (or `null` at index 0), and `entry_sha256`
/// must equal the recomputed hash. Any failure = chain broken = refuse.
fn verify_chain(store: &ConsentStore) -> ChainResult {
    let mut prev: Option<String> = None;
    for (i, e) in store.log.iter().enumerate() {
        let obj = match e.as_object() {
            Some(o) => o,
            None => {
                return ChainResult {
                    ok: false,
                    reason: format!("entry {i} is not an object"),
                };
            }
        };

        let seq_matches = obj.get("seq").and_then(|s| s.as_i64()) == Some(i as i64);
        if !seq_matches {
            return ChainResult {
                ok: false,
                reason: format!("entry {i} has seq {:?}, expected {i}", obj.get("seq")),
            };
        }

        let prev_field = obj.get("prev_sha256").cloned().unwrap_or(Value::Null);
        let expected_prev = match &prev {
            Some(h) => Value::String(h.clone()),
            None => Value::Null,
        };
        if prev_field != expected_prev {
            return ChainResult {
                ok: false,
                reason: format!(
                    "entry {i} prev_sha256 does not match entry {}",
                    i as i64 - 1
                ),
            };
        }

        let want = entry_hash(e);
        let got = obj.get("entry_sha256").and_then(|s| s.as_str());
        if got != Some(want.as_str()) {
            return ChainResult {
                ok: false,
                reason: format!("entry {i} content does not match its own hash (edited in place)"),
            };
        }
        prev = Some(want);
    }
    ChainResult {
        ok: true,
        reason: "chain verifies".to_string(),
    }
}

/// Derive current consent from the log by replaying in `seq` order; the last
/// entry per key wins. Entries with a `mode` outside the exact `MODES` set —
/// wrong case (`"AUTO"`), padded (`"auto "`), missing, or any other string —
/// are skipped during replay, exactly like the Python reference's
/// `e.get("mode") in MODES` filter. State is derived, never materialized
/// separately, so there is nothing else that can disagree with the log.
fn replay(store: &ConsentStore) -> HashMap<String, Value> {
    let mut state = HashMap::new();
    for e in &store.log {
        if let Some(obj) = e.as_object() {
            let key = obj.get("key").and_then(|k| k.as_str());
            let mode = obj.get("mode").and_then(|m| m.as_str());
            if let (Some(k), Some(m)) = (key, mode)
                && MODES.contains(&m)
            {
                state.insert(k.to_string(), e.clone());
            }
        }
    }
    state
}

// ── Depth guard ──────────────────────────────────────────────────────────────

struct DepthStatus {
    exceeded: bool,
    reason: String,
}

/// `depth_override` lets tests supply a raw string without mutating (and
/// racing on) the real process-wide `AGENT_DISPATCH_DEPTH`. `None` reads the
/// real env — the production path.
///
/// Unset (or explicitly empty) reads as depth 0. A non-integer value reads as
/// EXCEEDED, not as 0 — a garbage value is the shape a bypass attempt takes.
/// A NEGATIVE value likewise reads as exceeded: no legitimate caller counts
/// backwards, and `-1` would otherwise buy recursion headroom above the cap
/// instead of being clamped to it.
fn depth_status(depth_override: Option<&str>) -> DepthStatus {
    let raw = match depth_override {
        Some(s) => Some(s.to_string()),
        None => std::env::var(DEPTH_ENV).ok(),
    };
    let raw = match raw {
        None => {
            return DepthStatus {
                exceeded: false,
                reason: "depth unset; treated as 0".to_string(),
            };
        }
        Some(r) => r,
    };
    if raw.is_empty() {
        return DepthStatus {
            exceeded: false,
            reason: "depth unset; treated as 0".to_string(),
        };
    }
    match raw.parse::<i64>() {
        Err(_) => DepthStatus {
            exceeded: true,
            reason: format!("{DEPTH_ENV}={raw:?} is not an integer; refusing"),
        },
        Ok(d) if d < 0 => DepthStatus {
            exceeded: true,
            reason: format!("{DEPTH_ENV}={raw:?} is negative; refusing"),
        },
        Ok(d) => DepthStatus {
            exceeded: d > DEPTH_CAP,
            reason: format!("dispatch depth {d} (cap {DEPTH_CAP})"),
        },
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── test helpers ─────────────────────────────────────────────────────────

    /// Build one valid, correctly-hashed entry.
    fn make_entry(seq: i64, key: &str, mode: &str, decided_at: &str, prev: Option<&str>) -> Value {
        let mut e = json!({
            "seq": seq,
            "key": key,
            "mode": mode,
            "decided_at": decided_at,
            "decided_by": "user",
            "decided_via": "test",
            "decided_in_repo": "/tmp/test-repo",
            "prev_sha256": prev,
        });
        let h = entry_hash(&e);
        e["entry_sha256"] = json!(h);
        e
    }

    fn write_store(path: &Path, log: Vec<Value>) {
        let doc = json!({ "version": 2, "log": log });
        std::fs::write(path, serde_json::to_vec(&doc).unwrap()).unwrap();
    }

    /// Unique-per-test temp path — every test in this module passes its own
    /// path explicitly to `check_with`, so tests never read or write the
    /// operator's real `~/.agent-consent`, and never race on a shared file.
    fn temp_store_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "cockpitd-consent-test-{name}-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        p
    }

    struct TempFile(PathBuf);
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    // ── Cross-implementation digest check ───────────────────────────────────

    /// Digests below were obtained by actually running the Python reference
    /// implementation, from the build-loop checkout root:
    ///
    ///   python3 - <<'EOF'
    ///   import sys
    ///   sys.path.insert(0, "scripts")
    ///   from cli_dispatch_consent import entry_hash
    ///   entry = {
    ///       "seq": 0, "key": "rally-point:codex", "mode": "auto",
    ///       "decided_at": "2026-08-21T18:04:11Z", "decided_by": "user",
    ///       "decided_via": "claude_code_ask",
    ///       "decided_in_repo": "/Users/tyroneross/dev/git-folder/agent-rally-point",
    ///       "prev_sha256": None,
    ///   }
    ///   print(entry_hash(entry))
    ///   entry2 = dict(entry); entry2["entry_sha256"] = "deadbeef"
    ///   print(entry_hash(entry2))  # must be identical — stripped before hashing
    ///   entry3 = {**entry, "seq": 1, "mode": "denied",
    ///             "decided_at": "2026-08-21T18:10:00Z",
    ///             "prev_sha256": entry_hash(entry)}
    ///   print(entry_hash(entry3))
    ///   EOF
    #[test]
    fn cross_implementation_digest_matches_python() {
        let entry = json!({
            "seq": 0,
            "key": "rally-point:codex",
            "mode": "auto",
            "decided_at": "2026-08-21T18:04:11Z",
            "decided_by": "user",
            "decided_via": "claude_code_ask",
            "decided_in_repo": "/Users/tyroneross/dev/git-folder/agent-rally-point",
            "prev_sha256": null
        });
        assert_eq!(
            entry_hash(&entry),
            "ce3528c3a11578ff75845ba7989db63e839a1fca199037bdc61b57c5005a0a92",
            "Rust digest must match the Python reference's entry_hash() for the same entry"
        );

        // NON-ASCII IS THE CASE THAT ACTUALLY BREAKS INTEROP. The two ASCII
        // assertions above pass whether or not the canonicalizers agree on
        // escaping, so they cannot catch the real divergence: Python's json.dumps
        // escapes non-ASCII to \uXXXX by default while serde_json emits it raw, and
        // `decided_in_repo` is a filesystem path. One accented character in a repo
        // name gave the two implementations different digests for the same entry,
        // so each read the other's valid chain as broken. The contract now pins raw
        // UTF-8 (RFC 8785); this assertion is what holds Python to it.
        let non_ascii = json!({
            "seq": 0,
            "key": "rally-point:codex",
            "mode": "auto",
            "decided_at": "2026-08-21T18:04:11Z",
            "decided_by": "user",
            "decided_via": "claude_code_ask",
            "decided_in_repo": "/Users/tyroneross/dev/café-app",
            "prev_sha256": null
        });
        assert_eq!(
            entry_hash(&non_ascii),
            "ed7b39bc53fe45fbcf73b2a01f09b157c55a0c67a76e0c1605cdff5b44f23eba",
            "non-ASCII must hash raw, not \\uXXXX-escaped — see contract 'Wire format / Hashing'"
        );

        // entry_sha256 present in the input must be stripped before hashing
        // (idempotent — matches Python's `{k: v for k, v in entry.items() if k != "entry_sha256"}`).
        let mut with_hash = entry.clone();
        with_hash["entry_sha256"] = json!("deadbeef");
        assert_eq!(entry_hash(&with_hash), entry_hash(&entry));

        let entry2 = json!({
            "seq": 1,
            "key": "rally-point:codex",
            "mode": "denied",
            "decided_at": "2026-08-21T18:10:00Z",
            "decided_by": "user",
            "decided_via": "claude_code_ask",
            "decided_in_repo": "/Users/tyroneross/dev/git-folder/agent-rally-point",
            "prev_sha256": entry_hash(&entry)
        });
        assert_eq!(
            entry_hash(&entry2),
            "60961374d3560a00157d671fa88a86408014bf8c33349cc92563393eb445d3d0"
        );
    }

    // ── Mode grants / refusals ───────────────────────────────────────────────

    #[test]
    fn auto_mode_grants() {
        let path = temp_store_path("auto-grants");
        let _tmp = TempFile(path.clone());
        let e0 = make_entry(0, "rally-point:codex", "auto", "2026-08-21T00:00:00Z", None);
        write_store(&path, vec![e0]);

        let v = check_with("rally-point:codex", &path, Some("0"));
        assert!(v.allowed, "{v:?}");
        assert_eq!(v.reason_code, ReasonCode::Allowed);
    }

    #[test]
    fn once_ask_denied_refuse() {
        for mode in ["once", "ask", "denied"] {
            let path = temp_store_path(&format!("mode-{mode}"));
            let _tmp = TempFile(path.clone());
            let e0 = make_entry(0, "rally-point:codex", mode, "2026-08-21T00:00:00Z", None);
            write_store(&path, vec![e0]);

            let v = check_with("rally-point:codex", &path, Some("0"));
            assert!(!v.allowed, "mode {mode:?} should refuse: {v:?}");
        }
    }

    #[test]
    fn absent_key_refuses() {
        let path = temp_store_path("absent-key");
        let _tmp = TempFile(path.clone());
        write_store(&path, vec![]);

        let v = check_with("rally-point:codex", &path, Some("0"));
        assert!(!v.allowed);
        assert_eq!(v.reason_code, ReasonCode::NoRecord);
    }

    #[test]
    fn missing_store_file_refuses() {
        let path = temp_store_path("missing-file");
        let _ = std::fs::remove_file(&path); // ensure it does not exist

        let v = check_with("rally-point:codex", &path, Some("0"));
        assert!(!v.allowed);
        assert_eq!(v.reason_code, ReasonCode::NoRecord);
    }

    #[test]
    fn malformed_json_refuses() {
        let path = temp_store_path("malformed-json");
        let _tmp = TempFile(path.clone());
        std::fs::write(&path, b"{ this is not valid json").unwrap();

        let v = check_with("rally-point:codex", &path, Some("0"));
        assert!(!v.allowed);
        assert_eq!(
            v.reason_code,
            ReasonCode::NoRecord,
            "malformed JSON loads as an empty store"
        );
    }

    #[test]
    fn entry_missing_mode_field_refuses() {
        let path = temp_store_path("missing-mode-field");
        let _tmp = TempFile(path.clone());
        let mut e0 = json!({
            "seq": 0,
            "key": "rally-point:codex",
            "decided_at": "2026-08-21T00:00:00Z",
            "decided_by": "user",
            "decided_via": "test",
            "decided_in_repo": "/tmp/test-repo",
            "prev_sha256": null,
        });
        let h = entry_hash(&e0);
        e0["entry_sha256"] = json!(h);
        write_store(&path, vec![e0]);

        let v = check_with("rally-point:codex", &path, Some("0"));
        assert!(!v.allowed);
    }

    #[test]
    fn wrong_case_and_padded_modes_refuse() {
        for mode in ["AUTO", "Auto", "auto ", " auto"] {
            let path = temp_store_path(&format!("mode-variant-{}", mode.trim().len()));
            let _tmp = TempFile(path.clone());
            let e0 = make_entry(0, "rally-point:codex", mode, "2026-08-21T00:00:00Z", None);
            write_store(&path, vec![e0]);

            let v = check_with("rally-point:codex", &path, Some("0"));
            assert!(!v.allowed, "mode {mode:?} should refuse: {v:?}");
        }
    }

    // ── Chain integrity ──────────────────────────────────────────────────────

    #[test]
    fn edited_entry_breaks_chain() {
        let path = temp_store_path("chain-broken");
        let _tmp = TempFile(path.clone());
        let mut e0 = make_entry(0, "rally-point:codex", "auto", "2026-08-21T00:00:00Z", None);
        // Tamper: flip the mode after the hash was computed, without
        // recomputing entry_sha256 — exactly what a forging agent would do.
        e0["mode"] = json!("denied");
        write_store(&path, vec![e0]);

        let v = check_with("rally-point:codex", &path, Some("0"));
        assert!(!v.allowed);
        assert_eq!(v.reason_code, ReasonCode::ChainBroken);
    }

    #[test]
    fn broken_prev_link_breaks_chain() {
        let path = temp_store_path("chain-prev-broken");
        let _tmp = TempFile(path.clone());
        let e0 = make_entry(
            0,
            "rally-point:codex",
            "denied",
            "2026-08-21T00:00:00Z",
            None,
        );
        // e1's prev_sha256 should be e0's entry_sha256, but points at garbage.
        let e1 = make_entry(
            1,
            "rally-point:codex",
            "auto",
            "2026-08-21T00:00:01Z",
            Some("not-the-real-prev-hash"),
        );
        write_store(&path, vec![e0, e1]);

        let v = check_with("rally-point:codex", &path, Some("0"));
        assert!(!v.allowed);
        assert_eq!(v.reason_code, ReasonCode::ChainBroken);
    }

    #[test]
    fn wrong_seq_breaks_chain() {
        let path = temp_store_path("chain-seq-broken");
        let _tmp = TempFile(path.clone());
        let mut e0 = make_entry(0, "rally-point:codex", "auto", "2026-08-21T00:00:00Z", None);
        e0["seq"] = json!(5); // seq must equal index (0)
        write_store(&path, vec![e0]);

        let v = check_with("rally-point:codex", &path, Some("0"));
        assert!(!v.allowed);
        assert_eq!(v.reason_code, ReasonCode::ChainBroken);
    }

    #[test]
    fn valid_two_entry_chain_replays_last_entry_wins() {
        let path = temp_store_path("chain-valid-replay");
        let _tmp = TempFile(path.clone());
        let e0 = make_entry(0, "rally-point:codex", "auto", "2026-08-21T00:00:00Z", None);
        let h0 = e0["entry_sha256"].as_str().unwrap().to_string();
        let e1 = make_entry(
            1,
            "rally-point:codex",
            "denied",
            "2026-08-21T00:00:01Z",
            Some(&h0),
        );
        write_store(&path, vec![e0, e1]);

        // Last entry (denied) wins over the earlier auto grant.
        let v = check_with("rally-point:codex", &path, Some("0"));
        assert!(!v.allowed);
        assert_eq!(v.reason_code, ReasonCode::Denied);
    }

    // ── Depth guard ───────────────────────────────────────────────────────────

    #[test]
    fn depth_within_cap_passes() {
        let path = temp_store_path("depth-ok");
        let _tmp = TempFile(path.clone());
        let e0 = make_entry(0, "rally-point:codex", "auto", "2026-08-21T00:00:00Z", None);
        write_store(&path, vec![e0]);

        for ok_depth in ["", "0", "1", "2"] {
            let v = check_with("rally-point:codex", &path, Some(ok_depth));
            assert!(v.allowed, "depth {ok_depth:?} should pass: {v:?}");
        }
    }

    #[test]
    fn depth_above_cap_or_invalid_refuses() {
        let path = temp_store_path("depth-bad");
        let _tmp = TempFile(path.clone());
        let e0 = make_entry(0, "rally-point:codex", "auto", "2026-08-21T00:00:00Z", None);
        write_store(&path, vec![e0]);

        for bad_depth in ["3", "-1", "abc", "4"] {
            let v = check_with("rally-point:codex", &path, Some(bad_depth));
            assert!(!v.allowed, "depth {bad_depth:?} should refuse: {v:?}");
            assert_eq!(
                v.reason_code,
                ReasonCode::DepthExceeded,
                "depth {bad_depth:?}"
            );
        }
    }

    #[test]
    fn depth_exceeded_overrides_recorded_auto() {
        let path = temp_store_path("depth-beats-auto");
        let _tmp = TempFile(path.clone());
        let e0 = make_entry(0, "rally-point:codex", "auto", "2026-08-21T00:00:00Z", None);
        write_store(&path, vec![e0]);

        let v = check_with("rally-point:codex", &path, Some("5"));
        assert!(
            !v.allowed,
            "a recorded auto must not override the depth cap"
        );
        assert_eq!(v.reason_code, ReasonCode::DepthExceeded);
    }

    // ── Unknown agent_type ────────────────────────────────────────────────────

    #[test]
    fn unknown_agent_type_refuses() {
        // This path short-circuits before ever calling store_path(), so it is
        // safe to exercise via the public production entry point — it never
        // touches the operator's real ~/.agent-consent.
        for agent_type in ["gemini", "cursor", "ollama", "", "Claude", "CODEX"] {
            let v = check_for_agent_type(agent_type);
            assert!(!v.allowed, "agent_type {agent_type:?} should refuse: {v:?}");
            assert_eq!(
                v.reason_code,
                ReasonCode::UnknownAgentType,
                "agent_type {agent_type:?}"
            );
        }
    }

    #[test]
    fn recognized_agent_types_map_to_expected_keys() {
        assert_eq!(
            key_for_agent_type("claude").as_deref(),
            Some("rally-point:claude")
        );
        assert_eq!(
            key_for_agent_type("codex").as_deref(),
            Some("rally-point:codex")
        );
        assert_eq!(key_for_agent_type("gemini"), None);
    }
}
