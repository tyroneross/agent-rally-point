// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! #8 Cross-lane ripple detector.
//!
//! At `rally say artifact` / `rally check`, for source files that changed
//! (i.e. whose current hash differs from the claim-open hash), extract
//! `pub`/`pub(crate)` fn signatures via a lightweight Rust-aware grep.
//!
//! For each changed symbol, query active claims for OTHER tools whose owned
//! files reference the symbol → append a NON-blocking `ripple-alert` fact.
//!
//! Never blocks. Notification only.

use crate::store::{Fact, FactKind, RoomSnapshot};
use crate::{FACT_SCHEMA, new_id, now_string};
use std::fs;
use std::path::Path;

/// Extract `pub` and `pub(crate)` function names from a source file.
/// Best-effort: uses simple line scanning, not a full Rust parser.
///
/// Returns a deduplicated list of symbol names found.
pub(crate) fn extract_pub_fn_names(path: &Path) -> Vec<String> {
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        // Match `pub fn` or `pub(crate) fn` or `pub(super) fn` etc.
        let after_pub = if let Some(rest) = trimmed.strip_prefix("pub(") {
            // pub(…) fn: skip to `fn`
            rest.find(") fn ").map(|pos| &rest[pos + 5..])
        } else {
            trimmed.strip_prefix("pub fn ")
        };

        if let Some(rest) = after_pub {
            // fn name ends at `(` or `<`
            let name: String = rest
                .chars()
                .take_while(|&c| c.is_alphanumeric() || c == '_')
                .collect();
            if !name.is_empty() {
                names.push(name);
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Check whether a file at `path` references any of `symbols` (as whole-word
/// occurrences). Returns the subset of symbols found in the file.
pub(crate) fn file_references_symbols(path: &Path, symbols: &[String]) -> Vec<String> {
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    symbols
        .iter()
        .filter(|sym| {
            // Simple substring check; good enough for advisory purposes.
            content.contains(sym.as_str())
        })
        .cloned()
        .collect()
}

/// Build `ripple-alert` facts for changed symbols that affect other tools'
/// claimed files.
///
/// - `changed_files`: repo-relative paths of files that changed (no `file:` prefix).
/// - `repo_root`: used to resolve file paths.
/// - `calling_tool`: the tool that posted the artifact (we skip its own claims).
/// - `snapshot`: active room snapshot to query peer claims.
///
/// Returns facts to append (caller does the appending; this module stays pure).
pub(crate) fn build_ripple_alerts(
    changed_files: &[String],
    repo_root: &Path,
    calling_tool: &str,
    snapshot: &RoomSnapshot,
) -> Vec<Fact> {
    let mut facts = Vec::new();

    // Collect all changed symbols from changed files.
    let mut changed_symbols: Vec<String> = Vec::new();
    for rel in changed_files {
        let abs = repo_root.join(rel);
        let syms = extract_pub_fn_names(&abs);
        changed_symbols.extend(syms);
    }
    changed_symbols.sort();
    changed_symbols.dedup();

    if changed_symbols.is_empty() {
        return facts;
    }

    // For each peer claim (different tool), check if their owned files reference
    // any changed symbol.
    for claim in &snapshot.active_claims {
        let owner = match claim.tool.as_deref() {
            Some(t) if t != calling_tool => t,
            _ => continue,
        };

        // Collect file paths owned by this claim (file: scope entries).
        let owned_files: Vec<String> = claim
            .scope
            .iter()
            .filter(|s| s.starts_with("file:"))
            .map(|s| s.strip_prefix("file:").unwrap_or(s).to_string())
            .collect();

        let mut affected_files: Vec<String> = Vec::new();
        let mut affecting_symbols: Vec<String> = Vec::new();

        for owned_rel in &owned_files {
            let abs = repo_root.join(owned_rel);
            let refs = file_references_symbols(&abs, &changed_symbols);
            if !refs.is_empty() {
                affected_files.push(owned_rel.clone());
                affecting_symbols.extend(refs);
            }
        }

        affecting_symbols.sort();
        affecting_symbols.dedup();

        if affected_files.is_empty() {
            continue;
        }

        let symbol_list = affecting_symbols.join(", ");
        let file_list = affected_files.join(", ");
        let changed_file_list = changed_files.join(", ");

        let fact = Fact {
            from_session_id: None,
            principal_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("ripple"),
            seq: 0,
            thread_id: new_id("ripple"),
            kind: FactKind::Risk,
            tool: Some(calling_tool.to_string()),
            role: None,
            subject: format!("ripple-alert: {owner} may be affected by changes to [{symbol_list}]"),
            scope: Vec::new(),
            created_at: now_string(),
            summary: Some(format!(
                "ripple-alert: {calling_tool} changed pub symbols [{symbol_list}] in [{changed_file_list}]; {owner}'s claimed files [{file_list}] reference these symbols — advisory only, not blocked"
            )),
            evidence: {
                let mut ev = Vec::new();
                for sym in &affecting_symbols {
                    ev.push(format!("changed_symbol:{sym}"));
                }
                for f in &affected_files {
                    ev.push(format!("affected_file:{f}"));
                }
                ev.push(format!("affected_tool:{owner}"));
                ev
            },
            target: Some(owner.to_string()),
            ref_id: Some(claim.event_id.clone()),
            status: None,
            severity: Some("warn".to_string()),
            uri: None,
            session: None,
        };
        facts.push(fact);
    }

    facts
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("rally-rip-{label}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn extract_pub_fn_names_finds_pub_and_pub_crate() {
        let dir = tmp_dir("extract-fns");
        let src = r#"
pub fn alpha() {}
pub(crate) fn beta() {}
pub(super) fn gamma() {}
fn private() {}
pub fn delta_underscore() {}
"#;
        fs::write(dir.join("f.rs"), src).unwrap();
        let names = extract_pub_fn_names(&dir.join("f.rs"));
        assert!(names.contains(&"alpha".to_string()));
        assert!(names.contains(&"beta".to_string()));
        assert!(names.contains(&"gamma".to_string()));
        assert!(names.contains(&"delta_underscore".to_string()));
        assert!(!names.contains(&"private".to_string()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn extract_pub_fn_names_returns_empty_for_missing_file() {
        let dir = tmp_dir("extract-missing");
        let result = extract_pub_fn_names(&dir.join("nonexistent.rs"));
        assert!(result.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_references_symbols_finds_usage() {
        let dir = tmp_dir("refs-finds");
        let src = r#"
use crate::alpha;
let x = beta(1, 2);
"#;
        fs::write(dir.join("user.rs"), src).unwrap();
        let refs = file_references_symbols(
            &dir.join("user.rs"),
            &["alpha".to_string(), "beta".to_string(), "gamma".to_string()],
        );
        assert!(refs.contains(&"alpha".to_string()));
        assert!(refs.contains(&"beta".to_string()));
        assert!(!refs.contains(&"gamma".to_string()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_ripple_alerts_fires_on_pub_sig_change_affecting_another_claim() {
        use crate::store::{Fact as StoreFact, FactKind as StoreFk, RoomSnapshot};

        let dir = tmp_dir("ripple-alert");

        // changed file: defines pub fn my_func
        let changed_src = r#"pub fn my_func() {}"#;
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/lib.rs"), changed_src).unwrap();

        // peer file: references my_func
        fs::create_dir_all(dir.join("other")).unwrap();
        fs::write(dir.join("other/mod.rs"), "let x = my_func();").unwrap();

        // Build a minimal snapshot with one peer claim owning other/mod.rs
        let peer_claim = StoreFact {
            from_session_id: None,
            principal_id: None,
            schema: "agent-rally.fact.v1".to_string(),
            event_id: "claim-peer-001".to_string(),
            seq: 1,
            thread_id: "t1".to_string(),
            kind: StoreFk::Claim,
            tool: Some("peer-tool".to_string()),
            role: None,
            subject: "peer claim".to_string(),
            scope: vec!["file:other/mod.rs".to_string()],
            created_at: "2026-01-01T00:00:00Z".to_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };

        let snapshot = RoomSnapshot {
            active_claims: vec![peer_claim],
            ..Default::default()
        };

        let changed_files = vec!["src/lib.rs".to_string()];
        let alerts = build_ripple_alerts(&changed_files, &dir, "my-tool", &snapshot);

        assert_eq!(
            alerts.len(),
            1,
            "one ripple-alert expected; got {}",
            alerts.len()
        );
        let alert = &alerts[0];
        assert!(
            alert.subject.contains("ripple-alert"),
            "subject: {}",
            alert.subject
        );
        assert!(
            alert.subject.contains("peer-tool"),
            "subject: {}",
            alert.subject
        );
        assert!(
            alert.evidence.iter().any(|e| e.contains("my_func")),
            "evidence: {:?}",
            alert.evidence
        );
        assert!(
            alert
                .evidence
                .iter()
                .any(|e| e == "affected_tool:peer-tool"),
            "evidence: {:?}",
            alert.evidence
        );
        assert_eq!(alert.severity.as_deref(), Some("warn"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_ripple_alerts_does_not_alert_when_no_peer_refs() {
        use crate::store::{Fact as StoreFact, FactKind as StoreFk, RoomSnapshot};

        let dir = tmp_dir("ripple-no-alert");

        // changed file: defines pub fn unique_xyz_func
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/lib.rs"), "pub fn unique_xyz_func() {}").unwrap();

        // peer file: does NOT reference unique_xyz_func
        fs::create_dir_all(dir.join("other")).unwrap();
        fs::write(dir.join("other/mod.rs"), "fn unrelated() {}").unwrap();

        let peer_claim = StoreFact {
            from_session_id: None,
            principal_id: None,
            schema: "agent-rally.fact.v1".to_string(),
            event_id: "claim-peer-002".to_string(),
            seq: 2,
            thread_id: "t2".to_string(),
            kind: StoreFk::Claim,
            tool: Some("peer-tool".to_string()),
            role: None,
            subject: "peer claim".to_string(),
            scope: vec!["file:other/mod.rs".to_string()],
            created_at: "2026-01-01T00:00:00Z".to_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };

        let snapshot = RoomSnapshot {
            active_claims: vec![peer_claim],
            ..Default::default()
        };

        let changed_files = vec!["src/lib.rs".to_string()];
        let alerts = build_ripple_alerts(&changed_files, &dir, "my-tool", &snapshot);
        assert!(
            alerts.is_empty(),
            "no alert expected when peer does not reference changed symbol"
        );

        fs::remove_dir_all(&dir).ok();
    }
}
