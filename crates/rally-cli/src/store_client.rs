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

use crate::claim_authority::{self, ActiveClaimRecord};
use crate::error::{RallyError, Result};
use crate::store::{self, Fact, ReadReceipt, RoomSnapshot};

/// Discovery pointer filename inside `.rally/` (L7/ADR-02) — the daemon's
/// SOLE discovery mechanism; clients never guess or hardcode the socket path.
/// Duplicated as a literal rather than imported from `rallyd_core.rs`
/// (Chunk B's owned file, edited concurrently in this same parallel window):
/// the STRING is the frozen wire contract, not the module that happens to
/// hold the daemon's own copy of it.
const ADDR_FILENAME: &str = "rallyd.sock.addr";

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
/// connection, a timeout, or a version/root mismatch — the router treats all
/// of these identically ("no live daemon"), never as an error. This is
/// deliberately narrow: it is NOT where R6's mid-command dead-socket policy
/// lives (that's [`RoutedRoomStore::dispatch`], reached only AFTER routing
/// has already begun).
pub(crate) fn probe_identity(rally_dir: &Path, expected_repo_root: &str) -> Option<StoreIdentity> {
    let addr_path = rally_dir.join(ADDR_FILENAME);
    let socket_text = std::fs::read_to_string(&addr_path).ok()?;
    let socket = PathBuf::from(socket_text.trim());
    if socket.as_os_str().is_empty() || !socket.exists() {
        return None;
    }
    let req = StoreRequest::new(None, StoreOp::Ping);
    let reply = round_trip(&socket, &req, PROBE_TIMEOUT, true).ok()?;
    match reply {
        StoreResponse::Ok(StoreOk::Pong {
            repo_root,
            pid,
            wire_version,
        }) => {
            if wire_version != WIRE_VERSION || repo_root != expected_repo_root {
                return None;
            }
            Some(StoreIdentity {
                repo_root,
                pid,
                wire_version,
                socket,
            })
        }
        _ => None,
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
) -> Option<RoutedRoomStore> {
    let canonical = store::canonical_repo_root_string(repo_root);
    let identity = probe_identity(rally_dir, &canonical)?;
    let resolved_engagement = store::resolve_active_engagement_with_env(rally_dir, engagement);
    Some(RoutedRoomStore {
        socket: identity.socket,
        repo_root: repo_root.to_path_buf(),
        active_engagement: resolved_engagement,
        cursor_path: rally_dir.join("cursors.json"),
        log_dir: rally_dir.join(store::LOG_DIRNAME),
        claim_index_path: rally_dir.join(claim_authority::CLAIM_INDEX_FILENAME),
    })
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
) -> Option<RoutedRoomStore> {
    let deadline = Instant::now() + bound;
    loop {
        if let Some(routed) = probe_live(repo_root, rally_dir, engagement.clone()) {
            return Some(routed);
        }
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        std::thread::sleep(CORRIDOR_RETRY_SLEEP.min(deadline.saturating_duration_since(now)));
    }
}

/// Stable query context for a mutation whose wire outcome is unknown.
struct MutationQuery {
    operation: &'static str,
    selector: Option<String>,
}

/// Classify the closed wire operation set. This classification lives beside
/// dispatch so adding a mutating protocol variant cannot silently inherit the
/// old blind-retry behavior.
fn mutation_query(op: &StoreOp) -> Option<MutationQuery> {
    let fact_selector = |fact: &Value| {
        fact.get("event_id")
            .and_then(Value::as_str)
            .map(|event_id| format!("event_id={event_id}"))
    };
    match op {
        StoreOp::AppendFact { fact } => Some(MutationQuery {
            operation: "append_fact",
            selector: fact_selector(fact),
        }),
        StoreOp::AppendFactVerified { fact } => Some(MutationQuery {
            operation: "append_fact_verified",
            selector: fact_selector(fact),
        }),
        StoreOp::AppendStateTransitionVerified { fact } => Some(MutationQuery {
            operation: "append_state_transition_verified",
            selector: fact_selector(fact),
        }),
        StoreOp::AppendSessionFactIfContext { fact, .. } => Some(MutationQuery {
            operation: "append_session_fact_if_context",
            selector: fact_selector(fact),
        }),
        StoreOp::RebuildClaimIndex => Some(MutationQuery {
            operation: "rebuild_claim_index",
            selector: None,
        }),
        StoreOp::RenewClaimLease { claim_id, .. } => Some(MutationQuery {
            operation: "renew_claim_lease",
            selector: Some(format!("claim_id={claim_id}")),
        }),
        StoreOp::ExpireClaimLeasesAt { now_rfc3339 } => Some(MutationQuery {
            operation: "expire_claim_leases_at",
            selector: Some(format!("expiry_cutoff={now_rfc3339}")),
        }),
        StoreOp::MaybeAppendReadCheckpoint { tool, read_seq } => Some(MutationQuery {
            operation: "maybe_append_read_checkpoint",
            selector: Some(format!("tool={tool},read_seq={read_seq}")),
        }),
        StoreOp::Facts
        | StoreOp::SessionFactsWithContextVersion
        | StoreOp::SnapshotWithArchived { .. }
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
    /// Send one op over the wire, carrying this client's already-resolved
    /// engagement label (L9) with EVERY request — the daemon's per-request
    /// `set_engagement_scope` rebind depends on it being present on every op,
    /// not just appends.
    fn dispatch(&self, op: StoreOp) -> Result<StoreOk> {
        self.dispatch_with_engagement(self.active_engagement.clone(), op)
    }

    fn dispatch_with_engagement(&self, engagement: String, op: StoreOp) -> Result<StoreOk> {
        let req = StoreRequest::new(Some(engagement), op);
        match round_trip(&self.socket, &req, OP_TIMEOUT, false) {
            Ok(StoreResponse::Ok(ok)) => Ok(ok),
            Ok(StoreResponse::Err(err)) => Err(store_error_to_rally_error(err)),
            Err(io_err) => Err(transport_error(&req.op, &io_err)),
        }
    }

    // ----- ROUTED methods (one StoreOp round trip each) ----------------------

    pub(crate) fn append_fact(&self, fact: &Fact) -> Result<Fact> {
        match self.dispatch(StoreOp::AppendFact {
            fact: to_value(fact)?,
        })? {
            StoreOk::AppendFact { fact } => from_value(fact),
            _ => Err(unexpected_reply("append_fact")),
        }
    }

    pub(crate) fn append_fact_verified(&self, fact: &Fact) -> Result<Fact> {
        match self.dispatch(StoreOp::AppendFactVerified {
            fact: to_value(fact)?,
        })? {
            StoreOk::AppendFactVerified { fact } => from_value(fact),
            _ => Err(unexpected_reply("append_fact_verified")),
        }
    }

    pub(crate) fn append_state_transition_verified(&self, fact: &Fact) -> Result<Fact> {
        match self.dispatch(StoreOp::AppendStateTransitionVerified {
            fact: to_value(fact)?,
        })? {
            StoreOk::AppendStateTransitionVerified { fact } => from_value(fact),
            _ => Err(unexpected_reply("append_state_transition_verified")),
        }
    }

    pub(crate) fn append_session_fact_if_context(
        &self,
        fact: &Fact,
        expected_context_version: Option<u64>,
    ) -> Result<Option<Fact>> {
        match self.dispatch(StoreOp::AppendSessionFactIfContext {
            fact: to_value(fact)?,
            expected_context_version,
        })? {
            StoreOk::AppendSessionFactIfContext { fact } => fact.map(from_value).transpose(),
            _ => Err(unexpected_reply("append_session_fact_if_context")),
        }
    }

    pub(crate) fn facts(&self) -> Result<Vec<Fact>> {
        match self.dispatch(StoreOp::Facts)? {
            StoreOk::Facts { facts } => facts.into_iter().map(from_value).collect(),
            _ => Err(unexpected_reply("facts")),
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
    pub(crate) fn renew_claim_lease(
        &self,
        claim_id: &str,
        lease_expires_at: String,
    ) -> Result<Option<ActiveClaimRecord>> {
        match self.dispatch(StoreOp::RenewClaimLease {
            claim_id: claim_id.to_string(),
            lease_expires_at,
        })? {
            StoreOk::RenewClaimLease { record } => record.map(from_value).transpose(),
            _ => Err(unexpected_reply("renew_claim_lease")),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn expire_claim_leases_at(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<Fact>> {
        match self.dispatch(StoreOp::ExpireClaimLeasesAt {
            now_rfc3339: now.to_rfc3339(),
        })? {
            StoreOk::ExpireClaimLeasesAt { facts } => facts.into_iter().map(from_value).collect(),
            _ => Err(unexpected_reply("expire_claim_leases_at")),
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
            StoreOk::Snapshot { snapshot } => snapshot_from_value(snapshot),
            _ => Err(unexpected_reply("snapshot_with_archived")),
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
            StoreOk::Snapshot { snapshot } => snapshot_from_value(snapshot),
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
        tool: &str,
        read_seq: i64,
    ) -> Result<Option<Fact>> {
        match self.dispatch(StoreOp::MaybeAppendReadCheckpoint {
            tool: tool.to_string(),
            read_seq,
        })? {
            StoreOk::MaybeAppendReadCheckpoint { fact } => fact.map(from_value).transpose(),
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
        let err = routed
            .dispatch(StoreOp::AppendFact {
                fact: serde_json::json!({"event_id": "fact-partial-control"}),
            })
            .unwrap_err()
            .to_string();

        server.join().unwrap();
        std::fs::remove_file(&socket).ok();
        assert!(err.contains("outcome unknown"), "{err}");
        assert!(err.contains("event_id=fact-partial-control"), "{err}");
        assert!(err.contains("Query `rally room --json`"), "{err}");
        assert!(err.contains("direct fallback is forbidden"), "{err}");
        assert!(!err.contains("daemon stopped"), "{err}");
        assert!(!err.contains("; retry"), "{err}");
    }

    #[test]
    fn probe_rejects_a_v2_daemon_after_the_scoped_snapshot_wire_change() {
        assert_eq!(WIRE_VERSION, 3, "this control grades the v2 to v3 cutover");
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
            assert_eq!(request.wire_version, 3);
            let response = StoreResponse::Ok(StoreOk::Pong {
                repo_root: "/expected/repo".to_string(),
                pid: 42,
                wire_version: 2,
            });
            let mut writer = &stream;
            writeln!(writer, "{}", serde_json::to_string(&response).unwrap()).unwrap();
        });

        assert!(
            probe_identity(&rally_dir, "/expected/repo").is_none(),
            "a v2 daemon must not route a v3 client through a reply that lacks scoped snapshots"
        );
        server.join().unwrap();
        std::fs::remove_file(&socket).ok();
        std::fs::remove_dir_all(&rally_dir).ok();
    }
}
