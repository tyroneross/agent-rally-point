// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use serde::Serialize;
use serde_json::{Map, Value, json};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{RallyError, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfigScope {
    Repo,
    User,
}

impl ConfigScope {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Repo => "repo",
            Self::User => "user",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PromptMode {
    Once,
    Always,
    Off,
}

impl PromptMode {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "once" => Some(Self::Once),
            "always" => Some(Self::Always),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Always => "always",
            Self::Off => "off",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RoomDetail {
    Brief,
    Verbose,
}

impl RoomDetail {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "brief" => Some(Self::Brief),
            "verbose" => Some(Self::Verbose),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Brief => "brief",
            Self::Verbose => "verbose",
        }
    }
}

#[derive(Clone, Debug, Default)]
struct HookSettings {
    enabled: Option<bool>,
    prompt: Option<PromptMode>,
    room_detail: Option<RoomDetail>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct HooksEffective {
    pub(crate) enabled: bool,
    pub(crate) prompt: String,
    pub(crate) enabled_source: String,
    pub(crate) prompt_source: String,
    pub(crate) repo_config_path: String,
    pub(crate) user_config_path: Option<String>,
    pub(crate) session_hooks_override: Option<String>,
    pub(crate) session_prompt_override: Option<String>,
    pub(crate) room_detail: String,
    pub(crate) room_detail_source: String,
    pub(crate) session_room_detail_override: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ConfigWriteOutcome {
    pub(crate) action: String,
    pub(crate) scope: String,
    pub(crate) path: String,
    pub(crate) enabled: Option<bool>,
    pub(crate) prompt: Option<String>,
    pub(crate) room_detail: Option<String>,
}

pub(crate) fn resolve(repo_root: &Path) -> Result<HooksEffective> {
    let repo_path = repo_config_path(repo_root);
    let user_path = user_config_path();

    let repo_settings = read_settings(&repo_path)?;
    let user_settings = match user_path.as_ref() {
        Some(path) => read_settings(path)?,
        None => HookSettings::default(),
    };

    let mut enabled = true;
    let mut enabled_source = "default".to_string();
    if let Some(value) = user_settings.enabled {
        enabled = value;
        enabled_source = "user".to_string();
    }
    if let Some(value) = repo_settings.enabled {
        enabled = value;
        enabled_source = "repo".to_string();
    }

    let session_hooks_override = env::var("RALLY_HOOKS").ok();
    if let Some(value) = session_hooks_override
        .as_deref()
        .and_then(parse_enabled_override)
    {
        enabled = value;
        enabled_source = "env:RALLY_HOOKS".to_string();
    }

    let mut prompt = PromptMode::Once;
    let mut prompt_source = "default".to_string();
    if let Some(value) = user_settings.prompt {
        prompt = value;
        prompt_source = "user".to_string();
    }
    if let Some(value) = repo_settings.prompt {
        prompt = value;
        prompt_source = "repo".to_string();
    }

    let session_prompt_override = env::var("RALLY_HOOK_PROMPT").ok();
    if let Some(value) = session_prompt_override
        .as_deref()
        .and_then(PromptMode::parse)
    {
        prompt = value;
        prompt_source = "env:RALLY_HOOK_PROMPT".to_string();
    }

    let mut room_detail = RoomDetail::Brief;
    let mut room_detail_source = "default".to_string();
    if let Some(value) = user_settings.room_detail {
        room_detail = value;
        room_detail_source = "user".to_string();
    }
    if let Some(value) = repo_settings.room_detail {
        room_detail = value;
        room_detail_source = "repo".to_string();
    }

    let session_room_detail_override = env::var("RALLY_HOOK_ROOM_DETAIL").ok();
    if let Some(value) = session_room_detail_override
        .as_deref()
        .and_then(RoomDetail::parse)
    {
        room_detail = value;
        room_detail_source = "env:RALLY_HOOK_ROOM_DETAIL".to_string();
    }

    Ok(HooksEffective {
        enabled,
        prompt: prompt.as_str().to_string(),
        enabled_source,
        prompt_source,
        repo_config_path: repo_path.to_string_lossy().to_string(),
        user_config_path: user_path.map(|path| path.to_string_lossy().to_string()),
        session_hooks_override,
        session_prompt_override,
        room_detail: room_detail.as_str().to_string(),
        room_detail_source,
        session_room_detail_override,
    })
}

pub(crate) fn set_enabled(
    repo_root: &Path,
    scope: ConfigScope,
    enabled: bool,
) -> Result<ConfigWriteOutcome> {
    let path = config_path(repo_root, scope)?;
    let mut value = read_config_value(&path)?;
    set_hook_field(&mut value, "enabled", json!(enabled));
    write_config_value(&path, &value)?;
    Ok(ConfigWriteOutcome {
        action: "set-enabled".to_string(),
        scope: scope.as_str().to_string(),
        path: path.to_string_lossy().to_string(),
        enabled: Some(enabled),
        prompt: None,
        room_detail: None,
    })
}

pub(crate) fn set_prompt(
    repo_root: &Path,
    scope: ConfigScope,
    prompt: PromptMode,
) -> Result<ConfigWriteOutcome> {
    let path = config_path(repo_root, scope)?;
    let mut value = read_config_value(&path)?;
    set_hook_field(&mut value, "prompt", json!(prompt.as_str()));
    write_config_value(&path, &value)?;
    Ok(ConfigWriteOutcome {
        action: "set-prompt".to_string(),
        scope: scope.as_str().to_string(),
        path: path.to_string_lossy().to_string(),
        enabled: None,
        prompt: Some(prompt.as_str().to_string()),
        room_detail: None,
    })
}

pub(crate) fn set_room_detail(
    repo_root: &Path,
    scope: ConfigScope,
    room_detail: RoomDetail,
) -> Result<ConfigWriteOutcome> {
    let path = config_path(repo_root, scope)?;
    let mut value = read_config_value(&path)?;
    set_hook_field(&mut value, "room_detail", json!(room_detail.as_str()));
    write_config_value(&path, &value)?;
    Ok(ConfigWriteOutcome {
        action: "set-room-detail".to_string(),
        scope: scope.as_str().to_string(),
        path: path.to_string_lossy().to_string(),
        enabled: None,
        prompt: None,
        room_detail: Some(room_detail.as_str().to_string()),
    })
}

fn repo_config_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".rally").join("config.json")
}

fn user_config_path() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/rally/config.json"))
}

fn config_path(repo_root: &Path, scope: ConfigScope) -> Result<PathBuf> {
    match scope {
        ConfigScope::Repo => Ok(repo_config_path(repo_root)),
        ConfigScope::User => user_config_path().ok_or_else(|| {
            RallyError::Message("HOME is required for rally hooks --scope user".to_string())
        }),
    }
}

fn read_settings(path: &Path) -> Result<HookSettings> {
    let value = read_config_value(path)?;
    Ok(settings_from_value(&value))
}

fn read_config_value(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let raw = fs::read_to_string(path).map_err(RallyError::io(format!(
        "read hook config {}",
        path.display()
    )))?;
    serde_json::from_str(&raw).map_err(RallyError::json(format!(
        "parse hook config {}",
        path.display()
    )))
}

fn write_config_value(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(RallyError::io(format!(
            "create hook config dir {}",
            parent.display()
        )))?;
    }
    let rendered =
        serde_json::to_string_pretty(value).map_err(RallyError::json("render hook config"))? + "\n";
    fs::write(path, rendered).map_err(RallyError::io(format!(
        "write hook config {}",
        path.display()
    )))
}

fn settings_from_value(value: &Value) -> HookSettings {
    let Some(hooks) = value.get("hooks").and_then(Value::as_object) else {
        return HookSettings::default();
    };
    HookSettings {
        enabled: hooks.get("enabled").and_then(Value::as_bool),
        prompt: hooks
            .get("prompt")
            .and_then(Value::as_str)
            .and_then(PromptMode::parse),
        room_detail: hooks
            .get("room_detail")
            .and_then(Value::as_str)
            .and_then(RoomDetail::parse),
    }
}

fn set_hook_field(value: &mut Value, field: &str, field_value: Value) {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    let root = value.as_object_mut().expect("root was forced to object");
    let hooks = root
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !hooks.is_object() {
        *hooks = Value::Object(Map::new());
    }
    hooks
        .as_object_mut()
        .expect("hooks was forced to object")
        .insert(field.to_string(), field_value);
}

fn parse_enabled_override(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "on" | "true" | "yes" | "enabled" => Some(true),
        "0" | "off" | "false" | "no" | "disabled" => Some(false),
        _ => None,
    }
}

// --------------------------------------------------------------------------
// Coordination policy (recency decay + size-scaled reclaim) tunables
//
// Resolved from the SAME `.rally/config.json` files as hooks, under a
// `"coordination"` object, with the same default → user → repo → env
// precedence. Defaults come from `crate::decay` so the constants live in
// exactly one place.
// --------------------------------------------------------------------------

/// Effective coordination policy after resolving config + env overrides.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CoordinationConfig {
    /// Recency-decay half-life, in hours.
    pub(crate) half_life_hours: f64,
    /// Archive floor — messages whose weight drops below this are archived.
    pub(crate) archive_floor_weight: f64,
    /// Reclaim timeout for a SMALL (single-file) claim, in minutes.
    pub(crate) reclaim_small_minutes: i64,
    /// Reclaim timeout for a LARGE (multi-file / coarse) claim, in minutes.
    pub(crate) reclaim_large_minutes: i64,
    /// Adaptive-liveness: assumed planned heartbeat cadence (seconds) for a
    /// session that has not declared one.
    pub(crate) default_cadence_secs: i64,
    /// Adaptive-liveness: missed-beats multiplier (window = cadence*mult+grace).
    pub(crate) miss_multiplier: i64,
    /// Adaptive-liveness: extra grace (seconds) on top of the missed-beats window.
    pub(crate) grace_secs: i64,
    /// Room composition: relevance weights used to rank items for budget fill.
    pub(crate) relevance: crate::relevance::RelevanceWeights,
    /// Room composition: fraction of the consumer's context the room may occupy.
    /// `0.0` disables the byte ceiling entirely.
    pub(crate) room_budget_fraction: f64,
    /// Room composition: assumed consumer context size in bytes. Multiplied by
    /// `room_budget_fraction` to yield the ceiling.
    pub(crate) consumer_context_bytes: i64,
    /// `next`: how long an unanswered handoff stays an active obligation before
    /// it is de-prioritised out of the waiting/candidate projections.
    pub(crate) stale_wait_secs: i64,
    /// Reaper: how long an unanswered handoff stays OPEN before the reaper
    /// expires it. Distinct from `stale_wait_secs`, which only changes ranking
    /// — an unanswered handoff was otherwise immortal in `open_handoffs`.
    /// `0` disables handoff expiry entirely.
    pub(crate) handoff_expiry_secs: i64,
    /// Reaper: minimum seconds between automatic reap passes triggered by
    /// `rally enter`. `0` disables auto-reap, leaving
    /// `rally doctor --reap-stale --apply` as the only caller.
    pub(crate) auto_reap_interval_secs: i64,
}

impl Default for CoordinationConfig {
    fn default() -> Self {
        Self {
            half_life_hours: crate::decay::DEFAULT_HALF_LIFE_HOURS,
            archive_floor_weight: crate::decay::DEFAULT_ARCHIVE_FLOOR,
            reclaim_small_minutes: crate::decay::DEFAULT_RECLAIM_SMALL_MINUTES,
            reclaim_large_minutes: crate::decay::DEFAULT_RECLAIM_LARGE_MINUTES,
            default_cadence_secs: crate::liveness::DEFAULT_CADENCE_SECS,
            miss_multiplier: crate::liveness::MISS_MULTIPLIER,
            grace_secs: crate::liveness::GRACE_SECS,
            relevance: crate::relevance::RelevanceWeights::default(),
            room_budget_fraction: crate::relevance::DEFAULT_ROOM_BUDGET_FRACTION,
            consumer_context_bytes: crate::relevance::DEFAULT_CONSUMER_CONTEXT_BYTES,
            stale_wait_secs: crate::next::DEFAULT_STALE_WAIT_SECS,
            handoff_expiry_secs: crate::reaper::DEFAULT_HANDOFF_EXPIRY_SECS,
            auto_reap_interval_secs: crate::reaper::DEFAULT_AUTO_REAP_INTERVAL_SECS,
        }
    }
}

impl CoordinationConfig {
    /// Half-life expressed in seconds (the unit `decay::recency_weight` wants).
    pub(crate) fn half_life_secs(&self) -> i64 {
        (self.half_life_hours * 3600.0).round() as i64
    }

    /// The room byte ceiling, or `None` when the ceiling is disabled.
    pub(crate) fn room_budget_bytes(&self) -> Option<usize> {
        crate::relevance::budget_bytes(self.room_budget_fraction, self.consumer_context_bytes)
    }
}

fn coordination_from_value(value: &Value, into: &mut CoordinationConfig) {
    let Some(coord) = value.get("coordination").and_then(Value::as_object) else {
        return;
    };
    if let Some(v) = coord.get("half_life_hours").and_then(Value::as_f64)
        && v > 0.0
    {
        into.half_life_hours = v;
    }
    if let Some(v) = coord.get("archive_floor_weight").and_then(Value::as_f64)
        && v > 0.0
        && v < 1.0
    {
        into.archive_floor_weight = v;
    }
    if let Some(v) = coord.get("reclaim_small_minutes").and_then(Value::as_i64)
        && v > 0
    {
        into.reclaim_small_minutes = v;
    }
    if let Some(v) = coord.get("reclaim_large_minutes").and_then(Value::as_i64)
        && v > 0
    {
        into.reclaim_large_minutes = v;
    }
    if let Some(v) = coord.get("default_cadence_secs").and_then(Value::as_i64)
        && v > 0
    {
        into.default_cadence_secs = v;
    }
    if let Some(v) = coord.get("miss_multiplier").and_then(Value::as_i64)
        && v > 0
    {
        into.miss_multiplier = v;
    }
    if let Some(v) = coord.get("grace_secs").and_then(Value::as_i64)
        && v >= 0
    {
        into.grace_secs = v;
    }
    if let Some(v) = coord.get("room_budget_fraction").and_then(Value::as_f64)
        && (0.0..=1.0).contains(&v)
    {
        into.room_budget_fraction = v;
    }
    if let Some(v) = coord.get("consumer_context_bytes").and_then(Value::as_i64)
        && v >= 0
    {
        into.consumer_context_bytes = v;
    }
    if let Some(v) = coord.get("stale_wait_secs").and_then(Value::as_i64)
        && v > 0
    {
        into.stale_wait_secs = v;
    }
    // Zero is meaningful here: it turns handoff expiry off.
    if let Some(v) = coord.get("handoff_expiry_secs").and_then(Value::as_i64)
        && v >= 0
    {
        into.handoff_expiry_secs = v;
    }
    // Zero is meaningful here too: it turns auto-reap off.
    if let Some(v) = coord.get("auto_reap_interval_secs").and_then(Value::as_i64)
        && v >= 0
    {
        into.auto_reap_interval_secs = v;
    }
    let Some(rel) = coord.get("relevance").and_then(Value::as_object) else {
        return;
    };
    // stale_author_factor is clamped to (0, 1] at USE time in
    // `relevance::relevance` too — a value outside the range there falls back to
    // the default rather than inverting the signal. Rejecting it here as well
    // means a typo is ignored at config-read, matching the other knobs.
    if let Some(v) = rel.get("stale_author_factor").and_then(Value::as_f64)
        && v > 0.0
        && v <= 1.0
    {
        into.relevance.stale_author_factor = v;
    }
    if let Some(v) = rel.get("addressed_boost").and_then(Value::as_f64)
        && v >= 0.0
    {
        into.relevance.addressed_boost = v;
    }
    if let Some(v) = rel.get("path_overlap_boost").and_then(Value::as_f64)
        && v >= 0.0
    {
        into.relevance.path_overlap_boost = v;
    }
}

fn coord_env_f64(name: &str, slot: &mut f64, guard: impl Fn(f64) -> bool) {
    if let Some(v) = env::var(name)
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        && guard(v)
    {
        *slot = v;
    }
}

fn coord_env_i64(name: &str, slot: &mut i64) {
    if let Some(v) = env::var(name)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        && v > 0
    {
        *slot = v;
    }
}

/// Resolve the effective coordination policy: default → user → repo → env.
pub(crate) fn resolve_coordination(repo_root: &Path) -> Result<CoordinationConfig> {
    let mut cfg = CoordinationConfig::default();
    if let Some(user_path) = user_config_path() {
        let v = read_config_value(&user_path)?;
        coordination_from_value(&v, &mut cfg);
    }
    let repo_value = read_config_value(&repo_config_path(repo_root))?;
    coordination_from_value(&repo_value, &mut cfg);

    // Env overrides (highest precedence) — match the RALLY_* naming convention.
    coord_env_f64("RALLY_HALF_LIFE_HOURS", &mut cfg.half_life_hours, |v| {
        v > 0.0
    });
    coord_env_f64("RALLY_ARCHIVE_FLOOR", &mut cfg.archive_floor_weight, |v| {
        v > 0.0 && v < 1.0
    });
    coord_env_i64(
        "RALLY_RECLAIM_SMALL_MINUTES",
        &mut cfg.reclaim_small_minutes,
    );
    coord_env_i64(
        "RALLY_RECLAIM_LARGE_MINUTES",
        &mut cfg.reclaim_large_minutes,
    );
    coord_env_i64("RALLY_DEFAULT_CADENCE_SECS", &mut cfg.default_cadence_secs);
    coord_env_i64("RALLY_MISS_MULTIPLIER", &mut cfg.miss_multiplier);
    // grace may be 0; coord_env_i64 only accepts >0, which is fine — a 0 grace
    // override is a no-op (the missed-beats window already dominates).
    coord_env_i64("RALLY_GRACE_SECS", &mut cfg.grace_secs);
    // Room composition. The two budget knobs accept 0, which DISABLES the
    // ceiling — an operator must be able to turn the bound off, or it is not a
    // choice. `coord_env_f64`'s guard already decides acceptance, so the
    // fraction needs no special helper; the integer one does, because
    // `coord_env_i64` hardcodes `> 0`.
    coord_env_f64(
        "RALLY_ROOM_BUDGET_FRACTION",
        &mut cfg.room_budget_fraction,
        |v| (0.0..=1.0).contains(&v),
    );
    coord_env_i64_allow_zero(
        "RALLY_CONSUMER_CONTEXT_BYTES",
        &mut cfg.consumer_context_bytes,
    );
    coord_env_i64("RALLY_STALE_WAIT_SECS", &mut cfg.stale_wait_secs);
    coord_env_i64_allow_zero("RALLY_HANDOFF_EXPIRY_SECS", &mut cfg.handoff_expiry_secs);
    coord_env_i64_allow_zero(
        "RALLY_AUTO_REAP_INTERVAL_SECS",
        &mut cfg.auto_reap_interval_secs,
    );
    coord_env_f64(
        "RALLY_STALE_AUTHOR_FACTOR",
        &mut cfg.relevance.stale_author_factor,
        |v| v > 0.0 && v <= 1.0,
    );
    coord_env_f64(
        "RALLY_ADDRESSED_BOOST",
        &mut cfg.relevance.addressed_boost,
        |v| v >= 0.0,
    );
    coord_env_f64(
        "RALLY_PATH_OVERLAP_BOOST",
        &mut cfg.relevance.path_overlap_boost,
        |v| v >= 0.0,
    );

    Ok(cfg)
}

/// Like [`coord_env_i64`] but accepts 0 (a meaningful "disabled" value for the
/// budget knobs). Negative values are still rejected.
fn coord_env_i64_allow_zero(name: &str, slot: &mut i64) {
    if let Some(v) = env::var(name)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        && v >= 0
    {
        *slot = v;
    }
}

#[cfg(test)]
mod hooks_tests {
    use super::*;
    use std::fs;

    /// Isolated HOME so `resolve()`'s user-config read never sees the real
    /// developer's `~/.config/rally/config.json`. Mirrors the crate-wide
    /// HOME-override pattern used elsewhere (see `lib.rs`'s ptyd-detect
    /// tests): RAII guard restores the prior HOME on drop, even on panic.
    struct HomeGuard {
        prev: Option<String>,
    }
    impl HomeGuard {
        fn set(home: &Path) -> Self {
            let prev = env::var("HOME").ok();
            unsafe { env::set_var("HOME", home) };
            Self { prev }
        }
    }
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => env::set_var("HOME", v),
                    None => env::remove_var("HOME"),
                }
            }
        }
    }

    /// Precedence: repo config sets `room_detail=brief`; `RALLY_HOOK_ROOM_DETAIL`
    /// overrides it to `verbose` for the session, mirroring the existing
    /// `RALLY_HOOK_PROMPT` precedent this module has no standalone unit test
    /// for (it's covered via the `hooks_config::resolve` env branch above).
    #[test]
    fn room_detail_env_override_beats_repo_config() {
        let _g = crate::PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        unsafe { env::remove_var("RALLY_HOOK_ROOM_DETAIL") };

        let dir =
            std::env::temp_dir().join(format!("rally-hooks-room-detail-{}", std::process::id()));
        let home = dir.join("home");
        let _ = fs::create_dir_all(dir.join(".rally"));
        let _ = fs::create_dir_all(&home);
        let _home_guard = HomeGuard::set(&home);
        fs::write(
            dir.join(".rally").join("config.json"),
            r#"{"hooks":{"room_detail":"brief"}}"#,
        )
        .unwrap();

        let effective = resolve(&dir).unwrap();
        assert_eq!(effective.room_detail, "brief");
        assert_eq!(effective.room_detail_source, "repo");
        assert_eq!(effective.session_room_detail_override, None);

        unsafe { env::set_var("RALLY_HOOK_ROOM_DETAIL", "verbose") };
        let effective_env = resolve(&dir).unwrap();
        assert_eq!(effective_env.room_detail, "verbose");
        assert_eq!(
            effective_env.room_detail_source,
            "env:RALLY_HOOK_ROOM_DETAIL"
        );
        assert_eq!(
            effective_env.session_room_detail_override,
            Some("verbose".to_string())
        );

        unsafe { env::remove_var("RALLY_HOOK_ROOM_DETAIL") };
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod coordination_tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    // env var mutation is process-global; serialize these tests.
    // Unified with the crate-wide PROCESS_ENV_LOCK (was a private OnceLock<Mutex>).
    // A separate mutex let this module's set_var/remove_var run concurrently with
    // env reads in other tests (busy_but_quiet, status_post, retrospective) — and
    // Rust's set/remove_var corrupts the WHOLE environ during a concurrent read,
    // not just the one key. One lock = all env mutators/readers serialize.
    // Keep hook fixtures isolated from inherited Git process scope.
    fn env_lock() -> &'static Mutex<()> {
        &crate::PROCESS_ENV_LOCK
    }

    /// Config env vars that `resolve_coordination` reads. Tests set these; if a
    /// test panics between set and cleanup, the value LEAKS into later tests —
    /// e.g. a leaked `RALLY_RECLAIM_*` shrinks the takeover window and flips
    /// `busy_but_quiet_owner_is_warnable_but_not_takeover_eligible`. See
    /// This prevents inherited Git process scope from leaking into fixtures.
    const CONFIG_ENV_VARS: &[&str] = &[
        "RALLY_HALF_LIFE_HOURS",
        "RALLY_ARCHIVE_FLOOR",
        "RALLY_RECLAIM_SMALL_MINUTES",
        "RALLY_RECLAIM_LARGE_MINUTES",
        "RALLY_DEFAULT_CADENCE_SECS",
        "RALLY_MISS_MULTIPLIER",
        "RALLY_GRACE_SECS",
    ];

    /// RAII: removes every config env var on drop — including on an assertion
    /// panic — so a test can never leak coordination config into a later test.
    /// Declared AFTER the env-lock guard so it drops (cleans up) while the lock
    /// is still held. Pairs with the crate-wide `PROCESS_ENV_LOCK`.
    struct ConfigEnvGuard;
    impl Drop for ConfigEnvGuard {
        fn drop(&mut self) {
            for k in CONFIG_ENV_VARS {
                unsafe { std::env::remove_var(k) };
            }
        }
    }

    #[test]
    fn defaults_when_no_config() {
        let _g = env_lock().lock().unwrap_or_else(|p| p.into_inner());
        let _cfg_env = ConfigEnvGuard;
        for k in [
            "RALLY_HALF_LIFE_HOURS",
            "RALLY_ARCHIVE_FLOOR",
            "RALLY_RECLAIM_SMALL_MINUTES",
            "RALLY_RECLAIM_LARGE_MINUTES",
        ] {
            unsafe { env::remove_var(k) };
        }
        let dir = std::env::temp_dir().join(format!("rally-coord-def-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        for k in [
            "RALLY_DEFAULT_CADENCE_SECS",
            "RALLY_MISS_MULTIPLIER",
            "RALLY_GRACE_SECS",
        ] {
            unsafe { env::remove_var(k) };
        }
        let cfg = resolve_coordination(&dir).unwrap();
        assert_eq!(cfg, CoordinationConfig::default());
        assert_eq!(cfg.half_life_hours, 48.0);
        assert_eq!(cfg.archive_floor_weight, 0.05);
        assert_eq!(cfg.reclaim_small_minutes, 30);
        assert_eq!(cfg.reclaim_large_minutes, 120);
        // Adaptive-liveness defaults flow from the liveness module.
        assert_eq!(cfg.default_cadence_secs, 300);
        assert_eq!(cfg.miss_multiplier, 6);
        assert_eq!(cfg.grace_secs, 60);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn liveness_tunables_resolve_from_repo_and_env() {
        let _g = env_lock().lock().unwrap_or_else(|p| p.into_inner());
        let _cfg_env = ConfigEnvGuard;
        for k in [
            "RALLY_DEFAULT_CADENCE_SECS",
            "RALLY_MISS_MULTIPLIER",
            "RALLY_GRACE_SECS",
        ] {
            unsafe { env::remove_var(k) };
        }
        let dir = std::env::temp_dir().join(format!("rally-coord-live-{}", std::process::id()));
        let _ = fs::create_dir_all(dir.join(".rally"));
        fs::write(
            dir.join(".rally").join("config.json"),
            r#"{"coordination":{"default_cadence_secs":600,"miss_multiplier":4,"grace_secs":0}}"#,
        )
        .unwrap();
        let cfg = resolve_coordination(&dir).unwrap();
        assert_eq!(cfg.default_cadence_secs, 600);
        assert_eq!(cfg.miss_multiplier, 4);
        assert_eq!(cfg.grace_secs, 0);
        // env beats repo
        unsafe { env::set_var("RALLY_DEFAULT_CADENCE_SECS", "900") };
        let cfg2 = resolve_coordination(&dir).unwrap();
        assert_eq!(cfg2.default_cadence_secs, 900, "env beats repo");
        unsafe { env::remove_var("RALLY_DEFAULT_CADENCE_SECS") };
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn repo_config_overrides_default() {
        let _g = env_lock().lock().unwrap_or_else(|p| p.into_inner());
        let _cfg_env = ConfigEnvGuard;
        for k in [
            "RALLY_HALF_LIFE_HOURS",
            "RALLY_ARCHIVE_FLOOR",
            "RALLY_RECLAIM_SMALL_MINUTES",
            "RALLY_RECLAIM_LARGE_MINUTES",
        ] {
            unsafe { env::remove_var(k) };
        }
        let dir = std::env::temp_dir().join(format!("rally-coord-repo-{}", std::process::id()));
        let _ = fs::create_dir_all(dir.join(".rally"));
        fs::write(
            dir.join(".rally").join("config.json"),
            r#"{"coordination":{"half_life_hours":24,"archive_floor_weight":0.1,"reclaim_small_minutes":10,"reclaim_large_minutes":60}}"#,
        )
        .unwrap();
        let cfg = resolve_coordination(&dir).unwrap();
        assert_eq!(cfg.half_life_hours, 24.0);
        assert_eq!(cfg.archive_floor_weight, 0.1);
        assert_eq!(cfg.reclaim_small_minutes, 10);
        assert_eq!(cfg.reclaim_large_minutes, 60);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_overrides_repo() {
        let _g = env_lock().lock().unwrap_or_else(|p| p.into_inner());
        let _cfg_env = ConfigEnvGuard;
        let dir = std::env::temp_dir().join(format!("rally-coord-env-{}", std::process::id()));
        let _ = fs::create_dir_all(dir.join(".rally"));
        fs::write(
            dir.join(".rally").join("config.json"),
            r#"{"coordination":{"half_life_hours":24}}"#,
        )
        .unwrap();
        unsafe { env::set_var("RALLY_HALF_LIFE_HOURS", "72") };
        let cfg = resolve_coordination(&dir).unwrap();
        assert_eq!(cfg.half_life_hours, 72.0, "env beats repo");
        unsafe { env::remove_var("RALLY_HALF_LIFE_HOURS") };
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_values_ignored() {
        let _g = env_lock().lock().unwrap_or_else(|p| p.into_inner());
        let _cfg_env = ConfigEnvGuard;
        for k in [
            "RALLY_HALF_LIFE_HOURS",
            "RALLY_ARCHIVE_FLOOR",
            "RALLY_RECLAIM_SMALL_MINUTES",
            "RALLY_RECLAIM_LARGE_MINUTES",
        ] {
            unsafe { env::remove_var(k) };
        }
        let dir = std::env::temp_dir().join(format!("rally-coord-bad-{}", std::process::id()));
        let _ = fs::create_dir_all(dir.join(".rally"));
        // negative half-life + out-of-range floor must be ignored → defaults kept.
        fs::write(
            dir.join(".rally").join("config.json"),
            r#"{"coordination":{"half_life_hours":-5,"archive_floor_weight":2.0}}"#,
        )
        .unwrap();
        let cfg = resolve_coordination(&dir).unwrap();
        assert_eq!(cfg, CoordinationConfig::default());
        let _ = fs::remove_dir_all(&dir);
    }
}
