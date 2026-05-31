# Agent Cockpit

Drive **Claude Code + Codex** sessions running on an always-on Mac (Mac Studio /
mini) from a **native iOS app** — one timeline to read, steer, approve, and
launch sessions. The phone is a remote control; repos, CLIs, and credentials
never leave the Mac. Easy Terminal is the Mac-native sibling; this is the iOS
surface.

> Entry point for the whole project. Design rationale lives in
> [`../superpowers/specs/2026-05-31-ios-agent-cockpit-design.md`](../superpowers/specs/2026-05-31-ios-agent-cockpit-design.md);
> the build plan in [`../plans/2026-05-31-ios-agent-cockpit-PLAN.md`](../plans/2026-05-31-ios-agent-cockpit-PLAN.md);
> the wire contract in [`../plans/COCKPIT-WIRE.md`](../plans/COCKPIT-WIRE.md);
> everything not-yet-verifiable in [`../plans/DEFERRED.md`](../plans/DEFERRED.md).

## Shape

```
 iPhone (SwiftUI: ios/Cockpit)            Mac (Rust: crates/cockpitd)
 ┌───────────────────────────┐           ┌─────────────────────────────┐
 │ timeline · composer/steer  │  WebSocket │ transport (axum/ws)         │
 │ launcher · approval · settings│◀───────▶│  auth · 9 cmds · fan-out    │
 │ reconnect/replay (seq cursor)│  (wire   │  ~50ms coalesce · seq replay│
 └───────────────────────────┘  contract)│ supervisor → adapters       │
                                          │  claude (-p stream-json)    │
   cockpit-cli (crates/cockpit-cli)       │  codex  (exec --json)       │
   headless phone stand-in for E2E        │ store(SQLite) · approvals   │
                                          │ audit · authz · crypto      │
                                          └─────────────────────────────┘
```

## Components

| Path | What | Verified by |
|---|---|---|
| `crates/cockpitd` | Mac daemon: transport, supervisor, adapters, SQLite store, approvals (TTL/auto-deny), audit log, authz policy, crypto, zero-knowledge relay | `cargo test -p cockpitd` (lib + e2e) |
| `crates/cockpit-cli` | Headless phone stand-in (`list/open/send/approve/launch/audit`) | used by `tests/e2e.rs` + `scripts/e2e.sh` |
| `ios/Cockpit` | SwiftUI app: timeline, composer, launcher, approval, reconnect, settings | `xcodebuild test` (iPhone 17 Pro sim) |
| `deploy/` | launchd LaunchAgent + `install.sh` | `plutil -lint`, `bash -n` |
| `scripts/e2e.sh` | one-command localhost end-to-end | run it |

## Run

```bash
# daemon
COCKPIT_TOKEN=<dev-token> cargo run -p cockpitd -- serve      # ws://127.0.0.1:8787
# headless drive
cargo run -p cockpit-cli -- --token <dev-token> list
# full proof
bash scripts/e2e.sh
# iOS app
cd ios/Cockpit && xcodegen generate && open Cockpit.xcodeproj   # set URL+token in Settings
# always-on install
deploy/install.sh install
```

## What's real vs deferred

**Verified headlessly today:** daemon drives **mock** Claude + Codex sessions
over the wire with seq-replay, TTL approvals, WS-level approval round-trip, audit
log, deny-by-default authz policy, owner isolation, and a zero-knowledge relay
that provably forwards only ciphertext. iOS app compiles + passes sim tests for
all of the above surfaces. Live agent adapters are mock-verified; real-CLI smokes
are gated behind `COCKPIT_LIVE=1` (no credit burned).

**Deferred — needs hardware/accounts, scaffolded + `TAG:UNTESTED`** (see
`DEFERRED.md`): Tailscale tailnet binding, Secure-Enclave mTLS, Face ID, APNs,
CloudKit, and the *hosted* multi-user relay deployment. The crypto + relay code
exists and is tested in-process; only the device/network/account wiring is
outstanding.

## Security posture

v1: dev bearer token over loopback. Production (per design §9): Tailscale tailnet
+ Secure-Enclave mTLS + Face-ID-per-action + daemon-enforced deny-by-default
command authorization + append-only audit. The authorization, audit, and crypto
layers are built and tested; the transport-identity layer (tailnet + mTLS) is the
deferred piece.

## Data residency

Session data rests only on the phone, the Mac, the user's iCloud, a git repo, and
unavoidably the model vendors — never a third-party server in plaintext. The
multi-user relay is therefore **zero-knowledge** by construction (it routes
ciphertext only; verified by `transport::relay` tests).
