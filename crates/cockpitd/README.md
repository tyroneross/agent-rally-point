# cockpitd

The Mac-side daemon for **Agent Cockpit**. Supervises Claude Code + Codex
sessions on an always-on Mac and serves a normalized, sequence-numbered event
stream to clients (the iOS app, or the `cockpit-cli` phone stand-in) over a
WebSocket. The phone drives; repos, CLIs, and credentials stay on the Mac.

## Build & test

```bash
cargo build -p cockpitd
cargo test  -p cockpitd          # 59 lib + 5 e2e; 2 live smokes #[ignore]d
bash scripts/e2e.sh              # one-command localhost end-to-end
```

## Run

```bash
COCKPIT_TOKEN=<dev-token> \
COCKPIT_ADDR=127.0.0.1:8787 \
COCKPIT_DB=$HOME/Library/Application\ Support/cockpitd/cockpitd.db \
cockpitd serve
```

Install as an always-on LaunchAgent (RunAtLoad + KeepAlive):

```bash
deploy/install.sh install     # build, copy binary, render + load the LaunchAgent
deploy/install.sh status
deploy/install.sh uninstall
```

## Environment

| Var | Default | Meaning |
|---|---|---|
| `COCKPIT_TOKEN` | (empty → all auth fails) | dev bearer token required in the `hello` frame |
| `COCKPIT_ADDR` | `127.0.0.1:8787` | WebSocket bind address (loopback v1; tailnet iface when deployed) |
| `COCKPIT_DB` | `cockpitd.db` | SQLite event store path |
| `COCKPIT_CLAUDE_BIN` | `claude` | claude binary (tests point this at a mock) |
| `RUST_LOG` | `info` | tracing filter |

## Architecture

```
client (iOS / cockpit-cli)
        │  WebSocket — wire contract in docs/plans/COCKPIT-WIRE.md
        ▼
  transport/ws.rs ── auth (hello/token) · 9 commands · fan-out · ~50ms coalesce · seq replay
        │
   supervisor.rs ── session lifecycle + status state machine, per-session async pump
        │
   adapter/claude.rs   adapter/codex.rs       store.rs (SQLite)      approval.rs (TTL/auto-deny)
   claude -p           codex exec --json      sessions/events/       Clock-driven sweep
   stream-json         NDJSON                 approvals + replay
```

- **Adapters** (`adapter/`) normalize each agent's events into one `Event`
  envelope. `agent_type` is an open string — a third agent is config, not a
  schema change. Claude via headless `stream-json`; Codex via `codex exec --json`.
- **Store** (`store.rs`) is the source of truth: monotonic per-session `seq`,
  `replay_from(seq)` for reconnect, control-char sanitization, `owner_id`
  (multi-user seam).
- **Transport** (`transport/`) — `Transport` trait; `DirectWs` for v1. The
  `seams.rs` `AuthProvider` (`DevTokenAuth` now, `MtlsAuth` stub) and `BindTarget`
  (loopback/tailnet) are the deferred-surface extension points.

## Security (v1 vs deferred)

v1 authenticates with a dev bearer token over loopback. The production posture —
Tailscale tailnet, Secure-Enclave mTLS, Face ID, deny-by-default command
allowlist — is enumerated with the exact finishing step in
[`docs/plans/DEFERRED.md`](../../docs/plans/DEFERRED.md). Nothing gated is claimed
working.

## Status

61 daemon tests pass (0 warnings). See
[`docs/superpowers/specs/2026-05-31-ios-agent-cockpit-design.md`](../../docs/superpowers/specs/2026-05-31-ios-agent-cockpit-design.md)
for the design and [`docs/plans/2026-05-31-ios-agent-cockpit-PLAN.md`](../../docs/plans/2026-05-31-ios-agent-cockpit-PLAN.md)
for the build plan.
