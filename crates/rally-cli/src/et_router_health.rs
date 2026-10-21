// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
//! ET rally-router health check (stage2 plan LD-H / component U5).
//!
//! `say handoff --deliver inject` (the default) normally writes the pushed
//! line straight into the target's pane. When Easy Terminal's rally-router
//! already owns delivery to that identity, a second write from rally would
//! be a duplicate: two producers racing the same pane. `RALLY_ET_ROUTER_HEALTH`
//! (a path, exported by ET only when `rally.routing.enabled`) names a health
//! file the router writes atomically every 2s. This module decides, from
//! that file alone, whether rally should skip its own pane write for one
//! target identity and fall back to `--deliver record`'s ledger-only path.
//!
//! The check fails closed: any error reading or parsing the file, a stale or
//! degraded router, or a target identity absent from `routed_identities`
//! all mean "rally still owns delivery" — today's inject behavior,
//! unchanged. Only an unambiguous, fresh, healthy, matching health file
//! suppresses the pane write.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

/// `schema` value the router stamps on every health file it writes
/// (LD-H). A file with any other value, or none, is never trusted.
const HEALTH_SCHEMA: &str = "et.rally-router.health.v1";

/// A health file older than this (`now_ms - updated_ms`, in milliseconds)
/// is treated as stale — the router may have crashed without withdrawing
/// coverage, so rally must not rely on it.
pub const FRESHNESS_WINDOW_MS: i64 = 10_000;

/// Refuse to read a health file bigger than this. The router writes a few
/// hundred bytes; anything past 1 MiB is not a health file rally should
/// trust or spend time parsing.
const MAX_HEALTH_FILE_BYTES: u64 = 1024 * 1024;

/// The env var ET exports (a path, not a verdict — LD-H).
pub const HEALTH_ENV_VAR: &str = "RALLY_ET_ROUTER_HEALTH";

#[derive(Debug, Deserialize)]
struct HealthFile {
    schema: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // not read by this check; kept for schema fidelity
    pid: Option<u64>,
    updated_ms: i64,
    degraded: bool,
    #[serde(default)]
    routed_identities: Vec<String>,
}

/// `true` when `updated_ms` is at most [`FRESHNESS_WINDOW_MS`] in the past and
/// at most [`FUTURE_SKEW_MS`] in the future. An unbounded future allowance
/// would let one bad clock (or a crafted file) suppress rally's own inject
/// forever, so a stamp too far ahead is treated as stale.
pub fn is_fresh(updated_ms: i64, now_ms: i64) -> bool {
    let age = now_ms.saturating_sub(updated_ms);
    (-FUTURE_SKEW_MS..=FRESHNESS_WINDOW_MS).contains(&age)
}

/// How far ahead of this process's clock a health stamp may be.
pub const FUTURE_SKEW_MS: i64 = 5_000;

/// Current wall-clock time in milliseconds since the Unix epoch. Falls back
/// to 0 only if the clock is somehow before the epoch (never in practice);
/// that failure mode makes every health file look stale, which is the safe
/// direction to fail.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Reads `RALLY_ET_ROUTER_HEALTH` (if set) and decides whether the ET
/// router already owns delivery to `target_identity`. Returns `true` only
/// when every one of these holds:
/// - the env var is set and names an existing path;
/// - that path is a regular file, not a symlink, no larger than 1 MiB;
/// - it parses as JSON with `schema == "et.rally-router.health.v1"`;
/// - `degraded` is `false`;
/// - `updated_ms` is fresh relative to `now_ms` ([`is_fresh`]);
/// - `routed_identities` contains `target_identity` as an exact string
///   match.
///
/// Any other outcome — env unset, missing file, symlink, oversized file,
/// I/O error, malformed JSON, wrong/missing schema, degraded, stale, or the
/// identity absent — returns `false`, and the caller proceeds exactly as it
/// does today.
pub fn et_router_owns_delivery(target_identity: &str, now_ms: i64) -> bool {
    match std::env::var(HEALTH_ENV_VAR) {
        Ok(path) if !path.is_empty() => {
            et_router_owns_delivery_at(Path::new(&path), target_identity, now_ms)
        }
        _ => false,
    }
}

/// Path-parameterized core of [`et_router_owns_delivery`], factored out so
/// tests can point at a fixture file without mutating process env.
fn et_router_owns_delivery_at(path: &Path, target_identity: &str, now_ms: i64) -> bool {
    load_trusted_health(path, now_ms).is_some_and(|health| {
        health
            .routed_identities
            .iter()
            .any(|identity| identity == target_identity)
    })
}

/// Identities the ET router currently lists as routed, from the file named by
/// `RALLY_ET_ROUTER_HEALTH`. Applies the same fail-closed checks as
/// [`et_router_owns_delivery`]; any failure (env unset, symlink, oversized,
/// bad JSON, wrong schema, degraded, stale) yields an empty list. Read-only:
/// used to report routing truthfully, never to change delivery.
pub fn et_router_routed_identities(now_ms: i64) -> Vec<String> {
    match std::env::var(HEALTH_ENV_VAR) {
        Ok(path) if !path.is_empty() => et_router_routed_identities_at(Path::new(&path), now_ms),
        _ => Vec::new(),
    }
}

/// Path-parameterized core of [`et_router_routed_identities`].
fn et_router_routed_identities_at(path: &Path, now_ms: i64) -> Vec<String> {
    load_trusted_health(path, now_ms)
        .map(|health| health.routed_identities)
        .unwrap_or_default()
}

/// Shared load + validate step: returns the parsed health file only when it is
/// a regular (non-symlink) file within the size cap, parses, carries the
/// expected schema, is not degraded, and is fresh. Everything else is `None`.
fn load_trusted_health(path: &Path, now_ms: i64) -> Option<HealthFile> {
    // `symlink_metadata` does NOT follow the final component, so a symlink
    // reports `is_symlink() == true` / `is_file() == false` here, and is
    // refused before we ever open it.
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() || meta.len() > MAX_HEALTH_FILE_BYTES {
        return None;
    }
    let contents = fs::read_to_string(path).ok()?;
    let health: HealthFile = serde_json::from_str(&contents).ok()?;
    is_trusted(&health, now_ms).then_some(health)
}

/// Pure decision over an already-parsed health file. Split from the I/O
/// wrapper so the schema/degraded/freshness/membership logic is unit
/// testable without touching the filesystem.
#[cfg(test)]
fn evaluate(health: &HealthFile, target_identity: &str, now_ms: i64) -> bool {
    is_trusted(health, now_ms)
        && health
            .routed_identities
            .iter()
            .any(|identity| identity == target_identity)
}

/// Schema, degraded, and freshness checks shared by every consumer of the file.
fn is_trusted(health: &HealthFile, now_ms: i64) -> bool {
    health.schema.as_deref() == Some(HEALTH_SCHEMA)
        && !health.degraded
        && is_fresh(health.updated_ms, now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rally-et-router-health-test-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn write_health_json(path: &Path, body: &str) {
        fs::write(path, body).expect("write health fixture");
    }

    fn health_json(updated_ms: i64, degraded: bool, routed: &[&str]) -> String {
        serde_json::json!({
            "schema": HEALTH_SCHEMA,
            "pid": 4242,
            "updated_ms": updated_ms,
            "degraded": degraded,
            "routed_identities": routed,
        })
        .to_string()
    }

    // ---- freshness parser (pure) -------------------------------------

    #[test]
    fn is_fresh_at_exact_window_boundary() {
        let now = 1_000_000_i64;
        assert!(
            is_fresh(now - FRESHNESS_WINDOW_MS, now),
            "exactly at window is fresh"
        );
        assert!(
            !is_fresh(now - FRESHNESS_WINDOW_MS - 1, now),
            "1ms past window is stale"
        );
    }

    #[test]
    fn is_fresh_zero_age_is_fresh() {
        let now = 5_000_i64;
        assert!(is_fresh(now, now));
    }

    #[test]
    fn is_fresh_future_timestamp_is_bounded() {
        let now = 1_000_000;
        assert!(
            is_fresh(now + FUTURE_SKEW_MS, now),
            "small skew ahead is fresh"
        );
        assert!(
            !is_fresh(now + FUTURE_SKEW_MS + 1, now),
            "a stamp too far ahead never suppresses"
        );
        assert!(!is_fresh(now + 50_000, now));
    }

    #[test]
    fn is_fresh_far_past_is_stale() {
        assert!(!is_fresh(0, FRESHNESS_WINDOW_MS * 100));
    }

    // ---- evaluate() (pure, schema/degraded/membership) ----------------

    #[test]
    fn evaluate_rejects_wrong_schema() {
        let health = HealthFile {
            schema: Some("something.else.v1".to_string()),
            pid: None,
            updated_ms: 1_000,
            degraded: false,
            routed_identities: vec!["claude:01".to_string()],
        };
        assert!(!evaluate(&health, "claude:01", 1_000));
    }

    #[test]
    fn evaluate_rejects_missing_schema() {
        let health = HealthFile {
            schema: None,
            pid: None,
            updated_ms: 1_000,
            degraded: false,
            routed_identities: vec!["claude:01".to_string()],
        };
        assert!(!evaluate(&health, "claude:01", 1_000));
    }

    #[test]
    fn evaluate_rejects_degraded() {
        let health = HealthFile {
            schema: Some(HEALTH_SCHEMA.to_string()),
            pid: None,
            updated_ms: 1_000,
            degraded: true,
            routed_identities: vec!["claude:01".to_string()],
        };
        assert!(!evaluate(&health, "claude:01", 1_000));
    }

    #[test]
    fn evaluate_rejects_stale() {
        let health = HealthFile {
            schema: Some(HEALTH_SCHEMA.to_string()),
            pid: None,
            updated_ms: 0,
            degraded: false,
            routed_identities: vec!["claude:01".to_string()],
        };
        assert!(!evaluate(&health, "claude:01", FRESHNESS_WINDOW_MS + 1));
    }

    #[test]
    fn evaluate_rejects_identity_not_listed() {
        let health = HealthFile {
            schema: Some(HEALTH_SCHEMA.to_string()),
            pid: None,
            updated_ms: 1_000,
            degraded: false,
            routed_identities: vec!["codex:07".to_string()],
        };
        assert!(!evaluate(&health, "claude:01", 1_000));
    }

    #[test]
    fn evaluate_accepts_fresh_healthy_listed_identity() {
        let health = HealthFile {
            schema: Some(HEALTH_SCHEMA.to_string()),
            pid: None,
            updated_ms: 1_000,
            degraded: false,
            routed_identities: vec!["codex:07".to_string(), "claude:01".to_string()],
        };
        assert!(evaluate(&health, "claude:01", 1_000));
    }

    #[test]
    fn evaluate_identity_match_is_exact_string() {
        let health = HealthFile {
            schema: Some(HEALTH_SCHEMA.to_string()),
            pid: None,
            updated_ms: 1_000,
            degraded: false,
            routed_identities: vec!["claude:1".to_string()],
        };
        // "claude:01" != "claude:1" — no fuzzy/prefix matching.
        assert!(!evaluate(&health, "claude:01", 1_000));
    }

    // ---- et_router_owns_delivery_at() (I/O + refusal conditions) ------

    #[test]
    fn file_backed_fresh_healthy_listed_returns_true() {
        let dir = scratch_dir("ok");
        let path = dir.join("health.json");
        write_health_json(&path, &health_json(1_000, false, &["claude:01"]));
        assert!(et_router_owns_delivery_at(&path, "claude:01", 1_000));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_degraded_field_never_suppresses_injection() {
        let dir = scratch_dir("missing-degraded");
        let path = dir.join("health.json");
        write_health_json(
            &path,
            &serde_json::json!({
                "schema": HEALTH_SCHEMA,
                "updated_ms": 1_000,
                "routed_identities": ["claude:01"],
            })
            .to_string(),
        );
        assert!(!et_router_owns_delivery_at(&path, "claude:01", 1_000));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_returns_false() {
        let dir = scratch_dir("missing");
        let path = dir.join("does-not-exist.json");
        assert!(!et_router_owns_delivery_at(&path, "claude:01", 1_000));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn symlink_to_valid_file_is_refused() {
        let dir = scratch_dir("symlink");
        let real = dir.join("health.json");
        write_health_json(&real, &health_json(1_000, false, &["claude:01"]));
        let link = dir.join("health-link.json");
        std::os::unix::fs::symlink(&real, &link).expect("create symlink");
        assert!(!et_router_owns_delivery_at(&link, "claude:01", 1_000));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_file_is_refused() {
        let dir = scratch_dir("oversized");
        let path = dir.join("health.json");
        // Valid JSON but padded with a huge whitespace prefix past the cap.
        let padding = " ".repeat((MAX_HEALTH_FILE_BYTES + 16) as usize);
        let body = format!("{padding}{}", health_json(1_000, false, &["claude:01"]));
        write_health_json(&path, &body);
        assert!(!et_router_owns_delivery_at(&path, "claude:01", 1_000));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_json_is_refused() {
        let dir = scratch_dir("corrupt");
        let path = dir.join("health.json");
        write_health_json(&path, "{ not json ");
        assert!(!et_router_owns_delivery_at(&path, "claude:01", 1_000));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn directory_at_path_is_refused() {
        let dir = scratch_dir("isdir");
        let path = dir.join("health-as-dir");
        fs::create_dir_all(&path).expect("create dir standing in for the health file");
        assert!(!et_router_owns_delivery_at(&path, "claude:01", 1_000));
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- et_router_routed_identities_at() ------------------------------

    #[test]
    fn routed_identities_returns_list_for_fresh_healthy_file() {
        let dir = scratch_dir("routed-ok");
        let path = dir.join("health.json");
        write_health_json(
            &path,
            &health_json(1_000, false, &["claude:01", "codex:07"]),
        );
        assert_eq!(
            et_router_routed_identities_at(&path, 1_000),
            vec!["claude:01".to_string(), "codex:07".to_string()]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn routed_identities_empty_when_degraded_stale_or_symlink() {
        let dir = scratch_dir("routed-bad");
        let degraded = dir.join("degraded.json");
        write_health_json(&degraded, &health_json(1_000, true, &["claude:01"]));
        assert!(et_router_routed_identities_at(&degraded, 1_000).is_empty());

        let stale = dir.join("stale.json");
        write_health_json(&stale, &health_json(0, false, &["claude:01"]));
        assert!(et_router_routed_identities_at(&stale, FRESHNESS_WINDOW_MS + 1).is_empty());

        let real = dir.join("real.json");
        write_health_json(&real, &health_json(1_000, false, &["claude:01"]));
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&real, &link).expect("create symlink");
        assert!(et_router_routed_identities_at(&link, 1_000).is_empty());
        assert!(et_router_routed_identities_at(&dir.join("missing.json"), 1_000).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    // `et_router_owns_delivery`'s env-reading branch (unset vs set) is
    // exercised by the process-level integration tests in
    // `tests/et_router_record_only.rs`, which set the var on a CHILD
    // process's env rather than mutating this test binary's global
    // process env (unsafe and racy under parallel `cargo test`).

    #[test]
    fn permissions_bit_is_irrelevant_to_the_check() {
        // Sanity: an unreadable-by-others (but readable-by-us) file is still
        // evaluated on content, not mode bits — the mode check is the
        // caller's OS-level concern, not this module's.
        let dir = scratch_dir("perm");
        let path = dir.join("health.json");
        write_health_json(&path, &health_json(1_000, false, &["claude:01"]));
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&path, perms).unwrap();
        assert!(et_router_owns_delivery_at(&path, "claude:01", 1_000));
        let _ = fs::remove_dir_all(&dir);
    }
}
