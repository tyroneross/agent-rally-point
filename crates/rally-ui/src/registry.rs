// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Persisted registry of repos/folders the operator has added to `rally-ui`.
//!
//! Stored at `~/.rally-ui/registry.json`. Two kinds of entries:
//! - `Repo`: the path itself is expected to be a rally room (has a `.rally/`
//!   subdir, though it need not exist yet — a missing `.rally/` just shows up
//!   as a room in `error` health rather than being silently hidden).
//! - `Root`: a top-level folder to walk (bounded depth) looking for nested
//!   rooms. The root path itself is never treated as a room.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const MAX_WALK_DEPTH: usize = 3;
// `.git` (and all other dotdirs, including `.rally` itself) are skipped by
// the leading-dot check in `walk` below; these two are the non-dot noise.
const SKIP_DIR_NAMES: [&str; 2] = ["node_modules", "target"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Repo,
    Root,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub id: String,
    pub path: String,
    pub kind: EntryKind,
    pub added_at: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub entries: Vec<RegistryEntry>,
}

/// Deterministic short id for a path. Not cryptographic — just needs to be
/// stable across process runs so the same registered path always maps to the
/// same id (used as the wire identifier in `/api/rooms`).
pub fn short_id(path: &str) -> String {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn registry_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".rally-ui")
}

fn registry_file() -> PathBuf {
    registry_dir().join("registry.json")
}

pub fn load() -> Result<Registry> {
    let path = registry_file();
    if !path.exists() {
        return Ok(Registry::default());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading registry at {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Registry::default());
    }
    let registry: Registry = serde_json::from_str(&raw)
        .with_context(|| format!("parsing registry JSON at {}", path.display()))?;
    Ok(registry)
}

pub fn save(registry: &Registry) -> Result<()> {
    let dir = registry_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating registry dir {}", dir.display()))?;
    let path = registry_file();
    let body = serde_json::to_string_pretty(registry).context("serializing registry")?;
    std::fs::write(&path, body).with_context(|| format!("writing registry at {}", path.display()))
}

/// Add a new entry. Validates the path exists as a directory. Returns the
/// created entry (idempotent: re-adding an identical canonicalized path +
/// kind returns the existing entry rather than duplicating it).
pub fn add(path_str: &str, kind: EntryKind) -> Result<RegistryEntry> {
    let raw_path = Path::new(path_str);
    let canonical = std::fs::canonicalize(raw_path)
        .with_context(|| format!("path does not exist: {path_str}"))?;
    if !canonical.is_dir() {
        bail!("path is not a directory: {}", canonical.display());
    }
    let canonical_str = canonical.to_string_lossy().to_string();
    let id = short_id(&canonical_str);

    let mut registry = load()?;
    if let Some(existing) = registry
        .entries
        .iter()
        .find(|e| e.id == id && e.kind == kind)
    {
        return Ok(existing.clone());
    }
    let entry = RegistryEntry {
        id,
        path: canonical_str,
        kind,
        added_at: chrono::Utc::now().to_rfc3339(),
    };
    registry.entries.push(entry.clone());
    save(&registry)?;
    Ok(entry)
}

/// Remove an entry matched either by its own id, or by the id of any room
/// path it (directly or via discovery) resolves to. Returns true if an entry
/// was removed.
pub fn remove_by_room_or_entry_id(target_id: &str) -> Result<bool> {
    let mut registry = load()?;
    let before = registry.entries.len();
    let mut removed = false;
    registry.entries.retain(|entry| {
        if removed {
            return true;
        }
        let matches = entry.id == target_id
            || rooms_for_entry(entry)
                .iter()
                .any(|room| short_id(&room.to_string_lossy()) == target_id);
        if matches {
            removed = true;
            false
        } else {
            true
        }
    });
    if registry.entries.len() != before {
        save(&registry)?;
    }
    Ok(removed)
}

/// Room paths a registry entry resolves to. `Repo` entries are always
/// exactly the entry's own path (whether or not `.rally/` currently exists —
/// callers surface that as an error-health room rather than hiding it).
/// `Root` entries expand via bounded directory discovery; the root path
/// itself is never a room.
pub fn rooms_for_entry(entry: &RegistryEntry) -> Vec<PathBuf> {
    match entry.kind {
        EntryKind::Repo => vec![PathBuf::from(&entry.path)],
        EntryKind::Root => discover_rooms(Path::new(&entry.path)),
    }
}

/// Walk `root`'s children (never `root` itself — a `Root` entry is a scan
/// boundary, not a candidate room; use `EntryKind::Repo` to register a path
/// that is itself a room) up to `MAX_WALK_DEPTH` levels looking for
/// directories that contain a `.rally/` subdir. Skips `node_modules`,
/// `target`, `.git`, and other dotdirs. Directories that are themselves
/// rooms are not searched further beneath (nested "rooms within rooms" are
/// not a real topology here).
pub fn discover_rooms(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk_children(root, 0, &mut found);
    found
}

fn walk_children(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth >= MAX_WALK_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            // Skip all dotdirs — `.rally`, `.git`, incidental dirs like
            // `.cache`/`.venv`. A dotdir is never itself a candidate room
            // path (rooms are named after the project, not `.rally`).
            continue;
        }
        if SKIP_DIR_NAMES.contains(&name) {
            continue;
        }
        if path.join(".rally").is_dir() {
            found.push(path);
        } else {
            walk_children(&path, depth + 1, found);
        }
    }
}
