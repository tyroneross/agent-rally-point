// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Axum HTTP surface: one static HTML page + a small JSON API over the
//! registry + per-room snapshots.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Json};
use axum::routing::get;
use serde::Deserialize;
use serde_json::json;
use tokio::task::JoinSet;

use crate::registry::{self, EntryKind, Registry};
use crate::room_source::{self, RoomSummary};

#[derive(Clone)]
pub struct AppState {
    pub rally_bin: Arc<String>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/rooms", get(list_rooms).post(add_room))
        .route("/api/rooms/:id", axum::routing::delete(remove_room))
        .route("/api/room/:id", get(room_detail))
        .layer(axum::middleware::from_fn(reject_non_local_host))
        .with_state(state)
}

/// DNS-rebinding guard: the server is localhost-only by intent, but a
/// rebinding page (attacker domain re-resolving to 127.0.0.1) is same-origin
/// in the browser and would bypass CORS preflight. Reject any request whose
/// Host header isn't a localhost form; port is not validated (the OS already
/// scopes the bind, and proxies rewriting the port are out of scope).
async fn reject_non_local_host(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let host_ok = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(|h| {
            let name = h.rsplit_once(':').map_or(h, |(n, _)| n);
            matches!(name, "127.0.0.1" | "localhost" | "[::1]")
        })
        .unwrap_or(false);
    if !host_ok {
        return (StatusCode::FORBIDDEN, "forbidden: non-local Host header").into_response();
    }
    next.run(req).await
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../assets/index.html"))
}

/// All (id, path) pairs across every registry entry, deduped by id. A room
/// discovered under more than one entry (e.g. a `repo` entry that also falls
/// under a separately-registered `root` entry) is only listed once.
fn all_rooms(registry: &Registry) -> Vec<(String, PathBuf)> {
    let mut seen: HashMap<String, PathBuf> = HashMap::new();
    for entry in &registry.entries {
        for room_path in registry::rooms_for_entry(entry) {
            let id = registry::short_id(&room_path.to_string_lossy());
            seen.entry(id).or_insert(room_path);
        }
    }
    seen.into_iter().collect()
}

fn find_room(registry: &Registry, id: &str) -> Option<PathBuf> {
    for entry in &registry.entries {
        for room_path in registry::rooms_for_entry(entry) {
            if registry::short_id(&room_path.to_string_lossy()) == id {
                return Some(room_path);
            }
        }
    }
    None
}

async fn list_rooms(State(state): State<AppState>) -> impl IntoResponse {
    let registry = match registry::load() {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed to load registry: {e}") })),
            )
                .into_response();
        }
    };

    let rooms = all_rooms(&registry);
    let mut set: JoinSet<RoomSummary> = JoinSet::new();
    for (_, room_path) in rooms {
        let rally_bin = state.rally_bin.as_ref().clone();
        set.spawn(async move {
            let snapshot = room_source::fetch_snapshot(&room_path, &rally_bin).await;
            RoomSummary::from(&snapshot)
        });
    }

    let mut summaries = Vec::new();
    while let Some(joined) = set.join_next().await {
        if let Ok(summary) = joined {
            summaries.push(summary);
        }
    }
    summaries.sort_by(|a, b| a.name.cmp(&b.name));

    Json(summaries).into_response()
}

#[derive(Deserialize)]
struct AddRoomRequest {
    path: String,
    kind: String,
}

async fn add_room(Json(req): Json<AddRoomRequest>) -> impl IntoResponse {
    let kind = match req.kind.as_str() {
        "repo" => EntryKind::Repo,
        "root" => EntryKind::Root,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    json!({ "error": format!("kind must be \"repo\" or \"root\", got {other:?}") }),
                ),
            )
                .into_response();
        }
    };

    match registry::add(&req.path, kind) {
        Ok(entry) => (StatusCode::OK, Json(json!({ "added": entry }))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn remove_room(AxumPath(id): AxumPath<String>) -> impl IntoResponse {
    match registry::remove_by_room_or_entry_id(&id) {
        Ok(true) => (StatusCode::OK, Json(json!({ "removed": true }))).into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no registry entry or room matches that id" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn room_detail(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> impl IntoResponse {
    let registry = match registry::load() {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed to load registry: {e}") })),
            )
                .into_response();
        }
    };
    let Some(room_path) = find_room(&registry, &id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no room matches that id" })),
        )
            .into_response();
    };
    let snapshot = room_source::fetch_snapshot(&room_path, state.rally_bin.as_ref()).await;
    Json(snapshot).into_response()
}
