// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Thin-client routing (BACKLOG S-P3, Chunk C, ADR-01/ADR-02): the routed side
//! of `RoomStore`'s two-variant dispatcher (`store.rs`). [`RoutedRoomStore`]
//! speaks the `store_wire` protocol over `.rally/rallyd.sock` — discovered
//! EXCLUSIVELY via `.rally/rallyd.sock.addr` (L7), never a hardcoded path —
//! and holds NO facts.db handle (G3): every ROUTED method is a `round_trip`
//! wire call; the LOCAL accessors are answered from this struct's own fields.
//!
//! ## Wire framing
//!
//! Line-delimited JSON: one [`StoreRequest`] serialised to a single
//! `\n`-terminated line, one [`StoreResponse`] line read back. [`round_trip`]
//! mirrors `daemon_client.rs:639`'s helper shape (connect → write → read one
//! line → parse), typed against the frozen `store_wire` contract instead of
//! bare `serde_json::Value`. This is a SEPARATE socket from ptyd's — zero
//! collision (`daemon_client.rs`'s `rally_owned_socket()` is a different
//! daemon entirely; rallyd's socket lives at `.rally/rallyd.sock`).
//!
//! ## R6 — mid-command transport failure
//!
//! Every dispatch method funnels through [`RoutedRoomStore::dispatch`]. A
//! transport-level failure there (connect refused, read/write timeout,
//! connection reset) — as opposed to a well-formed `StoreResponse::Err` reply
//! — preserves the concrete I/O cause. Read-only operations report a transport
//! failure. Mutating operations report an UNKNOWN outcome and name the stable
//! event/request identifier when the request carries one, so callers query the
//! ledger before deciding what to do. The router in `store.rs` NEVER falls back
//! to a direct facts.db open mid-command on this path.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use rally_protocol::store_wire::{
    MAX_LINE_BYTES, StoreError, StoreErrorKind, StoreOk, StoreOp, StoreRequest, StoreResponse,
    WIRE_VERSION,
};

use crate::claim_authority;
use crate::error::{RallyError, Result};
use crate::store::{
    self, AppendOutcome, ConditionalAppendOutcome, Fact, ReadReceipt, RenewClaimLeaseOutcome,
    RoomSnapshot, SnapshotCacheCapture, SnapshotCacheFingerprint,
};

/// Discovery pointer filename inside `.rally/` (L7/ADR-02) — the daemon's
/// SOLE discovery mechanism; clients never guess or hardcode the socket path.
/// Duplicated as a literal rather than imported from `rallyd_core.rs`
/// (Chunk B's owned file, edited concurrently in this same parallel window):
/// the STRING is the frozen wire contract, not the module that happens to
/// hold the daemon's own copy of it.
const ADDR_FILENAME: &str = "rallyd.sock.addr";

/// Describe the daemon discovery pointer as it stands right now, WITHOUT
/// probing.
///
/// The router's busy timeout needs to distinguish three states that produce
/// identical `EWOULDBLOCK` symptoms: no daemon was ever here (no pointer), a
/// daemon published a pointer and then died (pointer present, socket file
/// gone), and a daemon is present and holding the room but not answering
/// (pointer and socket both present). Only the third explains a stall that runs
/// the full bound, because only there does every probe pay the full
/// [`PROBE_TIMEOUT`] before failing.
///
/// Deliberately does not ping: this runs on a path that has already exhausted
/// its wall-clock budget, and one more 3s probe would push the caller past the
/// watchdog that is about to fire.
pub(crate) fn daemon_route_state(rally_dir: &Path) -> String {
    let addr_path = rally_dir.join(ADDR_FILENAME);
    let Ok(text) = std::fs::read_to_string(&addr_path) else {
        return format!(
            "{ADDR_FILENAME} absent (no daemon has published a socket for this room)"
        );
    };
    let socket = PathBuf::from(text.trim());
    if socket.as_os_str().is_empty() {
        return format!("{ADDR_FILENAME} present but empty");
    }
    if socket.exists() {
        format!(
            "{ADDR_FILENAME} -> {} (socket file present, so every probe paid the full {}ms probe timeout)",
            socket.display(),
            PROBE_TIMEOUT.as_millis()
        )
    } else {
        format!(
            "{ADDR_FILENAME} -> {} (socket file MISSING — the pointer is stale: the daemon exited without clearing it, or the socket was removed underneath it)",
            socket.display()
        )
    }
}

/// Per-attempt connect+read/write timeout for the identity probe (used both
/// standalone and inside the bounded-block corridor). Mirrors
/// `daemon_client::DAEMON_TIMEOUT` — ADR-01 names this the corridor's
/// "extendable" per-attempt unit, 3s.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Connect+read/write timeout for an ORDINARY routed op (post-routing —
/// distinct from the probe). Sized to match the daemon's own per-connection
/// `CONN_TIMEOUT` (`rallyd_core.rs`, 10s): the single-threaded dispatcher
/// processes requests in total order (L10), so a legitimate op queued behind
/// others under load can take longer than the probe's 3s without the
/// connection itself being dead — a too-tight client timeout here would
/// misclassify a merely-slow reply as R6's dead-socket case.
const OP_TIMEOUT: Duration = Duration::from_secs(10);

/// Leave enough wall-clock after a not-started reply for the daemon reader,
/// socket write, client parse, and outer CLI watchdog rendering.
const MUTATION_DEADLINE_RESERVE: Duration = Duration::from_millis(250);

fn mutation_deadline() -> (u64, u64) {
    mutation_deadline_with_hook(|| {})
}

fn mutation_deadline_with_hook<F>(between_clock_samples: F) -> (u64, u64)
where
    F: FnOnce(),
{
    // Sample wall time before the watchdog's monotonic remainder. If this
    // thread is preempted between the two reads, adding the later/smaller
    // remainder to the earlier wall sample is conservative; the inverse order
    // would rebase that pause beyond the outer watchdog.
    let now = SystemTime::now();
    between_clock_samples();
    let outer = crate::watchdog_remaining().unwrap_or(OP_TIMEOUT);
    let budget = outer
        .min(OP_TIMEOUT)
        .saturating_sub(MUTATION_DEADLINE_RESERVE);
    let deadline = now.checked_add(budget).unwrap_or(now);
    let deadline_unix_ms = deadline
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0);
    let budget_ms = budget.as_millis().min(u128::from(u64::MAX)) as u64;
    (deadline_unix_ms, budget_ms)
}

/// Bounded-block corridor total budget (L12/ADR-01): once the SH try-lock is
/// refused (a daemon holds EX but hadn't answered a ping yet — a cold-start
/// reconcile can take seconds on a real room), the router re-probes up to this
/// bound before failing loud. Public so `store.rs`'s router and `lib.rs`'s
/// `daemon start` wait-for-ready loop can both size their own waits against
/// the same named corridor rather than each guessing a number.
pub(crate) const CORRIDOR_BOUND: Duration = Duration::from_secs(30);

/// Sleep between corridor re-probes when an attempt returns quickly (e.g.
/// connection refused) rather than blocking for the full per-attempt timeout.
const CORRIDOR_RETRY_SLEEP: Duration = Duration::from_millis(200);

/// Connect attempts (including the first) before surfacing a transient connect
/// failure. The rallyd daemon is a single-dispatcher, nonblocking-accept
/// server: under a burst of concurrent connects its kernel backlog can momentarily
/// fill and REFUSE a connect (ECONNREFUSED) even though the daemon is very much
/// alive — it answers a ping microseconds later once its accept loop drains.
/// Because every op opens a FRESH connection (no persistent socket), a single
/// transient refusal would otherwise surface as R6's retryable "daemon stopped
/// mid-request". Retrying the CONNECT (not the whole request) a few times with a
/// short jittered backoff absorbs the burst; a genuinely dead daemon still fails
/// all attempts and yields the retryable error.
const CONNECT_ATTEMPTS: u32 = 5;

/// Jitter floor for the connect backoff (ms). Actual sleep is
/// `CONNECT_BACKOFF_MIN_MS + (0..CONNECT_BACKOFF_SPREAD_MS)`, keeping retriers
/// from converging on the same wake instant and re-colliding on the backlog.
const CONNECT_BACKOFF_MIN_MS: u64 = 20;
/// Jitter spread (ms) added on top of the floor, giving a 20–50ms window.
const CONNECT_BACKOFF_SPREAD_MS: u64 = 31;

/// Connect to `socket`, retrying a TRANSIENT connect failure (backlog-full
/// refusal, a socket file that momentarily vanished during a daemon restart, or
/// a nonblocking `WouldBlock`) up to [`CONNECT_ATTEMPTS`] times with a short
/// jittered backoff. A non-transient error (or the last attempt) is returned
/// verbatim so the caller's existing dead-socket policy still fires for a truly
/// dead daemon. This wraps ONLY the connect — per-op read/write timeouts
/// (`PROBE_TIMEOUT`/`OP_TIMEOUT`) are applied by the caller after connecting and
/// are unchanged.
///
/// `probe` selects the transient set for [`ErrorKind::NotFound`] (f4): on the
/// PROBE path a missing socket means "no daemon here (yet / at all)" and must
/// resolve PROMPTLY to fail-open (direct mode) rather than burning all
/// [`CONNECT_ATTEMPTS`] backoffs on a socket that isn't there — so NotFound is
/// treated as non-transient (returned at once). On the DISPATCH path (`false`)
/// NotFound stays transient: a socket file can momentarily vanish while a daemon
/// restarts mid-command, and a short retry absorbs that race.
fn connect_with_retry(socket: &Path, probe: bool) -> std::io::Result<UnixStream> {
    let mut attempt: u32 = 0;
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                attempt += 1;
                let transient = matches!(
                    e.kind(),
                    ErrorKind::ConnectionRefused | ErrorKind::WouldBlock
                ) || (!probe && e.kind() == ErrorKind::NotFound);
                if !transient || attempt >= CONNECT_ATTEMPTS {
                    return Err(e);
                }
                let jitter = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| u64::from(d.subsec_nanos()))
                    .unwrap_or(0)
                    % CONNECT_BACKOFF_SPREAD_MS;
                std::thread::sleep(Duration::from_millis(CONNECT_BACKOFF_MIN_MS + jitter));
            }
        }
    }
}

/// Daemon identity from a successful `Ping` (ADR-02) — used both by the
/// router's liveness probe and by `rally daemon status`.
#[derive(Clone, Debug)]
pub(crate) struct StoreIdentity {
    /// Verified equal to the caller's own canonical repo_root by construction
    /// (`probe_identity` only returns `Some` on a match) — kept on the struct
    /// because it's part of the wire reply's shape, not because a caller
    /// currently re-reads it after the match already succeeded.
    #[allow(dead_code)]
    pub(crate) repo_root: String,
    pub(crate) pid: u32,
    pub(crate) wire_version: u32,
    pub(crate) socket: PathBuf,
}

/// The routed store client (Chunk C). Holds NO fact_store handle (G3) — every
/// ROUTED method below is a wire call; the LOCAL accessors are answered from
/// the fields here, resolved once at construction exactly like
/// `DirectRoomStore`'s equivalents (G1 parity).
pub(crate) struct RoutedRoomStore {
    socket: PathBuf,
    repo_root: PathBuf,
    active_engagement: String,
    cursor_path: PathBuf,
    log_dir: PathBuf,
    /// Only read via the `#[cfg(test)]` accessor below (mirrors
    /// `DirectRoomStore`'s field — always present, accessor test-gated).
    #[allow(dead_code)]
    claim_index_path: PathBuf,
}

/// One line-delimited JSON-RPC round trip against `socket`: connect, write one
/// `\n`-terminated [`StoreRequest`] line, read one reply line, parse it.
/// Mirrors `daemon_client.rs:639`'s `round_trip` shape (same connect/timeout/
/// write/read-one-line/parse structure), typed against `store_wire` instead
/// of bare JSON-RPC.
fn round_trip(
    socket: &Path,
    req: &StoreRequest,
    timeout: Duration,
    probe: bool,
) -> std::io::Result<StoreResponse> {
    let mut stream = connect_with_retry(socket, probe)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut line = serde_json::to_string(req).map_err(|e| std::io::Error::other(e.to_string()))?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    // Cap the reply read at MAX_LINE_BYTES + 1 (SEC-006): the daemon caps
    // REQUESTS at MAX_LINE_BYTES (`rallyd_core::read_request_line`), but the
    // client reply read was uncapped — a wedged/hostile peer streaming an
    // unbounded line would grow this buffer without limit. Reading one extra
    // byte lets an exactly-at-limit reply through while an over-cap reply is
    // detected and mapped to an R6 transport error (an io::Error here;
    // `dispatch` preserves its cause and mutation ambiguity while
    // `probe_identity` treats probe failure as not-live), mirroring the daemon's
    // own request cap.
    let mut reader = BufReader::new(stream).take(MAX_LINE_BYTES as u64 + 1);
    let mut resp = String::new();
    reader.read_line(&mut resp)?;
    if resp.len() > MAX_LINE_BYTES {
        return Err(std::io::Error::other(format!(
            "daemon reply exceeds {MAX_LINE_BYTES} bytes; run `rally daemon status`"
        )));
    }
    if resp.trim().is_empty() {
        return Err(std::io::Error::other("empty daemon reply"));
    }
    serde_json::from_str(resp.trim())
        .map_err(|e| std::io::Error::other(format!("bad reply json: {e}")))
}

/// One-shot liveness probe (ADR-01 §1): read `.addr`, connect, `Ping`, verify
/// `wire_version` + `repo_root`. `None` covers every ordinary not-live case —
/// missing/stale `.addr`, a socket file that doesn't exist, a refused
/// connection, a timeout, or a repo-root mismatch — the router treats those as
/// "no live daemon". A wire-version mismatch is different: it fails
/// immediately and never falls through to direct ownership. This is
/// deliberately narrow: it is NOT where R6's mid-command dead-socket policy
/// lives (that's [`RoutedRoomStore::dispatch`], reached only AFTER routing
/// has already begun).
pub(crate) fn probe_identity(
    rally_dir: &Path,
    expected_repo_root: &str,
) -> Result<Option<StoreIdentity>> {
    let addr_path = rally_dir.join(ADDR_FILENAME);
    let socket_text = match std::fs::read_to_string(&addr_path) {
        Ok(text) => text,
        Err(_) => return Ok(None),
    };
    let socket = PathBuf::from(socket_text.trim());
    if socket.as_os_str().is_empty() || !socket.exists() {
        return Ok(None);
    }
    let req = StoreRequest::new(None, StoreOp::Ping);
    let reply = match round_trip(&socket, &req, PROBE_TIMEOUT, true) {
        Ok(reply) => reply,
        Err(_) => return Ok(None),
    };
    match reply {
        StoreResponse::Ok(StoreOk::Pong {
            repo_root,
            pid,
            wire_version,
        }) => {
            if wire_version != WIRE_VERSION {
                return Err(RallyError::IncompatibleWire {
                    detail: format!(
                        "client speaks {WIRE_VERSION}, daemon speaks {wire_version}; run `rally daemon stop` before retrying"
                    ),
                });
            }
            if repo_root != expected_repo_root {
                return Ok(None);
            }
            Ok(Some(StoreIdentity {
                repo_root,
                pid,
                wire_version,
                socket,
            }))
        }
        StoreResponse::Err(error) if error.kind == StoreErrorKind::IncompatibleWire => {
            Err(store_error_to_rally_error(error))
        }
        _ => Ok(None),
    }
}

/// Construct a routed store for `repo_root` iff a live daemon answers the
/// identity probe. `engagement` mirrors the direct constructors' contract:
/// `Some(label)` is an explicit override (tests); `None` resolves the room
/// default LOCALLY via [`store::resolve_active_engagement_with_env`] — the
/// SAME function `DirectRoomStore::open_direct_at_with_engagement` uses (G1
/// parity: both variants resolve an unset default identically).
pub(crate) fn probe_live(
    repo_root: &Path,
    rally_dir: &Path,
    engagement: Option<String>,
) -> Result<Option<RoutedRoomStore>> {
    let canonical = store::canonical_repo_root_string(repo_root);
    let Some(identity) = probe_identity(rally_dir, &canonical)? else {
        return Ok(None);
    };
    let resolved_engagement = store::resolve_active_engagement_with_env(rally_dir, engagement);
    Ok(Some(RoutedRoomStore {
        socket: identity.socket,
        repo_root: repo_root.to_path_buf(),
        active_engagement: resolved_engagement,
        cursor_path: rally_dir.join("cursors.json"),
        log_dir: rally_dir.join(store::LOG_DIRNAME),
        claim_index_path: rally_dir.join(claim_authority::CLAIM_INDEX_FILENAME),
    }))
}

/// Bounded-block corridor (L12/ADR-01): re-probe until `bound` elapses. Used
/// ONLY when the SH try-lock was refused (a daemon holds EX but hadn't
/// answered a ping yet). Returns `None` once the bound is exhausted; the
/// caller (`store.rs`'s router) turns that into the fail-loud
/// `wedged_daemon_error`.
pub(crate) fn probe_live_bounded(
    repo_root: &Path,
    rally_dir: &Path,
    engagement: Option<String>,
    bound: Duration,
) -> Result<Option<RoutedRoomStore>> {
    let deadline = Instant::now() + bound;
    loop {
        if let Some(routed) = probe_live(repo_root, rally_dir, engagement.clone())? {
            return Ok(Some(routed));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        std::thread::sleep(CORRIDOR_RETRY_SLEEP.min(deadline.saturating_duration_since(now)));
    }
}

/// Stable query context for a mutation whose wire outcome is unknown.
struct MutationQuery {
    operation: &'static str,
    selector: Option<String>,
    event_id: Option<String>,
}

/// Classify the closed wire operation set. This classification lives beside
/// dispatch so adding a mutating protocol variant cannot silently inherit the
/// old blind-retry behavior.
fn mutation_query(op: &StoreOp) -> Option<MutationQuery> {
    let fact_event_id = |fact: &Value| {
        fact.get("event_id")
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    match op {
        StoreOp::AppendFact { fact } => Some(MutationQuery {
            operation: "append_fact",
            selector: fact_event_id(fact).map(|event_id| format!("event_id={event_id}")),
            event_id: fact_event_id(fact),
        }),
        StoreOp::AppendFactVerified { fact } => Some(MutationQuery {
            operation: "append_fact_verified",
            selector: fact_event_id(fact).map(|event_id| format!("event_id={event_id}")),
            event_id: fact_event_id(fact),
        }),
        StoreOp::AppendStateTransitionVerified { fact } => Some(MutationQuery {
            operation: "append_state_transition_verified",
            selector: fact_event_id(fact).map(|event_id| format!("event_id={event_id}")),
            event_id: fact_event_id(fact),
        }),
        StoreOp::AppendSessionFactIfContext { fact, .. } => Some(MutationQuery {
            operation: "append_session_fact_if_context",
            selector: fact_event_id(fact).map(|event_id| format!("event_id={event_id}")),
            event_id: fact_event_id(fact),
        }),
        StoreOp::RebuildClaimIndex => Some(MutationQuery {
            operation: "rebuild_claim_index",
            selector: None,
            event_id: None,
        }),
        StoreOp::RenewClaimLease {
            claim_id, event_id, ..
        } => Some(MutationQuery {
            operation: "renew_claim_lease",
            selector: Some(format!("event_id={event_id},claim_id={claim_id}")),
            event_id: Some(event_id.clone()),
        }),
        StoreOp::MaybeAppendReadCheckpoint { fact, read_seq } => {
            let event_id = fact_event_id(fact);
            Some(MutationQuery {
                operation: "maybe_append_read_checkpoint",
                selector: event_id
                    .as_ref()
                    .map(|event_id| format!("event_id={event_id},read_seq={read_seq}")),
                event_id,
            })
        }
        StoreOp::Facts
        | StoreOp::RepoWideClaimLifecycleFacts
        | StoreOp::SessionFactsWithContextVersion
        | StoreOp::SnapshotWithArchived { .. }
        | StoreOp::SnapshotForObligationTarget { .. }
        | StoreOp::SnapshotScoped { .. }
        | StoreOp::SnapshotWithReadersArchived { .. }
        | StoreOp::LastCheckpointSeq { .. }
        | StoreOp::ProjectReadReceipts { .. }
        | StoreOp::Ping => None,
    }
}

/// R6: preserve the concrete transport cause and never imply daemon death when
/// the daemon may still be live. A mutation may already be durable when its
/// reply is lost, so its only safe result is UNKNOWN + a ledger query selector.
fn transport_error(op: &StoreOp, io_err: &std::io::Error) -> RallyError {
    if let Some(query) = mutation_query(op) {
        if let Some(event_id) = query.event_id.as_deref() {
            return RallyError::outcome_unknown(
                event_id,
                "daemon-transport",
                format!(
                    "daemon transport failure during mutating operation {}: {io_err}; direct fallback is forbidden",
                    query.operation
                ),
            );
        }
        let selector = query
            .selector
            .as_deref()
            .map(|value| format!(" for {value}"))
            .unwrap_or_default();
        return RallyError::Command(format!(
            "daemon transport failure during mutating operation {}: {io_err}; outcome unknown. Query `rally room --json`{selector} before repeating the mutation; direct fallback is forbidden",
            query.operation
        ));
    }
    RallyError::Command(format!(
        "daemon transport failure during read-only operation: {io_err}; run `rally daemon status`"
    ))
}

/// Reconstruct the concrete `RallyError` a wire [`StoreError`] represents
/// (G8 exit-code parity). `Transport`-class errors (R7 — no direct-path
/// equivalent) reconstruct as `RallyError::Command` carrying the daemon's own
/// remedy text, same as `Internal` (Io/Json collapsed for the wire).
fn store_error_to_rally_error(err: StoreError) -> RallyError {
    match err.kind {
        StoreErrorKind::Usage => RallyError::Usage(err.message),
        StoreErrorKind::NotFound => RallyError::NotFound(err.message),
        StoreErrorKind::NotStarted => RallyError::NotStarted(err.message),
        StoreErrorKind::OutcomeUnknown => {
            let Some(event_id) = err.event_id else {
                return RallyError::IncompatibleWire {
                    detail: "daemon returned malformed outcome_unknown without event_id"
                        .to_string(),
                };
            };
            if let Err(error) = crate::store::validate_append_event_id(&event_id) {
                return RallyError::IncompatibleWire {
                    detail: format!("daemon returned malformed outcome_unknown event_id: {error}"),
                };
            }
            let Some(phase) = err.phase else {
                return RallyError::IncompatibleWire {
                    detail: "daemon returned malformed outcome_unknown without phase".to_string(),
                };
            };
            if phase.trim().is_empty() || phase.chars().any(char::is_control) {
                return RallyError::IncompatibleWire {
                    detail: "daemon returned malformed outcome_unknown phase".to_string(),
                };
            }
            RallyError::outcome_unknown(event_id, phase, err.message)
        }
        StoreErrorKind::IncompatibleWire => RallyError::IncompatibleWire {
            detail: err.message,
        },
        StoreErrorKind::Command
        | StoreErrorKind::Message
        | StoreErrorKind::Internal
        | StoreErrorKind::Transport => RallyError::Command(err.message),
    }
}

fn to_value<T: Serialize>(value: &T) -> Result<Value> {
    serde_json::to_value(value).map_err(RallyError::json("serialize wire request"))
}

fn from_value<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(RallyError::json("parse wire reply"))
}

/// Snapshots do NOT go through [`from_value`]. The daemon ships the four
/// `#[serde(skip)]` projections in a side-channel key so routed callers rank,
/// checkpoint, and coalesce the same way direct callers do (design audit
/// D1/D6). See `store::SnapshotInternals`.
fn snapshot_from_value(value: Value) -> Result<RoomSnapshot> {
    store::snapshot_from_wire_value(value).map_err(RallyError::json("parse wire snapshot reply"))
}

/// Reply-shape mismatch: the daemon answered a DIFFERENT `StoreOk` variant
/// than the op that was sent. Should be unreachable given the closed
/// `StoreOp`/`StoreOk` pairing, but the wire is still a boundary — never
/// panic on an unexpected (if impossible) shape from a peer process.
fn unexpected_reply(op: &str) -> RallyError {
    RallyError::Command(format!(
        "daemon returned an unexpected reply shape for {op}"
    ))
}

impl RoutedRoomStore {
    #[cfg(test)]
    pub(crate) fn for_test(socket: PathBuf, repo_root: PathBuf, engagement: &str) -> Self {
        let rally_dir = repo_root.join(".rally");
        Self {
            socket,
            repo_root,
            active_engagement: engagement.to_string(),
            cursor_path: rally_dir.join("cursors.json"),
            log_dir: rally_dir.join("log"),
            claim_index_path: rally_dir.join(claim_authority::CLAIM_INDEX_FILENAME),
        }
    }

    /// Send one op over the wire, carrying this client's already-resolved
    /// engagement label (L9) with EVERY request — the daemon's per-request
    /// `set_engagement_scope` rebind depends on it being present on every op,
    /// not just appends.
    fn dispatch(&self, op: StoreOp) -> Result<StoreOk> {
        self.dispatch_with_engagement(self.active_engagement.clone(), op)
    }

    #[cfg(test)]
    pub(crate) fn dispatch_for_test(&self, op: StoreOp) -> Result<StoreOk> {
        self.dispatch(op)
    }

    fn dispatch_with_engagement(&self, engagement: String, op: StoreOp) -> Result<StoreOk> {
        let mut req = StoreRequest::new(Some(engagement), op);
        if req.op.is_mutating() {
            let (deadline_unix_ms, mutation_budget_ms) = mutation_deadline();
            req.deadline_unix_ms = Some(deadline_unix_ms);
            req.mutation_budget_ms = Some(mutation_budget_ms);
        }
        match round_trip(&self.socket, &req, OP_TIMEOUT, false) {
            Ok(StoreResponse::Ok(ok)) => Ok(ok),
            Ok(StoreResponse::Err(err)) => Err(store_error_to_rally_error(err)),
            Err(io_err) => Err(transport_error(&req.op, &io_err)),
        }
    }

    // ----- ROUTED methods (one StoreOp round trip each) ----------------------

    pub(crate) fn append_fact(&self, fact: &Fact) -> Result<AppendOutcome> {
        match self.dispatch(StoreOp::AppendFact {
            fact: to_value(fact)?,
        })? {
            StoreOk::AppendFact { outcome } => from_value(outcome),
            _ => Err(unexpected_reply("append_fact")),
        }
    }

    pub(crate) fn append_fact_verified(&self, fact: &Fact) -> Result<AppendOutcome> {
        match self.dispatch(StoreOp::AppendFactVerified {
            fact: to_value(fact)?,
        })? {
            StoreOk::AppendFactVerified { outcome } => from_value(outcome),
            _ => Err(unexpected_reply("append_fact_verified")),
        }
    }

    pub(crate) fn append_state_transition_verified(&self, fact: &Fact) -> Result<AppendOutcome> {
        match self.dispatch(StoreOp::AppendStateTransitionVerified {
            fact: to_value(fact)?,
        })? {
            StoreOk::AppendStateTransitionVerified { outcome } => from_value(outcome),
            _ => Err(unexpected_reply("append_state_transition_verified")),
        }
    }

    pub(crate) fn append_session_fact_if_context(
        &self,
        fact: &Fact,
        expected_context_version: Option<u64>,
    ) -> Result<ConditionalAppendOutcome> {
        match self.dispatch(StoreOp::AppendSessionFactIfContext {
            fact: to_value(fact)?,
            expected_context_version,
        })? {
            StoreOk::AppendSessionFactIfContext { result } => from_value(result),
            _ => Err(unexpected_reply("append_session_fact_if_context")),
        }
    }

    pub(crate) fn facts(&self) -> Result<Vec<Fact>> {
        match self.dispatch(StoreOp::Facts)? {
            StoreOk::Facts { facts } => facts.into_iter().map(from_value).collect(),
            _ => Err(unexpected_reply("facts")),
        }
    }

    pub(crate) fn repo_wide_claim_lifecycle_facts(&self) -> Result<Vec<Fact>> {
        match self.dispatch(StoreOp::RepoWideClaimLifecycleFacts)? {
            StoreOk::Facts { facts } => facts.into_iter().map(from_value).collect(),
            _ => Err(unexpected_reply("repo_wide_claim_lifecycle_facts")),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn rebuild_claim_index(&self) -> Result<()> {
        match self.dispatch(StoreOp::RebuildClaimIndex)? {
            StoreOk::RebuildClaimIndex => Ok(()),
            _ => Err(unexpected_reply("rebuild_claim_index")),
        }
    }

    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn renew_claim_lease(
        &self,
        claim_id: &str,
        lease_expires_at: String,
        caller_tool: &str,
        caller_session_id: Option<&str>,
        expected_owner_session_id: Option<&str>,
        event_id: String,
        thread_id: String,
        created_at: String,
    ) -> Result<RenewClaimLeaseOutcome> {
        match self.dispatch(StoreOp::RenewClaimLease {
            claim_id: claim_id.to_string(),
            lease_expires_at,
            event_id,
            thread_id,
            created_at,
            caller_tool: Some(caller_tool.to_string()),
            caller_session_id: caller_session_id.map(str::to_string),
            expected_owner_session_id: expected_owner_session_id.map(str::to_string),
        })? {
            StoreOk::RenewClaimLease { outcome } => from_value(outcome),
            _ => Err(unexpected_reply("renew_claim_lease")),
        }
    }

    pub(crate) fn session_facts_with_context_version(&self) -> Result<(Vec<Fact>, Option<u64>)> {
        match self.dispatch(StoreOp::SessionFactsWithContextVersion)? {
            StoreOk::SessionFactsWithContextVersion {
                facts,
                context_version,
            } => Ok((
                facts.into_iter().map(from_value).collect::<Result<_>>()?,
                context_version,
            )),
            _ => Err(unexpected_reply("session_facts_with_context_version")),
        }
    }

    /// `RoomStore::snapshot()` is `snapshot_with_archived(false)` composed
    /// locally — mirroring `DirectRoomStore::snapshot()`'s own composition;
    /// there is no separate `StoreOp::Snapshot` wire variant.
    pub(crate) fn snapshot(&self) -> Result<RoomSnapshot> {
        self.snapshot_with_archived(false)
    }

    pub(crate) fn snapshot_with_archived(&self, include_archived: bool) -> Result<RoomSnapshot> {
        match self.dispatch(StoreOp::SnapshotWithArchived { include_archived })? {
            StoreOk::Snapshot { snapshot, .. } => snapshot_from_value(snapshot),
            _ => Err(unexpected_reply("snapshot_with_archived")),
        }
    }

    pub(crate) fn snapshot_for_obligation_target(
        &self,
        tool: &str,
        row_limit: usize,
    ) -> Result<RoomSnapshot> {
        match self.dispatch(StoreOp::SnapshotForObligationTarget {
            tool: tool.to_string(),
            row_limit,
            include_archived: false,
        })? {
            StoreOk::Snapshot { snapshot, .. } => snapshot_from_value(snapshot),
            _ => Err(unexpected_reply("snapshot_for_obligation_target")),
        }
    }

    pub(crate) fn snapshot_cache_capture(
        &self,
        include_archived: bool,
    ) -> Result<SnapshotCacheCapture> {
        match self.dispatch(StoreOp::SnapshotWithArchived { include_archived })? {
            StoreOk::Snapshot {
                snapshot,
                fingerprint,
            } => Ok(SnapshotCacheCapture {
                snapshot: snapshot_from_value(snapshot)?,
                fingerprint: fingerprint
                    .map(from_value::<SnapshotCacheFingerprint>)
                    .transpose()?,
            }),
            _ => Err(unexpected_reply("snapshot_cache_capture")),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn snapshot_scoped(
        &self,
        engagement: &str,
        run_id: Option<&str>,
        path: Option<&str>,
        include_archived: bool,
        include_presence_only: bool,
    ) -> Result<RoomSnapshot> {
        let op = StoreOp::SnapshotScoped {
            run_id: run_id.map(str::to_string),
            path: path.map(str::to_string),
            include_archived,
            include_presence_only,
        };
        match self.dispatch_with_engagement(engagement.to_string(), op)? {
            StoreOk::Snapshot { snapshot, .. } => snapshot_from_value(snapshot),
            _ => Err(unexpected_reply("snapshot_scoped")),
        }
    }

    pub(crate) fn snapshot_with_readers_archived(
        &self,
        include_archived: bool,
    ) -> Result<RoomSnapshot> {
        match self.dispatch(StoreOp::SnapshotWithReadersArchived { include_archived })? {
            StoreOk::SnapshotWithReaders { snapshot } => snapshot_from_value(snapshot),
            _ => Err(unexpected_reply("snapshot_with_readers_archived")),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn last_checkpoint_seq(&self, tool: &str) -> Result<i64> {
        match self.dispatch(StoreOp::LastCheckpointSeq {
            tool: tool.to_string(),
        })? {
            StoreOk::LastCheckpointSeq { seq } => Ok(seq),
            _ => Err(unexpected_reply("last_checkpoint_seq")),
        }
    }

    pub(crate) fn maybe_append_read_checkpoint(
        &self,
        checkpoint: &Fact,
        read_seq: i64,
    ) -> Result<ConditionalAppendOutcome> {
        match self.dispatch(StoreOp::MaybeAppendReadCheckpoint {
            fact: to_value(checkpoint)?,
            read_seq,
        })? {
            StoreOk::MaybeAppendReadCheckpoint { result } => from_value(result),
            _ => Err(unexpected_reply("maybe_append_read_checkpoint")),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn project_read_receipts(&self, max_seq: i64) -> Result<Vec<ReadReceipt>> {
        match self.dispatch(StoreOp::ProjectReadReceipts { max_seq })? {
            StoreOk::ProjectReadReceipts { receipts } => {
                receipts.into_iter().map(from_value).collect()
            }
            _ => Err(unexpected_reply("project_read_receipts")),
        }
    }

    // ----- LOCAL methods (no wire hop) ----------------------------------------

    /// R10 MIXED (see the classification note on `RoomStore` in `store.rs`):
    /// ledger-first — a ROUTED `last_checkpoint_seq` call, THEN a LOCAL
    /// `cursors.json` fallback. Mirrors `DirectRoomStore::cursor_for` exactly.
    pub(crate) fn cursor_for(&self, tool: &str) -> Result<i64> {
        let ledger_seq = self.last_checkpoint_seq(tool)?;
        if ledger_seq > 0 {
            return Ok(ledger_seq);
        }
        Ok(store::read_cursors_at(&self.cursor_path)?
            .get(tool)
            .copied()
            .unwrap_or(0))
    }

    pub(crate) fn set_cursor(&self, tool: &str, seq: i64) -> Result<()> {
        store::write_cursor_at(&self.cursor_path, tool, seq)
    }

    pub(crate) fn active_engagement(&self) -> &str {
        &self.active_engagement
    }

    pub(crate) fn room_id(&self) -> &str {
        &self.active_engagement
    }

    pub(crate) fn active_segment_path(&self) -> PathBuf {
        self.log_dir
            .join(format!("{}.jsonl", self.active_engagement))
    }

    pub(crate) fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    #[cfg(test)]
    pub(crate) fn claim_index_path(&self) -> &Path {
        &self.claim_index_path
    }

    #[cfg(test)]
    pub(crate) fn set_active_engagement_for_test(&mut self, engagement: &str) {
        self.active_engagement = engagement.to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_not_started_reconstructs_the_direct_error_class() {
        let error = store_error_to_rally_error(StoreError::new(
            StoreErrorKind::NotStarted,
            "mutation-not-started: no durable mutation started",
        ));
        assert!(matches!(error, RallyError::NotStarted(_)));
        assert_eq!(error.exit_code(), 4);
    }

    #[test]
    fn wire_deadline_consumes_preemption_between_clock_samples() {
        let _guard = crate::install_watchdog_deadline(
            Instant::now()
                .checked_add(Duration::from_millis(500))
                .unwrap(),
        );

        let (_deadline_unix_ms, budget_ms) = mutation_deadline_with_hook(|| {
            thread::sleep(Duration::from_millis(300));
        });
        let allowed_after_pause = crate::watchdog_remaining()
            .unwrap()
            .min(OP_TIMEOUT)
            .saturating_sub(MUTATION_DEADLINE_RESERVE)
            .as_millis() as u64;

        assert!(
            budget_ms <= allowed_after_pause.saturating_add(5),
            "wire budget rebased preemption beyond the outer watchdog: wire={budget_ms}ms allowed={allowed_after_pause}ms"
        );
    }
    use std::os::unix::net::UnixListener;
    use std::thread;

    fn unique_socket(tag: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tag = &tag[..tag.len().min(8)];
        PathBuf::from("/tmp").join(format!("rsc-{tag}-{nonce:x}.sock"))
    }

    #[test]
    fn partial_mutation_reply_is_unknown_and_queryable_by_event_id() {
        let socket = unique_socket("partial-mutation");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            assert!(request.contains("fact-partial-control"));

            // Negative control: the mutation may have committed, but its reply
            // is cut mid-frame. The client must not call this daemon death or
            // invite a blind retry.
            let mut writer = &stream;
            writer
                .write_all(b"{\"ok\":{\"kind\":\"append_fact\",\"fact\":")
                .unwrap();
            writer.flush().unwrap();
        });

        let routed = RoutedRoomStore {
            socket: socket.clone(),
            repo_root: PathBuf::from("/repo"),
            active_engagement: "test".to_string(),
            cursor_path: PathBuf::from("/repo/.rally/cursors.json"),
            log_dir: PathBuf::from("/repo/.rally/log"),
            claim_index_path: PathBuf::from("/repo/.rally/claim-index.json"),
        };
        let error = routed
            .dispatch(StoreOp::AppendFact {
                fact: serde_json::json!({"event_id": "fact-partial-control"}),
            })
            .unwrap_err();

        server.join().unwrap();
        std::fs::remove_file(&socket).ok();
        assert!(matches!(
            error,
            RallyError::OutcomeUnknown { ref event_id, ref phase, .. }
                if event_id == "fact-partial-control" && phase == "daemon-transport"
        ));
        let remedy = crate::locate_remedy("fact-partial-control");
        assert_eq!(
            shlex::split(&remedy).unwrap(),
            vec!["rally", "locate", "fact-partial-control", "--json"]
        );
        let err = error.to_string();
        assert!(err.contains("mutation-outcome-unknown"), "{err}");
        assert!(err.contains("event_id=fact-partial-control"), "{err}");
        assert!(err.contains("direct fallback is forbidden"), "{err}");
        assert!(!err.contains("daemon stopped"), "{err}");
        assert!(!err.contains("; retry"), "{err}");
    }

    #[test]
    fn wire_rejects_unqueryable_outcome_unknown_fields() {
        for error in [
            StoreError {
                code: 1,
                kind: StoreErrorKind::OutcomeUnknown,
                message: "missing id".to_string(),
                event_id: None,
                phase: Some("readback".to_string()),
            },
            StoreError {
                code: 1,
                kind: StoreErrorKind::OutcomeUnknown,
                message: "blank id".to_string(),
                event_id: Some("   ".to_string()),
                phase: Some("readback".to_string()),
            },
            StoreError {
                code: 1,
                kind: StoreErrorKind::OutcomeUnknown,
                message: "missing phase".to_string(),
                event_id: Some("opaque id $(safe)".to_string()),
                phase: None,
            },
            StoreError {
                code: 1,
                kind: StoreErrorKind::OutcomeUnknown,
                message: "blank phase".to_string(),
                event_id: Some("opaque id $(safe)".to_string()),
                phase: Some("\t".to_string()),
            },
        ] {
            assert!(matches!(
                store_error_to_rally_error(error),
                RallyError::IncompatibleWire { .. }
            ));
        }

        let hostile = "opaque id 'quoted' $(touch no);$HOME";
        let valid = store_error_to_rally_error(StoreError {
            code: 1,
            kind: StoreErrorKind::OutcomeUnknown,
            message: "reply lost".to_string(),
            event_id: Some(hostile.to_string()),
            phase: Some("daemon-transport".to_string()),
        });
        assert!(matches!(
            valid,
            RallyError::OutcomeUnknown { ref event_id, .. } if event_id == hostile
        ));
        assert_eq!(
            shlex::split(&crate::locate_remedy(hostile)).unwrap(),
            vec!["rally", "locate", hostile, "--json"]
        );
    }

    #[test]
    fn probe_rejects_a_v5_daemon_after_targeted_read_cutover() {
        assert_eq!(WIRE_VERSION, 6, "this control grades the v5 to v6 cutover");
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let rally_dir = std::env::temp_dir().join(format!("rally-wire-v2-{nonce}"));
        std::fs::create_dir_all(&rally_dir).unwrap();
        let socket = std::env::temp_dir().join(format!("rally-wire-v2-{nonce}.sock"));
        let listener = UnixListener::bind(&socket).unwrap();
        std::fs::write(
            rally_dir.join(ADDR_FILENAME),
            socket.to_string_lossy().as_bytes(),
        )
        .unwrap();

        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: StoreRequest = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(request.wire_version, 6);
            let response = StoreResponse::Ok(StoreOk::Pong {
                repo_root: "/expected/repo".to_string(),
                pid: 42,
                wire_version: 5,
            });
            let mut writer = &stream;
            writeln!(writer, "{}", serde_json::to_string(&response).unwrap()).unwrap();
        });

        let error = probe_identity(&rally_dir, "/expected/repo")
            .expect_err("a v5 daemon must fail a v6 client immediately");
        assert!(matches!(error, RallyError::IncompatibleWire { .. }));
        assert!(error.to_string().contains("client speaks 6"));
        assert!(error.to_string().contains("daemon speaks 5"));
        server.join().unwrap();
        std::fs::remove_file(&socket).ok();
        std::fs::remove_dir_all(&rally_dir).ok();
    }

    #[test]
    fn wire_rejects_malformed_append_outcome_invariants() {
        let fact = serde_json::json!({
            "schema": crate::FACT_SCHEMA,
            "event_id": "wire-outcome",
            "seq": 1,
            "thread_id": "wire-thread",
            "kind": "decision",
            "tool": "test:01",
            "subject": "wire outcome",
            "scope": [],
            "created_at": "2026-08-10T00:00:00Z",
            "evidence": []
        });
        let uncommitted = serde_json::json!({
            "fact": fact,
            "committed": false,
            "projection_complete": true,
            "warnings": []
        });
        let error = serde_json::from_value::<AppendOutcome>(uncommitted)
            .expect_err("wire must not construct a successful uncommitted outcome");
        assert!(error.to_string().contains("committed=true"));

        let inconsistent = serde_json::json!({
            "fact": serde_json::json!({
                "schema": crate::FACT_SCHEMA,
                "event_id": "wire-outcome-2",
                "seq": 2,
                "thread_id": "wire-thread",
                "kind": "decision",
                "tool": "test:01",
                "subject": "wire outcome",
                "scope": [],
                "created_at": "2026-08-10T00:00:00Z",
                "evidence": []
            }),
            "committed": true,
            "projection_complete": true,
            "warnings": [{"code": "facts_db", "message": "degraded"}]
        });
        let error = serde_json::from_value::<ConditionalAppendOutcome>(serde_json::json!({
            "status": "applied",
            "outcome": inconsistent
        }))
        .expect_err("conditional wire outcome must enforce nested invariants");
        assert!(error.to_string().contains("projection_complete"));
    }

    #[test]
    fn routed_snapshot_without_server_fingerprint_is_usable_but_not_cacheable() {
        let socket = unique_socket("snapshot-no-fingerprint");
        let listener = UnixListener::bind(&socket).unwrap();
        let expected = RoomSnapshot::default();
        let wire_snapshot = store::snapshot_to_wire_value(&expected).unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let request: StoreRequest = serde_json::from_str(request.trim()).unwrap();
            assert!(matches!(
                request.op,
                StoreOp::SnapshotWithArchived {
                    include_archived: false
                }
            ));
            let response = StoreResponse::Ok(StoreOk::Snapshot {
                snapshot: wire_snapshot,
                fingerprint: None,
            });
            let mut writer = &stream;
            writer
                .write_all(format!("{}\n", serde_json::to_string(&response).unwrap()).as_bytes())
                .unwrap();
            writer.flush().unwrap();
        });

        let cache_root = std::env::temp_dir().join(format!(
            "routed-snapshot-cache-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let routed = RoutedRoomStore {
            socket: socket.clone(),
            repo_root: cache_root.clone(),
            active_engagement: "test".to_string(),
            cursor_path: cache_root.join(".rally/cursors.json"),
            log_dir: cache_root.join(".rally/log"),
            claim_index_path: cache_root.join(".rally/claim-index.json"),
        };
        let capture = routed.snapshot_cache_capture(false).unwrap();
        assert!(capture.fingerprint.is_none());
        assert_eq!(
            store::snapshot_to_wire_value(&capture.snapshot).unwrap(),
            store::snapshot_to_wire_value(&expected).unwrap()
        );
        store::write_snapshot_cache_for(&cache_root, &capture);
        assert!(!cache_root.join(".rally/snapshot.cache.json").exists());
        assert!(
            !cache_root.join(".rally/mutation.lock").exists(),
            "a routed snapshot capture must not acquire a client-side mutation lock"
        );

        server.join().unwrap();
        std::fs::remove_file(&socket).ok();
        std::fs::remove_dir_all(&cache_root).ok();
    }
}
