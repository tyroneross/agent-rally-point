# Build-loop plan: iOS Agent Cockpit — 24h autonomous run

**Spec:** `docs/superpowers/specs/2026-05-31-ios-agent-cockpit-design.md`
**Mode:** build-loop autonomous, ~24h wall-clock, **no questions after approval**
**Branch:** `feat/agent-cockpit` (off `main`)
**Date:** 2026-05-31

---

## 0. Autonomy contract (read first — governs every chunk)

Because there are **no questions after approval**, the executing loop obeys these
rules deterministically:

1. **Resolve, don't ask.** Every choice the loop would normally raise is resolved
   by the labeled assumptions in §1 or the default in the chunk. Log the choice in
   the commit message; never pause.
2. **Blocker → skip, don't stop.** If a chunk needs hardware / an Apple account /
   network not present (device, signing, tailnet, APNs), mark it
   `BLOCKED:NEEDS-HUMAN <reason>` in `docs/plans/DEFERRED.md`, leave the interface
   compiling with a stub, and continue to the next unblocked chunk.
3. **Verification is mandatory for "done".** A chunk is `done` only when its named
   verification command exits 0. If verification cannot run (deferred surface),
   the chunk is `TAG:UNTESTED` and says so — never claimed working.
4. **No live-credit burn.** Real `claude`/`codex` invocations are gated behind
   `COCKPIT_LIVE=1`, default **off**. All adapter tests use mock CLIs. At most one
   optional gated live smoke per adapter, run once, near the end.
5. **Checkpoint commits.** Commit after every verified chunk with a
   `feat(cockpit): <chunk-id> <summary>` message + the verification line.
6. **Order.** Follow dependency order (A→B→C→D→E). If toolchain install (A0) fails,
   pivot to the iOS track (D*, needs only Xcode) and mark the Rust track blocked.
7. **Final gate.** Before the closing report, re-run full `cargo test --workspace`
   and `xcodebuild test` (simulator); report pass/fail verbatim, list every
   `DEFERRED`/`UNTESTED` item, and update the rally ledger with artifacts.
8. **Validation cleanup register.** Any dev shortcut (mock CLIs, dev-token auth,
   simulator-only paths) is logged as `[CLEANUP]` in `DEFERRED.md` with its
   verify-gone condition.

## 1. Labeled assumptions (TAG:ASSUMED)

- **A.** Rust daemon drives Claude via headless `claude -p --output-format
  stream-json --input-format stream-json` subprocess (no official Rust Agent SDK),
  and Codex via `codex app-server` JSON-RPC (fallback `codex exec --json`).
- **B.** `rustup`/`cargo` install is permitted in chunk A0.
- **C.** Transport v1 = WebSocket on `127.0.0.1` + dev bearer token. Tailnet,
  Secure-Enclave mTLS, Face ID, APNs, CloudKit are **deferred + stubbed behind
  interfaces** (hardware/account-gated) and marked `TAG:UNTESTED`.
- **D.** New crates `crates/cockpitd` (daemon) + `crates/cockpit-cli` (phone
  stand-in test client) in the existing workspace. SQLite via `rusqlite` (or the
  already-present `factstr-sqlite` if it fits) — loop picks one and records it.
- **E.** iOS: SwiftUI, min target **iOS 18**, placeholder bundle id
  `ai.rosslabs.cockpit`; project generated under `ios/Cockpit/`.
- **F.** Work on `feat/agent-cockpit` off `main`; never the current `b19` branch.
- **G.** Mock data permitted for unit/integration tests (no live backend/device);
  real-data rule waived for tests only, rationale logged.
- **H.** Agent set = Claude + Codex only; `agent_type` stays a free string so a
  third agent is later config.

## 2. Chunks (MECE; each has owned-files + deterministic verification)

### Phase A — Rust daemon foundation
- **A0 · Toolchain + workspace** — install rustup if absent; add `crates/cockpitd`
  + `crates/cockpit-cli` to `Cargo.toml` workspace; hello-world bins.
  *Owned:* `Cargo.toml`, `crates/cockpitd/**`, `crates/cockpit-cli/**`.
  *Verify:* `cargo build --workspace` exits 0.
- **A1 · Wire envelope + event model** — serde/schemars types for the normalized
  envelope, extending `dynamic-workflows/PROTOCOL.md`; Session/Event/Approval
  structs; `seq` monotonic.
  *Owned:* `crates/cockpitd/src/protocol.rs`, `.../model.rs`.
  *Verify:* `cargo test -p cockpitd protocol::` (JSON round-trip + schema gen).
- **A2 · SQLite event store** — tables, append, `replay_from(seq)`, diff/control-
  char sanitize, `owner_id` column (multi-user seam).
  *Owned:* `crates/cockpitd/src/store.rs`, `migrations/**`.
  *Verify:* `cargo test -p cockpitd store::` (append→replay, seq gaps).
- **A3 · Session supervisor** — spawn/track/kill, status state machine
  (active/awaiting_input/paused/stale/completed/failed/killed/disconnected),
  injectable `Clock` trait (avoids wall-clock in tests).
  *Owned:* `crates/cockpitd/src/supervisor.rs`, `.../clock.rs`.
  *Verify:* `cargo test -p cockpitd supervisor::` with a fake adapter.

### Phase B — Agent adapters
- **B1 · Adapter trait + Claude adapter** — spawn `claude` stream-json subprocess,
  parse events→envelope, push prompt/steer via stdin, capture `session_id`,
  `--resume`. Mock `claude` shell script for tests.
  *Owned:* `crates/cockpitd/src/adapter/mod.rs`, `.../claude.rs`,
  `crates/cockpitd/tests/mock-bin/claude`.
  *Verify:* `cargo test -p cockpitd adapter::claude` (mock); optional
  `COCKPIT_LIVE=1` smoke.
- **B2 · Codex adapter** — `codex app-server` JSON-RPC client, persistent
  `threadId`, approval elicitation; fallback `codex exec --json`. Mock for tests.
  *Owned:* `crates/cockpitd/src/adapter/codex.rs`,
  `crates/cockpitd/tests/mock-bin/codex`.
  *Verify:* `cargo test -p cockpitd adapter::codex` (mock); optional live smoke.
- **B3 · Approval state machine** — pending row, TTL + auto-deny, resolve via
  reply, abort handling.
  *Owned:* `crates/cockpitd/src/approval.rs`.
  *Verify:* `cargo test -p cockpitd approval::` incl. TTL expiry (fake clock).

### Phase C — Transport + phone stand-in
- **C1 · WebSocket server** — axum/tokio-tungstenite on `127.0.0.1`; snapshot→delta,
  seq-numbered, ~50ms output coalescing; dev bearer-token auth; `Transport` trait
  so a future relay can slot in (multi-user seam).
  *Owned:* `crates/cockpitd/src/transport/mod.rs`, `.../ws.rs`, `.../auth.rs`,
  `crates/cockpitd/src/main.rs`.
  *Verify:* `cargo test -p cockpitd transport::` (in-proc client round-trip).
- **C2 · `cockpit-cli` (phone stand-in)** — connect, `list`, `open <id>` (stream),
  `send`, `approve`, `launch <agent> <repo>`. This is the headless E2E harness.
  *Owned:* `crates/cockpit-cli/**`.
  *Verify:* `crates/cockpitd/tests/e2e.rs` — boot daemon → cli launches a **mock**
  session → drives + approves → asserts event log. `cargo test -p cockpitd e2e`.
- **C3 · launchd LaunchAgent** — plist (RunAtLoad/KeepAlive) + install/uninstall
  script; runs as current user (dedicated-user hardening deferred).
  *Owned:* `deploy/ai.rosslabs.cockpitd.plist`, `deploy/install.sh`.
  *Verify:* `plutil -lint` on plist + `bash -n` on script (load = deploy-time,
  `TAG:UNTESTED`).

### Phase D — iOS app (simulator-verified)
- **D1 · Xcode project + wire models + WS client** — SwiftUI app under
  `ios/Cockpit/`; `Codable` models mirroring the envelope; `URLSessionWebSocketTask`
  client; dev-token auth.
  *Owned:* `ios/Cockpit/**`.
  *Verify:* `xcodebuild -scheme Cockpit -destination 'generic/platform=iOS
  Simulator' build` + `xcodebuild test` (model decode round-trip).
- **D2 · Timeline UI** — session list + status; message/tool-call/diff stream views.
  *Owned:* `ios/Cockpit/Views/**`, `.../ViewModels/**`.
  *Verify:* `xcodebuild test` (view-model logic) + SwiftUI previews compile.
- **D3 · Composer + steer + launcher + approval** — send/steer; new-session
  launcher; approval prompt (Face-ID gate **stubbed** dev-mode, `TAG:UNTESTED` on
  device).
  *Owned:* `ios/Cockpit/Views/Composer*`, `.../Launcher*`, `.../Approval*`.
  *Verify:* `xcodebuild test` (view-model unit tests).
- **D4 · Reconnect + replay** — seq-cursor resync on foreground; treat backgrounding
  as disconnect.
  *Owned:* `ios/Cockpit/Net/Resync*`.
  *Verify:* `xcodebuild test` (resync state machine unit test).

### Phase E — Integration, deferred-surface scaffolding, docs
- **E1 · Localhost end-to-end** — boot daemon + boot simulator app; launch a mock
  session, drive, approve. *Owned:* `ios/CockpitUITests/**`, `scripts/e2e.sh`.
  *Verify:* `scripts/e2e.sh` (daemon + `xcodebuild test` UITest). Best-effort; if
  the sim UITest flakes, the C2 `cockpit-cli` E2E remains the verified artifact.
- **E2 · Deferred-surface interfaces** — compiling stubs + `DEFERRED.md` register
  for: tailnet binding config, SE-mTLS auth provider, APNs registration, CloudKit
  sync. All `TAG:UNTESTED`, each with the device/account step needed to finish.
  *Owned:* `crates/cockpitd/src/transport/tailnet.rs` (stub),
  `crates/cockpitd/src/transport/mtls.rs` (trait+stub), `ios/Cockpit/Push/**`,
  `docs/plans/DEFERRED.md`. *Verify:* `cargo build` + `xcodebuild build`.
- **E3 · Docs + rally ledger** — daemon + app READMEs, run-it instructions, and
  `rally say artifact` entries for every produced component.
  *Owned:* `crates/cockpitd/README.md`, `ios/Cockpit/README.md`, `.rally/log/**`.
  *Verify:* markdown present; `rally room --json` shows artifacts.

## 3. Definition of done (24h window)

- `cargo test --workspace` green; daemon + `cockpit-cli` drive a mock Claude **and**
  mock Codex session end-to-end over localhost with seq-numbered replay + TTL
  approvals.
- `xcodebuild test` green; iOS app compiles for the simulator, decodes the wire
  protocol, renders the timeline, and (best-effort E1) drives the localhost daemon.
- `DEFERRED.md` enumerates every device/account/network item with the exact
  human step to finish it, each marked `TAG:UNTESTED`.
- No data written outside repo / Mac (residency rule); no live credits burned
  unless a single gated smoke was explicitly run.
- Final report states pass/fail verbatim and lists all DEFERRED/UNTESTED/CLEANUP.

## 4. Explicitly out of scope for the autonomous run (needs human/hardware)

Tailnet provisioning · real Secure-Enclave key + mTLS handshake on device · Face ID
on device · APNs end-to-end · CloudKit sync with entitlements · App Store /
TestFlight · multi-user relay. All scaffolded (E2), none claimed working.
