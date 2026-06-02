# Cockpit wire protocol v1 (contract)

Source of truth for **both** the Rust daemon (`cockpitd`, serde) and the iOS app
(`Cockpit`, Codable). Extends the spirit of `dynamic-workflows/PROTOCOL.md`.

- **Transport:** WebSocket. v1 frames are **JSON text**; a binary delta mode is a
  later optimization (not v1).
- **Auth:** the first client frame MUST be `hello` carrying a dev bearer token
  (`COCKPIT_TOKEN`). Secure-Enclave mTLS replaces/augments this later (deferred).
- **Seq:** every session has a **monotonic `seq` (u64, starts at 1)** over its
  events. Clients track the highest seq seen and reconnect with `open_session
  { from_seq }`; the server replays events with `seq > from_seq` then resumes live
  deltas. This is the reconnect/replay backbone (iOS backgrounding = disconnect).

## Frame shape

```jsonc
{ "t": "<type>", ...fields }   // `t` discriminates; all frames carry it
```

## Client → server (commands)

| `t` | fields | meaning |
|---|---|---|
| `hello` | `token: string`, `protocol: 1` | auth handshake; must be first |
| `list_sessions` | — | request current session list |
| `open_session` | `session_id`, `from_seq: u64` | subscribe + replay from seq |
| `send_prompt` | `session_id`, `text` | send a new prompt turn |
| `steer` | `session_id`, `text` | inject a steering message mid-run |
| `approve` | `approval_id`, `decision: "allow"\|"deny"`, `reason?` | resolve a pending approval |
| `launch_session` | `agent_type: string`, `repo_path`, `prompt?` | start a new session ("new window") |
| `close_session` | `session_id` | stop/kill a session |
| `ping` | — | keepalive |

## Server → client (events)

| `t` | fields | meaning |
|---|---|---|
| `hello_ok` | `server_version`, `protocol: 1` | auth accepted |
| `error` | `code: string`, `message` | command/protocol error |
| `session_list` | `sessions: [Session]` | snapshot of all sessions |
| `snapshot` | `session_id`, `session: Session`, `events: [Event]`, `cursor_seq: u64` | sent on `open_session` |
| `event` | `session_id`, `event: Event` | live delta (carries `event.seq`) |
| `session_status` | `session_id`, `status: SessionStatus` | status transition |
| `approval_request` | `approval: Approval` | a tool call needs approval (also emitted as an Event of kind `approval_request`) |
| `pong` | — | keepalive reply |

## Domain types

```jsonc
// Session
{ "id": "uuid", "owner_id": "string", "agent_type": "claude"|"codex"|string,
  "repo_path": "string", "status": SessionStatus, "title": "string|null",
  "created_at": "rfc3339", "last_seq": u64 }

// Event  (kind ∈ message | tool_call | tool_result | diff | status | approval_request | error)
{ "session_id": "uuid", "seq": u64, "sender": "agent"|"user"|"system",
  "kind": "string", "content": "string", "requires_user_input": bool,
  "created_at": "rfc3339", "metadata": { } }

// Approval
{ "id": "uuid", "session_id": "uuid", "event_seq": u64, "tool": "string",
  "args": { }, "created_at": "rfc3339", "ttl_secs": u64,
  "resolution": null|"allow"|"deny"|"auto_denied"|"aborted" }

// SessionStatus (string enum)
"active" | "awaiting_input" | "paused" | "stale" | "completed" | "failed" | "killed" | "disconnected"
```

## Invariants both sides MUST honor

1. `agent_type` is an open string (forward-compat for a third agent) — never an exhaustive enum that rejects unknowns.
2. Unknown `t` / unknown `kind` / unknown `status` MUST be tolerated (log + ignore or render as generic), never a hard decode failure. (Vendor-drift resilience.)
3. `metadata` / `args` are free-form JSON objects.
4. Timestamps are RFC3339 strings.
5. A client that reconnects with `from_seq = N` must receive every event with `seq > N` before any newer live delta — no gaps, no dupes.
