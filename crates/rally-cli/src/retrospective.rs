// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! `rally retrospective` — render a human-readable digest of the rally
//! point's history, grouped by engagement.
//!
//! The retrospective is a **derived view** over the segmented ledger
//! (`.rally/log/<engagement>.jsonl` + the migrated archive). Each
//! engagement is one section; within a section, facts are bucketed by
//! kind: handoffs, ownership (claims/releases), decisions, artifacts,
//! blockers/resolutions. Output is **deterministic** (same input ⇒ same
//! markdown — no `Date.now()`, no random ordering), so it can be checked
//! in as a durable retrospective record alongside the segments.
//!
//! The default output path is `.rally/RETROSPECTIVE.md`. Re-running
//! `rally retrospective` overwrites the file in place (idempotent
//! regeneration); the data source is the segment set, so the source of
//! truth is always the ledger, never the rendered markdown.

use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{RallyError, Result};
use crate::short_id;
use crate::store::{Fact, FactKind, RoomStore};

pub(crate) const RETROSPECTIVE_FILENAME: &str = "RETROSPECTIVE.md";

#[derive(Debug, Serialize)]
pub(crate) struct EngagementSummary {
    pub(crate) engagement: String,
    pub(crate) total_facts: usize,
    pub(crate) handoffs: usize,
    pub(crate) claims: usize,
    pub(crate) releases: usize,
    pub(crate) decisions: usize,
    pub(crate) artifacts: usize,
    pub(crate) blockers: usize,
    pub(crate) resolves: usize,
    pub(crate) first_seq: i64,
    pub(crate) last_seq: i64,
}

#[derive(Debug, Serialize)]
pub(crate) struct RetrospectiveOutcome {
    pub(crate) output_path: String,
    pub(crate) action: &'static str, // "created" | "updated" | "unchanged"
    pub(crate) engagements: Vec<EngagementSummary>,
    pub(crate) total_facts: usize,
    pub(crate) total_engagements: usize,
}

/// Group the supplied facts by the engagement they belong to.
///
/// **Engagement-resolution rules for retrospective grouping** (we do NOT
/// re-key from env / active-engagement here — those are stamps for *new*
/// appends; the retrospective reads what is already in the ledger):
///
/// 1. Explicit `--engagement <label>` filter wins: only facts tagged with
///    that label (or matching the date heuristic below) appear, all under
///    one section.
/// 2. Otherwise: facts are grouped by their on-disk segment file. Each
///    segment file is one engagement section. The grouping is keyed by
///    *segment file name* (== engagement label) rather than by a
///    per-fact tag, because pre-R5 (R1) rows have no engagement field and
///    are filed under their date-derived segment during migration.
///
/// For "all engagements" mode the caller passes `engagement_filter: None`
/// and we use the segment-based grouping. We read the segment files
/// directly because the `Fact` payload doesn't expose the file-of-origin
/// at the read API. Iteration mirrors what `RoomStore::open_at` walks.
fn group_facts_by_engagement(
    facts: &[Fact],
    engagement_filter: Option<&str>,
    log_dir: &Path,
    archive_dir: &Path,
) -> Result<BTreeMap<String, Vec<Fact>>> {
    let mut by_engagement: BTreeMap<String, Vec<Fact>> = BTreeMap::new();

    // Build a seq → engagement label lookup from the on-disk segment files.
    let mut seq_to_engagement: BTreeMap<i64, String> = BTreeMap::new();
    for dir in [log_dir, archive_dir] {
        if !dir.exists() {
            continue;
        }
        let mut entries: Vec<PathBuf> = fs::read_dir(dir)
            .map_err(RallyError::io(format!("read_dir {}", dir.display())))?
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_none_or(|n| !n.contains(".tmp-"))
            })
            .collect();
        entries.sort();
        for path in entries {
            let label = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();
            let content = fs::read_to_string(&path)
                .map_err(RallyError::io(format!("read {}", path.display())))?;
            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(line)
                    && let Some(seq) = parsed.get("seq").and_then(|s| s.as_i64())
                {
                    // First occurrence wins (live segments come before archive
                    // in the dir iteration order); migration may duplicate.
                    seq_to_engagement
                        .entry(seq)
                        .or_insert_with(|| label.clone());
                }
            }
        }
    }

    for fact in facts {
        let engagement = seq_to_engagement
            .get(&fact.seq)
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());
        if let Some(filter) = engagement_filter
            && engagement != filter
        {
            continue;
        }
        by_engagement
            .entry(engagement)
            .or_default()
            .push(fact.clone());
    }

    Ok(by_engagement)
}

/// Render the markdown body. Deterministic for a given input — no clocks,
/// no random salt, no order-dependent enumeration.
fn render_markdown(
    grouped: &BTreeMap<String, Vec<Fact>>,
    overall_total: usize,
) -> (String, Vec<EngagementSummary>) {
    let mut out = String::new();
    out.push_str("<!--\n");
    out.push_str(
        "SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>\n",
    );
    out.push_str("SPDX-License-Identifier: Apache-2.0\n");
    out.push_str("-->\n\n");
    out.push_str("# Rally retrospective\n\n");
    out.push_str(
        "Auto-generated digest of every engagement recorded in this repo's rally point. Source of truth is `.rally/log/<engagement>.jsonl`; this markdown is a derived view. Regenerate with `rally retrospective`; do not hand-edit.\n\n",
    );
    out.push_str(&format!(
        "**Total facts:** {} across {} engagement(s).\n\n",
        overall_total,
        grouped.len()
    ));

    let mut summaries = Vec::with_capacity(grouped.len());

    for (engagement, facts) in grouped {
        let summary = build_summary(engagement, facts);
        out.push_str(&format!("## Engagement: `{engagement}`\n\n"));
        out.push_str(&format!(
            "- **Facts:** {} (seq {} → {})\n",
            summary.total_facts, summary.first_seq, summary.last_seq
        ));
        out.push_str(&format!(
            "- **Breakdown:** {} handoffs · {} claims · {} releases · {} decisions · {} artifacts · {} blockers · {} resolves\n\n",
            summary.handoffs,
            summary.claims,
            summary.releases,
            summary.decisions,
            summary.artifacts,
            summary.blockers,
            summary.resolves,
        ));

        // Sub-section: handoffs (from → to).
        render_section(
            &mut out,
            "Handoffs",
            facts,
            |f| f.kind == FactKind::Handoff,
            |f| {
                let from = f.tool.as_deref().unwrap_or("?");
                let to = f.target.as_deref().unwrap_or("?");
                format!("- **seq {}** · `{from}` → `{to}` · {}", f.seq, f.subject)
            },
        );

        // Sub-section: ownership (claims + releases interleaved by seq).
        render_section(
            &mut out,
            "Ownership",
            facts,
            |f| f.kind == FactKind::Claim || f.kind == FactKind::Release,
            |f| {
                let verb = if f.kind == FactKind::Release {
                    "released"
                } else {
                    "claimed"
                };
                let owner = f.tool.as_deref().unwrap_or("?");
                let scope = if f.scope.is_empty() {
                    String::new()
                } else {
                    format!(" `{}`", f.scope.join(" "))
                };
                format!(
                    "- **seq {}** · `{owner}` {verb}{scope} — {}",
                    f.seq, f.subject
                )
            },
        );

        // Sub-section: decisions.
        render_section(
            &mut out,
            "Decisions",
            facts,
            |f| f.kind == FactKind::Decision,
            |f| {
                let tool = f.tool.as_deref().unwrap_or("?");
                let status = f
                    .status
                    .as_deref()
                    .map(|s| format!(" *({s})*"))
                    .unwrap_or_default();
                format!("- **seq {}** · `{tool}`{status} — {}", f.seq, f.subject)
            },
        );

        // Sub-section: artifacts.
        render_section(
            &mut out,
            "Artifacts",
            facts,
            |f| f.kind == FactKind::Artifact,
            |f| {
                let tool = f.tool.as_deref().unwrap_or("?");
                let uri = f
                    .uri
                    .as_deref()
                    .map(|u| format!(" → `{u}`"))
                    .unwrap_or_default();
                let evidence = if f.evidence.is_empty() {
                    String::new()
                } else {
                    format!(" · evidence: {}", f.evidence.join("; "))
                };
                format!(
                    "- **seq {}** · `{tool}`{uri} — {}{evidence}",
                    f.seq, f.subject
                )
            },
        );

        // Sub-section: blockers + resolutions.
        render_section(
            &mut out,
            "Blockers / resolutions",
            facts,
            |f| f.kind == FactKind::Blocker || f.kind == FactKind::Resolve,
            |f| {
                let tool = f.tool.as_deref().unwrap_or("?");
                let kind_label = if f.kind == FactKind::Resolve {
                    "resolved"
                } else {
                    "blocker"
                };
                let severity = f
                    .severity
                    .as_deref()
                    .map(|s| format!(" *(severity: {s})*"))
                    .unwrap_or_default();
                let ref_id = f
                    .ref_id
                    .as_deref()
                    .map(|r| format!(" → ref `{r}`"))
                    .unwrap_or_default();
                format!(
                    "- **seq {}** · `{tool}` · {kind_label}{severity}{ref_id} — {}",
                    f.seq, f.subject
                )
            },
        );

        summaries.push(summary);
    }

    (out, summaries)
}

fn render_section<F, R>(out: &mut String, title: &str, facts: &[Fact], predicate: F, render_line: R)
where
    F: Fn(&&Fact) -> bool,
    R: Fn(&Fact) -> String,
{
    let mut entries: Vec<&Fact> = facts.iter().filter(predicate).collect();
    entries.sort_by_key(|f| f.seq);
    if entries.is_empty() {
        return;
    }
    out.push_str(&format!("### {title}\n\n"));
    for f in entries {
        out.push_str(&render_line(f));
        out.push('\n');
    }
    out.push('\n');
}

fn build_summary(engagement: &str, facts: &[Fact]) -> EngagementSummary {
    let mut s = EngagementSummary {
        engagement: engagement.to_string(),
        total_facts: facts.len(),
        handoffs: 0,
        claims: 0,
        releases: 0,
        decisions: 0,
        artifacts: 0,
        blockers: 0,
        resolves: 0,
        first_seq: facts.iter().map(|f| f.seq).min().unwrap_or(0),
        last_seq: facts.iter().map(|f| f.seq).max().unwrap_or(0),
    };
    for f in facts {
        match f.kind {
            FactKind::Handoff => s.handoffs += 1,
            FactKind::Claim => s.claims += 1,
            FactKind::Release => s.releases += 1,
            FactKind::Decision => s.decisions += 1,
            FactKind::Artifact => s.artifacts += 1,
            FactKind::Blocker => s.blockers += 1,
            FactKind::Resolve => s.resolves += 1,
            _ => {}
        }
    }
    s
}

/// Atomic markdown write: tmp file + rename, mirroring R4's `init.rs`
/// pattern. Tolerates the rename race the same way [`crate::store`]'s
/// `refresh_log_index` does (NotFound at rename time = a peer already
/// replaced the file with an equivalent render, treat as no-op).
fn atomic_write(target: &Path, content: &str) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(RallyError::io(format!("create {}", parent.display())))?;
    }
    let temp_path = target.with_extension(format!(
        "{}.tmp-{}",
        target.extension().and_then(|e| e.to_str()).unwrap_or("md"),
        short_id()
    ));
    fs::write(&temp_path, content)
        .map_err(RallyError::io(format!("write {}", temp_path.display())))?;
    match fs::rename(&temp_path, target) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let _ = fs::remove_file(&temp_path);
            Ok(())
        }
        Err(err) => {
            let _ = fs::remove_file(&temp_path);
            Err(RallyError::Io {
                context: format!("replace {} with {}", target.display(), temp_path.display()),
                source: err,
            })
        }
    }
}

/// Run the retrospective generator: read every fact, group by engagement,
/// render markdown, write atomically.
pub(crate) fn run_retrospective(
    repo_root: PathBuf,
    engagement_filter: Option<&str>,
    out_override: Option<&str>,
) -> Result<RetrospectiveOutcome> {
    let room = RoomStore::open_at(repo_root.clone())?;
    let facts = room.facts()?;
    let total = facts.len();

    let log_dir = repo_root.join(".rally").join(crate::store::LOG_DIRNAME);
    let archive_dir = repo_root.join(".rally").join(crate::store::ARCHIVE_DIRNAME);

    let grouped = group_facts_by_engagement(&facts, engagement_filter, &log_dir, &archive_dir)?;

    let (markdown, summaries) = render_markdown(&grouped, total);

    let output_path = match out_override {
        Some(path) => PathBuf::from(path),
        None => repo_root.join(".rally").join(RETROSPECTIVE_FILENAME),
    };

    let action = if output_path.exists() {
        let existing = fs::read_to_string(&output_path)
            .map_err(RallyError::io(format!("read {}", output_path.display())))?;
        if existing == markdown {
            "unchanged"
        } else {
            "updated"
        }
    } else {
        "created"
    };
    if action != "unchanged" {
        atomic_write(&output_path, &markdown)?;
    }

    Ok(RetrospectiveOutcome {
        output_path: output_path.display().to_string(),
        action,
        total_facts: total,
        total_engagements: summaries.len(),
        engagements: summaries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FACT_SCHEMA;
    use crate::now_string;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rally-retro-{label}-{nanos}"));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn make_fact(event_id: &str, kind: FactKind, scope: &str, summary: &str) -> Fact {
        Fact {
            schema: FACT_SCHEMA.to_string(),
            event_id: event_id.to_string(),
            seq: 0,
            thread_id: format!("t-{event_id}"),
            kind,
            tool: Some("test-tool".to_string()),
            role: Some("test".to_string()),
            subject: format!("subject {summary}"),
            scope: vec![scope.to_string()],
            created_at: now_string(),
            summary: Some(summary.to_string()),
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        }
    }

    /// Seed a room with two engagements, run retrospective, assert the
    /// markdown groups by engagement and lists each kind's section.
    #[test]
    fn retrospective_groups_by_engagement_with_all_sections() {
        let _env = crate::PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // SAFETY: env mutation serialized by PROCESS_ENV_LOCK above.
        unsafe {
            std::env::remove_var(crate::store::ENGAGEMENT_ENV_VAR);
        }
        let root = unique_root("groups");
        let mut store = RoomStore::open_at(root.clone()).unwrap();

        store.set_active_engagement_for_test("alpha");
        store
            .append_fact(&make_fact("e1", FactKind::Claim, "src/x.rs", "alpha claim"))
            .unwrap();
        store
            .append_fact(&make_fact(
                "e2",
                FactKind::Decision,
                "src/x.rs",
                "alpha decision",
            ))
            .unwrap();
        let mut handoff = make_fact("e3", FactKind::Handoff, "src/x.rs", "alpha handoff");
        handoff.target = Some("peer-tool".to_string());
        store.append_fact(&handoff).unwrap();

        store.set_active_engagement_for_test("beta");
        let mut blocker = make_fact("e4", FactKind::Blocker, "src/y.rs", "beta blocker");
        blocker.severity = Some("medium".to_string());
        store.append_fact(&blocker).unwrap();
        let mut resolve = make_fact("e5", FactKind::Resolve, "src/y.rs", "beta resolved");
        resolve.ref_id = Some("fact_blocker".to_string());
        store.append_fact(&resolve).unwrap();
        let mut artifact = make_fact("e6", FactKind::Artifact, "src/y.rs", "beta artifact");
        artifact.uri = Some("docs/output.md".to_string());
        artifact.evidence = vec!["cargo test green".to_string()];
        store.append_fact(&artifact).unwrap();
        drop(store);

        let outcome = run_retrospective(root.clone(), None, None).unwrap();
        assert_eq!(outcome.total_facts, 6);
        assert_eq!(outcome.total_engagements, 2);
        assert_eq!(outcome.action, "created");

        let md = fs::read_to_string(&outcome.output_path).unwrap();
        // Header.
        assert!(md.contains("# Rally retrospective"));
        // Both engagement sections present (alpha + beta).
        assert!(md.contains("## Engagement: `alpha`"));
        assert!(md.contains("## Engagement: `beta`"));
        // alpha sections.
        assert!(md.contains("### Handoffs"));
        assert!(md.contains("`test-tool` → `peer-tool`"));
        assert!(md.contains("### Ownership"));
        assert!(md.contains("`test-tool` claimed"));
        assert!(md.contains("### Decisions"));
        // beta sections.
        assert!(md.contains("### Blockers / resolutions"));
        assert!(md.contains("severity: medium"));
        assert!(md.contains("### Artifacts"));
        assert!(md.contains("docs/output.md"));
        assert!(md.contains("evidence: cargo test green"));

        fs::remove_dir_all(&root).ok();
    }

    /// Regenerating without changes yields byte-identical markdown
    /// (deterministic output → action == "unchanged").
    #[test]
    fn retrospective_is_idempotent() {
        let _env = crate::PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // SAFETY: env mutation serialized by PROCESS_ENV_LOCK above.
        unsafe {
            std::env::remove_var(crate::store::ENGAGEMENT_ENV_VAR);
        }
        let root = unique_root("idempotent");
        let store = RoomStore::open_at(root.clone()).unwrap();
        store
            .append_fact(&make_fact("e1", FactKind::Decision, "src/", "first"))
            .unwrap();
        store
            .append_fact(&make_fact("e2", FactKind::Decision, "src/", "second"))
            .unwrap();
        drop(store);

        let outcome1 = run_retrospective(root.clone(), None, None).unwrap();
        let body1 = fs::read_to_string(&outcome1.output_path).unwrap();
        assert_eq!(outcome1.action, "created");

        let outcome2 = run_retrospective(root.clone(), None, None).unwrap();
        let body2 = fs::read_to_string(&outcome2.output_path).unwrap();
        assert_eq!(outcome2.action, "unchanged");
        assert_eq!(body1, body2);

        fs::remove_dir_all(&root).ok();
    }

    /// `--engagement` filter limits the output to one section.
    #[test]
    fn retrospective_filter_limits_to_one_engagement() {
        let _env = crate::PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // SAFETY: env mutation serialized by PROCESS_ENV_LOCK above.
        unsafe {
            std::env::remove_var(crate::store::ENGAGEMENT_ENV_VAR);
        }
        let root = unique_root("filter");
        let mut store = RoomStore::open_at(root.clone()).unwrap();
        store.set_active_engagement_for_test("alpha");
        store
            .append_fact(&make_fact("e1", FactKind::Decision, "src/", "alpha"))
            .unwrap();
        store.set_active_engagement_for_test("beta");
        store
            .append_fact(&make_fact("e2", FactKind::Decision, "src/", "beta"))
            .unwrap();
        drop(store);

        let outcome = run_retrospective(root.clone(), Some("alpha"), None).unwrap();
        assert_eq!(outcome.total_engagements, 1);
        let md = fs::read_to_string(&outcome.output_path).unwrap();
        assert!(md.contains("## Engagement: `alpha`"));
        assert!(!md.contains("## Engagement: `beta`"));

        fs::remove_dir_all(&root).ok();
    }
}
