// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! `rally init` — make the rally point a **findable, self-describing front door**.
//!
//! Two artifacts:
//!
//! 1. A fenced, idempotent pointer block injected into the repo's `CLAUDE.md`
//!    and `AGENTS.md` (created if absent, updated in place between stable
//!    `<!-- rally:start -->` / `<!-- rally:end -->` markers). Any agent that
//!    lands in the repo immediately sees where to enter and where the deeper
//!    docs live.
//! 2. `.rally/manifest.json` — a small self-description (committed,
//!    un-gitignored) that the rally point exposes: schema, repo, doc
//!    pointers, ledger location, room command. Pointers are real
//!    repo-relative paths verified to exist at init time.
//!
//! Both writes are idempotent: re-running `rally init` updates content
//! between the markers (or rewrites manifest.json) without duplicating
//! anything. Returns a JSON envelope describing what was created or updated.

use serde::Serialize;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{RallyError, Result};
use crate::short_id;
use crate::store::{LEDGER_FILENAME, LOG_DIRNAME};

pub(crate) const MANIFEST_SCHEMA: &str = "agent-rally.manifest.v1";
pub(crate) const MANIFEST_FILENAME: &str = "manifest.json";

pub(crate) const POINTER_START: &str = "<!-- rally:start -->";
pub(crate) const POINTER_END: &str = "<!-- rally:end -->";

const POINTER_DOC_TARGETS: &[&str] = &["CLAUDE.md", "AGENTS.md"];

const DOC_GUIDE: &str = "RALLY.md";
const DOC_DOCTRINE: &str = "dynamic-workflows/COORDINATION.md";
const DOC_PROTOCOL: &str = "dynamic-workflows/PROTOCOL.md";
const DOC_BOARD: &str = "docs/ORCHESTRATION.md";
const DOC_ANY_AGENT_ONBOARDING: &str = "docs/ANY-AGENT-ONBOARDING.md";

/// Result of `rally init` for one of the pointer-doc targets.
#[derive(Debug, Serialize)]
pub(crate) struct PointerOutcome {
    pub(crate) path: String,
    pub(crate) action: &'static str, // "created", "updated", "unchanged"
}

/// Result of writing `.rally/manifest.json`.
#[derive(Debug, Serialize)]
pub(crate) struct ManifestOutcome {
    pub(crate) path: String,
    pub(crate) action: &'static str, // "created", "updated", "unchanged"
}

#[derive(Debug, Serialize)]
pub(crate) struct InitOutcome {
    pub(crate) repo_root: String,
    pub(crate) manifest: ManifestOutcome,
    pub(crate) pointers: Vec<PointerOutcome>,
    pub(crate) docs: ManifestDocs,
    pub(crate) ledger_dir: String,
    pub(crate) room_cmd: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ManifestDocs {
    pub(crate) guide: String,
    pub(crate) doctrine: String,
    pub(crate) protocol: String,
    pub(crate) board: String,
    pub(crate) any_agent_onboarding: String,
}

/// Build the pointer block written into `CLAUDE.md`/`AGENTS.md`. Plain
/// markdown — fenced by stable `<!-- rally:start -->` / `<!-- rally:end -->`
/// markers so it can be re-rendered on every `rally init` without duplication.
fn pointer_block() -> String {
    let mut s = String::new();
    s.push_str(POINTER_START);
    s.push('\n');
    s.push_str("## Agent Rally Point\n\n");
    s.push_str(
        "This repo coordinates parallel coding agents via **agent-rally-point** \
         (per-repo, no external service).\n\n",
    );
    s.push_str("- **Self-locate FIRST:** `rally whoami --tool <you> --json` — host runtime, room, lead, mission, ack status. If `host_runtime.ambiguous` is true, STOP and resolve which host before acting (never guess).\n");
    s.push_str("- **Enter + acknowledge:** `rally enter --tool <host-llm-role-number> --json` (e.g. `claude_code:01`), then `rally ack --tool <you>` to confirm you ingested the rules/guardrails/lead/mission.\n");
    s.push_str("- **Resolve targets from live state:** Treat lead/tool ids as runtime data, not constants. Use `whoami`, `lead show`, `room`, `next`, and explicit handoff targets; do not copy ids from examples, old logs, or another repo.\n");
    s.push_str("- **What to do next:** `rally next --tool <you> --json`\n");
    s.push_str("- **Current state:** `rally room --json`\n");
    s.push_str("- **History (durable, per-engagement):** `.rally/log/`\n");
    s.push_str("- **Self-description (machine-readable pointers):** `.rally/manifest.json`\n\n");
    s.push_str("### Deeper docs\n\n");
    s.push_str("- **Guide (60-second):** [RALLY.md](RALLY.md)\n");
    s.push_str("- **Doctrine (Rally Flow):** [dynamic-workflows/COORDINATION.md](dynamic-workflows/COORDINATION.md)\n");
    s.push_str(
        "- **Wire protocol:** [dynamic-workflows/PROTOCOL.md](dynamic-workflows/PROTOCOL.md)\n",
    );
    s.push_str("- **Board / current lanes:** [docs/ORCHESTRATION.md](docs/ORCHESTRATION.md)\n");
    s.push_str("- **Any-agent onboarding contract:** [docs/ANY-AGENT-ONBOARDING.md](docs/ANY-AGENT-ONBOARDING.md)\n");
    s.push_str("- **Handoffs & managed agents:** [docs/HANDOFFS-AND-LAUNCHING-AGENTS.md](docs/HANDOFFS-AND-LAUNCHING-AGENTS.md)\n");
    s.push_str(POINTER_END);
    s.push('\n');
    s
}

/// Render the markdown body that *seeds* a freshly-created pointer doc. The
/// fenced pointer block goes near the top so an agent landing on the file
/// finds the rally entry immediately.
fn fresh_doc_body(filename: &str) -> String {
    let mut s = String::new();
    s.push_str(&format!("# {filename}\n\n"));
    s.push_str(
        "This file is read by coding agents on entry. The rally pointer below \
         tells agents where to coordinate and which deeper docs to load.\n\n",
    );
    s.push_str(&pointer_block());
    s
}

/// Atomic write: write to a sibling temp file in the same directory, then
/// `rename` over the target. Mirrors the cursor-write pattern in `store.rs`
/// (rename is atomic on local filesystems).
fn atomic_write(target: &Path, content: &str) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(RallyError::io(format!("create {}", parent.display())))?;
    }
    let temp_path = target.with_extension(format!(
        "{}.tmp-{}",
        target
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("rally"),
        short_id()
    ));
    fs::write(&temp_path, content)
        .map_err(RallyError::io(format!("write {}", temp_path.display())))?;
    fs::rename(&temp_path, target).map_err(|err| {
        let _ = fs::remove_file(&temp_path);
        RallyError::Io {
            context: format!("replace {} with {}", target.display(), temp_path.display()),
            source: err,
        }
    })
}

/// Inject (or refresh) the rally pointer block in the given doc.
///
/// * File absent → create with the seeded body containing one pointer block.
/// * File present, no markers → append the pointer block at the end with a
///   leading blank line; the rest of the file is untouched.
/// * File present, markers present → replace the content **between** the
///   markers in-place (no duplication, no growth on re-run).
fn upsert_pointer_in_doc(repo_root: &Path, filename: &str) -> Result<PointerOutcome> {
    let target = repo_root.join(filename);
    let block = pointer_block();

    if !target.exists() {
        let body = fresh_doc_body(filename);
        atomic_write(&target, &body)?;
        return Ok(PointerOutcome {
            path: filename.to_string(),
            action: "created",
        });
    }

    let existing = fs::read_to_string(&target)
        .map_err(RallyError::io(format!("read {}", target.display())))?;

    // Try to find existing markers and rewrite between them.
    if let (Some(start_idx), Some(end_idx)) =
        (existing.find(POINTER_START), existing.find(POINTER_END))
        && start_idx < end_idx
    {
        // Replace [start_idx .. end_idx + len(end_marker)] with the new block.
        let mut new_doc = String::with_capacity(existing.len() + block.len());
        new_doc.push_str(&existing[..start_idx]);
        // pointer_block() already ends with the end marker + newline; the
        // existing tail picks up after the end marker.
        new_doc.push_str(block.trim_end_matches('\n'));
        let after_end = end_idx + POINTER_END.len();
        // Skip exactly one trailing newline if present so we don't keep
        // accumulating blank lines on re-run.
        let tail = if existing[after_end..].starts_with('\n') {
            &existing[after_end + 1..]
        } else {
            &existing[after_end..]
        };
        // Re-add the newline that pointer_block() owns at its tail.
        new_doc.push('\n');
        new_doc.push_str(tail);

        if new_doc == existing {
            return Ok(PointerOutcome {
                path: filename.to_string(),
                action: "unchanged",
            });
        }
        atomic_write(&target, &new_doc)?;
        return Ok(PointerOutcome {
            path: filename.to_string(),
            action: "updated",
        });
    }

    // No markers (or malformed order) → append block with one separating blank line.
    let mut new_doc = existing.clone();
    if !new_doc.ends_with('\n') {
        new_doc.push('\n');
    }
    new_doc.push('\n');
    new_doc.push_str(&block);
    atomic_write(&target, &new_doc)?;
    Ok(PointerOutcome {
        path: filename.to_string(),
        action: "updated",
    })
}

/// Build the manifest JSON value. Pointers are repo-relative strings; each
/// one is verified to resolve at write time (failure surfaces as a hard
/// error rather than shipping a stale pointer). Pointer resolution is
/// checked against `worktree_root` because that is where the docs live for
/// the currently-checked-out branch; a fresh linked worktree may not have
/// the same docs as the main checkout.
fn build_manifest(repo_root: &Path, worktree_root: &Path) -> Result<(Value, ManifestDocs)> {
    let docs = ManifestDocs {
        guide: DOC_GUIDE.to_string(),
        doctrine: DOC_DOCTRINE.to_string(),
        protocol: DOC_PROTOCOL.to_string(),
        board: DOC_BOARD.to_string(),
        any_agent_onboarding: DOC_ANY_AGENT_ONBOARDING.to_string(),
    };

    // Verify each doc pointer resolves now. If something has moved we want
    // a loud failure, not a manifest full of dead links.
    for (label, rel) in [
        ("docs.guide", &docs.guide),
        ("docs.doctrine", &docs.doctrine),
        ("docs.protocol", &docs.protocol),
        ("docs.board", &docs.board),
        ("docs.any_agent_onboarding", &docs.any_agent_onboarding),
    ] {
        let resolved = worktree_root.join(rel);
        if !resolved.exists() {
            return Err(RallyError::Message(format!(
                "manifest pointer {label} -> {rel} does not resolve under {} \
                 (move or update the constant in init.rs)",
                worktree_root.display()
            )));
        }
    }

    let repo_name = repo_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let manifest = json!({
        "schema": MANIFEST_SCHEMA,
        "repo": repo_name,
        "docs": {
            "guide": docs.guide,
            "doctrine": docs.doctrine,
            "protocol": docs.protocol,
            "board": docs.board,
            "any_agent_onboarding": docs.any_agent_onboarding,
        },
        "ledger": format!(".rally/{LOG_DIRNAME}/"),
        "ledger_filename_legacy": format!(".rally/{LEDGER_FILENAME}"),
        "room_cmd": "rally room",
        "whoami_cmd": "rally whoami",
        "init_cmd": "rally init",
        "pointer_markers": {
            "start": POINTER_START,
            "end": POINTER_END,
        },
    });

    Ok((manifest, docs))
}

fn write_manifest(
    repo_root: &Path,
    worktree_root: &Path,
) -> Result<(ManifestOutcome, ManifestDocs)> {
    let (manifest, docs) = build_manifest(repo_root, worktree_root)?;
    let target = repo_root.join(".rally").join(MANIFEST_FILENAME);
    let rendered =
        serde_json::to_string_pretty(&manifest).map_err(RallyError::json("render manifest"))?;
    // Trailing newline so editors and `cat` behave.
    let rendered = format!("{rendered}\n");

    let action = if target.exists() {
        let existing = fs::read_to_string(&target)
            .map_err(RallyError::io(format!("read {}", target.display())))?;
        if existing == rendered {
            "unchanged"
        } else {
            "updated"
        }
    } else {
        "created"
    };

    if action != "unchanged" {
        atomic_write(&target, &rendered)?;
    }

    Ok((
        ManifestOutcome {
            path: format!(".rally/{MANIFEST_FILENAME}"),
            action,
        },
        docs,
    ))
}

/// Run the full `rally init` flow.
///
/// * `repo_root` — the **shared** dir all worktrees agree on (`.rally/`
///   coordination state lives here). For linked worktrees this resolves to
///   the main checkout via git's `commondir`. The manifest is written here so
///   every worktree sees one machine-readable self-description.
/// * `worktree_root` — the **current** checkout (active branch). The
///   `CLAUDE.md`/`AGENTS.md` pointer docs land here so they ride the active
///   branch's history; doc-pointer existence is also verified against this
///   root because a linked worktree may have a different doc set than the
///   main one.
///
/// Idempotent: re-running this function leaves all three artifacts
/// byte-for-byte identical when nothing has changed.
pub(crate) fn run_init(repo_root: PathBuf, worktree_root: PathBuf) -> Result<InitOutcome> {
    // Ensure `.rally/` exists so manifest write doesn't race with first room open.
    let rally_dir = repo_root.join(".rally");
    fs::create_dir_all(&rally_dir)
        .map_err(RallyError::io(format!("create {}", rally_dir.display())))?;

    let (manifest, docs) = write_manifest(&repo_root, &worktree_root)?;

    let mut pointers = Vec::with_capacity(POINTER_DOC_TARGETS.len());
    for filename in POINTER_DOC_TARGETS {
        pointers.push(upsert_pointer_in_doc(&worktree_root, filename)?);
    }

    Ok(InitOutcome {
        repo_root: repo_root.display().to_string(),
        manifest,
        pointers,
        docs,
        ledger_dir: format!(".rally/{LOG_DIRNAME}/"),
        room_cmd: "rally room".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Spin up a fake repo root mimicking arp-lead's layout closely enough
    /// that the manifest's pointer-resolution check passes.
    fn fake_repo(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rally-init-{label}-{nanos}"));
        fs::create_dir_all(&root).unwrap();
        for rel in [
            DOC_GUIDE,
            DOC_DOCTRINE,
            DOC_PROTOCOL,
            DOC_BOARD,
            DOC_ANY_AGENT_ONBOARDING,
        ] {
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            if !path.exists() {
                fs::write(&path, format!("# stub {rel}\n")).unwrap();
            }
        }
        root
    }

    #[test]
    fn init_creates_manifest_and_pointer_blocks() {
        let root = fake_repo("create");
        let outcome = run_init(root.clone(), root.clone()).unwrap();
        assert_eq!(outcome.manifest.action, "created");
        assert_eq!(outcome.pointers.len(), 2);
        for p in &outcome.pointers {
            assert_eq!(p.action, "created");
        }

        // Manifest file exists, parses, has the expected pointers.
        let manifest_path = root.join(".rally/manifest.json");
        assert!(manifest_path.exists());
        let parsed: Value =
            serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
        assert_eq!(parsed["schema"], MANIFEST_SCHEMA);
        assert_eq!(parsed["docs"]["guide"], DOC_GUIDE);
        assert_eq!(parsed["docs"]["board"], DOC_BOARD);
        assert_eq!(
            parsed["docs"]["any_agent_onboarding"],
            DOC_ANY_AGENT_ONBOARDING
        );
        assert_eq!(parsed["room_cmd"], "rally room");
        assert_eq!(parsed["ledger"], format!(".rally/{LOG_DIRNAME}/"));

        // Each pointer doc exists and contains exactly one start + one end marker.
        for filename in POINTER_DOC_TARGETS {
            let doc = fs::read_to_string(root.join(filename)).unwrap();
            assert_eq!(
                doc.matches(POINTER_START).count(),
                1,
                "exactly one start marker in {filename}"
            );
            assert_eq!(
                doc.matches(POINTER_END).count(),
                1,
                "exactly one end marker in {filename}"
            );
            assert!(doc.contains("rally enter"));
            assert!(doc.contains("RALLY.md"));
        }

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn init_is_idempotent_on_second_run() {
        let root = fake_repo("idempotent");
        let _ = run_init(root.clone(), root.clone()).unwrap();

        // Capture file states.
        let claude_before = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
        let agents_before = fs::read_to_string(root.join("AGENTS.md")).unwrap();
        let manifest_before = fs::read_to_string(root.join(".rally/manifest.json")).unwrap();

        // Re-run.
        let outcome2 = run_init(root.clone(), root.clone()).unwrap();
        assert_eq!(outcome2.manifest.action, "unchanged");
        for p in &outcome2.pointers {
            // Both docs were just freshly created (no manual edits between
            // runs), so the upsert path finds matching markers and produces
            // identical content → "unchanged".
            assert_eq!(p.action, "unchanged", "{} should be unchanged", p.path);
        }

        // Files byte-for-byte identical.
        assert_eq!(
            claude_before,
            fs::read_to_string(root.join("CLAUDE.md")).unwrap()
        );
        assert_eq!(
            agents_before,
            fs::read_to_string(root.join("AGENTS.md")).unwrap()
        );
        assert_eq!(
            manifest_before,
            fs::read_to_string(root.join(".rally/manifest.json")).unwrap()
        );

        // Exactly one marker pair, still.
        for filename in POINTER_DOC_TARGETS {
            let doc = fs::read_to_string(root.join(filename)).unwrap();
            assert_eq!(doc.matches(POINTER_START).count(), 1);
            assert_eq!(doc.matches(POINTER_END).count(), 1);
        }

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn init_preserves_existing_doc_body_when_appending_pointer() {
        let root = fake_repo("preserve");
        let claude_path = root.join("CLAUDE.md");
        let original = "# CLAUDE.md\n\nLocal project rules:\n- be concise\n- no filler\n";
        fs::write(&claude_path, original).unwrap();

        let outcome = run_init(root.clone(), root.clone()).unwrap();
        let after = fs::read_to_string(&claude_path).unwrap();
        // Original body preserved verbatim at the head.
        assert!(after.starts_with(original));
        // Pointer block appended.
        assert!(after.contains(POINTER_START));
        assert!(after.contains(POINTER_END));
        assert!(after.contains("rally enter"));
        // Pointer recorded as "updated" (not "created") on this doc.
        let claude_outcome = outcome
            .pointers
            .iter()
            .find(|p| p.path == "CLAUDE.md")
            .unwrap();
        assert_eq!(claude_outcome.action, "updated");

        // Second run on this mixed doc: between-markers replacement keeps body intact,
        // no duplication, marker counts stay at 1.
        let _ = run_init(root.clone(), root.clone()).unwrap();
        let after2 = fs::read_to_string(&claude_path).unwrap();
        assert!(after2.starts_with(original));
        assert_eq!(after2.matches(POINTER_START).count(), 1);
        assert_eq!(after2.matches(POINTER_END).count(), 1);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn init_refreshes_pointer_block_in_place_without_duplication() {
        let root = fake_repo("refresh");
        let agents_path = root.join("AGENTS.md");
        // Seed with a stale rally block between the markers.
        let stale = format!(
            "# AGENTS.md\n\nNotes\n\n{POINTER_START}\nOLD STALE POINTER CONTENT\n{POINTER_END}\n\nMore notes\n"
        );
        fs::write(&agents_path, &stale).unwrap();

        let outcome = run_init(root.clone(), root.clone()).unwrap();
        let after = fs::read_to_string(&agents_path).unwrap();
        // Stale content gone, fresh pointer-block content present.
        assert!(!after.contains("OLD STALE POINTER CONTENT"));
        assert!(after.contains("rally enter"));
        // Surrounding notes preserved.
        assert!(after.contains("# AGENTS.md"));
        assert!(after.contains("Notes"));
        assert!(after.contains("More notes"));
        // Single marker pair.
        assert_eq!(after.matches(POINTER_START).count(), 1);
        assert_eq!(after.matches(POINTER_END).count(), 1);

        let agents_outcome = outcome
            .pointers
            .iter()
            .find(|p| p.path == "AGENTS.md")
            .unwrap();
        assert_eq!(agents_outcome.action, "updated");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn init_fails_when_doc_pointer_does_not_resolve() {
        // Build a fake repo missing the doctrine doc.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rally-init-missing-{nanos}"));
        fs::create_dir_all(&root).unwrap();
        // Create all docs *except* the doctrine one.
        for rel in [DOC_GUIDE, DOC_PROTOCOL, DOC_BOARD, DOC_ANY_AGENT_ONBOARDING] {
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, format!("# stub {rel}\n")).unwrap();
        }
        let err = run_init(root.clone(), root.clone()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("docs.doctrine"), "got: {msg}");
        assert!(msg.contains(DOC_DOCTRINE), "got: {msg}");

        fs::remove_dir_all(&root).ok();
    }
}
