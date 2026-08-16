<!-- SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# before-write hook latency: option A, before and after

Baseline `96a431c`, after `39b27c1`+ (`bd4532e` adds no runtime code). Harness:
`scripts/bench_hook_latency.py --repeat 20`, which times
`bash hooks/rally-coordination-hook.sh before-write <host>` end to end with a
host envelope on stdin, against a throwaway repo holding a real `.rally` store
and a seeded peer claim.

## Headline

**Median latency improved 6.8x. Tail latency got about 5x worse, and the tail
degrades with every claim the session accumulates.** Both halves of that
sentence are load-bearing; the second one is the reason this document exists.

| Scenario | before p50 | after p50 | | before p95 | after p95 | |
|---|---|---|---|---|---|---|
| `claude_1path` | 608.4 ms | **89.3 ms** | 6.8x faster | 752.4 ms | **3968.5 ms** | 5.3x slower |
| `codex_4path` | 798.5 ms | **380.0 ms** | 2.1x faster | 821.9 ms | **1526.9 ms** | 1.9x slower |
| `claude_pure_read` | 28.1 ms | **47.6 ms** | 1.7x slower | 28.9 ms | 63.8 ms | 2.2x slower |

Node spawns per fire went from 9 to 0, and the perl watchdog from 1 to 0.

## Against the stated targets

| Target | Result |
|---|---|
| p50 <= 100 ms, 1 path | **met** — 89.3 ms |
| p50 <= 150 ms, 4 paths | met on a fresh store (54.9-84.8 ms measured separately); **missed** at 380.0 ms in the 20-iteration run, for the reason below |
| p95 <= 250 ms at load-avg >= 5 | **missed**, decisively |

## Why the tail grows

`RALLY_HOOK_TRACE=1` emits one stderr line of per-stage milliseconds. On a store
where the invoking tool owns no claims:

```
{"parse":0.01,"classify":0.00,"root":0.02,"open":61.0,"snapshot":59.5,
 "check":0.09,"append":54.4,"render":0.0,"total":174.9}
```

On the same store, same binary, when that tool owns 30 active claims:

```
{"parse":0.01,"classify":0.00,"root":0.02,"open":8.4,"snapshot":1578.2,
 "check":0.04,"append":51.7,"render":0.0,"total":1638.4}
```

Parse, classify, root, check and render together stay under a fifth of a
millisecond in both. Everything else is the store, and `snapshot` alone moves
from 59 ms to 1578 ms.

The cause is `renew_owned_claim_leases` (`crates/rally-cli/src/lib.rs:2288`). It
takes its **own** full `room.snapshot()` — a second one, on top of the snapshot
`run_before_write` already holds — and then appends one lease renewal per claim
the tool owns. `hook_runtime.rs:1909` calls it on every fire. Cost is therefore
O(claims owned by this session), paid on every edit, and a long-lived session
accumulates claims all day. That is the production condition, not the corner
case.

The benchmark reproduces it honestly because each iteration uses a fresh target
path, so the tool holds 20 claims by the end of a 20-iteration run and the later
iterations are the tail.

## Is this a regression?

Partly, and the comparison is worth stating precisely. On the same store where
the invoking tool owned about 35 active claims:

- native path: **p50 1875 ms**
- `RALLY_NATIVE_HOOK=off` (the Node fallback it replaces): **p50 709 ms**

Both completed the auto-claim — verified by reading the claim back out of the
ledger — so the fallback is not winning by giving up. Under accumulated claims,
the native path is 2.6x **slower** than the path it replaces, while being 6.8x
faster on a fresh store.

The structural difference is visible in the call sites: `command_status_post`
(`lib.rs:5150`) invokes `renew_owned_claim_leases` inside
`with_watchdog_command_commit`, and the shell drove it through a 400 ms perl
watchdog on top of that; `hook_runtime.rs:1909` calls it bare under the
transaction's single 3000 ms deadline. The old path was being cut off partway
through renewal. The new one finishes the job — which is more correct and also
much slower.

## Pure reads got slower, on purpose

28.1 ms to 47.6 ms. The native branch runs before the envelope is read, so a
pure read now resolves the binary and consults the cached probe before reaching
the short-circuit, where previously it exited earlier in shell. The binary still
classifies it as a pure read and still returns `{}` with no ledger work. This is
the cost of exec-first and it was a deliberate choice, not a discovery.

## Next lever

Two changes, in order of expected effect:

1. Pass the snapshot `run_before_write` already holds into
   `renew_owned_claim_leases` instead of taking a second one.
2. Bound renewal to leases actually near expiry, rather than renewing every
   owned claim on every edit.

Neither is in this build. The first is mechanical; the second is a policy change
about lease durability and belongs with its own review.

## Method notes

This machine sat at load-avg 3-12 with about nine user sessions throughout, so
no genuinely unloaded number was available; every figure records its own
`getloadavg()`. `bash -x` attribution falls back to stderr capture because
macOS ships bash 3.2.57 and `BASH_XTRACEFD` needs 4.1+. Spawn counts are
approximate: xtrace marks the start of a traced command, not every line of a
multi-line quoted argument.
