# Claims and dirty-owner output contracts

These projections retain claims after lease expiry. Expiry alone does not prove
abandonment and does not delete, release, or reap a claim.

## `rally claims --json`

Envelope schema: `agent-rally.command.claims.v1`. `data.claims.rows` retains
all existing fact fields and adds:

| Field | Type | Meaning |
| --- | --- | --- |
| `lease_expires_at` | string or null | Lease marker parsed by claim authority; null if absent. |
| `expired` | boolean | A parseable RFC 3339 lease is at or before the current time. Missing or invalid timestamps yield false. |
| `lease_state` | string | `active` for a parseable future lease, `expired` for a parseable lease at or before now, `none` for a missing or invalid lease. |
| `age_seconds` | integer or null | Current time minus `created_at` in whole seconds; null if the creation timestamp is unparseable. Future creation timestamps yield negative ages. |
| `stale_no_lease` | boolean | `lease_state` is `none` and age strictly exceeds the threshold. Defaults to 7 days; `RALLY_CLAIM_STALE_NO_LEASE_DAYS` accepts a nonnegative integer, including zero. Invalid, negative, or overflowing values fall back to 7 days. |

Rows appear with unexpired claims first (including no-lease claims), preserving
ledger order within each group. These fields are display metadata only and do
not change claim authority, release, reaping, or dirty-owner behavior.

Human output starts with `claims N (active A, expired E, no-lease L)`, followed
by one line per claim: `<event_id> <tool> <lease-state> <subject>`.
Lease state is `expires HH:MMZ` in UTC, `expired Nh ago` in whole hours, or
`no-lease age Nd` in whole days. Unparseable creation timestamps display
`no-lease age unknown`; future ages display zero days. Subjects have whitespace
flattened to spaces and are capped at 72 Unicode characters, including a trailing
`…` when truncated. JSON retains the full original subject.

## `rally owners --dirty --json`

Envelope schema: `agent-rally.command.owners.v1`. Existing field names and types
remain unchanged. Each `data.owners.dirty` row adds:

| Field | Type | Meaning |
| --- | --- | --- |
| `ownership_status` | string | `claimed` for an unexpired match; `expired_match` for expired evidence alongside an unexpired match; `unclaimed_after_expiry` when the path has only expired matches. |
| `expired_match` | fact object or null | Original expired claim, including claimant identity, scope, and evidence; null for unexpired matches. |

`owner_tool` keeps its existing value for every row, including expired
matches; read `ownership_status` and `lease_expired` to tell an expired match
from a current owner. Session and heartbeat fields, including `is_owner_live`,
continue to describe the claimant's observed liveness; they do not renew its
lease. Missing or invalid lease timestamps do not establish expiry.

Every overlapping match remains a separate row. `unclaimed_dirty_paths` keeps
its existing meaning: dirty paths with no matching claim at all. Two fields are
added to `data.owners`:

| Field | Type | Meaning |
| --- | --- | --- |
| `unclaimed_or_expired_dirty_paths` | string array | Dirty paths with no unexpired match, including paths matched only by expired claims. |
| `dirty_after_expiry` | integer | Distinct dirty paths whose matches are all expired, counting overlapping expired claims once. |

Human output (not a stable contract) reports `unclaimed=` as the length of
`unclaimed_or_expired_dirty_paths`, the `dirty_after_expiry` count, and one line
per matching row with its ownership status and claimant tool.
