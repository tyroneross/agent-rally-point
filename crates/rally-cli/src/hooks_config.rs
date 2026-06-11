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
