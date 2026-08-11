// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! External liveness observation for claim owners.
//!
//! Presence facts stamp where the agent was working and, when the host hook can
//! provide it, the long-lived host process pid. This module independently reads
//! that worktree and process table. A dead pid can therefore override a fresh
//! self-authored heartbeat; missing, unreadable, or cross-repository evidence
//! remains Unknown and cannot authorize automatic removal.

use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use crate::store::{Fact, FactKind};

const WORKTREE_PATH_PREFIX: &str = "worktree_path:";
const BRANCH_HEAD_PREFIX: &str = "branch_head_sha:";
const OBSERVER_PID_PREFIX: &str = "observer_pid:";

/// An observer verdict is deliberately stronger than the four self-reported
/// age signals. Live protects work; Stale can corroborate automatic reaping;
/// Unknown always fails open.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObservedLiveness {
    Live,
    Stale,
    Unknown,
}

impl ObservedLiveness {
    pub(crate) fn as_signal(self) -> Option<bool> {
        match self {
            Self::Live => Some(true),
            Self::Stale => Some(false),
            Self::Unknown => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Stale => "stale",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug)]
struct ObservationStamp {
    tool: String,
    session_key: String,
    worktree_path: PathBuf,
    recorded_head: Option<String>,
    observer_pid: Option<i32>,
    reported_at: DateTime<Utc>,
    seq: i64,
}

/// One probe pass indexed both by exact protocol session and by legacy tool.
/// Destructive callers must use [`Self::for_claim`], never the tool aggregate
/// directly: the aggregate exists only for claims that lack session identity.
#[derive(Clone, Debug, Default)]
pub(crate) struct ObservationIndex {
    by_session: BTreeMap<(String, String), ObservedLiveness>,
    by_tool: BTreeMap<String, ObservedLiveness>,
}

impl ObservationIndex {
    pub(crate) fn for_claim(
        &self,
        tool: Option<&str>,
        from_session_id: Option<&str>,
    ) -> ObservedLiveness {
        let Some(tool) = tool else {
            return ObservedLiveness::Unknown;
        };
        match from_session_id {
            Some(session_id) => self
                .by_session
                .get(&(tool.to_string(), session_id.to_string()))
                .copied()
                .unwrap_or(ObservedLiveness::Unknown),
            None => self
                .by_tool
                .get(tool)
                .copied()
                .unwrap_or(ObservedLiveness::Unknown),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct WorktreeProbe {
    readable: bool,
    head: Option<String>,
    dirty_count: Option<usize>,
    newest_tracked_mtime: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default)]
struct ProbeSample {
    worktree: WorktreeProbe,
    pid_alive: Option<bool>,
}

/// Read the current HEAD without spawning git. This stays on the heartbeat
/// path, where an extra subprocess per beat would widen the hook budget.
pub(crate) fn current_head_sha(worktree: &Path) -> Option<String> {
    let git_path = worktree.join(".git");
    let git_dir = if git_path.is_dir() {
        git_path
    } else {
        let raw = fs::read_to_string(&git_path).ok()?;
        let target = raw.trim().strip_prefix("gitdir:")?.trim();
        let target = Path::new(target);
        if target.is_absolute() {
            target.to_path_buf()
        } else {
            worktree.join(target)
        }
    };

    let common_dir = fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                git_dir.join(path)
            }
        })
        .and_then(|path| fs::canonicalize(path).ok())
        .unwrap_or_else(|| git_dir.clone());

    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(ref_name) = head.strip_prefix("ref:").map(str::trim) else {
        return valid_sha(head);
    };
    if let Ok(direct) = fs::read_to_string(common_dir.join(ref_name))
        && let Some(sha) = valid_sha(direct.trim())
    {
        return Some(sha);
    }
    let packed = fs::read_to_string(common_dir.join("packed-refs")).ok()?;
    packed.lines().find_map(|line| {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with('^') {
            return None;
        }
        let (sha, name) = line.split_once(' ')?;
        (name.trim() == ref_name).then(|| valid_sha(sha))?
    })
}

fn valid_sha(candidate: &str) -> Option<String> {
    let ok = matches!(candidate.len(), 40 | 64) && candidate.chars().all(|c| c.is_ascii_hexdigit());
    ok.then(|| candidate.to_ascii_lowercase())
}

fn evidence_value<'a>(fact: &'a Fact, prefix: &str) -> Option<&'a str> {
    fact.evidence
        .iter()
        .find_map(|item| item.strip_prefix(prefix))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn stamp_from_fact(fact: &Fact) -> Option<ObservationStamp> {
    if fact.kind != FactKind::Presence {
        return None;
    }
    let tool = fact.tool.clone()?;
    let worktree_path = PathBuf::from(evidence_value(fact, WORKTREE_PATH_PREFIX)?);
    let recorded_head = evidence_value(fact, BRANCH_HEAD_PREFIX).map(str::to_string);
    let observer_pid = evidence_value(fact, OBSERVER_PID_PREFIX)
        .and_then(|raw| raw.parse::<i32>().ok())
        .filter(|pid| *pid > 1);
    let reported_at = DateTime::parse_from_rfc3339(&fact.created_at)
        .ok()?
        .with_timezone(&Utc);
    let session_key = fact
        .from_session_id
        .clone()
        .unwrap_or_else(|| format!("{tool}:{}:{observer_pid:?}", worktree_path.display()));
    Some(ObservationStamp {
        tool,
        session_key,
        worktree_path,
        recorded_head,
        observer_pid,
        reported_at,
        seq: fact.seq,
    })
}

fn git_common_dir(worktree: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["-C", worktree.to_str()?, "rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    let path = PathBuf::from(raw.trim());
    let resolved = if path.is_absolute() {
        path
    } else {
        worktree.join(path)
    };
    fs::canonicalize(resolved).ok()
}

fn dirty_count(worktree: &Path) -> Option<usize> {
    let output = Command::new("git")
        .args([
            "-C",
            worktree.to_str()?,
            "status",
            "--porcelain=v1",
            "--untracked-files=normal",
        ])
        .output()
        .ok()?;
    output.status.success().then(|| {
        output
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .count()
    })
}

fn newest_tracked_mtime(worktree: &Path) -> Option<Option<DateTime<Utc>>> {
    let output = Command::new("git")
        .args(["-C", worktree.to_str()?, "ls-files", "-z"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let newest = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|raw| !raw.is_empty())
        .filter_map(|raw| std::str::from_utf8(raw).ok())
        .filter_map(|relative| fs::metadata(worktree.join(relative)).ok())
        .filter_map(|metadata| metadata.modified().ok())
        .max();
    Some(newest.map(system_time_to_utc))
}

fn system_time_to_utc(time: SystemTime) -> DateTime<Utc> {
    DateTime::<Utc>::from(time)
}

fn probe_worktree(worktree: &Path, expected_common_dir: &Path) -> WorktreeProbe {
    if git_common_dir(worktree).as_deref() != Some(expected_common_dir) {
        return WorktreeProbe::default();
    }
    let head = current_head_sha(worktree);
    let dirty_count = dirty_count(worktree);
    let newest_tracked_mtime = newest_tracked_mtime(worktree);
    WorktreeProbe {
        readable: head.is_some() && dirty_count.is_some() && newest_tracked_mtime.is_some(),
        head,
        dirty_count,
        newest_tracked_mtime: newest_tracked_mtime.flatten(),
    }
}

fn process_alive(pid: i32) -> Option<bool> {
    let output = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .ok()?;
    if output.status.success() {
        return Some(true);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    stderr.contains("no such process").then_some(false)
}

fn grade_observation(stamp: &ObservationStamp, sample: &ProbeSample) -> ObservedLiveness {
    if sample.pid_alive == Some(true) {
        return ObservedLiveness::Live;
    }
    // An unreadable or cross-repository worktree can never demote. Requiring
    // the observed HEAD also guards against acting on a bare pid assertion.
    if !sample.worktree.readable {
        return ObservedLiveness::Unknown;
    }
    if sample.pid_alive == Some(false) {
        return match (&stamp.recorded_head, &sample.worktree.head) {
            (Some(recorded), Some(current)) if recorded == current => ObservedLiveness::Stale,
            // Something moved after the stamped beat. It may be a sibling
            // session in the same worktree, so refuse destructive certainty.
            _ => ObservedLiveness::Unknown,
        };
    }
    if matches!(
        (&stamp.recorded_head, &sample.worktree.head),
        (Some(recorded), Some(current)) if recorded != current
    ) || sample
        .worktree
        .newest_tracked_mtime
        .is_some_and(|mtime| mtime > stamp.reported_at)
    {
        return ObservedLiveness::Live;
    }
    // Dirty count is intentionally observed but not treated as proof of a live
    // process: crash residue is dirty too. Without a process verdict or newer
    // filesystem activity, absence of progress is Unknown rather than Stale.
    let _dirty_count = sample.worktree.dirty_count;
    ObservedLiveness::Unknown
}

fn aggregate(verdicts: impl IntoIterator<Item = ObservedLiveness>) -> ObservedLiveness {
    let mut saw_stale = false;
    let mut saw_unknown = false;
    for verdict in verdicts {
        match verdict {
            ObservedLiveness::Live => return ObservedLiveness::Live,
            ObservedLiveness::Stale => saw_stale = true,
            ObservedLiveness::Unknown => saw_unknown = true,
        }
    }
    if saw_unknown {
        ObservedLiveness::Unknown
    } else if saw_stale {
        ObservedLiveness::Stale
    } else {
        ObservedLiveness::Unknown
    }
}

/// Probe each latest stamped session once and retain both exact-session and
/// tool-aggregate views. Sessionful claims use only the exact-session view;
/// the tool aggregate is the explicit compatibility fallback for legacy,
/// sessionless claims.
pub(crate) fn observe_sessions(room_repo_root: &Path, facts: &[Fact]) -> ObservationIndex {
    let Some(expected_common_dir) = git_common_dir(room_repo_root) else {
        return ObservationIndex::default();
    };
    let mut latest: BTreeMap<(String, String), ObservationStamp> = BTreeMap::new();
    for stamp in facts.iter().filter_map(stamp_from_fact) {
        let key = (stamp.tool.clone(), stamp.session_key.clone());
        if latest
            .get(&key)
            .is_none_or(|existing| stamp.seq > existing.seq)
        {
            latest.insert(key, stamp);
        }
    }

    let mut worktrees: BTreeMap<PathBuf, WorktreeProbe> = BTreeMap::new();
    let mut by_session = BTreeMap::new();
    let mut per_tool: BTreeMap<String, Vec<ObservedLiveness>> = BTreeMap::new();
    for stamp in latest.into_values() {
        let worktree = worktrees
            .entry(stamp.worktree_path.clone())
            .or_insert_with(|| probe_worktree(&stamp.worktree_path, &expected_common_dir));
        let sample = ProbeSample {
            worktree: worktree.clone(),
            pid_alive: stamp.observer_pid.and_then(process_alive),
        };
        let verdict = grade_observation(&stamp, &sample);
        by_session.insert((stamp.tool.clone(), stamp.session_key.clone()), verdict);
        per_tool
            .entry(stamp.tool.clone())
            .or_default()
            .push(verdict);
    }
    let by_tool = per_tool
        .into_iter()
        .map(|(tool, verdicts)| (tool, aggregate(verdicts)))
        .collect();
    ObservationIndex {
        by_session,
        by_tool,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_root(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rally-observed-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn stamp() -> ObservationStamp {
        ObservationStamp {
            tool: "agent".to_string(),
            session_key: "session".to_string(),
            worktree_path: PathBuf::from("/tmp/worktree"),
            recorded_head: Some("a".repeat(40)),
            observer_pid: Some(42),
            reported_at: Utc::now(),
            seq: 1,
        }
    }

    fn readable_probe(head: &str, pid_alive: Option<bool>) -> ProbeSample {
        ProbeSample {
            worktree: WorktreeProbe {
                readable: true,
                head: Some(head.to_string()),
                dirty_count: Some(0),
                newest_tracked_mtime: None,
            },
            pid_alive,
        }
    }

    #[test]
    fn gone_process_and_unchanged_head_is_observed_stale() {
        let stamp = stamp();
        let sample = readable_probe(stamp.recorded_head.as_deref().unwrap(), Some(false));
        assert_eq!(grade_observation(&stamp, &sample), ObservedLiveness::Stale);
    }

    #[test]
    fn unreadable_worktree_and_moved_head_fail_open() {
        let stamp = stamp();
        assert_eq!(
            grade_observation(
                &stamp,
                &ProbeSample {
                    pid_alive: Some(false),
                    ..Default::default()
                }
            ),
            ObservedLiveness::Unknown
        );
        assert_eq!(
            grade_observation(&stamp, &readable_probe(&"b".repeat(40), Some(false))),
            ObservedLiveness::Unknown
        );
    }

    #[test]
    fn live_sibling_wins_tool_aggregation() {
        assert_eq!(
            aggregate([
                ObservedLiveness::Stale,
                ObservedLiveness::Unknown,
                ObservedLiveness::Live,
            ]),
            ObservedLiveness::Live
        );
        assert_eq!(
            aggregate([ObservedLiveness::Stale, ObservedLiveness::Stale]),
            ObservedLiveness::Stale
        );
    }

    #[test]
    fn claim_observation_is_exact_session_with_legacy_tool_fallback() {
        let index = ObservationIndex {
            by_session: BTreeMap::from([
                (
                    ("agent".to_string(), "dead-owner".to_string()),
                    ObservedLiveness::Stale,
                ),
                (
                    ("agent".to_string(), "live-sibling".to_string()),
                    ObservedLiveness::Live,
                ),
            ]),
            by_tool: BTreeMap::from([("agent".to_string(), ObservedLiveness::Live)]),
        };

        assert_eq!(
            index.for_claim(Some("agent"), Some("dead-owner")),
            ObservedLiveness::Stale,
            "a live sibling must not protect the dead claim owner"
        );
        assert_eq!(
            index.for_claim(Some("agent"), Some("missing-session")),
            ObservedLiveness::Unknown,
            "missing exact-session evidence must fail closed"
        );
        assert_eq!(
            index.for_claim(Some("agent"), None),
            ObservedLiveness::Live,
            "only a sessionless legacy claim may use the tool aggregate"
        );
    }

    #[test]
    fn current_head_sha_resolves_linked_worktree_common_refs() {
        let root = unique_root("linked-head");
        let linked = root.with_extension("linked");
        fs::create_dir_all(&root).unwrap();
        crate::test_git_fixture::fixture_git(&root, &["init"]);
        fs::write(root.join("tracked.txt"), "tracked\n").unwrap();
        crate::test_git_fixture::fixture_git(&root, &["add", "tracked.txt"]);
        crate::test_git_fixture::fixture_git(&root, &["commit", "-m", "fixture"]);
        crate::test_git_fixture::fixture_git(
            &root,
            &[
                "worktree",
                "add",
                linked.to_str().unwrap(),
                "-b",
                "observer-linked",
            ],
        );
        let expected = crate::test_git_fixture::fixture_git(&linked, &["rev-parse", "HEAD"]);

        assert_eq!(current_head_sha(&linked).as_deref(), Some(expected.trim()));

        fs::remove_dir_all(&linked).ok();
        fs::remove_dir_all(&root).ok();
    }
}
