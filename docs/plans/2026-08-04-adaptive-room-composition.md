<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Plan — adaptive room composition, claim/handoff expiry, v0.2.0

## Governing thought

`rally room --json` costs 1.55 MB because Rally computes correct adaptive verdicts and
then does not act on them. Three instances of one root cause; fix the cause, not the
symptom, and add a byte budget as the backstop rather than the mechanism.

## Measured baseline (this repo, 2026-08-04, HEAD fa82c00)

| Bucket | Bytes | Share | n | per-item |
|---|---:|---:|---:|---:|
| `stale_facts` | 1,308,136 | 84.2% | 1390 | 941 B |
| `open_handoffs` | 59,880 | 3.9% | 51 | 1174 B |
| `system_health` | 57,426 | 3.7% | 78 | 736 B |
| `active_claims` | 47,520 | 3.1% | 69 | 688 B |
| `recent_artifacts` | 23,773 | 1.5% | 20 | 1188 B |
| `unconsumed_artifacts` | 21,461 | 1.4% | 18 | 1192 B |
| `squads` | 20,674 | 1.3% | 155 | 133 B |
| everything else | ~2,700 | 0.2% | — | — |
| **total (`data`)** | **1,553,233** | | | |

Leak composition, re-confirmed at source:
- `active_claims` 69 — **65 held by one session**; `rally doctor --reap-stale` dry-run
  reports **69/69 already eligible** (65 `lease-expired`, 4 `owner-stale+lease-expired`).
- `open_handoffs` 51 — 42 older than 30 days; largest holder `claude_code:rca-obs` (13).

## Root cause

**Rally reaches an adaptive verdict and then does not honor it.**

| Instance | Verdict computed | What happens instead | Cite |
|---|---|---|---|
| `stale_facts` | below `archive_floor_weight` → partitioned out of active buckets | serialized in full anyway | `store.rs:2738-2820`, `store.rs:403` (no `skip_serializing_if`) |
| `active_claims` | lease + owner-staleness eligibility, fail-closed | reaper is reachable only via `rally doctor --reap-stale --apply`; nothing invokes it | `reaper.rs:82-198`, sole caller `lib.rs:2953` |
| `open_handoffs` | none exists | handoffs are immortal; only a 24 h de-prioritization in `next` | `store.rs:2506-2510`, `next.rs:10,723-733` |

Squads in the same function do it correctly (`store.rs:2929`): a provably-stale squad is
dropped from the default snapshot and restored under `include_archived`. Facts must
follow the squad pattern.

**No-silent-truncation precedent:** RC-027 — the watcher tailed a dead channel for five
weeks because "no events" and "no source" are indistinguishable from inside a tail.

## Design principle (binding, operator-set)

> Blind default cuts are never good. Everything should be based upon dynamic signals and
> able to flex across different use cases.

No fixed count caps. The three existing `truncate(20)` calls are in scope for removal.
Every threshold resolves through the existing `hooks_config` chain
(default → user → repo → env).

## Approach lenses

**Clean sheet.** Room composition is a ranking problem: score every candidate by signals
already in the system, fill a byte budget by descending score, report what was left out.

**Current constraints.** `decay::recency_weight`, `liveness::is_live`, and
`CoordinationConfig` already exist, are pure, time-injected, and pinned by golden-vector
fixtures shared with a Python mirror. The clean-sheet answer is reachable by composing
them — no new coordination surface. Both lenses agree.

## Relevance model

`relevance = recency × author_liveness × addressed_to_me × path_overlap`

Multiplicative, with recency as the spine. Every factor defaults to **1.0 when its signal
is absent**, so a missing signal never demotes an item — the fail-open contract, stated as
an invariant a test can assert.

| Factor | Source | Absent → | Config key |
|---|---|---|---|
| recency | `decay::recency_weight(age, half_life)` | unparseable ts → 1.0 (existing) | `half_life_hours` |
| author liveness | `liveness::is_live` over the 4 signals | `Live`/`Unknown` → 1.0; only provable `Stale` demotes | `relevance.stale_author_factor` |
| addressed-to-me | `fact.target == caller`, `to:<caller>` evidence, caller in scope | no caller → 1.0 | `relevance.addressed_boost` |
| path overlap | fraction of caller `--path` args matching `fact.scope` | no paths → 1.0 | `relevance.path_overlap_boost` |

## Bucket classes (MECE)

**Never cut — correctness-bearing.** Dropping one risks the write collision Rally exists
to prevent, or hides an assignment.
`active_claims`, `active_blockers`, `squads`, `lead`, `mission`, `pending_wakes`, and any
`open_handoffs` whose `target` is the caller. Their size is controlled by *correctness*
(expiry), never by cutting.

**Budgeted — informational.** Relevance-ordered, budget-filled, with a guaranteed
top-1-per-bucket pass so no non-empty bucket vanishes.
`current_decisions`, `current_risks`, `system_health`, `recent_artifacts`,
`unconsumed_artifacts`, `open_handoffs` not addressed to the caller.

**Archived.** `stale_facts` — not serialized unless `--include-archived`. This honors the
verdict the fold already reached.

## Safety bound

`room_budget_bytes = room_budget_fraction × consumer_context_bytes`, both config values,
both env-overridable, plus a `--budget-bytes` flag for an explicit caller. Setting either
to 0 disables the ceiling entirely (unbounded = prior behavior). The ceiling is a
backstop, not the mechanism: after the leak fix a healthy room sits well under it.

**Graceful degradation is mandatory.** Every room response carries `totals` (true
pre-budget counts for every bucket, always, whether or not anything was omitted). When
anything is omitted the response also carries `composition` with per-bucket
`{total, emitted, omitted}` and, for the actionable classes, `omitted_ids`. Silent
truncation is forbidden — that is the RC-027 failure mode.

## Work chunks

| # | Chunk | Owned files | Depends on |
|---|---|---|---|
| C1 | `relevance` module + config surface | `crates/rally-cli/src/relevance.rs` (new), `hooks_config.rs` | — |
| C2 | Snapshot composition: drop archived, totals, budget fill | `store.rs` | C1 |
| C3 | Handoff expiry parity (`handoff.expired` kind + reaper arm) | `store.rs`, `reaper.rs`, `claim_authority.rs` | C2 |
| C4 | Activation: reap-on-enter, `rally stop` unmanaged fallback, SessionEnd hook | `lib.rs`, `config/host-integrations.json`, generated hook surfaces | C3 |
| C5 | Rotate threshold via config; `next` stale-wait via config | `init.rs`, `.rally/manifest.json`, `next.rs`, `hooks_config.rs` | — |
| C6 | Regression + mutation-validated controls | `crates/rally-cli/tests/room_budget.rs` (new), in-crate unit tests | C2–C4 |
| C7 | Docs: RALLY.md, README.md, PROTOCOL.md, root-cause register | docs only | C2–C5 |
| C8 | Release v0.2.0 | version files, CHANGELOG.md, tag | all |

C1–C6 all land in `crates/rally-cli/src/store.rs` or its immediate neighbours, so they run
**sequentially in one owner** — parallel fan-out into an 8 400-line file holding the
system's core invariants would trade correctness for wall-clock. C7 runs in parallel once
the design is frozen.

## Path A vs Path B

**Path A** — hardcode a byte cap in `command_room`. Ships in an hour, adds a second
capping layer beside the decay policy, and re-creates the defect class the plan exists to
remove.

**Path B (chosen)** — extend the typed policy surface (`CoordinationConfig`) with a
relevance sub-config and add `RoomTotals`/`RoomComposition` to the snapshot type. Named
future capability it unlocks: per-consumer room views (`rally room --tool X` returning a
genuinely different ranking), which the `agent_injectability` and `next` surfaces already
want, and a `handoff.expired` kind that the retrospective and `check-ci` surfaces can read.
No gate blocks: no missing dep, no missing design decision, time cost well under 2×.

## Version decision — 0.2.0, not 0.1.8

`stale_facts` goes from 1390 items to `[]` by default. The schema contract holds (the
field stays required and stays an array), and `--include-archived` restores the content,
but any consumer that read it gets an empty array where it used to get data. In 0.x the
minor is the breaking slot. 0.1.8 would understate a change that alters 84% of the default
payload; 0.2.0 tells consumers to look.

## Falsifier

If after the leak fix a healthy room still needs the byte ceiling to bind in normal
operation, the diagnosis was wrong — the size would be genuine working state, not
abandoned residue, and capping it would be the blind cut this plan rejects.

## Acceptance

1. `rally room --json` on this repo drops below 250 KB with no ceiling engaged.
2. `rally room --include-archived --json` still returns every archived fact.
3. `totals.stale_facts` reports 1390 while `stale_facts` is `[]` — the count never lies.
4. A synthetic 10 000-fact ledger stays under budget, and the response says what it omitted.
5. Each control fails when its fix is reverted (mutation-validated).
6. Release workflow succeeds and attaches all four target assets.
