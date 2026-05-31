# Convergence: Cockpit → one daemon (ptyd)

**Decision (user, 2026-05-31):** converge to ONE daemon now. **The daemon is
`ptyd`** (`~/dev/git-folder/ptyd`) — Easy Terminal's Apache-2.0 greenfield
substrate. Cockpit's distinctive layer ports into ptyd; the iOS app becomes a
ptyd thin client per `easy-terminal/docs/SECURE-MOBILE.md`.

## Why ptyd is the base (not cockpitd)

ptyd is already far ahead on the substrate: `pane.rs`/`agent.rs`/`orchestrate.rs`,
`persist.rs`, `detect.rs` (agent-status detection), `mobile.rs` (secure-mobile
auth core, committed), `broadcast.rs`, a frozen JSON-line `CLIENT-API.md`, and an
MCP binary (`bin/ptyd-mcp.rs`). cockpitd's substrate (sessions/store/socket) is a
subset. So cockpitd does **not** become the daemon — it **donates** its
distinctive parts:

| Cockpit asset | Disposition in ptyd |
|---|---|
| Structured **adapters** (Claude/Codex → Event timeline) | **Port** — repointed (see reconciliation) |
| **Event model** (message/tool_call/tool_result/diff/approval) | **Port** as ptyd's structured-agent API |
| **Approval gating + TTL sweep** (`approval`, authz `decide`, per-session gate) | **Port** — security §9 value |
| **Audit log** | **Port** |
| iOS app (timeline/composer/launcher/approval/settings/connection-status) | **Move** → ptyd TLS thin client |
| cockpitd **WebSocket transport + dev-token auth** | **Drop** — superseded by ptyd Unix-socket + SECURE-MOBILE TLS+pairing |
| cockpit **Tailscale-only** assumption | **Drop** — ptyd D1: loopback + user's own relay |
| cockpit **crypto + zero-knowledge relay** (`crypto.rs`, `relay.rs`) | **Defer** — feeds ptyd's later multi-user phase, not v1 |
| `cockpit-cli` | **Keep** as a ptyd client (retarget to ptyd wire) for headless E2E |

## The one hard reconciliation: raw-PTY vs structured

ptyd's bet is *"raw stays raw"* — agent runs as a full-screen TUI in a PTY; the
Mac renders raw bytes (SwiftTerm); status comes from **screen-scrape detection**
(`detect.rs`). Cockpit's bet is **structured** — parse the agent's machine-readable
output into a timeline; no terminal. A single agent process cannot be *both* an
interactive TUI *and* a `-p --output-format stream-json` headless run.

**Resolution — derive structured from the SAME running agent via its session
transcript (no second process):** ptyd keeps running the agent interactively in a
PTY (unchanged, serves the Mac's raw view). The structured layer **tails the
agent's own session JSONL** — `~/.claude/projects/**/*.jsonl`,
`~/.codex/sessions/**/rollout-*.jsonl` — to derive the Event timeline for the iOS
client. This is exactly the maintainable path the prior-art research validated
(Omnara/Happy parse the transcript; nobody screen-scrapes the model output). So
cockpit's adapters change their **source** (transcript JSONL instead of a
spawned `-p` pipe) but keep their **mapping** (→ Event envelope). One agent, two
views: **raw (Mac) + structured (iOS), both off one ptyd session.**

→ This also *strengthens* ptyd's `detect.rs`: structured transcript events give
agent status (awaiting-input / tool-call / done) deterministically, complementing
the heuristic screen-scrape.

## Target shape

```
                         ptyd (one Apache daemon)
   PTY layer ───────────────────────────────────────────────
   pane/agent/orchestrate/persist/detect/broadcast  (existing)
        │ raw bytes                         │ session transcript JSONL
        ▼                                    ▼
   pane.subscribe_raw (existing)        agent.timeline / agent.subscribe_structured (NEW, ported)
        │                                    │  + approval methods + audit + authz gate
   Unix socket + SECURE-MOBILE TLS/pairing (one transport, both APIs)
        │                                    │
   Mac app (SwiftTerm, raw)             iOS Cockpit app (structured timeline, TLS thin client)
```

New ptyd surface (additive, JSON-line per CLIENT-API): `agent.subscribe_structured`
(streams Events for a pane/agent), `agent.approve`, `agent.get_audit`. The iOS
app speaks these over the SECURE-MOBILE TLS transport + pairing token.

## Safe execution plan (respects ptyd's live tranche)

ptyd has an **in-flight T1** (`claude_code:builder`, `perf/reader-loop-rework`,
scope `src/main.rs`+`src/pane.rs`, "do not deploy"). Therefore:

1. **Coordinate via rally first** — `rally enter`/`say handoff` in ptyd announcing
   the convergence intent + owned files; avoid T1's scope (`main.rs`, `pane.rs`).
2. **Branch** off ptyd `main` (not into T1's branch). All work **additive** new
   modules: `src/agent_structured.rs` (Event model + transcript adapters),
   `src/approval.rs` (port), `src/audit.rs` (port), wire methods registered in
   the dispatch table without touching the T1 reader loop.
3. **Port in tranches**, each `cargo build`+`cargo test` green, clippy clean,
   std-only / no-AGPL (ptyd ethos): (a) Event model + transcript tailer +
   Claude adapter; (b) Codex adapter; (c) approval gate + audit + authz;
   (d) wire methods + tests; (e) `cockpit-cli` retargeted as ptyd client E2E.
4. **iOS app**: move into the ET org; swap `CockpitClient` from cockpit's
   WebSocket to ptyd's JSON-line-over-TLS + `hello` handshake + pairing token
   (SECURE-MOBILE pieces 1–5). The timeline/composer/approval UI is unchanged —
   only the transport + the structured-event source change.
5. **Defer**: cockpit's `crypto.rs`/`relay.rs` → ptyd's later multi-user phase
   (they're tested, parked).

## What this makes true

One Apache daemon (ptyd), one session model, one transport (Unix + opt-in TLS),
two render philosophies served from the same running agent: raw (Mac) and
structured (iOS). Easy Terminal sheds AGPL herdr (its #1 driver) and gains the
iOS surface; Cockpit gains ptyd's mature PTY/persistence/detection/mobile-auth
instead of its own thinner versions. Rally Point sits on the unified socket as
the user's North Star intends.

## Status

PLAN — pending user approval to execute. No ptyd files touched. cockpit work to
date stays on `agent-rally-point` branch `feat/agent-cockpit` (pushed) as the
donor source until ported.
