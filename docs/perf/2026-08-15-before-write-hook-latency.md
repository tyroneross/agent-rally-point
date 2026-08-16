<!-- SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# before-write hook latency: option A, before and after

Baseline `96a431c` (`rally 0.2.1+96a431c`), after `f18b583` (`rally 0.2.1+f18b583`).
Harness: `scripts/bench_hook_latency.py --repeat 20`, which times
`bash hooks/rally-coordination-hook.sh before-write <host>` end to end with a
host envelope on stdin, against a throwaway repo holding a real `.rally` store
and a seeded peer claim. Each iteration uses a fresh target path, so a run
accumulates claims exactly the way a working session does.

## Result

| Scenario | before p50 | after p50 | | before p95 | after p95 | |
|---|---|---|---|---|---|---|
| `claude_1path` | 608.4 ms | **59.9 ms** | 10.2x | 752.4 ms | **70.1 ms** | 10.7x |
| `codex_4path` | 798.5 ms | **65.3 ms** | 12.2x | 821.9 ms | **107.7 ms** | 7.6x |
| `claude_pure_read` | 28.1 ms | **20.0 ms** | 1.4x | 28.9 ms | 20.9 ms | 1.4x |

Load average 8.0-9.3 before, 6.4 after. Nine `node` spawns and one perl
watchdog per fire became zero.

| Target | Result |
|---|---|
| p50 <= 100 ms, 1 path | **met** — 59.9 ms |
| p50 <= 150 ms, 4 paths | **met** — 65.3 ms |
| p95 <= 250 ms at load-avg >= 5 | **met** — 70.1 ms and 107.7 ms |

## What the win actually was, and what it was not

The obvious story is that removing nine node spawns removed nine node spawns.
That is real — about 219 ms of measured interpreter startup on the baseline —
but it is not most of the difference, and an intermediate build proves it.

Before the renewal fix, the native path had already removed every interpreter
and still measured p50 89 ms with **p95 3969 ms**, five times worse than the
shell it replaced. `RALLY_HOOK_TRACE=1` attributed it: with the invoking tool
owning no claims, `snapshot` cost 59 ms; with 30 claims owned, the same call
cost 1578 ms, while parse, classify, root, check and render stayed under a fifth
of a millisecond in both. Cost scaled with claims the session owned, and a
session that edits all day accumulates claims all day.

`renew_owned_claim_leases` was taking its own full `room.snapshot()` on top of
the one the transaction already held, then renewing every owned claim. The
transaction now captures the snapshot once and hands it down, and renewal is
skipped with one stderr line when under 300 ms of budget remains.

Measured at 30 owned claims, interleaved before/after: `snapshot` collapsed from
691-1312 ms to 4-10 ms. **The duplicate snapshot was worth about 29%, not the
bulk.** What remained was thirty sequential `renew_claim_lease` calls, each
taking the room mutation lock and re-scanning every segment — now visible as its
own `status` trace stage rather than hidden inside `snapshot`.

Two other fixes mattered more than their size suggests. The capability probe was
discarding a verdict it had already computed whenever the marker could not be
persisted, so a read-only `.rally/.hook-seen` meant the native path was never
taken and every fire paid a probe spawn *plus* the whole Node path. And the
probe cache never actually cached: it trusted the marker while
`[ "$marker" -nt "$bin" ]`, and macOS ships bash 3.2.57, whose `-nt` compares
whole seconds — the marker was routinely written in the same second as the
binary, the comparison tied, and every fire re-probed.

Together those are why `claude_pure_read` is now *faster* than baseline rather
than slower, having gone the wrong way (28 ms to 48 ms) in the intermediate
build.

## Stage attribution now

A single-path fire on a fresh store:

```
{"parse":0.01,"classify":0.00,"root":0.02,"open":25.2,"snapshot":3.1,
 "status":28.0,"check":0.10,"append":23.8,"render":0.00,"total":80.3}
```

Everything that is not the store totals under a fifth of a millisecond. The
remaining cost is four ledger operations, which is the honest floor for a
transaction that must read the room and durably record a claim.

## Next lever

Batched lease renewal: one lock acquisition, one segment read, N appends,
instead of N of each. That is what the `status` stage still pays at high claim
counts, and it is a lease-durability change that deserves its own review rather
than a rider on this one.

## Method notes

This machine carried nine user sessions throughout and never sat idle; every
figure records its own `getloadavg()` and none should be read as an unloaded
number. An earlier draft of this document reported a p95 regression measured
while a full `cargo test` run and leaked load generators were competing for the
machine; those numbers are superseded by the table above, which was taken at a
load average comparable to the baseline's.

`bash -x` attribution falls back to stderr capture because macOS ships bash
3.2.57 and `BASH_XTRACEFD` needs 4.1+. Spawn counts are approximate: xtrace
marks the start of a traced command, not every line of a multi-line quoted
argument.
