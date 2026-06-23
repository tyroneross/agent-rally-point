// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! `rally doctor` — diagnostics and remediation for path hygiene, room registry, and stale state.
//!
//! Three independent modes:
//!   --canonical-paths  scan active claims for non-canonical scopes and suffix collisions
//!   --prune-rooms      classify registry entries as live/stale; remove stale ones with --apply
//!   --reap-stale       reap over-TTL in-room claims and stale lead leases (dry-run; commit with --apply)

use schemars::JsonSchema;
use serde::Serialize;
use std::path::PathBuf;

use crate::discovery::{
    DiscoveryWarning, KnownRoom, RoomIndex, read_room_index_at, room_index_path,
    write_room_index_at,
};
use crate::error::Result;
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
    let index_path = match room_index_path() {
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

    let index = read_room_index_at(&index_path)?;

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
        let updated = RoomIndex {
            schema: index.schema,
            rooms: live_rooms,
        };
        write_room_index_at(&index_path, &updated)?;
    }

    Ok(PruneRoomsReport {
        live: live_count,
        stale: stale_rooms,
        applied: apply,
        warnings: Vec::new(),
    })
}
