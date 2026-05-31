# Design: iOS single-pane cockpit for Claude Code + Codex on an always-on Mac

**Working name:** Rally Cockpit (final name TBD)
**Date:** 2026-05-31
**Status:** Design — pending user review before implementation planning
**Author:** drafted with Claude Code via the brainstorming skill

---

## 1. Summary

A native **iOS app** that is the primary surface for driving Claude Code and Codex
sessions running on an **always-on Mac** (Mac Studio / Mac mini). The phone is a
remote control; the agents, repos, and credentials stay on the Mac. The app
unifies both agents into one timeline — list every session, read the live
message / tool-call / diff stream, **send prompts, steer mid-run, approve tool
calls, and launch new sessions** — with first-class coordination via the
existing `agent-rally-point` ledger.

This replaces the current two-app reality (Anthropic's Claude app + OpenAI's
ChatGPT/Codex app, each a closed silo) with one owned surface. Easy Terminal
remains the Mac-native sibling; this spec covers the iOS surface only.

## 2. Why build vs. buy (decision record)

The remote-access problem is already solved twice by first parties — **Claude
Code Remote Control** (`claude --rc`, local execution, Anthropic relay, official
Claude app) and **Codex in the ChatGPT mobile app** (pairs to the macOS Codex
desktop app). Both are closed relays a third-party client cannot join. So a
unified surface must wrap the **CLIs/SDKs directly**.

Third-party unifiers exist — **Omnara** (Claude Code + Codex + Cursor, OSS,
self-hostable), **Happy Coder** (E2E-encrypted, OSS), **VibeTunnel** (raw PTY
streaming). We mine them for lessons (§4) but build our own, because the
differentiator — **rally-point as the cross-agent coordination brain** — is
something none of them provide, and the data-residency rule (§3) rules out their
default SaaS relays.

## 3. Goals, non-goals, constraints

### Goals (v1)
- One iOS app listing all Claude + Codex sessions on the Mac, with live status.
- Per-session: read structured stream (messages, tool calls, diffs), send a
  prompt, steer mid-run, approve/deny a tool call.
- **Launch a new session** ("new window") for either agent against a chosen repo.
- Survive iOS backgrounding: reconnect and replay missed events.
- Security posture appropriate to "a phone that can run agents on my dev Mac."

### Non-goals (v1 — YAGNI)
- No raw terminal emulator (structured timeline only; raw PTY view is a later phase).
- No relay server, no multi-tenant deployment (seams kept open — §8).
- No Android, no web client.
- No parallel-worktree orchestration UI.
- Gemini / other agents: schema-ready (`agent_type` is a free string) but not wired.

### Hard constraints
- **Data residency:** session data may rest **only** on the phone, the Mac,
  the user's **iCloud** (CloudKit private DB), a **git repo** (GitHub or other),
  and unavoidably the **model vendors** (Anthropic / OpenAI). **No third-party
  server may store this data in plaintext, ever.** This forecloses a SaaS relay
  and constrains any future relay to zero-knowledge or self-hosted.
- **iOS only** for now.
- **Daemon language: Rust** (shares the toolchain with the existing `rally-cli`
  crate; lower memory than a Node/Swift server — VibeTunnel's stated regret).

## 4. Lessons absorbed from prior art (not anchored to)

| Lesson | Source | Our decision |
|---|---|---|
| PTY/JSONL **scraping of Claude Code rots** with every CLI update | Omnara killed its v1 wrapper | Drive via **Claude Agent SDK** `query()` + `canUseTool`; **`codex app-server`** JSON-RPC. Structured, not scraped. |
| Relay servers exist only because they're **multi-tenant SaaS** | Omnara (PG+SSE), Happy (Socket.IO) | **No relay in v1.** Tailscale tailnet is connection *and* trust boundary; phone↔Mac direct. |
| **Terminal rendering + resize-on-foreground** is the bug swamp | VibeTunnel #199, #544, resize-loop fix | **No terminal emulator v1.** Native SwiftUI timeline over structured events. |
| Missing **sequence numbers** → gap/dup on reconnect | VibeTunnel, Omnara SSE | Seq-numbered event log + replay-from-cursor, day one. |
| **24h blocking server poll** per approval | Omnara `wait_for_answer` | Durable pending-approval row + **TTL + auto-deny**, event-driven. |
| **No approval timeout** wedges sessions | Happy | Enforce TTL on every pending approval. |
| App-quit **kills server**; in-memory sessions **lost on restart** | VibeTunnel menu-bar model | **launchd LaunchAgent** (`RunAtLoad`+`KeepAlive`) + **SQLite-persisted** state. |
| **Two crypto schemes** = migration debt | Happy (secretbox + AES-GCM) | One model; mesh removes most app-layer crypto need in v1. |
| **`canUseTool` as single permission chokepoint** + push + RPC answer | Happy | Adopt directly. |
| **Everything-is-a-Message** unified log; `agent_type` free string; `git_diff` first-class; steering cursor | Omnara data model | Adopt directly. |
| Signing-seed **challenge-response auth**, **wrapped per-session keys** | Happy encryption | Hold for the multi-user/relay phase (§8), not v1. |
| **Detach PTY/session lifecycle from transport**; buffer in a separate component | VibeTunnel | Sessions live in the daemon, independent of any connected client. |

## 5. Architecture — three planes

```
 iPhone (SwiftUI)            Tailscale tailnet            Mac Studio/mini
┌──────────────────┐      (WireGuard mesh, P2P)      ┌────────────────────────┐
│  Timeline UI      │  WebSocket (TLS over tailnet)  │  rally-cockpitd (Rust)  │
│  - session list   │ <===========================>  │  LaunchAgent, KeepAlive │
│  - stream + diff  │   normalized event envelope    │                         │
│  - prompt/steer   │   snapshot→delta, seq-numbered │  ┌───────────────────┐  │
│  - approve (FaceID)│                               │  │ Session supervisor│  │
│  - launch session │   APNs push (approvals/done)   │  │  + SQLite log     │  │
└──────────────────┘  <----------------------------  │  └───────────────────┘  │
   Secure Enclave                                     │  ┌──────┐  ┌────────┐  │
   mTLS client cert                                   │  │Claude│  │ Codex  │  │
   CloudKit (history/settings backup)                 │  │adapter│ │adapter │  │
                                                       │  └──────┘  └────────┘  │
                                                       │  rally ledger (.rally) │
                                                       └────────────────────────┘
```

**Plane 1 — Mac daemon (`rally-cockpitd`, Rust, launchd LaunchAgent):**
supervises agent sessions, hosts per-agent adapters, owns the SQLite event
store, enforces the security policy, and serves the wire protocol over a
WebSocket bound to the tailnet interface. `RunAtLoad` + `KeepAlive` so it
survives crash, quit, and reboot. Runs as a **dedicated non-admin user** scoped
to project directories.

**Plane 2 — Transport:** Tailscale tailnet (deny-by-default grants, device
approval, MFA-tied identity) carrying a single **WebSocket**. One normalized
event envelope (extends `dynamic-workflows/PROTOCOL.md`), **snapshot-then-delta**,
**sequence-numbered**, output coalesced ~50 ms. No public ports, no relay, no
third party in the data path. The transport is behind a `Transport` trait so a
zero-knowledge relay can slot in later (§8) without touching the daemon core.

**Plane 3 — iOS app (SwiftUI):** native agent-timeline. Sessions list across both
agents; tap → message/tool-call/diff stream + inline composer (prompt + steer) +
approval prompt (Face-ID gated) + "new session" launcher. Treats every
backgrounding as a disconnect → reconnect-and-replay from its seq cursor.

## 6. Agent adapters → one wire schema

Each agent is a bespoke adapter normalizing into one envelope. `agent_type` is a
free string so a third agent is config, not a schema migration.

- **Claude adapter** — Claude **Agent SDK** `query()`; `canUseTool` is the single
  permission chokepoint; `resume` / `--continue` for session continuity; reads
  the SDK's native message objects (no JSONL scraping).
- **Codex adapter** — **`codex app-server`** JSON-RPC over stdio/socket; persistent
  `threadId` across turns; approvals via Codex's elicitation / approval-policy
  channel; `codex resume <id>` for continuity.

The daemon owns the loop; adapters translate vendor events ⇄ the normalized
envelope.

## 7. Data model (Omnara's "everything-is-a-Message", persisted to SQLite)

- **Session** — `id`, `owner_id` (tenant seam, §8), `agent_type` (free string),
  `repo_path`, `status` (active / awaiting_input / paused / stale / completed /
  failed / killed / disconnected), `git_diff` (validated unified diff,
  control-char-sanitized), `last_read_cursor`, metadata JSON.
- **Event** — `session_id`, `seq` (monotonic), `sender` (agent / user),
  `kind` (message / tool_call / tool_result / diff / status / approval_request),
  `content`, `requires_user_input` bool, metadata JSON. Steps, questions,
  approvals, and answers are all Events on one ordered log → the UI is one timeline.
- **Approval** — `id`, `session_id`, `event_seq`, `tool`, `args`, `created_at`,
  `ttl`, `resolution` (allow / deny / auto_denied / aborted).

Persisted on the Mac (SQLite). **CloudKit private DB** optionally mirrors session
history + app settings for phone-side backup/sync — an allowed residency location.
SQLite-on-disk means a daemon restart recovers state rather than wiping it.

## 8. Multi-user seam (designed, not built in v1)

MVP runs personal on the tailnet with no relay. To avoid a future rewrite:
- **`owner_id` on every Session/Event** from day one.
- **`Transport` trait** with a `DirectTailnet` impl (v1) and a future
  `ZeroKnowledgeRelay` impl. The relay, if ever built, only routes **ciphertext**
  — data rests in the user's iCloud or a self-hosted instance, satisfying §3.
- **Crypto seam:** v1 relies on mesh + mTLS (direct connection, no app-layer
  payload encryption needed). Multi-user adds Happy-style app-layer E2E
  (signing-seed identity, challenge-response auth, per-session data keys wrapped
  via ephemeral `box`) so the relay is zero-knowledge.

Build multi-user only after the personal MVP proves the workflow.

## 9. Security model (the part worth over-building)

Layered, mesh-first; the daemon — never the phone — enforces policy:
1. **Tailnet membership** — only the user's devices; MFA-tied identity; device approval. Not Tailscale Funnel; not port-forwarding.
2. **Secure-Enclave mTLS** — iPhone client cert with a non-exportable key generated in the Secure Enclave; second independent gate.
3. **Biometric per privileged action** — Face ID before approvals / destructive commands, not just at login.
4. **Deny-by-default allowlist** in the daemon — free-form shell is the top tier requiring explicit per-command approval. Phone holds only a short-lived, revocable session token; **never** Mac secrets.
5. **Blast-radius reduction** — agents run as a dedicated non-admin user scoped to project dirs; no passwordless sudo.
6. **Append-only audit log** of every command, approval, and result (device + timestamp), surfaced in-app.

## 10. Approvals + iOS background lifecycle

`canUseTool` (Claude) / elicitation (Codex) fires on the Mac → daemon writes a
durable **pending-approval row** with a **TTL + auto-deny** → **APNs visible push**
(reliable delivery for must-not-miss approvals; silent push to wake/resync) → user
opens app, Face-ID approves → reply over the WebSocket resolves the pending
promise into allow/deny. A backgrounded/roaming phone reconnects and **replays
missed events from its seq cursor** (server-resident session state makes this
free). Never fire a no-op resize on reconnect (VibeTunnel's signature bug) — N/A
in v1 since there is no terminal, but the rule stands for the later raw-PTY view.

## 11. rally-point as the coordination brain

The daemon's session/event store is a superset of what `.rally/` already
models (owned / blocked / handed-off / decided / produced + `rally next`'s action
contract). The iOS app surfaces the **rally room as a live mobile surface**:
"Claude plans → hand off → Codex implements" is an existing rally primitive, now
driven from the phone. The wire envelope (§5) extends
`dynamic-workflows/PROTOCOL.md` rather than inventing a parallel schema.

## 12. Cross-cutting answers (the user's original questions)

- **Integration:** structured SDK / app-server adapters, not scraping (§6).
- **Security:** §9 — mesh + SE-mTLS + biometric + daemon-enforced allowlist + audit.
- **Speed/latency:** transport is not the bottleneck — model inference dominates
  (seconds). Tailscale P2P adds ~ms.
- **Performance:** daemon footprint is small; the ceiling is **concurrent agent
  sessions on the Mac** (CPU/RAM per session) — size the box to fan-out.
- **Data transfer:** structured events + diffs are KB/s text; repos/context never
  leave the Mac. Seq-numbered WS + server-side replay bounds reconnect cost.

## 13. Phasing

- **P0** — `rally-cockpitd` skeleton + Claude SDK adapter + SQLite event log +
  local WebSocket; drive one Claude session from a CLI test client over the tailnet.
- **P1** — SwiftUI app: session list + live timeline + composer + **new-session
  launcher**; pairing + Secure-Enclave mTLS; one live Claude session end-to-end on
  the phone, including steering.
- **P2** — Codex `app-server` adapter; both agents in one list; approval flow +
  APNs push + TTL/auto-deny.
- **P3** — rally ledger integration (handoffs, `rally next` surfaced on phone);
  audit-log UI; CloudKit history/settings sync.
- **P4 (optional)** — raw-PTY "drop to terminal" view (SwiftTerm); then the
  multi-user/zero-knowledge-relay track (§8).

## 14. Key risks

1. **iOS background execution** — no persistent sockets when suspended; reconnect-
   and-replay + APNs is the only reliable model. (Hard constraint, designed for.)
2. **Security blast radius** — a compromised phone session can run agents on the
   dev Mac; §9 is the mitigation and is worth over-engineering.
3. **Vendor surface drift** — Agent SDK and `codex app-server` are young and
   moving; isolate them behind adapters (§6) so a vendor change touches one file.
4. **Scope creep into the multi-user/relay track** — resist until the personal
   MVP proves the workflow; the seams (§8) make deferral cheap.

## 15. Success criteria (v1 / P1–P2)

- From the phone, off the LAN, on the tailnet: list sessions, open one, read the
  live stream, send a prompt, steer mid-run, approve a tool call, and launch a new
  session for either agent.
- Backgrounding then reopening the app reconnects and shows no missed/duplicated
  events.
- No session data is ever written to a server outside phone / Mac / iCloud / git /
  the model vendors.
- Daemon survives a Mac reboot and resumes serving existing session history.

## 16. Open questions (resolve during planning)

- Final product name.
- iOS minimum version target (drives APNs / CloudKit / SwiftUI API choices).
- Whether CloudKit sync lands in P3 or is deferred (residency-allowed but extra work).
- Exact Tailscale grant policy shape (which devices, which tags).
