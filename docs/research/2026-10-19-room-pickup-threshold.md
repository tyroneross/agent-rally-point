<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr | SPDX-License-Identifier: Apache-2.0 -->
# Per-repo open-queue revalidation threshold

The age signal uses each repo's own history. No other repo's data enters the calculation. The policy is implemented in `crates/rally-cli/src/pickup_threshold.rs`.

- With at least 8 pickups, the threshold is the greater of 1 hour and that repo's nearest-rank p95 pickup time.
- Below 8 pickups, no age signal is emitted (`pickup_p95_source: insufficient_history`). Scope drift, later completion evidence and linked-plan status still apply.
- A pickup is the first non-system resolve, receipt or artifact referencing an original open-queue handoff. A sender closing its own posting does not count.
- Crossing the threshold asks the agent to revalidate before claiming. Age does not close work or authorize releasing another session's claim.

## Why these defaults

The minimum sample of 8 limits the early-labeling bias of a p95 estimated from very few pickups. Historical analysis supported this default, but rested on only four rooms with at least 20 pickups; it does not establish a universal cutoff. The 1-hour floor prevents near-instant acknowledgments from making almost every queued item look overdue.

## Verify or reproduce

`crates/rally-cli/src/pickup_threshold.rs` tests the minimum sample, percentile calculation and per-repo isolation. The open-queue CLI regressions in `crates/rally-cli/tests/open_queue_pickup.rs` verify how the signal reaches agents.

For a local history analysis, run:

    python3 -I scripts/research/room_pickup_threshold.py <db-list.txt> [--names]

Each ledger is opened read-only with immutable SQLite access. The analysis writes no ledger state and uses a fixed bootstrap seed for reproducibility. Keep local corpus lists and per-run measurement output private.
