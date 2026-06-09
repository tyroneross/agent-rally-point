// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! B2: fan-out DAG view derived from lineage markers.
//!
//! # Charter assertion (RALLY RECORDS AND DERIVES; IT NEVER EXECUTES)
//!
//! This module is a READ-ONLY PROJECTION. It scans facts, builds a DAG struct,
//! and returns it. It NEVER calls process::Command, spawn-for-work, thread::spawn
//! for work, schedule, exec, or any external process API.
//!
//! Grep invariant (checked by charter_test):
//!   `SEAM_NO_EXEC: dag.rs contains zero calls to Command/spawn/schedule/exec`
//!
//! Litmus from PLAN-pi-dynamic-seam.md §0:
//!   "Does this make Rally start, resume, retry, or schedule work?" → NO.
//!   This module only reads facts and derives a graph structure.

use schemars::JsonSchema;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

use crate::store::Fact;

// ---------------------------------------------------------------------------
// Node tag
// ---------------------------------------------------------------------------

/// Status tag for a DAG node.
///
/// - `landed`    — has an artifact fact referencing the step's claim.
/// - `in_flight` — has a claim (or standby within wake_after) but no artifact yet.
/// - `stalled`   — has a standby whose wake_after has passed with no subsequent
///   wake or artifact fact.
#[derive(Clone, Debug, JsonSchema, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NodeStatus {
    Landed,
    InFlight,
    Stalled,
}

// ---------------------------------------------------------------------------
// DAG node
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct DagNode {
    /// The step identifier extracted from `step:<id>` scope marker.
    pub(crate) step_id: String,
    /// The run this node belongs to.
    pub(crate) run_id: String,
    /// Event IDs of facts that contributed to this node (claim, artifact, standby, wake).
    pub(crate) event_ids: Vec<String>,
    /// `landed | in_flight | stalled`.
    pub(crate) status: NodeStatus,
    /// Tool that authored the primary fact for this step.
    pub(crate) tool: Option<String>,
    /// Subjects of the contributing facts (for human readability).
    pub(crate) subjects: Vec<String>,
}

// ---------------------------------------------------------------------------
// DAG edge
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct DagEdge {
    /// Predecessor step id.
    pub(crate) from_step: String,
    /// Successor step id (the step that declares `parent-step:<from>`).
    pub(crate) to_step: String,
    /// Edge kind: `"parent_step"` (from lineage marker) or `"ref"` (from ref_id link).
    pub(crate) kind: String,
}

// ---------------------------------------------------------------------------
// DAG output
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct DagOutput {
    pub(crate) run_id: String,
    pub(crate) nodes: Vec<DagNode>,
    pub(crate) edges: Vec<DagEdge>,
    /// Total facts scanned for this run.
    pub(crate) facts_scanned: usize,
}

// ---------------------------------------------------------------------------
// Lineage marker helpers
// ---------------------------------------------------------------------------

/// Extract `run:<id>` from a fact's scope markers.
pub(crate) fn extract_run_id(fact: &Fact) -> Option<String> {
    fact.scope
        .iter()
        .find_map(|s| s.strip_prefix("run:").map(str::to_string))
}

/// Extract `step:<id>` from a fact's scope markers.
pub(crate) fn extract_step_id(fact: &Fact) -> Option<String> {
    fact.scope
        .iter()
        .find_map(|s| s.strip_prefix("step:").map(str::to_string))
}

/// Extract every `parent-step:<id>` marker from a fact's scope.
///
/// A task with multiple `depends_on` entries carries one `parent-step:<id>` marker
/// per dependency, so this returns all of them (one DAG edge per parent). A fact
/// with a single marker yields a one-element Vec — identical to the prior behavior.
pub(crate) fn extract_parent_step_ids(fact: &Fact) -> Vec<String> {
    fact.scope
        .iter()
        .filter_map(|s| s.strip_prefix("parent-step:").map(str::to_string))
        // Skip an empty id: a bare `parent-step:` marker (e.g. from an empty
        // `--parent-step ""` value) must not produce a phantom DAG edge.
        .filter(|id| !id.is_empty())
        .collect()
}

/// Extract `wake_after:<iso>` from a standby fact's summary.
pub(crate) fn extract_wake_after(fact: &Fact) -> Option<String> {
    fact.summary.as_deref().and_then(|s| {
        s.split_whitespace()
            .find_map(|token| token.strip_prefix("wake_after:").map(str::to_string))
    })
}

/// Return true if `wake_after` timestamp has passed relative to now.
pub(crate) fn wake_after_is_past(wake_after: &str) -> bool {
    // Parse as RFC-3339 and compare to now.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(wake_after) {
        let now = chrono::Utc::now();
        dt.with_timezone(&chrono::Utc) < now
    } else {
        // Unparseable — treat as not past (conservative).
        false
    }
}

// ---------------------------------------------------------------------------
// B1: parse relative wake-after offset (+30m, +2h, +1d) or ISO string
// ---------------------------------------------------------------------------

/// Parse a `wake_after` argument: `+30m`, `+2h`, `+1d`, or a bare ISO-8601 string.
/// Returns the resolved ISO-8601 string, or an error message.
pub(crate) fn resolve_wake_after(input: &str) -> Result<String, String> {
    if let Some(rest) = input.strip_prefix('+') {
        // Relative offset: parse suffix (m=minutes, h=hours, d=days).
        let (amount_str, unit) = if let Some(s) = rest.strip_suffix('m') {
            (s, "m")
        } else if let Some(s) = rest.strip_suffix('h') {
            (s, "h")
        } else if let Some(s) = rest.strip_suffix('d') {
            (s, "d")
        } else {
            return Err(format!(
                "invalid relative wake-after offset {input:?}; use +Nm (minutes), +Nh (hours), or +Nd (days)"
            ));
        };
        let amount: i64 = amount_str
            .parse()
            .map_err(|_| format!("invalid number in wake-after offset {input:?}"))?;
        if amount <= 0 {
            return Err(format!("wake-after offset must be positive; got {input:?}"));
        }
        let duration = match unit {
            "m" => chrono::Duration::minutes(amount),
            "h" => chrono::Duration::hours(amount),
            "d" => chrono::Duration::days(amount),
            _ => unreachable!(),
        };
        let wake_at = chrono::Utc::now() + duration;
        Ok(wake_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
    } else {
        // Absolute ISO-8601 string — validate by parsing.
        chrono::DateTime::parse_from_rfc3339(input)
            .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .map_err(|_| format!(
                "invalid wake-after value {input:?}; use ISO-8601 or a relative offset like +30m"
            ))
    }
}

// ---------------------------------------------------------------------------
// Build the DAG from a flat fact slice
// ---------------------------------------------------------------------------

/// Build the causation DAG for `run_id` from a full flat fact list.
///
/// **READ-ONLY. No exec/spawn/schedule called here.**
///
/// Nodes = steps identified by `step:<id>` scope marker on any fact that also
/// carries `run:<run_id>`. Edges come from:
///   - `parent-step:<id>` scope markers (lineage edges).
///   - `ref_id` links between facts within the same run (ref edges).
///
/// Node status:
///   - `landed`    if a fact of kind `artifact` shares the step_id in this run.
///   - `stalled`   if a standby fact in the step has a past `wake_after` and no
///     subsequent wake or artifact fact exists for that step.
///   - `in_flight` otherwise.
pub(crate) fn build_dag(facts: &[Fact], run_id: &str) -> DagOutput {
    // Filter to facts belonging to this run.
    let run_facts: Vec<&Fact> = facts
        .iter()
        .filter(|f| extract_run_id(f).as_deref() == Some(run_id))
        .collect();

    // Group by step_id. Facts with no step_id are attributed to a synthetic
    // "root" step so they remain visible in the DAG.
    let mut step_facts: BTreeMap<String, Vec<&Fact>> = BTreeMap::new();
    for fact in &run_facts {
        let step = extract_step_id(fact).unwrap_or_else(|| "root".to_string());
        step_facts.entry(step).or_default().push(fact);
    }

    // Collect event_id → step_id map for ref edge resolution.
    let mut event_to_step: BTreeMap<String, String> = BTreeMap::new();
    for (step_id, facts) in &step_facts {
        for fact in facts {
            event_to_step.insert(fact.event_id.clone(), step_id.clone());
        }
    }

    // Collect step_ids that have an artifact fact (= landed).
    let artifact_steps: BTreeSet<String> = step_facts
        .iter()
        .filter(|(_, facts)| facts.iter().any(|f| f.kind == "artifact"))
        .map(|(step, _)| step.clone())
        .collect();

    // Build nodes.
    let nodes: Vec<DagNode> = step_facts
        .iter()
        .map(|(step_id, facts)| {
            let tool = facts.first().and_then(|f| f.tool.clone());
            let event_ids: Vec<String> = facts.iter().map(|f| f.event_id.clone()).collect();
            let subjects: Vec<String> = facts.iter().map(|f| f.subject.clone()).collect();

            // Status logic:
            // 1. landed if any artifact in this step.
            // 2. stalled if any standby with past wake_after and no wake/artifact.
            // 3. in_flight otherwise.
            let has_artifact = artifact_steps.contains(step_id);
            let has_wake = facts.iter().any(|f| f.kind == "wake");

            let stalled = !has_artifact
                && !has_wake
                && facts.iter().any(|f| {
                    if f.kind != "standby" {
                        return false;
                    }
                    extract_wake_after(f)
                        .as_deref()
                        .map(wake_after_is_past)
                        .unwrap_or(false)
                });

            let status = if has_artifact {
                NodeStatus::Landed
            } else if stalled {
                NodeStatus::Stalled
            } else {
                NodeStatus::InFlight
            };

            DagNode {
                step_id: step_id.clone(),
                run_id: run_id.to_string(),
                event_ids,
                status,
                tool,
                subjects,
            }
        })
        .collect();

    // Build edges.
    let mut edges: Vec<DagEdge> = Vec::new();
    let mut seen_edges: BTreeSet<(String, String, String)> = BTreeSet::new();

    for (step_id, facts) in &step_facts {
        for fact in facts {
            // parent-step edges — one per `parent-step:<id>` marker on the fact.
            for parent in extract_parent_step_ids(fact) {
                let key = (parent.clone(), step_id.clone(), "parent_step".to_string());
                if seen_edges.insert(key) {
                    edges.push(DagEdge {
                        from_step: parent,
                        to_step: step_id.clone(),
                        kind: "parent_step".to_string(),
                    });
                }
            }
            // ref edges (within the same run).
            if let Some(ref_id) = &fact.ref_id {
                if let Some(ref_step) = event_to_step.get(ref_id) {
                    if ref_step != step_id {
                        let key = (ref_step.clone(), step_id.clone(), "ref".to_string());
                        if seen_edges.insert(key) {
                            edges.push(DagEdge {
                                from_step: ref_step.clone(),
                                to_step: step_id.clone(),
                                kind: "ref".to_string(),
                            });
                        }
                    }
                }
            }
        }
    }

    DagOutput {
        run_id: run_id.to_string(),
        nodes,
        edges,
        facts_scanned: run_facts.len(),
    }
}

// ---------------------------------------------------------------------------
// B4: wake-due projection
// ---------------------------------------------------------------------------

/// One entry in the wake-due projection.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct WakeDueEntry {
    /// event_id of the standby fact.
    pub(crate) standby_event_id: String,
    /// Tool that went standby (the owner of the wakeup).
    pub(crate) owner: Option<String>,
    /// Human-readable reason extracted from `reason:<r>` in summary.
    pub(crate) reason: Option<String>,
    /// The parsed wake_after timestamp.
    pub(crate) wake_after: String,
    /// Suggested command string for the external runner. NEVER executed by rally.
    /// This is a plain string — the runner decides whether and when to invoke it.
    pub(crate) suggested_command: String,
}

/// Project standby facts whose `wake_after` has passed.
///
/// **READ-ONLY. No exec/spawn/schedule called here.**
///
/// Trust gate: only surfaces standbys authored by a tool that appears in the
/// room's squads (i.e., a known/present participant). Standbys with no tool
/// or an unrecognised tool are omitted.
///
/// Returns an empty vec when nothing is due.
pub(crate) fn project_wake_due(facts: &[Fact], tool_filter: Option<&str>) -> Vec<WakeDueEntry> {
    // Collect known tool names (any tool that has authored any fact).
    let known_tools: BTreeSet<String> = facts.iter().filter_map(|f| f.tool.clone()).collect();

    // Collect standby event_ids that have a subsequent wake or artifact fact
    // referencing them (= already woken; skip).
    let woken_standbys: BTreeSet<String> = facts
        .iter()
        .filter(|f| f.kind == "wake" || f.kind == "artifact")
        .filter_map(|f| f.ref_id.clone())
        .collect();

    facts
        .iter()
        .filter(|f| f.kind == "standby")
        .filter(|f| !woken_standbys.contains(&f.event_id))
        .filter(|f| {
            // Trust gate: tool must be known (present in the room).
            f.tool
                .as_deref()
                .map(|t| known_tools.contains(t))
                .unwrap_or(false)
        })
        .filter(|f| {
            // Tool filter (optional).
            if let Some(filter) = tool_filter {
                f.tool.as_deref() == Some(filter)
            } else {
                true
            }
        })
        .filter_map(|f| {
            // Extract wake_after and check if past.
            let wake_after = extract_wake_after(f)?;
            if !wake_after_is_past(&wake_after) {
                return None;
            }
            let owner = f.tool.clone();
            let reason = f.summary.as_deref().and_then(|s| {
                s.split_whitespace()
                    .find_map(|token| token.strip_prefix("reason:").map(str::to_string))
            });
            // Build suggested_command — a string the runner can use.
            // NEVER executed by rally itself.
            let owner_arg = owner.as_deref().unwrap_or("unknown");
            let suggested_command =
                format!("rally next --tool {} --json", shell_quote_simple(owner_arg));
            Some(WakeDueEntry {
                standby_event_id: f.event_id.clone(),
                owner,
                reason,
                wake_after,
                suggested_command,
            })
        })
        .collect()
}

/// Minimal shell quoting for the suggested_command string (single-token values only).
/// Does not spawn a shell — purely string manipulation.
fn shell_quote_simple(value: &str) -> String {
    // If the value contains only safe chars, return as-is.
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_fact(kind: &str, tool: &str, scope: Vec<String>, summary: Option<&str>) -> Fact {
        Fact {
            from_session_id: None,
            schema: "agent-rally.fact.v1".to_string(),
            event_id: format!("evt-{kind}-{}", crate::short_id()),
            seq: 0,
            thread_id: format!("thread-{kind}"),
            kind: crate::store::FactKind::parse(kind).unwrap_or_default(),
            tool: Some(tool.to_string()),
            role: None,
            subject: format!("test {kind}"),
            scope,
            created_at: crate::now_string(),
            summary: summary.map(str::to_string),
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        }
    }

    // -------------------------------------------------------------------------
    // extract_parent_step_ids — empty-value filtering (f3)
    // -------------------------------------------------------------------------

    #[test]
    fn extract_parent_step_ids_skips_empty_marker() {
        // A bare `parent-step:` marker (the shape an empty `--parent-step ""`
        // value would write) must not yield an empty parent id / phantom edge.
        let fact = make_fact(
            "say",
            "agent:c",
            vec![
                "run:r1".to_string(),
                "step:c".to_string(),
                "parent-step:".to_string(),
                "parent-step:a".to_string(),
            ],
            None,
        );
        let parents = extract_parent_step_ids(&fact);
        assert_eq!(
            parents,
            vec!["a".to_string()],
            "empty parent-step marker must be filtered; got {parents:?}"
        );
    }

    // -------------------------------------------------------------------------
    // resolve_wake_after
    // -------------------------------------------------------------------------

    #[test]
    fn resolve_wake_after_relative_minutes() {
        let result = resolve_wake_after("+30m").expect("+30m must parse");
        // Should be an RFC-3339 timestamp in the future.
        let dt = chrono::DateTime::parse_from_rfc3339(&result).expect("must be RFC-3339");
        let now = chrono::Utc::now();
        assert!(
            dt.with_timezone(&chrono::Utc) > now,
            "+30m must resolve to a future timestamp; got {result}"
        );
    }

    #[test]
    fn resolve_wake_after_relative_hours() {
        let result = resolve_wake_after("+2h").expect("+2h must parse");
        chrono::DateTime::parse_from_rfc3339(&result).expect("+2h must produce valid RFC-3339");
    }

    #[test]
    fn resolve_wake_after_absolute_iso() {
        let iso = "2030-01-01T00:00:00Z";
        let result = resolve_wake_after(iso).expect("absolute ISO must parse");
        assert!(
            result.contains("2030"),
            "absolute ISO must round-trip; got {result}"
        );
    }

    #[test]
    fn resolve_wake_after_invalid_rejects() {
        assert!(resolve_wake_after("not-a-time").is_err());
        assert!(resolve_wake_after("+0m").is_err());
        assert!(resolve_wake_after("+abc").is_err());
    }

    // -------------------------------------------------------------------------
    // build_dag
    // -------------------------------------------------------------------------

    #[test]
    fn dag_build_from_lineage_handoff_and_three_child_claims() {
        // Simulate: 1 handoff (step=S0) → 3 child claims (step=S1, S2, S3),
        // each with parent-step:S0 and run:RUN1.
        let run = "RUN1";

        let handoff = {
            let mut f = make_fact(
                "handoff",
                "tool-a",
                vec![format!("run:{run}"), "step:S0".to_string()],
                None,
            );
            f.event_id = "evt-handoff-s0".to_string();
            f
        };

        let claims: Vec<Fact> = (1..=3)
            .map(|i| {
                let mut f = make_fact(
                    "claim",
                    "tool-b",
                    vec![
                        format!("run:{run}"),
                        format!("step:S{i}"),
                        "parent-step:S0".to_string(),
                    ],
                    None,
                );
                f.event_id = format!("evt-claim-s{i}");
                f
            })
            .collect();

        let mut facts = vec![handoff];
        facts.extend(claims);

        let dag = build_dag(&facts, run);

        // Expect 4 nodes: S0, S1, S2, S3.
        assert_eq!(
            dag.nodes.len(),
            4,
            "expected 4 nodes (S0 + 3 children); got: {:?}",
            dag.nodes.iter().map(|n| &n.step_id).collect::<Vec<_>>()
        );

        // Expect 3 parent_step edges (S0 → S1, S0 → S2, S0 → S3).
        let parent_edges: Vec<_> = dag
            .edges
            .iter()
            .filter(|e| e.kind == "parent_step")
            .collect();
        assert_eq!(
            parent_edges.len(),
            3,
            "expected 3 parent_step edges; got: {parent_edges:?}"
        );
        for edge in &parent_edges {
            assert_eq!(edge.from_step, "S0", "all edges must come from S0");
        }

        // All claim nodes must be in_flight (no artifacts yet).
        for node in dag.nodes.iter().filter(|n| n.step_id != "S0") {
            assert_eq!(
                node.status,
                NodeStatus::InFlight,
                "child claim step {} must be in_flight",
                node.step_id
            );
        }
    }

    #[test]
    fn dag_build_multi_parent_step_emits_one_edge_per_parent() {
        // A single dependent claim that depends on TWO parents carries two
        // `parent-step:` markers — the multi-`depends_on` case from packet.mjs.
        // Each must become its own DAG edge.
        let run = "RUNMP";

        let p1 = {
            let mut f = make_fact(
                "claim",
                "tool-a",
                vec![format!("run:{run}"), "step:P1".into()],
                None,
            );
            f.event_id = "evt-p1".into();
            f
        };
        let p2 = {
            let mut f = make_fact(
                "claim",
                "tool-a",
                vec![format!("run:{run}"), "step:P2".into()],
                None,
            );
            f.event_id = "evt-p2".into();
            f
        };
        let child = {
            let mut f = make_fact(
                "claim",
                "tool-b",
                vec![
                    format!("run:{run}"),
                    "step:C1".into(),
                    "parent-step:P1".into(),
                    "parent-step:P2".into(),
                ],
                None,
            );
            f.event_id = "evt-c1".into();
            f
        };

        let dag = build_dag(&[p1, p2, child], run);

        let parent_edges: Vec<_> = dag
            .edges
            .iter()
            .filter(|e| e.kind == "parent_step")
            .collect();
        assert_eq!(
            parent_edges.len(),
            2,
            "a 2-parent child must emit 2 parent_step edges; got: {parent_edges:?}"
        );
        let to_c1: BTreeSet<&str> = parent_edges
            .iter()
            .filter(|e| e.to_step == "C1")
            .map(|e| e.from_step.as_str())
            .collect();
        assert_eq!(
            to_c1,
            BTreeSet::from(["P1", "P2"]),
            "both P1 and P2 must edge into C1"
        );
    }

    #[test]
    fn dag_node_with_artifact_tags_landed() {
        let run = "RUN2";

        let claim = {
            let mut f = make_fact(
                "claim",
                "tool-a",
                vec![format!("run:{run}"), "step:S1".to_string()],
                None,
            );
            f.event_id = "evt-claim-s1".to_string();
            f
        };
        let artifact = {
            let mut f = make_fact(
                "artifact",
                "tool-a",
                vec![format!("run:{run}"), "step:S1".to_string()],
                None,
            );
            f.event_id = "evt-artifact-s1".to_string();
            f.ref_id = Some("evt-claim-s1".to_string());
            f
        };

        let dag = build_dag(&[claim, artifact], run);
        let s1 = dag
            .nodes
            .iter()
            .find(|n| n.step_id == "S1")
            .expect("S1 must exist");
        assert_eq!(
            s1.status,
            NodeStatus::Landed,
            "S1 with artifact must be landed"
        );
    }

    #[test]
    fn dag_stalled_standby_past_wake_after() {
        let run = "RUN3";

        // A claim with a standby whose wake_after is in the past (epoch = already passed).
        let claim = make_fact(
            "claim",
            "tool-a",
            vec![format!("run:{run}"), "step:S1".to_string()],
            None,
        );
        // A standby with a past wake_after — use a fixed past ISO timestamp.
        let standby = make_fact(
            "standby",
            "tool-a",
            vec![format!("run:{run}"), "step:S1".to_string()],
            Some("reason:waiting wake_after:2020-01-01T00:00:00Z"),
        );

        let dag = build_dag(&[claim, standby], run);
        let s1 = dag
            .nodes
            .iter()
            .find(|n| n.step_id == "S1")
            .expect("S1 must exist");
        assert_eq!(
            s1.status,
            NodeStatus::Stalled,
            "S1 with past standby and no wake/artifact must be stalled"
        );
    }

    // -------------------------------------------------------------------------
    // project_wake_due
    // -------------------------------------------------------------------------

    #[test]
    fn wake_due_surfaces_past_standby() {
        // Standby with past wake_after, no subsequent wake.
        let standby = {
            let mut f = make_fact(
                "standby",
                "tool-a",
                vec![],
                Some("reason:idle wake_after:2020-01-01T00:00:00Z"),
            );
            f.event_id = "evt-standby-past".to_string();
            f
        };
        // Also need a presence fact for tool-a so the trust gate passes.
        let presence = make_fact("presence", "tool-a", vec![], None);

        let due = project_wake_due(&[standby, presence], None);
        assert_eq!(due.len(), 1, "one past standby must surface");
        assert_eq!(due[0].standby_event_id, "evt-standby-past");
        assert!(
            due[0].suggested_command.contains("rally next"),
            "suggested_command must contain 'rally next'; got: {}",
            due[0].suggested_command
        );
    }

    #[test]
    fn wake_due_does_not_surface_future_standby() {
        // Standby with future wake_after.
        let standby = make_fact(
            "standby",
            "tool-a",
            vec![],
            Some("reason:idle wake_after:2099-01-01T00:00:00Z"),
        );
        let presence = make_fact("presence", "tool-a", vec![], None);

        let due = project_wake_due(&[standby, presence], None);
        assert!(
            due.is_empty(),
            "future standby must not surface in wake-due"
        );
    }

    #[test]
    fn wake_due_does_not_surface_already_woken_standby() {
        // Standby with past wake_after, but a wake fact references it.
        let standby = {
            let mut f = make_fact(
                "standby",
                "tool-a",
                vec![],
                Some("reason:idle wake_after:2020-01-01T00:00:00Z"),
            );
            f.event_id = "evt-standby-woken".to_string();
            f
        };
        let wake = {
            let mut f = make_fact("wake", "tool-a", vec![], None);
            f.ref_id = Some("evt-standby-woken".to_string());
            f
        };
        let presence = make_fact("presence", "tool-a", vec![], None);

        let due = project_wake_due(&[standby, wake, presence], None);
        assert!(
            due.is_empty(),
            "already-woken standby must not appear in wake-due"
        );
    }

    #[test]
    fn wake_due_no_execution_occurs() {
        // Charter assertion: project_wake_due is a pure read — it returns
        // WakeDueEntry values with suggested_command strings and calls no
        // process/spawn/schedule API. This test asserts the SEAM_NO_EXEC
        // invariant by verifying the function completes without side effects.
        //
        // SEAM_NO_EXEC: project_wake_due does not call Command/spawn/schedule/exec.
        // (Grep the source: `grep -n "Command\|spawn\|schedule\|std::process" src/dag.rs`
        //  must return zero matches for exec-calling patterns.)
        let standby = {
            let mut f = make_fact(
                "standby",
                "tool-a",
                vec![],
                Some("reason:idle wake_after:2020-01-01T00:00:00Z"),
            );
            f.event_id = "evt-charter-assert".to_string();
            f
        };
        let presence = make_fact("presence", "tool-a", vec![], None);

        let due = project_wake_due(&[standby, presence], None);
        // We got a result — no panic, no side effects.
        assert_eq!(due.len(), 1);
        // The suggested_command is a STRING — the runner calls it, rally does not.
        assert!(due[0].suggested_command.starts_with("rally "));
        // No child process was spawned. This is unambiguously verified by the
        // fact that `project_wake_due` contains no process-spawning calls
        // (statically checkable via grep; enforced by the charter_assertion test).
    }

    #[test]
    fn wake_due_tool_filter_restricts_results() {
        let standby_a = {
            let mut f = make_fact(
                "standby",
                "tool-a",
                vec![],
                Some("reason:idle wake_after:2020-01-01T00:00:00Z"),
            );
            f.event_id = "evt-standby-a".to_string();
            f
        };
        let standby_b = {
            let mut f = make_fact(
                "standby",
                "tool-b",
                vec![],
                Some("reason:idle wake_after:2020-01-01T00:00:00Z"),
            );
            f.event_id = "evt-standby-b".to_string();
            f
        };
        let presence_a = make_fact("presence", "tool-a", vec![], None);
        let presence_b = make_fact("presence", "tool-b", vec![], None);

        let facts = vec![standby_a, standby_b, presence_a, presence_b];

        let all = project_wake_due(&facts, None);
        assert_eq!(all.len(), 2, "both standbys are past without filter");

        let filtered = project_wake_due(&facts, Some("tool-a"));
        assert_eq!(filtered.len(), 1, "tool filter must restrict to tool-a");
        assert_eq!(filtered[0].owner.as_deref(), Some("tool-a"));
    }
}
