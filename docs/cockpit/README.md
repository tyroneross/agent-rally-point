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
>
> Consolidation boundary: `cockpitd` is preserved here as the tested donor
> implementation for the operator cockpit. The target daemon convergence is
> `ptyd`; see [`CONVERGENCE.md`](CONVERGENCE.md).

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
log, per-connection session ownership on writes, a canonicalizing `repo_path`
allowlist, a constant-time token check, refusal to bind non-loopback, and a
zero-knowledge relay that provably forwards only ciphertext. iOS app compiles +
passes sim tests for all of the above surfaces. Live agent adapters are
mock-verified; real-CLI smokes are gated behind `COCKPIT_LIVE=1` (no credit
burned).

**Not verified, and not claimed:** the approval gate does not control the child
agent. Cockpit spawns the CLI and reads its stdout, so a denied `tool_call` is
withheld from clients but was never prevented — the tool may already have run.
`tool_blocked` says so on the wire (`advisory: true`, `enforced: false`). Real
containment needs an OS sandbox around the child, or a broker that executes
tools itself. Tracked as ARP-003; the acceptance test that would close it is
`arp003_execution_gate_definition_of_done` in `crates/cockpitd/tests/e2e.rs`.

**Deferred — needs hardware/accounts, scaffolded + `TAG:UNTESTED`** (see
`DEFERRED.md`): Tailscale tailnet binding, Secure-Enclave mTLS, Face ID, APNs,
CloudKit, and the *hosted* multi-user relay deployment. The crypto + relay code
exists and is tested in-process; only the device/network/account wiring is
outstanding.

## Security posture

v1: one dev bearer token over loopback, checked in constant time and failing
closed when unset. Each connection owns the sessions it launches; another client
cannot send, steer, close, or approve them. `repo_path` is canonicalized against
an allowlist before the child agent is spawned. Binding beyond loopback requires
an explicit `COCKPIT_ALLOW_NON_LOOPBACK=i-understand-the-risk`.

Two limits stated plainly. **The authz policy is not enforced against the
agent** — it filters the event stream and records an audit trail; the child
process runs its tools regardless (ARP-003). **One shared token is not per-client
identity** — ownership separates well-behaved clients, not attackers, because
any token holder can assert any `client_id` (ARP-005 residual).

Production (per design §9): Tailscale tailnet + Secure-Enclave mTLS +
Face-ID-per-action + append-only audit, plus an OS sandbox or tool broker so the
authorization decision actually binds the agent. The audit and crypto layers are
built and tested; the transport-identity layer (tailnet + mTLS) and the
enforcement layer are the deferred pieces.

## Data residency

Session data rests only on the phone, the Mac, the user's iCloud, a git repo, and
unavoidably the model vendors — never a third-party server in plaintext. The
multi-user relay is therefore **zero-knowledge** by construction (it routes
ciphertext only; verified by `transport::relay` tests).
