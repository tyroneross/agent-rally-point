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
    /// Externally OBSERVED dead: a stamped `observer_pid:` whose probe reported
    /// the process gone. This is direct evidence about a specific process.
    Stale,
    /// INFERRED stale: no `observer_pid:` was ever stamped, and the session went
    /// quiet past the takeover bar with a provably unmoved, quiescent worktree.
    ///
    /// Carries the same destructive weight as [`Self::Stale`] for a pass a human
    /// invoked, and deliberately LESS for the automatic one. The difference is
    /// not confidence, it is consent: a heuristic may inform an operator who
    /// chose to run a takeover sweep, but it must not authorize a background
    /// release performed on an unrelated peer's `rally enter`. Every input the
    /// inference reads — worktree quiescence, unmoved HEAD, ledger silence — is
    /// blind to a live agent whose work leaves no trace in those three places.
    StaleUnobserved,
    Unknown,
}

impl ObservedLiveness {
    pub(crate) fn as_signal(self) -> Option<bool> {
        match self {
            Self::Live => Some(true),
            Self::Stale | Self::StaleUnobserved => Some(false),
            Self::Unknown => None,
        }
    }

    /// Either flavour of stale. Use for the human-invoked reap and for any
    /// re-check of a decision that pass already made.
    pub(crate) fn is_stale(self) -> bool {
        matches!(self, Self::Stale | Self::StaleUnobserved)
    }

    /// May this verdict corroborate an AUTOMATIC reap? Only direct observation
    /// qualifies. See [`Self::StaleUnobserved`] for why.
    pub(crate) fn authorizes_automatic_reap(self) -> bool {
        matches!(self, Self::Stale)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Stale => "stale",
            Self::StaleUnobserved => "stale-unobserved",
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
    by_session_reported_at: BTreeMap<(String, String), DateTime<Utc>>,
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

    /// A live process observation predating the claim's effective lease expiry
    /// cannot prove that the owner renewed work past that boundary. This keeps
    /// an ancient/recycled PID from vetoing writer-stamped expiry forever.
    pub(crate) fn for_claim_since(
        &self,
        tool: Option<&str>,
        from_session_id: Option<&str>,
        not_before: Option<DateTime<Utc>>,
    ) -> ObservedLiveness {
        let verdict = self.for_claim(tool, from_session_id);
        let (Some(tool), Some(session_id), Some(not_before)) = (tool, from_session_id, not_before)
        else {
            return verdict;
        };
        if verdict == ObservedLiveness::Live
            && self
                .by_session_reported_at
                .get(&(tool.to_string(), session_id.to_string()))
                .is_some_and(|reported_at| *reported_at < not_before)
        {
            ObservedLiveness::Unknown
        } else {
            verdict
        }
    }
}

#[derive(Clone, Debug, Default)]
struct WorktreeProbe {
    readable: bool,
    head: Option<String>,
    dirty_count: Option<usize>,
    newest_worktree_mtime: Option<DateTime<Utc>>,
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

fn newest_worktree_mtime(worktree: &Path) -> Option<Option<DateTime<Utc>>> {
    // `--cached --others --exclude-standard` covers tracked AND untracked
    // (non-ignored) files: an agent authoring only brand-new files for hours
    // must still read as filesystem activity, or the takeover-bar branch in
    // `grade_observation` would count real work as silence (fix-critique
    // finding, task 2914419f). Ignored files stay out — build artifacts churn
    // without proving an agent.
    let output = Command::new("git")
        .args([
            "-C",
            worktree.to_str()?,
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
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
    let newest_worktree_mtime = newest_worktree_mtime(worktree);
    WorktreeProbe {
        readable: head.is_some() && dirty_count.is_some() && newest_worktree_mtime.is_some(),
        head,
        dirty_count,
        newest_worktree_mtime: newest_worktree_mtime.flatten(),
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

fn grade_observation(
    stamp: &ObservationStamp,
    sample: &ProbeSample,
    now: DateTime<Utc>,
    last_authored_at: Option<DateTime<Utc>>,
) -> ObservedLiveness {
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
        .newest_worktree_mtime
        .is_some_and(|mtime| mtime > stamp.reported_at)
    {
        return ObservedLiveness::Live;
    }
    // Dirty count is intentionally observed but not treated as proof of a live
    // process: crash residue is dirty too. Without a process verdict or newer
    // filesystem activity, absence of progress is Unknown rather than Stale.
    let _dirty_count = sample.worktree.dirty_count;

    // Fail-closed after the takeover bar for the NEVER-OBSERVED session. A
    // stamp with no `observer_pid:` evidence (RALLY_OBSERVER_PID unset — e.g.
    // presence written by the CLI outside the shipped hook) used to grade
    // Unknown forever, silently exempting the session from reaping no matter
    // how long it had been silent.
    //
    // The silence clock CANNOT be the stamp's own age: presence facts are
    // written ONCE per session (`ensure_presence_tiered_for_session` early-
    // returns when one exists), and outside the shipped hook nothing refreshes
    // them, so `reported_at` measures session age, not silence. An agent
    // renewing its claim lease every 30 minutes would still age past any bar.
    // The clock is therefore the session's newest AUTHORED non-system fact
    // (`last_authored_at`, exact session preferred, tool aggregate fallback) —
    // the same last-seen clock `takeover_eligible_owners` reads — with the
    // stamp as its floor.
    //
    // Stale requires ALL of: no observer pid ever stamped, a readable same-
    // repo worktree, a HEAD that provably has not moved since the stamp, no
    // tracked-or-untracked file written after the stamped beat (checked
    // above), and authored-fact silence past `TAKEOVER_STALE_SECS` — the
    // identical 2h bar the destructive takeover release applies. A stamp that
    // HAS an observer pid whose probe returned no verdict stays Unknown: a
    // probe failure is not evidence of absence. Accepted residual risk: a
    // single tool call longer than 2h that writes no ledger fact and touches
    // no non-ignored file reads as silence — the same residual the takeover
    // release already accepts ("2h ≫ any plausible work-pause").
    let head_provably_unmoved = matches!(
        (&stamp.recorded_head, &sample.worktree.head),
        (Some(recorded), Some(current)) if recorded == current
    );
    let silence_anchor = last_authored_at.map_or(stamp.reported_at, |authored| {
        authored.max(stamp.reported_at)
    });
    if stamp.observer_pid.is_none()
        && head_provably_unmoved
        && now.signed_duration_since(silence_anchor).num_seconds()
            > crate::store::TAKEOVER_STALE_SECS
    {
        return ObservedLiveness::StaleUnobserved;
    }
    ObservedLiveness::Unknown
}

fn aggregate(verdicts: impl IntoIterator<Item = ObservedLiveness>) -> ObservedLiveness {
    let mut saw_stale = false;
    let mut saw_stale_unobserved = false;
    let mut saw_unknown = false;
    for verdict in verdicts {
        match verdict {
            ObservedLiveness::Live => return ObservedLiveness::Live,
            ObservedLiveness::Stale => saw_stale = true,
            ObservedLiveness::StaleUnobserved => saw_stale_unobserved = true,
            ObservedLiveness::Unknown => saw_unknown = true,
        }
    }
    if saw_unknown {
        ObservedLiveness::Unknown
    } else if saw_stale {
        // Direct observation outranks inference when one tool has both.
        ObservedLiveness::Stale
    } else if saw_stale_unobserved {
        ObservedLiveness::StaleUnobserved
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

    let now = Utc::now();
    // Newest authored non-system fact per exact session and per tool: the
    // silence clock for the takeover-bar branch in `grade_observation`.
    // Presence stamps alone cannot serve — they are write-once per session
    // outside the shipped hook, so any ledger write (claim renewal, artifact,
    // status) must reset the clock, matching `takeover_eligible_owners`.
    let mut authored_by_session: BTreeMap<(String, String), DateTime<Utc>> = BTreeMap::new();
    let mut authored_by_tool: BTreeMap<String, DateTime<Utc>> = BTreeMap::new();
    for fact in facts {
        if crate::store::is_system_authored(fact) {
            continue;
        }
        let Some(tool) = fact.tool.clone() else {
            continue;
        };
        let Ok(at) = DateTime::parse_from_rfc3339(&fact.created_at) else {
            continue;
        };
        let at = at.with_timezone(&Utc);
        authored_by_tool
            .entry(tool.clone())
            .and_modify(|existing| *existing = (*existing).max(at))
            .or_insert(at);
        if let Some(session_id) = fact.from_session_id.clone() {
            authored_by_session
                .entry((tool, session_id))
                .and_modify(|existing| *existing = (*existing).max(at))
                .or_insert(at);
        }
    }
    let mut worktrees: BTreeMap<PathBuf, WorktreeProbe> = BTreeMap::new();
    let mut by_session = BTreeMap::new();
    let mut by_session_reported_at = BTreeMap::new();
    let mut per_tool: BTreeMap<String, Vec<ObservedLiveness>> = BTreeMap::new();
    for stamp in latest.into_values() {
        let worktree = worktrees
            .entry(stamp.worktree_path.clone())
            .or_insert_with(|| probe_worktree(&stamp.worktree_path, &expected_common_dir));
        let sample = ProbeSample {
            worktree: worktree.clone(),
            pid_alive: stamp.observer_pid.and_then(process_alive),
        };
        // MAX, not `or_else`. On a host with no stable session identity
        // (`derive_endpoint` falls back to `proc:<host>:<pid>`, so every rally
        // invocation mints a fresh id) the per-session entry is ALWAYS present —
        // `ensure_presence` writes one with that very key — so an `or_else`
        // fallback can never fire, and the agent's writes from five minutes ago,
        // filed under the previous invocation's id, stay invisible. Taking the
        // max can only move the anchor FORWARD, i.e. only ever preserve a
        // session; a backdated fact planted by a peer cannot lower a maximum.
        let last_authored_at = [
            authored_by_session.get(&(stamp.tool.clone(), stamp.session_key.clone())),
            authored_by_tool.get(&stamp.tool),
        ]
        .into_iter()
        .flatten()
        .copied()
        .max();
        let verdict = grade_observation(&stamp, &sample, now, last_authored_at);
        let session_key = (stamp.tool.clone(), stamp.session_key.clone());
        by_session.insert(session_key.clone(), verdict);
        by_session_reported_at.insert(session_key, stamp.reported_at);
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
        by_session_reported_at,
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
                newest_worktree_mtime: None,
            },
            pid_alive,
        }
    }

    #[test]
    fn gone_process_and_unchanged_head_is_observed_stale() {
        let stamp = stamp();
        let sample = readable_probe(stamp.recorded_head.as_deref().unwrap(), Some(false));
        assert_eq!(
            grade_observation(&stamp, &sample, Utc::now(), None),
            ObservedLiveness::Stale
        );
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
                },
                Utc::now(),
                None,
            ),
            ObservedLiveness::Unknown
        );
        assert_eq!(
            grade_observation(
                &stamp,
                &readable_probe(&"b".repeat(40), Some(false)),
                Utc::now(),
                None,
            ),
            ObservedLiveness::Unknown
        );
    }

    /// The observer fail-open gap: a session whose presence stamps never
    /// carried an `observer_pid:` (RALLY_OBSERVER_PID unset) graded Unknown
    /// forever and was never reaped. With a readable worktree, an unmoved
    /// HEAD, and silence past the 2h takeover bar, it now grades stale.
    /// Fails on the pre-fix behavior (which returned Unknown here).
    ///
    /// The flavour is `StaleUnobserved`, not `Stale`, and the distinction is
    /// load-bearing rather than cosmetic: nothing here was OBSERVED dead, so
    /// this grade may inform a human-invoked sweep but must never corroborate
    /// the automatic one.
    #[test]
    fn unobserved_session_past_takeover_bar_fails_closed() {
        let mut stamp = stamp();
        stamp.observer_pid = None;
        stamp.reported_at = Utc::now() - chrono::Duration::hours(3);
        let sample = readable_probe(stamp.recorded_head.as_deref().unwrap(), None);
        let verdict = grade_observation(&stamp, &sample, Utc::now(), None);
        assert_eq!(
            verdict,
            ObservedLiveness::StaleUnobserved,
            "an unobserved session silent past the takeover bar must not stay Unknown forever"
        );
        assert!(verdict.is_stale());
        assert!(
            !verdict.authorizes_automatic_reap(),
            "an INFERRED grade must never corroborate the automatic reap"
        );
    }

    /// Within the takeover bar the same unobserved session stays Unknown:
    /// fail-closed only after the identical 2h bar the destructive takeover
    /// release applies.
    #[test]
    fn unobserved_session_within_takeover_bar_stays_unknown() {
        let mut stamp = stamp();
        stamp.observer_pid = None;
        stamp.reported_at = Utc::now() - chrono::Duration::minutes(30);
        let sample = readable_probe(stamp.recorded_head.as_deref().unwrap(), None);
        assert_eq!(
            grade_observation(&stamp, &sample, Utc::now(), None),
            ObservedLiveness::Unknown
        );
    }

    /// Fix-critique falsifier: presence stamps are WRITE-ONCE per session
    /// outside the shipped hook, so a stamp 3h old proves session age, not
    /// silence. A session that authored a ledger fact 10 minutes ago (claim
    /// renewal, artifact, status) is not silent and must stay Unknown even
    /// though its presence stamp is past the takeover bar. Fails on a version
    /// that anchors the bar on the stamp's own age.
    #[test]
    fn unobserved_session_with_fresh_authored_fact_stays_unknown() {
        let mut stamp = stamp();
        stamp.observer_pid = None;
        stamp.reported_at = Utc::now() - chrono::Duration::hours(3);
        let sample = readable_probe(stamp.recorded_head.as_deref().unwrap(), None);
        assert_eq!(
            grade_observation(
                &stamp,
                &sample,
                Utc::now(),
                Some(Utc::now() - chrono::Duration::minutes(10)),
            ),
            ObservedLiveness::Unknown,
            "a fresh authored fact must reset the silence clock"
        );
        // And an authored fact that is ITSELF past the bar does not rescue.
        assert_eq!(
            grade_observation(
                &stamp,
                &sample,
                Utc::now(),
                Some(Utc::now() - chrono::Duration::hours(3)),
            ),
            ObservedLiveness::StaleUnobserved
        );
    }

    /// A probe FAILURE on a stamped observer pid is not evidence of absence —
    /// only the never-stamped session takes the takeover-bar branch.
    /// `readable` is `head && dirty_count && newest_worktree_mtime` all present
    /// (`probe_worktree`), and `current_head_sha` reads `.git/HEAD` directly
    /// without spawning git — so a failed `git ls-files` leaves a PRESENT head
    /// beside ABSENT activity evidence. The `!readable` early return is what
    /// stops the fail-closed arm from grading stale on absent evidence, which
    /// would be the fail-open bug's mirror image. That early return had no test
    /// covering this shape; reds when it is removed.
    #[test]
    fn unreadable_worktree_stays_unknown_even_with_an_unmoved_head() {
        let mut stamp = stamp();
        stamp.observer_pid = None;
        stamp.reported_at = Utc::now() - chrono::Duration::hours(3);
        let sample = ProbeSample {
            worktree: WorktreeProbe {
                readable: false,
                head: stamp.recorded_head.clone(),
                dirty_count: None,
                newest_worktree_mtime: None,
            },
            pid_alive: None,
        };
        assert_eq!(
            grade_observation(&stamp, &sample, Utc::now(), None),
            ObservedLiveness::Unknown,
            "absent filesystem evidence is not evidence of absence"
        );
    }

    #[test]
    fn probe_failure_with_stamped_observer_pid_stays_unknown() {
        let mut stamp = stamp();
        stamp.reported_at = Utc::now() - chrono::Duration::hours(3);
        assert!(stamp.observer_pid.is_some());
        let sample = readable_probe(stamp.recorded_head.as_deref().unwrap(), None);
        assert_eq!(
            grade_observation(&stamp, &sample, Utc::now(), None),
            ObservedLiveness::Unknown
        );
    }

    /// Without a provably-unmoved HEAD (stamp recorded none), the unobserved
    /// session cannot be demoted: quiescence must be positive evidence.
    #[test]
    fn unobserved_session_without_recorded_head_stays_unknown() {
        let mut stamp = stamp();
        stamp.observer_pid = None;
        stamp.recorded_head = None;
        stamp.reported_at = Utc::now() - chrono::Duration::hours(3);
        let sample = readable_probe(&"a".repeat(40), None);
        assert_eq!(
            grade_observation(&stamp, &sample, Utc::now(), None),
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
            by_session_reported_at: BTreeMap::new(),
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
    fn live_pid_observation_before_lease_expiry_is_not_fresh_renewal_evidence() {
        let key = ("agent".to_string(), "owner-session".to_string());
        let reported_at = Utc::now() - chrono::Duration::hours(3);
        let index = ObservationIndex {
            by_session: BTreeMap::from([(key.clone(), ObservedLiveness::Live)]),
            by_session_reported_at: BTreeMap::from([(key, reported_at)]),
            by_tool: BTreeMap::new(),
        };
        assert_eq!(
            index.for_claim_since(
                Some("agent"),
                Some("owner-session"),
                Some(reported_at + chrono::Duration::hours(1)),
            ),
            ObservedLiveness::Unknown
        );
    }

    /// F3 falsifier for the untracked-activity guard: an agent authoring only
    /// brand-new (untracked, non-ignored) files must read as filesystem
    /// activity, or the takeover-bar branch counts real work as silence.
    /// Fails when `newest_worktree_mtime` drops `--others --exclude-standard`:
    /// the probe then sees only the backdated tracked file, the fresh scratch
    /// file is invisible, and the 3h-silent unstamped session grades Stale.
    /// The silence clock must not be blind to the SAME agent working under a
    /// different session id.
    ///
    /// On a host with no stable session identity, `derive_endpoint` falls back
    /// to `proc:<host>:<pid>`, so every rally invocation mints a fresh
    /// `from_session_id`. `ensure_presence` writes a presence fact keyed by that
    /// id, which means the per-session anchor is ALWAYS populated for the stamp
    /// being graded — and an `or_else` fallback to the tool aggregate can
    /// therefore never fire. The agent's writes from a minute ago, filed under
    /// the next invocation's id, would be invisible, and every claim on such a
    /// host would grade stale two hours after it was made no matter how busy the
    /// agent was. Reds on `or_else`; passes on `max`.
    #[test]
    fn a_sibling_session_of_the_same_tool_resets_the_silence_clock() {
        let root = unique_root("sibling-session-clock");
        fs::create_dir_all(&root).unwrap();
        crate::test_git_fixture::fixture_git(&root, &["init"]);
        fs::write(root.join("tracked.txt"), "tracked\n").unwrap();
        crate::test_git_fixture::fixture_git(&root, &["add", "tracked.txt"]);
        crate::test_git_fixture::fixture_git(&root, &["commit", "-m", "fixture"]);
        std::process::Command::new("touch")
            .args([
                "-t",
                "200001010000",
                root.join("tracked.txt").to_str().unwrap(),
            ])
            .status()
            .unwrap();
        let head = current_head_sha(&root).expect("fixture HEAD");
        let worktree = fs::canonicalize(&root).unwrap();

        let base = |kind: FactKind, session: &str, ago_hours: i64| Fact {
            from_session_id: Some(session.to_string()),
            schema: crate::FACT_SCHEMA.to_string(),
            event_id: crate::new_id("fact"),
            seq: 0,
            thread_id: crate::new_id("room"),
            kind,
            tool: Some("proc-agent".to_string()),
            role: None,
            subject: "fixture".to_string(),
            scope: Vec::new(),
            created_at: (Utc::now() - chrono::Duration::hours(ago_hours))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };

        // Invocation 1, three hours ago: presence stamped, no observer pid.
        let mut presence = base(FactKind::Presence, "proc-session-1", 3);
        presence.evidence = vec![
            format!("branch_head_sha:{head}"),
            format!("worktree_path:{}", worktree.display()),
        ];
        // Invocation 2, one minute ago: the SAME agent, a new proc id.
        let mut recent = base(FactKind::Artifact, "proc-session-2", 0);
        recent.subject = "still working".to_string();

        let index = observe_sessions(&root, &[presence, recent.clone()]);
        assert_eq!(
            index.for_claim(Some("proc-agent"), Some("proc-session-1")),
            ObservedLiveness::Unknown,
            "a sibling session's fresh fact must rescue the graded session"
        );

        // Non-vacuity: without the sibling's fact the same fixture DOES grade
        // stale, so the assertion above cannot be passing for another reason.
        let mut presence_only = base(FactKind::Presence, "proc-session-1", 3);
        presence_only.evidence = vec![
            format!("branch_head_sha:{head}"),
            format!("worktree_path:{}", worktree.display()),
        ];
        let alone = observe_sessions(&root, &[presence_only]);
        assert_eq!(
            alone.for_claim(Some("proc-agent"), Some("proc-session-1")),
            ObservedLiveness::StaleUnobserved,
            "fixture must be stale without the sibling, or this test proves nothing"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn untracked_file_activity_reads_as_live() {
        let root = unique_root("untracked-activity");
        fs::create_dir_all(&root).unwrap();
        crate::test_git_fixture::fixture_git(&root, &["init"]);
        fs::write(root.join("tracked.txt"), "tracked\n").unwrap();
        crate::test_git_fixture::fixture_git(&root, &["add", "tracked.txt"]);
        crate::test_git_fixture::fixture_git(&root, &["commit", "-m", "fixture"]);
        // Backdate the tracked file so only the untracked scratch file is
        // fresh, then write the scratch file an agent would be authoring.
        std::process::Command::new("touch")
            .args([
                "-t",
                "200001010000",
                root.join("tracked.txt").to_str().unwrap(),
            ])
            .status()
            .unwrap();
        fs::write(root.join("scratch-notes.md"), "new work\n").unwrap();

        let common = git_common_dir(&root).expect("fixture common dir");
        let probe = probe_worktree(&root, &common);
        assert!(probe.readable);
        let fresh = probe.newest_worktree_mtime.expect("mtime must be observed");
        assert!(
            Utc::now().signed_duration_since(fresh).num_seconds() < 120,
            "the untracked scratch file must set the newest mtime; got {fresh}"
        );

        let mut stamp = stamp();
        stamp.observer_pid = None;
        stamp.reported_at = Utc::now() - chrono::Duration::hours(3);
        stamp.recorded_head = current_head_sha(&root);
        let sample = ProbeSample {
            worktree: probe,
            pid_alive: None,
        };
        assert_eq!(
            grade_observation(&stamp, &sample, Utc::now(), None),
            ObservedLiveness::Live,
            "untracked-file activity newer than the stamp must protect the session"
        );

        fs::remove_dir_all(&root).ok();
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
