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
| `COCKPIT_ADDR` | `127.0.0.1:8787` | WebSocket bind address. Non-loopback is refused — see below |
| `COCKPIT_ALLOW_NON_LOOPBACK` | unset | must equal `i-understand-the-risk` to bind anything but loopback |
| `COCKPIT_REPO_ALLOWLIST` | `$HOME` | colon-separated directories a session may launch in |
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

## Security (what holds, what does not)

v1 authenticates with one dev bearer token over loopback. Four guards are real
and tested:

- **Token check is constant-time** and fails closed when `COCKPIT_TOKEN` is
  unset or empty (`transport/auth.rs`).
- **Every connection gets a principal** that owns the sessions it launches.
  `send_prompt`, `steer`, `close_session`, and `approve` reject a non-owner with
  `forbidden`. Reads (`list_sessions`, `open_session`, `get_audit`) are
  deliberately unscoped so a reconnecting client keeps its timeline.
- **`repo_path` is canonicalized and allowlisted** before the child agent is
  spawned in it. `..` traversal and symlinks pointing out of a root are refused
  (`policy.rs`).
- **Non-loopback binds are refused** unless the operator sets
  `COCKPIT_ALLOW_NON_LOOPBACK=i-understand-the-risk`.

Two things this does **not** give you:

- **The approval gate does not control the agent.** Cockpit spawns the CLI and
  reads its stdout. A denied `tool_call` is not forwarded to clients; the child
  process was never stopped and may already have run the tool. `tool_blocked`
  carries `advisory: true` and `enforced: false` for this reason. Run child
  agents under an OS-level sandbox if you need containment. See
  `transport::ws::run_pump` and `arp003_execution_gate_definition_of_done` in
  `tests/e2e.rs`.
- **One shared token is not per-client identity.** Ownership separates
  well-behaved clients; anyone holding the token can claim any `client_id`.
  Secure-Enclave mTLS is the planned replacement — see
  [`docs/plans/DEFERRED.md`](../../docs/plans/DEFERRED.md).

## Status

All daemon tests pass with `clippy -D warnings` clean. See
[`docs/superpowers/specs/2026-05-31-ios-agent-cockpit-design.md`](../../docs/superpowers/specs/2026-05-31-ios-agent-cockpit-design.md)
for the design and [`docs/plans/2026-05-31-ios-agent-cockpit-PLAN.md`](../../docs/plans/2026-05-31-ios-agent-cockpit-PLAN.md)
for the build plan.
