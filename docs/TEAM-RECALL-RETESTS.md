<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Team recall retests (R1–R3)

Simple lead-authors / members-recall checks for shared-context consistency.
Run before starting work (R1, R2) and before merge in `strict` mode (R3).
Background: no TEAM/CHARTER/GOAL file existed before this change (workflow
slots 0/3 confirmed); per-repo `.rally/log/*.jsonl` is the canonical durable
record with `facts.db`/`snapshot.cache.json` disposable.

## Setup (all retests)

1. `rally enter --tool <you> --json`, then `rally ack --tool <you>`.
2. Lead writes mission + ordered task list with a `context_version`
   (e.g. `TEAM-CTX-1`) and a checksum last line.
3. Each member quotes back version + order before starting.

## R1 — ordered task-list recall (the core ask)

Lead file (example):

```text
TEAM-CTX-1
1. ALPHA: Stabilize shared-memory write protocol
2. BRAVO: Add TEAM_CHARTER.md with roles
3. CHARLIE: Wire rally native event notifications
Checksum: ALPHA-BRAVO-CHARLIE-1
```

Pass: every member reproduces all items verbatim in order + checksum.
Fail: any dropped, reordered, or reworded item. Verified live 2026-09-20 with
two fresh Muse subagents (both verbatim, checksum matched).

## R2 — instruction recall

Lead adds one constraint line (e.g. "Resolve tradeoffs from the mission; don't
stall."). Pass: each member quotes the constraint + names its source
(`context_version` + mission seq). Catches paraphrase drift R1 misses.

## R3 — mutation recall (strict mode / pre-merge)

Lead issues a mid-task mutation (swap two items or reword one) under a new
`context_version` (`TEAM-CTX-2`) and requires re-ack. Pass: members quote the
new order and cite the new version; any quote of the old version fails.
Catches stale-context work before the merge gate.

## Offline simulator

`scripts/team_recall_retest.py` replays R1/R3 without agents: lead writes a
list file, N simulated members read it back, one may be given a stale or
mutated copy; mismatch exits non-zero. Committed coverage in
`tests/test_team_recall.py`.
