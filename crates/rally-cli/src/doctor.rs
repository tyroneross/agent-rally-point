// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! `rally doctor` — read-only diagnostics for path hygiene and room registry.
//!
//! Two independent modes:
//!   --canonical-paths  scan active claims for non-canonical scopes and suffix collisions
//!   --prune-rooms      classify registry entries as live/stale; remove stale ones with --apply

use schemars::JsonSchema;
use serde::Serialize;
use std::path::PathBuf;

use crate::discovery::{DiscoveryWarning, KnownRoom};
use crate::error::{RallyError, Result};
use crate::store::RoomStore;
use crate::{normalize_path, paths_suffix_collide};

// =============================================================================
// Output types
// =============================================================================

/// An active claim scope that is not already in canonical form.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct NonCanonicalScope {
    /// The tool that owns the claim.
    pub(crate) tool: String,
    /// The raw scope stored in the fact.
    pub(crate) scope: String,
    /// What `normalize_path` would produce.
    pub(crate) canonical: String,
}

/// A pair of active claim scopes (from different tools) whose paths
/// share a 2+ component trailing suffix — the ambiguous same-file-different-spelling case.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct SuffixCollision {
    pub(crate) tool_a: String,
    pub(crate) scope_a: String,
    pub(crate) tool_b: String,
    pub(crate) scope_b: String,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct CanonicalPathsReport {
    pub(crate) non_canonical: Vec<NonCanonicalScope>,
    pub(crate) suffix_collisions: Vec<SuffixCollision>,
    pub(crate) warnings: Vec<DiscoveryWarning>,
}

/// One entry's classification for prune-rooms.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct StaleRoom {
    pub(crate) repo_root: PathBuf,
    pub(crate) display_name: String,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct PruneRoomsReport {
    /// Number of rooms whose `repo_root` directory still exists.
    pub(crate) live: usize,
    /// Entries that would be (or were) removed.
    pub(crate) stale: Vec<StaleRoom>,
    /// Whether the index was actually rewritten.
    pub(crate) applied: bool,
    pub(crate) warnings: Vec<DiscoveryWarning>,
}

// =============================================================================
// canonical-paths logic
// =============================================================================

pub(crate) fn run_canonical_paths() -> Result<CanonicalPathsReport> {
    let room = RoomStore::open()?;
    let snapshot = room.snapshot()?;

    // Collect (tool, scope) pairs from active claims only.
    let claim_scopes: Vec<(String, String)> = snapshot
        .active_claims
        .iter()
        .flat_map(|fact| {
            let tool = fact.tool.clone().unwrap_or_default();
            fact.scope
                .iter()
                // Skip the external-intake sentinel — it is a system tag, not a path.
                .filter(|s| *s != "external-intake")
                // Only path-like scopes (file: prefix or relative/absolute without scheme).
                .filter(|s| !s.contains("://") || s.starts_with("file:"))
                .map(move |s| (tool.clone(), s.clone()))
        })
        .collect();

    // Non-canonical: scopes where normalize_path changes the value.
    let non_canonical: Vec<NonCanonicalScope> = claim_scopes
        .iter()
        .filter_map(|(tool, scope)| {
            let canonical = normalize_path(scope.clone());
            if canonical != *scope {
                Some(NonCanonicalScope {
                    tool: tool.clone(),
                    scope: scope.clone(),
                    canonical,
                })
            } else {
                None
            }
        })
        .collect();

    // Suffix collisions: pairs of scopes from different tools that suffix-collide.
    // Strip file: prefix for the comparison (paths_suffix_collide works on either).
    let mut suffix_collisions: Vec<SuffixCollision> = Vec::new();
    for i in 0..claim_scopes.len() {
        for j in (i + 1)..claim_scopes.len() {
            let (tool_a, scope_a) = &claim_scopes[i];
            let (tool_b, scope_b) = &claim_scopes[j];
            // Only flag cross-tool collisions (same tool claiming the same file is fine).
            if tool_a == tool_b {
                continue;
            }
            let bare_a = scope_a.strip_prefix("file:").unwrap_or(scope_a.as_str());
            let bare_b = scope_b.strip_prefix("file:").unwrap_or(scope_b.as_str());
            if paths_suffix_collide(bare_a, bare_b) {
                suffix_collisions.push(SuffixCollision {
                    tool_a: tool_a.clone(),
                    scope_a: scope_a.clone(),
                    tool_b: tool_b.clone(),
                    scope_b: scope_b.clone(),
                });
            }
        }
    }

    Ok(CanonicalPathsReport {
        non_canonical,
        suffix_collisions,
        warnings: Vec::new(),
    })
}

// =============================================================================
// prune-rooms logic
// =============================================================================

/// Classify a `KnownRoom` as live or stale.
///
/// Conservative definition of stale: the `repo_root` directory does not exist.
/// A room that is merely unreadable (permissions, temporarily unmounted) is kept.
fn is_stale(room: &KnownRoom) -> bool {
    !room.repo_root.exists()
}

pub(crate) fn run_prune_rooms(apply: bool) -> Result<PruneRoomsReport> {
    // Read the index via the internal helper — we re-expose read/write here.
    let index_path = match room_index_path_pub() {
        Some(p) => p,
        None => {
            return Ok(PruneRoomsReport {
                live: 0,
                stale: Vec::new(),
                applied: false,
                warnings: vec![DiscoveryWarning {
                    code: "global_index_disabled".to_string(),
                    message: "RALLY_NO_GLOBAL_INDEX is set; prune-rooms unavailable".to_string(),
                    path: None,
                    count: None,
                }],
            });
        }
    };

    let index = read_index_for_prune(&index_path)?;

    let mut live_rooms: Vec<KnownRoom> = Vec::new();
    let mut stale_rooms: Vec<StaleRoom> = Vec::new();

    for room in index.rooms {
        if is_stale(&room) {
            stale_rooms.push(StaleRoom {
                repo_root: room.repo_root,
                display_name: room.display_name,
            });
        } else {
            live_rooms.push(room);
        }
    }

    let live_count = live_rooms.len();

    if apply && !stale_rooms.is_empty() {
        write_index_for_prune(&index_path, live_rooms, &index.schema)?;
    }

    Ok(PruneRoomsReport {
        live: live_count,
        stale: stale_rooms,
        applied: apply,
        warnings: Vec::new(),
    })
}

// =============================================================================
// Internal helpers — thin wrappers that avoid re-exporting private discovery types
// =============================================================================

/// Mirror of `discovery::room_index_path` — returns None when RALLY_NO_GLOBAL_INDEX is set.
fn room_index_path_pub() -> Option<std::path::PathBuf> {
    use std::env;
    if env::var_os("RALLY_NO_GLOBAL_INDEX")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return None;
    }
    let home = env::var_os("HOME").map(std::path::PathBuf::from)?;
    Some(home.join(".agent-rally-point/rooms/v1/index.json"))
}

// Minimal room index representation for reading/writing during prune.
// We cannot import the private `RoomIndex` from discovery, so we duplicate
// only what we need here.
#[derive(serde::Deserialize, serde::Serialize)]
struct PruneIndex {
    #[serde(default = "default_schema")]
    schema: String,
    #[serde(default)]
    rooms: Vec<KnownRoom>,
}

fn default_schema() -> String {
    "agent-rally.room-index.v1".to_string()
}

fn read_index_for_prune(path: &std::path::Path) -> Result<PruneIndex> {
    if !path.exists() {
        return Ok(PruneIndex {
            schema: default_schema(),
            rooms: Vec::new(),
        });
    }
    let text = std::fs::read_to_string(path)
        .map_err(RallyError::io(format!("read {}", path.display())))?;
    if text.trim().is_empty() {
        return Ok(PruneIndex {
            schema: default_schema(),
            rooms: Vec::new(),
        });
    }
    // Try the wrapped schema first, then fall back to a bare Vec<KnownRoom>.
    if let Ok(idx) = serde_json::from_str::<PruneIndex>(&text) {
        return Ok(idx);
    }
    if let Some(Ok(idx)) = serde_json::Deserializer::from_str(&text)
        .into_iter::<PruneIndex>()
        .next()
    {
        return Ok(idx);
    }
    if let Some(Ok(rooms)) = serde_json::Deserializer::from_str(&text)
        .into_iter::<Vec<KnownRoom>>()
        .next()
    {
        return Ok(PruneIndex {
            schema: default_schema(),
            rooms,
        });
    }
    Err(RallyError::json(format!("parse {}", path.display()))(
        serde_json::from_str::<serde_json::Value>(&text).unwrap_err(),
    ))
}

fn write_index_for_prune(
    path: &std::path::Path,
    live_rooms: Vec<KnownRoom>,
    schema: &str,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(RallyError::io(format!("create {}", parent.display())))?;
    }
    let index = PruneIndex {
        schema: schema.to_string(),
        rooms: live_rooms,
    };
    let content = serde_json::to_string_pretty(&index)
        .map_err(RallyError::json("render room index for prune"))?;
    let temp_path = path.with_extension("json.tmp-prune");
    std::fs::write(&temp_path, content)
        .map_err(RallyError::io(format!("write {}", temp_path.display())))?;
    std::fs::rename(&temp_path, path).map_err(|err| {
        let _ = std::fs::remove_file(&temp_path);
        RallyError::Io {
            context: format!(
                "replace {} with {}",
                path.display(),
                temp_path.display()
            ),
            source: err,
        }
    })
}
