<!-- SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Changelog

All notable changes to Agent Rally Point are documented here.

## Unreleased

### Added — Zombie-tmux prevention: three layers over one liveness model

Stops accreted zombie `rally-*` tmux sessions at the source instead of relying on a
clock. Root cause: rally `exec`s the agent, so a session auto-closes when its agent
EXITS — but agents that never exit (a disabled autonomy poller, idle detached panes)
leave the session forever, and tmux has no native idle/lifetime timeout. All three
layers REUSE the single `liveness::is_live` 4-signal model + adaptive window; none
adds a fixed idle clock.

- **Layer 1 — completion-scoped self-exit.** New `rally self-exit-check --tool
  <self> [--persistent] [--required-streak N]`: a task-scoped session that holds no
  active claims AND for which `rally next` is non-actionable for a SUSTAINED streak
  (default 2, persisted in the session's own `RALLY_SELFEXIT_STREAK` tmux env so it
  dies with the session) self-kills its own tmux session → `exec` auto-closes it.
  `--persistent` opts a deliberately-long-lived session out of the implicit "work
  done" path; `rally stop` remains the explicit path. Decision:
  `liveness::completion_self_exit_eligible`.
- **Layer 2 — event-driven liveness-lease safety net.** `rally enter` now
  opportunistically sweeps detached `rally-*` orphan tmux sessions (in addition to
  `rally sessions --reap`), via one shared actuator (`sweep_orphan_tmux`).
  Best-effort + fail-open: runs after presence, never blocks enter, never raises,
  never reaps a live / parent-alive session. No daemon/cron.
- **Layer 3 — parent-lifecycle binding.** `tmux_start_command` stamps
  `RALLY_PARENT_PID=<launcher pid>` into the new session's env in the same atomic
  `tmux new-session -e` call. The reaper reads it back, probes `kill -0 <pid>` (no
  new crate dependency), and feeds the result to the single shared
  `liveness::reapable(liveness, parent_alive)` authority.
- **One reaper-eligibility authority** `liveness::reapable` (mirrored Rust↔Python;
  `liveness_vectors.json` gains `reapable_cases` + `self_exit_cases`). Fail-safe:
  Live/Unknown are NEVER reaped; parent-dead reaps only a session ALSO `Stale` by
  liveness; missing parent info degrades to the window criterion alone (prior
  orphan behavior, unchanged); `kill -0` non-ESRCH failures read ALIVE.
- Tests: `liveness.rs` parity + dedicated `reapable`/`self_exit` cases;
  `backends.rs` Layer-3 classifier cases (dead-parent reaped, live-parent kept,
  code-progressing-with-dead-parent kept, missing-info window fallback, `kill -0`
  self/dead probe).

### Added — Adaptive, multi-signal session liveness (squad-projection decay + tmux orphan reaper)

Replaces fixed staleness cutoffs for the squad/presence projection with liveness
that ADAPTS to each session's planned heartbeat cadence and weighs four signals.

- **One liveness function** (`src/liveness.rs`, mirrored in build-loop's
  `scripts/rally_point/liveness.py`). Staleness is RELATIVE to the declared
  cadence: `window = planned_interval * MISS_MULTIPLIER + GRACE`. Defaults
  `DEFAULT_CADENCE_SECS=300`, `MISS_MULTIPLIER=6`, `GRACE_SECS=60` → a 5-min
  cadence is stale at ~31 min (≈6 missed beats); a 5-hour cadence not until
  ~30 h. Exposed as `.rally/config.json` `coordination{}` tunables
  (`default_cadence_secs`, `miss_multiplier`, `grace_secs`) + `RALLY_*` env.
- **Four signals — LIVE if ANY is fresh within the adaptive window:**
  (a) heartbeat/presence age, (b) inject/ack (receipt/wake/handoff naming the
  tool), (c) forward code progress (the tool's worktree branch HEAD moved
  between its two newest presence facts), (d) declared active work (a live claim
  or authored mission/handoff).
- **Squad-projection decay (the gap fix).** `snapshot_from_facts_with_policy`
  now DROPS a squad whose four signals are ALL provably stale from the default
  room view; `--include-archived` restores it (mirrors the message archive
  model). **FAIL-OPEN:** a Live OR Unknown (any absent/unparseable signal)
  verdict KEEPS the squad visible — hiding a still-alive peer is the dangerous
  direction (it could cause the very write-collision this system prevents). This
  is deliberately the OPPOSITE fail-direction from the reaper's fail-CLOSED
  removal path.
- **tmux orphan reaper.** `rally sessions --reap` now also detects DETACHED
  `rally-*` tmux sessions whose last activity is past the adaptive window and
  which are not tracked as managed sessions, kills them, and tombstones the
  reap. Closes the gap where `--reap` saw 0 of the real detached orphans.
- **`rally stop` self-kill.** On stop, if the stopping process is itself inside
  a `rally-*` tmux session distinct from the managed target, it kills that
  session too (contain at source — it can never become an orphan).
- Parity double-pinned by the byte-identical golden fixture
  `tests/fixtures/liveness_vectors.json` (identical copy in build-loop) +
  the `_provenance.json` drift manifest.

### Added — In-room stale-state REAPER + heartbeat parity + session-end self-release

Three new actuators that make coordination state self-cleaning:

- **`rally doctor --reap-stale` (REAPER).** New sub-command (with `--apply` to
  commit writes, dry-run by default) that physically removes over-TTL claims and
  stale squad-lead leases. Implemented in `crates/rally-cli/src/reaper.rs`.
  Composes existing eligibility functions (`claim_reclaim_eligible`,
  `takeover_eligible_owners`) without reimplementing staleness math.

  **Dual-signal eligibility (2026-06-22 fix):** a claim is now reaped when
  EITHER (1) its owner-squad is >timeout stale (`claim_reclaim_eligible`) OR
  (2) its own `lease_expires_at` evidence timestamp has provably passed
  (`claim_authority::expired_claims`). This closes the shared-tool-identity
  gap: a claim owned by an identity like `claude_code` (which appears "live"
  because the current session IS that identity) is still reaped when its
  individual lease has expired. Both signals are fail-closed: an unparseable
  owner timestamp keeps the claim, and an unparseable or missing
  `lease_expires_at` keeps the claim. The union preserves every
  future-dated-lease claim. Each reaped claim now carries a `reason` field
  in `ReapedClaim`: `"owner-stale"` | `"lease-expired"` |
  `"owner-stale+lease-expired"`.

  FAIL-CLOSED on any unparseable owner timestamp or lease (inherited
  guarantees from both composing functions). Race-safe: appends via
  `append_fact_verified` under the existing mutation lock. Idempotent: a
  re-run finds nothing eligible because `active_claims` projects only open
  claims. Output: `ReapReport { claims_reaped, squads_idle_cleared,
  lead_relinquished, preserved_future_or_active, applied }`.

- **Session-end self-release (LEVER 3).** `rally stop` now self-releases all
  active claims owned by the stopping tool before removing the session record.
  Self-release is authoritative (bypasses the 2h reclaim bar — the owner is
  declaring itself done), keeps SEC-001 dormant (no stale-owner evidence marker),
  and is best-effort (never blocks the stop path). Implemented inside the
  `SessionAction::Stop` arm in `lib.rs`.

- **Heartbeat parity fixture.**
  `crates/rally-cli/tests/fixtures/heartbeat_parity_vectors.json` — a new
  golden-vector file (byte-identical to the build-loop mirror at
  `scripts/rally_point/heartbeat_parity_vectors.json`) asserting that a
  `claude_code` session and a `codex` session that emit presence/heartbeat at the
  same age decay IDENTICALLY (heartbeat is tool-agnostic, curve is shared).
  Validated by a new `reaper::tests::heartbeat_parity_vectors_match_expected` test
  checking each vector against `decay::recency_weight` and the stale-at-15m
  verdict to 1e-4 precision.

### Test coverage (reaper.rs)

10 `#[cfg(test)]` cases inside `reaper::tests`:
(a) over-TTL claim is staged + leaves `active_claims` after apply;
(b) unparseable owner ts is never staged (fail-closed);
(c) fresh-owner claim is not staged;
(d) idempotent: second run finds nothing;
(e) stop self-release only releases the stopping tool's claims, not peers';
(f) heartbeat parity vectors match expected weight and stale verdict;
(g) **lease-expired claim with live owner IS reaped** (dual-signal fix, the 76-claim case);
(h) **future-lease claim with live owner is preserved** (the 9-claim keep case);
(+2) dry-run writes no facts; `squads_idle_cleared` enumerates stale owners.

Verified: `cargo build -p rally-cli && cargo test -p rally-cli` (349 pass, 0 fail).

---

### Added — Coordination recency decay + size-scaled lead/ownership auto-reclaim

A single shared coordination policy now governs two behaviors. All tunables live
under a `"coordination"` object in `.rally/config.json` (default → user → repo →
env precedence, mirroring `hooks`). The math is the single source of truth in the
new `crates/rally-cli/src/decay.rs`.

- **Time-based recency decay.** Every coordination message (fact) gets a
  continuously-computed weight from its age, `weight = 0.5 ^ (age_hours /
  half_life)` (exponential half-life, default 48h). `rally room` orders the
  decision / risk / artifact buckets fresh-first by weight; `rally recent` and
  `rally next` inherit recency ordering. A message whose weight falls below the
  archive floor (default `0.05`, ≈14d) is moved OUT of the active view into
  `stale_facts` (losslessly — the raw segments stay on disk). Re-include
  decayed messages with `rally room --include-archived` / `rally recent
  --include-archived`. Active state (claims, blockers, open handoffs) is never
  decayed — only historical message buckets. Tunables: `half_life_hours`,
  `archive_floor_weight` (env `RALLY_HALF_LIFE_HOURS`, `RALLY_ARCHIVE_FLOOR`).
- **Size-scaled lead/ownership auto-reclaim.** A stale owner's claim becomes
  reclaimable on a timeout that SCALES with the claimed work: a single-file
  claim after the small timeout (default 30m), a multi-file / directory / repo /
  task claim after the large timeout (default 2h — equal to the prior flat
  `TAKEOVER_STALE_SECS`, so coarse claims keep their existing grace window).
  Size is derived from the claim's existing `ResourceType` breadth + scope
  count (no new claim metadata). The size-scaled window also sets the claim's
  `lease_expires_at` evidence at claim time. The destructive reclaim path
  (`command_release_by_path`) records the reason + size class in the release
  fact's provenance (`reclaim-reason:stale-by-timeout;work-size=…`). Tunables:
  `reclaim_small_minutes`, `reclaim_large_minutes` (env
  `RALLY_RECLAIM_SMALL_MINUTES`, `RALLY_RECLAIM_LARGE_MINUTES`).
- **Preserved invariants.** Reclaim stays race-safe (the `.rally/mutation.lock`
  flock is untouched) and FAIL-CLOSED: an owner whose `last_seen_ts` is missing
  or unparseable is never reclaimable. Recency decay fails OPEN: a message with
  an unparseable `created_at` is treated as fresh and never hidden.
- **Behavior change to note.** A single-file claim that previously had the flat
  2h takeover grace is now reclaimable after 30m by default — an intentional
  tightening of narrow claims (multi-file / coarse claims are unchanged).

Verified: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
(workspace green; new unit + integration coverage for the reference decay ages,
the archive-floor boundary, and the small/large reclaim timing incl. fail-closed).

## 0.1.2 — Binary auto-provision hardening (2026-06-11)

Hardening of `hooks/ensure-rally-binary.sh` across five rounds of dual-vendor
adversarial review (Fable + Codex), all verified under stock macOS `/bin/bash`
3.2:

- **Verified downloads.** A downloaded binary is SHA256-verified against the
  release's `<asset>.sha256` before it is made executable; the download path is
  fail-closed (a mismatch OR an unverifiable download is rejected, never run —
  cargo-from-source is the fallback). Releases now publish per-asset `.sha256` +
  a sigstore build-provenance attestation (`gh attestation verify`).
- **Never blocks the session.** All network + compiler work runs in one
  backgrounded, fd-detached worker; local liveness probes are time-bounded
  (perl/setsid shim) so even a hung or crashing on-disk binary can't stall the
  hook. A signal-killed `version` probe is treated as failure on every
  acceptance path (PATH, `~/.local/bin`, shipped, cached, downloaded).
- **Concurrency.** A single atomic pid lock (noclobber + verify-after-write,
  parent writes the worker's real pid — `$BASHPID`-independent for bash 3.2);
  the `building` short-circuit is gated on worker-pid liveness so a crashed
  worker no longer wedges provisioning.
- **Charter robustness.** Unset `HOME`, a corrupt state file, and a missing
  script dir no longer abort under `set -euo pipefail` — exit 0 always. A
  checksum mismatch records a durable trace to `download-rejections.log`.

## 0.1.1 — Plugin auto-launch + hooks policy (2026-06-11)

- **Auto-launch on install.** `.claude-plugin/marketplace.json` makes the repo a self-hosting single-plugin marketplace, so `claude plugin marketplace add tyroneross/agent-rally-point` + `claude plugin install` work; hooks and skills activate on install.
- **Binary auto-provision.** `hooks/ensure-rally-binary.sh` provisions the `rally` CLI on first SessionStart (present-check → shipped prebuilt → GitHub-release download → backgrounded `cargo build` → advisory). `.github/workflows/release.yml` builds per-triple binaries on tag.
- **Offer-on-first-session.** In a git repo without `.rally/`, the SessionStart hook surfaces a one-time `rally init` offer instead of silently no-opping; it never auto-creates `.rally/` (no-litter charter preserved). Repos with `.rally/` keep full auto-coordination.
- **`rally hooks` policy command** (`status|on|off|prompt`) with session/repo/user/default resolution and `RALLY_HOOKS=off` opt-out.

- Hardened the fan-out path: `workstream-lint.mjs` now also rejects shell-unsafe `output` and `owns`/`id` chars; the empirical packet gate runs in CI (`rally-gate.yml` builds the release binary + runs the node suite); empty `--parent-step` values no longer write phantom DAG edges; and inject sanitization is hoisted to the `inject_commands` chokepoint so every backend (tmux + cmux) is covered.

### `packet.mjs` fan-out now generates CLI-executable rally commands (2026-06-09)

Fixes four findings where the emitted fan-out packet named rally markers that the
real CLI rejected — escaped review because the tests asserted marker *presence*,
not executability against the binary.

- **Repeated `--path` on `before-write` (HIGH).** `renderRallyLoop` emitted one
  `rally check before-write --tool <t> --path a --path b --strict` line, but
  `rally check` rejects repeated `--path`. A multi-owns task failed at its
  before-write step. Now emits one before-write line per owned path. The claim
  line is unchanged — `rally say --path` is repeatable.
- **Repeated `--parent-step` on `rally say` (HIGH).** A task with ≥2 `depends_on`
  emitted one `--parent-step` per dep, but `rally say` accepted at most one, so
  the task failed at its first command. Durable fix in the CLI:
  `SayArgs.parent_step_id` (`Option`) → `parent_step_ids` (`Vec`), one
  `parent-step:<id>` scope marker written per value, and `dag.rs` now extracts
  every marker (one DAG edge per parent). Zero/one value behaves exactly as
  before; existing ledger facts parse unchanged.
- **Test fixture didn't exercise the multi case (MEDIUM).** Added a
  2-path-owns/2-`depends_on` fixture and an empirical gate
  (`packet-empirical.test.mjs`) that dry-runs the emitted claim + before-write
  lines against the built release binary in a throwaway rally room and asserts
  `rally dag` shows two parent edges — so flag-arity drift fails tests.
- **Shell-unsafe descriptor fields (LOW).** `workstream-lint.mjs` now rejects
  `"`, `$`, or backtick in `intent` (break the emitted `--subject` quoting) and
  whitespace in any `owns` path (would split into multiple `--path` tokens).

### `rally inject` now actually submits + waits for an ACK (2026-06-09)

Fixes the long-recurring "inject delivered but never ACKed" signature (L5 /
`incident-rally-inject-not-acked`). Two independent root causes, both repaired:

- **Submit semantics (tmux fallback).** The tmux inject path built FOUR separate
  commands — `C-u`, `set-buffer`, `paste-buffer`, then a SEPARATE `send-keys
  Enter` — and that separate Enter never submitted against Codex's bracketed-
  paste TUI: the message landed in the input box and sat at the prompt. It now
  ships the whole frame as ONE atomic `send-keys -t <t> -H <hex…>` write —
  `ESC[200~ <text> ESC[201~` followed by a CR placed AFTER the close marker so
  it submits rather than pasting as literal text (ported from ptyd `frame_line`,
  `src/comms.rs` §4.1/§4.2, no path dependency). `C-u` stays a discrete clear.
  cmux keeps its separate-submit sequence (no raw-byte `send`); documented inline.
- **ACK wait never ran (watchdog pre-emption).** `inject --handoff
  --timeout-seconds 75` returned a bare `{ok:true,product:rally}` immediately —
  not because the `InjectData` envelope was missing (it was already built with
  `delivery_state`/`ack_state`/`fallback_plan` and polled the ledger via
  `wait_for_resolution`), but because the global 3s-default / 60s-max wall-clock
  watchdog killed the process before the 75s ACK poll could run, emitting the
  neutral fail-open payload. `inject` — the one deliberately-blocking interactive
  verb — now sizes its watchdog from `--timeout-seconds` + headroom (ceiling
  605s), bypassing the 60s hook cap. An explicit `--timeout-ms` /
  `RALLY_HOOK_TIMEOUT_MS` override still wins (clamped to the hook band); all
  other (hook-invoked) commands keep the 3s default unchanged.

Follow-up (spec only): [`docs/PLAN-daemon-first-inject-routing.md`](docs/PLAN-daemon-first-inject-routing.md)
describes the daemon-first routing this framed tmux write is the fallback for.

### Reliability & performance — store durability for scale (2026-06-04)

Foundation for durable coordination at thousands of agents. Commits `5c68dac`..`32d21be`.
See [`docs/SCALE-ROADMAP.md`](docs/SCALE-ROADMAP.md) for the measured roadmap.

- **facts.db corruption is now a non-event.** A malformed/missing `facts.db` is quarantined
  (`facts.db.corrupt.<ts>`) and the derived cache is rebuilt from the canonical JSONL ledger —
  zero history loss. Handles header (`SQLITE_NOTADB`), mid-page (`SQLITE_CORRUPT`), extended
  corruption codes, and torn trailing ledger lines (crash mid-append). Resolves the 2026-06-01
  easy-terminal `facts.db.corrupt` incident.
- **O(1) happy-path reconcile.** `reconcile_segments_and_db` no longer runs an O(N) segment scan +
  full SQLite load on every open/append; a disposable fingerprint sidecar
  (`.rally/.reconcile-cache.json`, deterministic FNV-1a) short-circuits when in sync, falling
  through to the authoritative scan+rebuild on any drift. Measured flat (150µs at n=200 and n=4000).
- **Active-segment-first R9 readback** and **thread-aware open jitter** (replaces constant `pid%17`),
  hardening concurrent opens and making the parallel test suite deterministic (flake 25% → 0%).

### Verification (2026-06-04)

- `cargo test --workspace` (0 failures); 12–30× parallel determinism runs green.
- `cargo clippy --package rally-cli --lib --no-deps -- -D warnings` clean.
- `independent-auditor` pass per integrity-critical change (caught a cross-process cache-key bug).
- End-to-end: real `rally room` recovers a corrupted store; `scripts/scale_reliability_test.sh`
  `SILENT_LOSS=0` at N≤128.

### Changed

- Cut the product architecture over to Rust. The user-facing command is `rally`.
- Removed the legacy Python runtime package, Python packaging metadata, and
  legacy discovery/migration documentation.
- Kept the durable product contract centered on `changes.jsonl`, portable
  events, signed trust, sync packets, and stable JSON command envelopes.

### Verification

- `cargo test`
- `cargo clippy --all-targets -- -D warnings`
- `git diff --check`
