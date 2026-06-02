<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr | SPDX-License-Identifier: Apache-2.0 -->
# Canonical Checkout Migration

This repo's canonical source checkout is:

```text
/Users/tyroneross/dev/git-folder/agent-rally-point
```

The checkout should be on `main` and track `origin/main`. Other local worktrees
are temporary lanes, not canonical source locations.

## What Moves

Move source work into this checkout when it belongs to the `agent-rally-point`
product itself:

- Rally CLI, store, discovery, managed-session, and schema changes.
- Rally documentation and migration guides.
- Plugin extraction surfaces that are part of the Rally product.
- Vendored legacy references under `tools/` (e.g.
  [`tools/agent-rally-watcher/`](../tools/agent-rally-watcher/), superseded by
  [`docs/SPEC-rally-watch-autonomy.md`](SPEC-rally-watch-autonomy.md)).

Keep project coordination in the project repo that owns the work:

- Easy Terminal work coordinates from `/Users/tyroneross/dev/git-folder/easy-terminal/.rally`.
- Build Loop work coordinates from the Build Loop repo or its resolved canonical channel.
- A plugin or sibling repo keeps its own `.rally` unless the work is explicitly an
  `agent-rally-point` source change.

Do not merge another project's live `.rally` ledger into this repo. A universal
registry may point to canonical ledgers, but the ledger itself stays with the
owning repo.

## Universal Ledger Locator

Use one resolver shape everywhere:

1. Start from the repo that owns the work.
2. Read that repo's `.rally/manifest.json`.
3. Write facts to that repo's `.rally/log/<engagement>.jsonl`.
4. Use `~/.agent-rally-point/rooms/v1/index.json` only as a pointer index for
   discovery, status rollups, and "where is this room?" lookups.

That gives separately installed repos and plugins a common path to find each
other without centralizing their ledgers. The global index is backup/discovery
metadata; it is not the communication source of truth.

## Active Rally Migration Rule

Existing active rallies finish where they started. Do not repoint a live room
while agents are still using it.

Use this cutover sequence:

1. Check the owning room: `rally room --json`.
2. Wait until `active_claims=0`, `active_blockers=0`, and `open_handoffs=0`, or
   until the lead posts an explicit cutover decision.
3. Preserve the old ledger with a bundle or archive copy.
4. Move only source changes that belong in `agent-rally-point`.
5. Leave project facts in the project ledger and post a final pointer fact from
   the old room to the new source location if needed.

For Easy Terminal specifically, the priority rule is:

```text
If the work affects Easy Terminal, coordinate from easy-terminal/.rally even
when the source edit happens in ptyd, build-loop, or agent-rally-point.
```

That rule prevents a sibling repo from becoming a hidden coordination hub for an
Easy Terminal fleet run.

## Local Worktree Cleanup

Before retiring a local worktree:

1. `git status --short --branch`
2. `git rev-list --left-right --count origin/main...HEAD`
3. `git diff --stat origin/main...HEAD`
4. Archive any dirty diff with `git diff --binary --output <archive>.diff`.
5. Remove the worktree only after its branch is merged, intentionally deferred,
   or archived.

Known local lanes after this migration:

- `feat/agent-cockpit`: large feature branch; review separately.
- `fix/lane-a-test-flake`: old branch with a local test serialization patch; port
  only after review.
- `fix/b19-codex-hook-repoint`: preserved branch with coordination-ledger history;
  do not merge wholesale into `main`.
- `agent-rally-point-coord`: broken stale worktree snapshot, archived at
  `/Users/tyroneross/dev/git-folder/_archive/agent-rally-point-canonical-migration-20260601T231008Z/agent-rally-point-coord-broken-snapshot`.
