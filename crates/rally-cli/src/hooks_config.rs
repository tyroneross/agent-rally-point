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

#[derive(Clone, Debug, Default)]
struct HookSettings {
    enabled: Option<bool>,
    prompt: Option<PromptMode>,
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
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ConfigWriteOutcome {
    pub(crate) action: String,
    pub(crate) scope: String,
    pub(crate) path: String,
    pub(crate) enabled: Option<bool>,
    pub(crate) prompt: Option<String>,
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

    Ok(HooksEffective {
        enabled,
        prompt: prompt.as_str().to_string(),
        enabled_source,
        prompt_source,
        repo_config_path: repo_path.to_string_lossy().to_string(),
        user_config_path: user_path.map(|path| path.to_string_lossy().to_string()),
        session_hooks_override,
        session_prompt_override,
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
        }
    }
}

impl CoordinationConfig {
    /// Half-life expressed in seconds (the unit `decay::recency_weight` wants).
    pub(crate) fn half_life_secs(&self) -> i64 {
        (self.half_life_hours * 3600.0).round() as i64
    }
}

fn coordination_from_value(value: &Value, into: &mut CoordinationConfig) {
    let Some(coord) = value.get("coordination").and_then(Value::as_object) else {
        return;
    };
    if let Some(v) = coord.get("half_life_hours").and_then(Value::as_f64) {
        if v > 0.0 {
            into.half_life_hours = v;
        }
    }
    if let Some(v) = coord.get("archive_floor_weight").and_then(Value::as_f64) {
        if v > 0.0 && v < 1.0 {
            into.archive_floor_weight = v;
        }
    }
    if let Some(v) = coord.get("reclaim_small_minutes").and_then(Value::as_i64) {
        if v > 0 {
            into.reclaim_small_minutes = v;
        }
    }
    if let Some(v) = coord.get("reclaim_large_minutes").and_then(Value::as_i64) {
        if v > 0 {
            into.reclaim_large_minutes = v;
        }
    }
    if let Some(v) = coord.get("default_cadence_secs").and_then(Value::as_i64) {
        if v > 0 {
            into.default_cadence_secs = v;
        }
    }
    if let Some(v) = coord.get("miss_multiplier").and_then(Value::as_i64) {
        if v > 0 {
            into.miss_multiplier = v;
        }
    }
    if let Some(v) = coord.get("grace_secs").and_then(Value::as_i64) {
        if v >= 0 {
            into.grace_secs = v;
        }
    }
}

fn coord_env_f64(name: &str, slot: &mut f64, guard: impl Fn(f64) -> bool) {
    if let Some(v) = env::var(name)
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
    {
        if guard(v) {
            *slot = v;
        }
    }
}

fn coord_env_i64(name: &str, slot: &mut i64) {
    if let Some(v) = env::var(name)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
    {
        if v > 0 {
            *slot = v;
        }
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

    Ok(cfg)
}

#[cfg(test)]
mod coordination_tests {
    use super::*;
    use std::fs;
    use std::sync::{Mutex, OnceLock};

    // env var mutation is process-global; serialize these tests.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn defaults_when_no_config() {
        let _g = env_lock().lock().unwrap();
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
        let _g = env_lock().lock().unwrap();
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
        let _g = env_lock().lock().unwrap();
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
        let _g = env_lock().lock().unwrap();
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
        let _g = env_lock().lock().unwrap();
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
