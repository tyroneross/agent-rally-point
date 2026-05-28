# Discovery Re-Port Design

PR45 replaced the PR40 multi-crate channel model with one `rally` CLI and a
repo-local `.rally/facts.db` RoomStore. The old `rally-core::channel_index`
implementation must not be merged into this branch. The useful product behavior
should be re-ported against the new RoomStore boundary.

## Goal

Reintroduce local discovery without reviving the old global channel substrate:

- `rally locate <event-id> --json` finds a fact or managed-session event when
  the user has an id but not the repo or room.
- `rally recent --all --limit <n> --json` shows recent local Rally activity
  across known repo rooms.
- Legacy `~/.agent-rally-point/apps/*/changes.jsonl` channels remain visible so
  older coordination history is not silently stranded.

Discovery is read-only. It does not schedule work, mutate room state, or
resolve coordination obligations.

## Current Boundary

PR45 state lives here:

```text
<repo>/.rally/facts.db      canonical factstr-sqlite event store
<repo>/.rally/cursors.json  per-tool read cursors
```

`RoomStore::open()` resolves the current repo root and opens `.rally/facts.db`.
Room projections come from `RoomStore::facts()` and `RoomStore::snapshot()`.
Discovery should call that boundary or a small sibling reader, not parse
factstr internals ad hoc.

## Proposed Shape

Add a small `discovery` module inside `crates/rally-cli/src/`.

```text
crates/rally-cli/src/discovery.rs
  KnownRoom
  RoomIndex
  IndexedFact
  DiscoveryWarning
  locate_fact(id)
  recent_facts(limit, include_legacy)
```

Keep the module independent from command rendering. CLI commands should stay in
`cli.rs` / `lib.rs`; discovery should only return typed data.

## Room Registry

Because `.rally/` is repo-local, cross-repo discovery needs an index. The index
should be metadata only:

```text
~/.agent-rally-point/rooms/v1/index.json
```

Each entry:

```json
{
  "schema": "agent-rally.room-index.v1",
  "repo_root": "/abs/path/to/repo",
  "display_name": "agent-rally-point",
  "facts_db": "/abs/path/to/repo/.rally/facts.db",
  "last_seen_seq": 42,
  "last_seen_at": "2026-05-28T19:00:00Z"
}
```

Rules:

- Opening a `RoomStore` may refresh the current repo's index entry.
- Missing or moved repos produce warnings, not hard failures.
- The index is not coordination truth. If it disagrees with `.rally/facts.db`,
  the fact store wins.
- Manual users can still pass a repo path later if an explicit
  `--repo <path>` flag is added.

## Locate

`rally locate <event-id> --json` should:

1. Search the current repo first through `RoomStore::facts()`.
2. Search indexed `.rally/facts.db` rooms.
3. Optionally search legacy channels under `~/.agent-rally-point/apps/*`.
4. Return the first exact fact id match plus all source metadata needed for a
   human or agent to act.

Output contract:

```json
{
  "ok": true,
  "product": "rally",
  "command": "locate",
  "schema": "agent-rally.command.locate.v1",
  "data": {
    "located": {
      "source": "room",
      "repo_root": "/abs/path",
      "display_name": "agent-rally-point",
      "fact": {}
    },
    "warnings": []
  }
}
```

If only a legacy record is found, set `source` to `legacy_channel` and include
the legacy channel path. Do not rewrite it into `.rally/facts.db` from `locate`.

## Recent

`rally recent --all --limit <n> --json` should:

1. Read indexed rooms.
2. Merge facts by sequence-aware room order and timestamp as a secondary key.
3. Include room metadata with every row.
4. Include legacy rows only when `--include-legacy` is present, or include a
   warning that legacy activity exists but is hidden by default.

Defaulting legacy rows off keeps the PR45 room model clean while making old
channels visible enough to avoid silent data loss.

## Legacy Visibility

The old path family is:

```text
~/.agent-rally-point/apps/<repo_id>/changes.jsonl
```

Discovery should support three behaviors:

- **Detect:** count legacy channels and expose warnings when records exist.
- **Read:** parse legacy rows for `locate` and optional `recent` display.
- **Defer import:** leave migration to an explicit future command such as
  `rally import legacy --channel <path>`.

No automatic import should happen during discovery. Legacy rows use different
identity, trust, and hash-chain semantics than PR45 facts, so conversion needs a
separate reviewable path.

## Non-Goals

- No resurrection of `rally-core/src/channel_index.rs`.
- No global append log.
- No automatic migration from legacy `changes.jsonl`.
- No watcher or daemon behavior.
- No cross-machine transport.

## Implementation Plan

1. Add the room index writer behind `RoomStore::open()` or an adjacent helper.
2. Add a read-only discovery module that opens known `.rally/facts.db` stores.
3. Add `locate` and `recent` parser entries and JSON schemas.
4. Add tests with two temporary repos plus one synthetic legacy channel.
5. Add warning coverage for stale registry entries and malformed legacy rows.

## Acceptance Criteria

- A fact written in repo A can be found from repo B after both repos have opened
  Rally once.
- A stale index entry reports a warning and does not fail the command.
- A legacy `changes.jsonl` event can be located by id and is clearly marked as
  `legacy_channel`.
- `recent --all` never mutates `.rally/facts.db`.
