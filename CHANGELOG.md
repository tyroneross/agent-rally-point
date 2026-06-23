<!-- SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Changelog

All notable changes to Agent Rally Point are documented here.

## Unreleased

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
