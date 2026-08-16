<!-- SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Changelog

All notable changes to Agent Rally Point are documented here.

## v0.2.5 - 2026-08-15

> Release date is set when the tag is cut; entries for the native before-write hook and RC-073 land under this heading before then.

### Fixed — every repo but this one committed rally's derived state (RC-072)

RALLY.md's "Where State Lives" table called `facts.db`, `rallyd.sock`, and the
`*.owner.lock` family gitignored. Nothing in the product ever wrote an ignore rule.
agent-rally-point's own root `.gitignore` carried one by hand, so the gap was invisible
here and universal everywhere else: measured at `56a6e39`, `rally init` in a scratch
repo left `git check-ignore .rally/facts.db` at exit 1 — not ignored — for all eight
documented paths, alongside `cursors.json`, `claim-index.json`, `.reconcile-cache.json`,
and `snapshot.cache.json`. A consumer following the docs committed a SQLite cache, a
host-scoped flock, and a socket path naming the machine they were cut on.

`rally init` now writes `.rally/.gitignore` covering the derived, lock, and cache
artifacts — including the WAL/SHM sidecars a `facts.db`-only rule would miss, which is
how a committed WAL carries frames the ledger never saw. **First-open auto-init writes
it too**, on both the direct-open and daemon cold-start paths: `rally enter` creates
`.rally/` far more often than anyone runs `rally init`, so an init-only fix would have
left the common case exactly as broken. `log/`, `archive/`, `manifest.json`, and the
`.gitignore` itself stay committable. Regeneration is idempotent and touches only the
fenced `# rally:ignore:start` … `# rally:ignore:end` block, so rules you add outside it
survive.

**Your repo's root `.gitignore` is never read or written.** A nested ignore file needs no
cooperation from the repo owner, and a tool that edits the root file eventually clobbers
a rule it did not write.

The list is a denylist, which carries one named risk: a derived artifact added to rally
later must be added to it. The control is a test that exercises a live room and then
sweeps `.rally/` — any non-canonical file left committable fails the build, so a new
cache lands as a red test rather than a surprise in someone's `git status`.

### Fixed — retraction and release could close a claim through spellings the write gate did not check

One defect wearing five names (R1–R5): the write-authority gate asserted a correct rule
on the paths it knew, and the ledger accepted the same EFFECT through paths it did not.
This is the fifth cycle of that class here (RC-029, ARP-R-01, ARP-R-02, retraction, and
now the release scope-sweep), so the last change is a test that enumerates the class
rather than another instance of it.

- **A non-owner could strip a live claim with `rally retract` (R1).** A retraction drops
  its target from every projection bucket, so pointing one at an active claim removed it
  from `rally room`, from `check before-write`, and from every peer's session-start
  context — the same observable effect a `release` has, reached by a path the gate never
  saw, because a retraction is not one of the four kinds `closes_active_claim` lists.
  Claim-targeting retractions now run the SAME three-arm policy as claim-close (owner,
  stale-owner takeover, typed expired-lease cleanup), sharing one policy body rather than
  a second copy. Retraction of a non-claim fact stays open to anyone, on purpose: an
  honest mistake has to stay fixable without asking permission.

- **A release could sweep a peer's claim through `--scope` (R5).** A release closes every
  active claim whose scope overlaps its own free-text `fact.scope`, while the gate
  authorized only the claim named by `ref_id` and never read `fact.scope`. Authorization
  and effect were keyed off two different fields, so
  `rally say release --ref <your own claim> --path <victim's path>` passed a gate that
  had nothing to do with the claim being closed. Every claim the sweep would take is now
  checked, by calling the projection's own predicate rather than restating it.

- **A retracted claim still blocked its own scope (R3).** The room projection filters
  retracted facts before projecting active claims; `detect_conflict` read raw facts. A
  withdrawn claim was therefore invisible in `rally room` yet still refused every later
  claim on its scope, and no surface could explain the refusal. Conflict detection now
  reuses the projection's own retraction filter, so both answers agree by construction —
  including the second-order case, where a retracted RELEASE revives the claim it closed.

- **Retraction detection lost a carrier (R2).** The `retracts=<id>` summary token was a
  third spelling of one act, and every gate reasoning about retraction had to cover all
  three. build-loop's resolver already writes the anchored `retract: <id>` subject
  redundantly and checks it first, so the token caught nothing the subject missed. It is
  still EMITTED — build-loop parses `superseded_by` out of the same bracket block, and
  strips exactly `[retracts=...]` off a surfaced reason — but it is no longer read here.

- **The docs said something untrue, and now do not (R4).** `RALLY.md`'s "Nothing is ever
  dropped silently" described budget truncation only. Retraction is a second removal path
  with a different signal: truncation reports itself through `totals` and `composition`,
  retraction removes by design and leaves the retraction record behind. Both are named
  now, and `RALLY.md` plus `rally retract --help` state the claim-close rule and its
  limit — identity is self-asserted and unsigned, so this stops accidents and honest
  mistakes, not an agent that lies about its name.

- **New: an exhaustive closing-effects test.** `FactKind` is matched with no wildcard arm,
  so a new kind cannot compile until someone decides whether it can close or mask a claim
  and, if it can, routes it through the authorization gate. The class of "a correct rule
  guarding one spelling" now fails at compile time rather than in the next audit.

### Fixed — `rally retract` could move the lead seat (RC-071a)

The R1 ruling gated retraction of an active CLAIM and left everything else open. The lead
seat fell through that line: it is a non-claim fact that carries authority. Every control in
the room hangs off the seat — the room-wide claim gate and the room freeze both read "is this
agent the lead" — and `lead handoff`, `lead assign`, and `lead relinquish` were all gated for
exactly that reason, while `rally retract <the seat's decision>` moved the room's authority
root by the one path nothing checked. Sixth instance of this codebase's oldest defect: a
correct rule guarding one spelling while the ledger accepts the act.

The rule now, operator-ruled: **authority-carrying facts are gated; prose is free.**

- **The seat's removal follows the seat's own policy.** Withdrawing the decision the lead seat
  rests on moves the seat, so it needs what `lead handoff` needs: the holder, a takeover after
  the holder's silence window, or an acknowledged `--force` seizure. One policy body, two entry
  points — the gate and the transfer command cannot drift apart. Retracting an artifact, a risk,
  or an ordinary decision stays open to anyone: an honest mistake has to stay fixable without
  asking permission.

- **The gate asks the room, not a list of spellings.** It does not test "is this a lead
  decision". It derives the seat, derives it again with the target withdrawn, and gates only
  when the two answers differ. That covers the case a spelling-keyed gate would still have
  missed — withdrawing the current decision so the seat falls BACK to an earlier lead — and
  correctly leaves a superseded lead decision open, because withdrawing it moves nothing.

- **The room-wide claim gate now reads the seat the room actually shows.** It read the raw
  ledger, so a retracted lead decision still conferred room-wide capability while `rally room`
  already reported no lead. That disagreement was filed and deliberately left, because
  resolving it while retraction was ungated would have let anyone strip the lead's authority.
  With the seat gated, both halves land together — including for the transfer gate, so a lead
  that legitimately withdraws its own seat decision leaves a seat the next agent can take
  rather than one wedged shut.

- **New: an exhaustive seat-movement test.** For every `FactKind` and every shape that could
  move the seat, the room is projected and any fact that actually changed the lead must have
  been refused by the gate. The oracle is the lead projection, not a maintained list, so the
  next single-fact way to move the seat fails this test without anyone remembering it exists.
  Both class tests now also assert they were not skipped end to end — a removal path that stops
  being detected used to turn them green while they asserted nothing.

- **Known limit, recorded rather than implied.** This covers every way a SINGLE retraction can
  move the seat. It does not cover a SEQUENCE that first moves the seat's authorization input:
  the stale-owner arms authorize against a liveness projection that ungated retractions can
  regress. That is filed as RC-071b with the owed decision, and it affects claim takeover the
  same way — it predates this change on both arms.

- **Upgrade note.** A ledger that already contains a retraction of its seated lead decision will
  report no lead after upgrading, and the seat becomes takeable. Before this change that same
  ledger was worse off, not better: the room showed no lead while every `lead assign` was
  refused and `rally enter` failed outright. This repo's ledger contains no retractions at all;
  a deployment with retraction traffic should check before upgrading.

### Fixed — a kind read off the ledger can now be typed back into `rally say`

- **`rally say backlog_item` is accepted.** `FactKind::BacklogItem` reaches disk as
  `backlog_item` (serde's `rename_all = "snake_case"`), but `FactKind::parse` matched only
  `backlog-item`, so the one spelling a caller could observe was the one spelling the CLI
  refused. Both are accepted now, the same way `claim_expired` already aliases
  `claim.expired`; `backlog-item` stays canonical in `--help` and in `as_str`. A unit test
  round-trips every variant's real serde output back through `parse`, so the next variant
  whose wire spelling and match arm disagree fails in CI rather than at a caller's prompt.

### `facts.db` corruption (RC-044) — reproduced and diagnosed; one arm fixed, the rest escalated

- **A quarantine no longer triggers itself.** `is_malformed_db_error` substring-matched
  `"corrupt"` anywhere in an error, and every quarantine file is literally named
  `facts.db.corrupt.<stamp>` — so an ordinary I/O error that merely *named* leftover debris
  was read as a corruption report and renamed a live database out from under whoever held it
  open. The matcher now blanks the `.corrupt.` filename token before the word test; SQLite's
  numeric codes and human messages are matched exactly as before. (RC-044, second-order arm)

- **New: `scripts/repro_facts_db_corruption.sh`** — the reproduction RC-044 lacked. First-run
  `enter` does *not* reproduce the fault (20/20 clean); concurrent mutation of an established
  room does, yielding the same multi-quarantine bursts and `(code: 522) disk I/O error` seen in
  production. RC-044's standing rule — no fix claim without N-consecutive evidence — is now
  enforceable rather than aspirational.

- **Diagnosis, recorded in `docs/ROOT-CAUSE-REGISTER.md`.** `PRAGMA integrity_check` on the
  accumulated snapshots shows genuine structural damage (`2nd reference to page N`, rowid
  disorder, index/table divergence) — the signature of two writers allocating one page, i.e.
  SQLite's cross-process locking defeated. The recovery path itself is the generator: unlinking
  and renaming a live `facts.db` while peers hold it open, with `-wal`/`-shm` resolved by path,
  cross-wires two databases through one WAL. Each recovery seeds the next corruption. The
  previously suspected binary-version mismatch is **falsified** — the debris continues through
  2026-08-04 on a single version.

- **Not fixed, and deliberately so:** the remaining fix is a product decision about rally's
  concurrency model (serialize all database access, versus a shared-lease/exclusive-recovery
  scheme that additionally needs a daemon recovery protocol). A candidate response-side fix —
  gating quarantine on a re-probe — was built and measured, made quarantines **30% worse**, and
  was reverted; the A/B is recorded so the shape is not retried. `rally doctor --sweep-corrupt`
  remains the supported way to clear accumulated debris in the meantime.

### Added — wrong facts can finally be withdrawn

- **`rally retract <fact-id> --tool <tool> --reason <why> [--superseded-by <fact-id>]`** appends
  a retraction naming the target's event id; the ledger is never rewritten. Read paths —
  `rally room`, `rally next`, `rally recent`, and the session-start hook that shells to them —
  stop surfacing the withdrawn fact while the retraction itself stays visible and auditable.
  The wire shape is cross-store on purpose (kind `artifact`, subject `retract: <event-id>`,
  `ref` = target, `retracts=<id>[ superseded_by=<id>]` summary tokens), so build-loop's
  existing resolver in `scripts/rally_point/retraction.py` honors rally-written retractions
  and vice versa — including under older binaries that remap unknown kinds to `artifact`.
  Guardrails: an unknown target id is rejected before anything is posted, a second retraction
  of the same fact is a reported no-op that points at the prior retraction (and where the
  correction lives), and retracting a retraction is refused so the correction trail cannot
  be erased.

## v0.2.1 - 2026-08-07

### Ledger — one additive schema change, called out because the version number understates it

- **Claim-lease renewal is now a durable `ClaimRenewed` fact** rather than an edit to
  `.rally/claim-index.json`. Renewal previously wrote only to that sidecar, which
  `append_fact` rebuilds from facts after every claim-class write — so a renewal survived
  until the next claim by anyone in the room, and neither expiry path read it. Every claim
  expired at `claim_time + lease` unconditionally, no matter what a renewal caller did.
  `claim_authority::latest_renewed_lease` now folds the fact into the active-claim projection
  and the reaper reads that same projection, so the "no reader honours it" mechanism is gone
  structurally rather than patched. Ownership and monotonicity are re-checked under the
  mutation lock, and a `ClaimExpired` append is refused outright if the lease was renewed.
  Six controls, including daemon parity. (RC-053)

  **Compatibility, stated plainly because a patch bump does not signal it:** this adds a fact
  kind to the ledger schema. A **0.2.0 reader replaying a 0.2.1 ledger will meet a
  `ClaimRenewed` kind it does not know.** Rooms are repo-local and single-operator, so in
  practice this bites only a machine running a stale `rally` against a room another version
  has written. Upgrade both sides together.

### Fixed — reaping no longer acts on evidence it cannot trust

- **Automatic reap requires observed death, not just a stale timestamp.** `last_seen_ts` is
  the `created_at` of the highest-seq fact naming a tool — a value written verbatim from the
  ledger line, not from the reader's clock. Owner-staleness alone could therefore be asserted
  by any writer. The automatic path now requires **both** writer-stamped lease expiry **and**
  an external observed-dead verdict; owner-staleness stays behind the deliberate
  `doctor --reap-stale` operator command, and unknown observer evidence never authorises
  removal. `ReapMode::Full` remains the operator escape hatch.

- **The room enforces its emitted response ceiling.** `over_budget` could report `false` while
  the response was over budget, because the ceiling was a bucket allocator rather than a bound
  on what the caller receives. (D4/D12)

- **The coordination hook and the Rust room projection agree.** They had drifted, so the hook
  and `rally room` could describe the same room differently.

### Removed

- **Unused advisory authorization in `rally-protocol`.** A third implemented policy with no
  consumer. Dead policy reads as an enforced one to the next reader. (D13)

### Documentation — four claims that misled the reader, and two new register findings

- **A false in-code claim about the authority gates is corrected.** The lead-transfer doc
  comment said a hand-built fact "clears the same bar this command does". True of a `Fact`
  passed to `append_fact`; **false of a line appended directly to a segment file**, which
  never reaches the write boundary and therefore bypasses every gate — lead transfer, claim
  close, breadth, field bounds. The comment now says which writers the gate binds and which it
  does not, and points at `TRUST-MODEL.md` instead of reading as an authority claim.

- **The bundle credential audit is recorded in-repo.** `TRUST-MODEL.md` told every reader the
  18 git bundles "have not been audited for credentials … none has been ruled out". They were
  audited on 2026-08-05 — verified, fetched into throwaway repos, scanned blob-by-blob after
  decompression across 626 distinct paths, with the scanner mutation-validated against
  planted-then-deleted secrets across all 11 detector classes. **Zero credentials.** The
  result had lived only as a fact in a de-tracked ledger. Scoped explicitly: it clears
  secrets, not the machine paths and hostname that are present by inspection.

- **RC-053 and ARP-R-10 register entries corrected/added.** RC-053 read "not fixed" for five
  merged commits after the fix landed. ARP-R-10 had no entry anywhere and is now recorded
  unrecoverable with the search that proves the absence is real. Root cause of both, now a
  merge-checklist line at each end: nothing wrote an entry when a finding *arrived*, and
  nothing updated its state at *merge*.

- **Two new findings recorded, no code changed.** **RC-068** — `system_health` is 56% of the
  room payload (109 KB of 196 KB), never-cut and never deduplicated, so it starves every
  coordination bucket to one item; measured after a full reap drain, with fix options modelled
  against live data. **RC-069** — the pre-push vacuity gate tests SHA identity rather than
  gate-script change, so it refuses every ordinary push and its acknowledgement degrades to
  reflex.

- `cargo doc` warnings: 3 → 0.

## v0.2.0 - 2026-08-04

### Fixed — routing no longer changes behaviour

- **The room composes the same way with and without `rallyd`.** Four
  `RoomSnapshot` projections were `#[serde(skip)]` to keep them out of the public
  room JSON, and the daemon reply used the same serializer — so routing dropped
  them. Three behaviours changed depending on whether a daemon happened to be
  running: relevance ranking stopped demoting stale authors, `enter` and `next`
  wrote no read checkpoint at all (the coalescing guard skips a zero position),
  and repeated `next` polls appended a DUPLICATE wake intent every time. The
  fields now ride a `__internals` side-channel beside the snapshot; the public
  `rally room --json` schema is unchanged. Controls:
  `crates/rally-cli/tests/snapshot_wire_internals.rs` asserts all three through
  the real CLI and daemon, plus a structural check that a FIFTH skipped field
  cannot be added without carrying it. A full public `RoomSnapshot` key golden
  now pins the external schema. The wire version is **2**, so an installed v1
  daemon is rejected during the identity probe instead of silently supplying
  empty internals; client- and daemon-side rejection controls grade the cutover.
  The side-channel also fails loud above 1,024 pending wakes, 4,096 stale
  authors, or 512 KiB total, preserving exact behavior without letting private
  state consume the 8 MiB frame. Mutation-validated controls cover each bound.

- **`rally doctor --reap-stale --apply` now fails when its writes fail.** It
  returned exit 0, `ok: true` and `applied: true` against a fully unwritable
  ledger, because `applied` was a copy of the `--apply` flag and a failed append
  was counted into `preserved_future_or_active` — a field whose own doc says
  "future-dated lease, owner unparseable, or owner still active". The per-item
  lists were honest; the summary a script reads was not. Failed appends now have
  their own `write_failures` count, `applied` means the writes landed, and the
  command answers `ok: false` at exit 1 while still printing the full report.
  Closes the second half of RC-056.

### Fixed — the stale-state reaper finishes

- **`rally doctor --reap-stale --apply` completes on a real ledger.** It could
  not: 3 of 3 attempts against this repo's room returned
  `watchdog-timeout-uncommitted-mutation`, and 63 lease-expired claims had been
  unreclaimable since 2026-08-03 — the cleanup that would shrink the working set
  was blocked by the size of the working set. Measured on a synthetic ledger the
  same size (6,563 facts, 63 expired claims), a full drain took **40.6 s** against
  a 3 s watchdog. These performance figures are manual observations from the
  D10 author run, not a checked-in benchmark artifact.

  Two cost cuts took it to **29.5 s**: the segment fold is memoized on the
  fingerprint the reconcile sidecar already trusts, and
  `append_state_transition_verified` takes its before/after room projections
  only inside the `Release` and `Resolve` arms that read them — `ClaimExpired`
  computed two full projections per claim and used neither, so a 63-claim reap
  built and discarded 126 of them.

  That is not enough to fit 63 appends in 3 s, so the pass is **bounded** rather
  than cheap: `--apply` stops when its wall-clock budget is spent and reports
  `remaining`, and the operator runs it again. The same manual run completed in
  15 passes, every one under 2.5 s, with zero watchdog failures and zero active
  claims left.
  `RALLY_REAP_BUDGET_MS` raises the budget for a deliberate bulk drain; `0`
  restores the old unbounded pass. **The remaining cost is not hidden** — four
  full ledger reads per verified append are enumerated and still open as RC-058.
  Committed controls separate the evidence: `reaper_scale.rs` proves a
  repo-sized claim queue drains under the default watchdog, while deterministic
  zero-duration unit controls prove first-action progress across claim,
  handoff, and lead queues.

### Changed — one demotion contract instead of three

- **Relevance demotes an author that has gone quiet, and now says so
  everywhere.** The producer demoted on heartbeat age; the field doc, the
  `relevance` module doc, and the `Option<Liveness>` signal type all claimed only
  a provably-`Stale` author could be demoted. Heartbeat age is the contract that
  shipped and the one that was kept — `Liveness::Stale` needs a code-progress
  signal no writer produced until this release, so keying the demotion on it
  would have been dead on every existing ledger. Ranking and dropping use
  different bars on purpose: hiding a live peer causes a write collision, ranking
  one lower hides nothing. The signal is now
  `author_past_heartbeat_window: bool`, so the sources cannot drift apart by
  prose again. The config key `coordination.relevance.stale_author_factor` is
  unchanged. Registered as RC-066.

### Fixed — a mutation could not finish inside its own timeout

- **Contended writes now fail in 1.415s with a usable error instead of dying at
  3.0s blaming a contender.** Four timeout budgets governed one mutation and
  nothing coupled them: SQLite's `busy_timeout` (5000ms, blocking inside a
  single call), the `open_fact_store` retry loop (2720ms), the append retry loop
  (2040ms), and the watchdog that had to contain all three (3000ms). The busy
  timeout is the one that fired — SQLite swallowed the lock error for 5s while
  the watchdog killed the process at 3s, so **the two retry loops never ran a
  single iteration.** Measured on an empty scratch repo with no peers, no
  daemon, hooks off, debug build, against a real `BEGIN EXCLUSIVE` holder:
  before, exit 4 at 3.040s with `watchdog-timeout-uncommitted-mutation`; after,
  exit 1 at 1.415s naming the exhausted budget and the path being held.
  Uncontended writes are unchanged — 0.028s both sides, medians of 10
  interleaved runs on a quiesced host.

  On the watchdog-armed path all three budgets now derive from the single
  deadline the watchdog enforces (`crates/rally-cli/src/retry_budget.rs`). One
  function returns the blocking budget and the retry budget together, because
  they are not independent: a loop stops *starting* attempts at its deadline, so
  an attempt begun just inside it still blocks a full `busy_timeout` past it.
  Pool acquisition is a separate wait capped at one quarter of that SQLite
  budget, and the invariant counts both waits for both retry loops. A saturated
  pool now consumes the same deadline-derived retry budget as `SQLITE_BUSY`
  instead of failing the command on its first checkout timeout.
  An invariant test walks every watchdog setting from the 100ms floor to
  `inject`'s 605s ceiling and asserts the composed worst case — both loops plus
  both blocking overshoots — with an eighth of the budget left over.

  `rally daemon serve` deliberately runs with no watchdog, so nothing is derived
  there and it keeps upstream's 5s blocking budget unchanged. Deriving a short
  deadline from an absent watchdog would be the same defect pointed the other
  way. The real daemon serve entry point now rejects an accidentally armed
  watchdog, and sqlx pool acquisition uses one quarter of the SQLite blocking
  budget.

  Validated by mutation in three directions: restoring the 5s busy timeout,
  restoring the independent retry fraction, and stubbing the retry loop to never
  retry each fail a control. The wall-clock assertions are opt-in behind
  `RALLY_TIMING_TESTS=1` — they fail on a saturated host for reasons unrelated
  to the fix — while the shape assertions run unconditionally.

- **The watchdog timeout message says what was observed.** It previously read
  "retry after contention clears", which asserted a cause nothing had measured
  and advised an action that re-ran the same arithmetic to the same end. It now
  reports a wall-clock budget expiry, points at `rally doctor --reap-stale` to
  find actual holders, and says that retrying unchanged will hit the same
  budget.

- **Correction to the record:** a stale or zero-length `facts.db-wal`/`-shm`
  pair does **not** cause SQLite to report busy/locked, and was wrongly named as
  the trigger. Measured against both the 0.1.7 and 0.2.0 binaries, every variant
  — zero-length, garbage non-empty, mismatched salt, garbage `-shm` — completes
  in 0.049–0.073s. A stale WAL is the fingerprint of an abandoned holder, not a
  cause of contention; the negative result is pinned as a test. Separately, the
  reaper timeout previously attributed to the quadratic projection cost (design
  audit D10) was misattributed: `rally doctor --reap-stale` failed 3/3 with a
  watchdog timeout under contention and completed in 1.43s once uncontended.
  The projection cost is real and still worth fixing; it is not why the reaper
  timed out. See RC-067 in `docs/ROOT-CAUSE-REGISTER.md`.

### Fixed — verdicts that nothing acted on

Rally kept computing correct cleanup decisions and never calling them. Each of
these is a call site, not new policy.

- **The stale-state reaper now runs.** `rally doctor --reap-stale --apply` was
  its only caller and nothing invoked it, so a dry run against this repo's own
  room found **82 of 98 active claims already eligible** (measured 2026-08-04;
  an earlier dry run on 2026-08-03, before the fixture deletion below, reported
  69 of 69 — different ledger, different day, both real). `rally enter` can now
  reap, but it is **OFF by default** and opt-in via
  `coordination.auto_reap_interval_secs` or `RALLY_AUTO_REAP_INTERVAL_SECS`.
  It shipped on for one commit and an independent audit measured three failures
  against the release binary: 8 concurrent `rally enter` returned 8/8 exit 4
  with auto-reap on versus 8/8 exit 0 with it off; it closed a LIVE agent's
  claim (nothing in production renews `lease_expires_at`, so every single-file
  claim expires 30 minutes after it is made); and it widened RC-044, an
  already-unfixed concurrent-enter store-corruption path. The call site exists —
  that was the actual finding — and stays opt-in until lease renewal exists and
  concurrent enter is bounded.
- **Handoffs can expire.** A handoff closed only on a `Resolve`/`Receipt`/
  `Artifact` that referenced it, so an unanswered one was immortal; `next`
  de-prioritised after 24 h, which changed ranking and nothing else. Measured:
  **42 open handoffs aged 31–56 days**, every one still projected and budgeted on
  every room read. Default expiry is 30 days
  (`coordination.handoff_expiry_secs`, `0` disables), fail-closed on an
  unparseable timestamp exactly like claims.
- **A test fixture stopped being replayed into the production room.**
  `.rally/log/test.jsonl` was git-tracked and loaded on every read: 1,140 facts,
  70 claims, 46 real tool ids, 15.8% of the live ledger. Deleting it cut warm
  `rally room` by 17.5 ms (14.2%) and cold replay by 58 ms (33.5%) — and **grew
  the payload by 4,295 bytes of real content**, because the fixture had been
  displacing genuine coordination facts out of a fixed budget.
- **`rally doctor --binary-skew`** compares the running binary's build stamp
  against the checkout's HEAD. Measured on this machine: the installed
  `~/.local/bin/rally` predated the `--version` fix and returned **1,746,109
  bytes** from `rally room --json` where a current build returned **232,616** —
  7.5x the context, injected into every agent, with nothing anywhere saying the
  binary was stale. (A doc comment in `doctor.rs` rounds the same pair to
  1.99 MB / 230 KB from an earlier measurement on a larger ledger; the byte
  figures here are the ones taken on 2026-08-04.) The check states what it
  compared and what that does not prove, and it never blocks — but it has **no
  automatic caller**: it runs only when someone types `--binary-skew`, so an
  agent that does not already suspect skew still will not be told.

### Fixed — `rally … --json | head` no longer panics

Rust ignores `SIGPIPE`, so a reader that stops early arrived as an `EPIPE` write
error and `println!` panics on that: `failed printing to stdout: Broken pipe`,
exit 101. Every `| head`, `| jq`, `| grep -q`, `| less` that quit early printed a
panic. The room payload is hundreds of kilobytes of JSON, so piping it into
something that stops reading is how people read it, not an edge case. Rally now
exits 0 quietly, matching every other Unix CLI — handled in `std`, with no new
dependency and no `unsafe`. The first fix covered only the main output path, so
`rally watch --json` — the command most likely to be piped into `head` — still
panicked; the streaming emitters and the watchdog fail-open payloads now route
through the same writer.

### Fixed — errors that named the wrong thing

- **`rally run` says "tmux" when tmux is missing.** It launched with no
  availability probe, so the failure never named the dependency. The message now
  names it, gives the install command for macOS and Linux, and offers only
  backends it probed as usable.
- **`rally inject` stopped recommending the command that just failed.** Its
  remediation led with `rally run <agent>` — which fails for the same missing
  backend. It now leads with `rally adopt`.

### Security — ARP-004 sanitizer gaps (RC-040, partial)

- **Peer text can no longer read as hook narration.** `ident()` allowlisted
  `- . : /`, so hyphen-joined words rendered as fluent English OUTSIDE the
  guillemet contract while the preamble told the reader only guillemet spans were
  quoted data. Identifiers are now classified by prose density — over three
  vowel-bearing words gets quoted — chosen over a length cap because the longest
  benign scope in this ledger is 177 characters and the RC-040 payloads score 12
  and 13 words against a real-identifier maximum of 7. Scope rendering is capped
  at 200 characters per claim and says how many it dropped.
- **`validate_agent_id` rejects directive-shaped ids.** The bound is 64 bytes and
  8 prose words. Every real id passes, longest 52 bytes. Two counts of "every"
  appear in this repo because they measure different sets: 157 distinct ids
  across live segments plus archives, 125 across live segments alone. Both were
  checked; neither has an id over 64 bytes.
- **The sanitization suite now grades coverage, not just presence.** It asserted
  "exactly two sanitizer blocks" and never "every model-context sink routes
  through one". It now enumerates every context sink and fails on an
  unsanitized, un-allowlisted one, and its hostile fixtures no longer forge lines
  only with `\n` — which is why the `ident()` gap survived a green suite.

- **The sanitized path stopped advertising an unsanitized one.** The hook's
  preamble told the reader to "read the full item with `rally room --json`" —
  which returns the same peer text unquoted and uncapped. It now says so plainly,
  and `skills/agent-rally-point/SKILL.md` documents that `--json` is the source
  rather than a safer view, and that a fact's `tool` field is self-asserted.

Not closed: a three-word directive still renders bare, and the `--json` sink
itself still returns peer prose verbatim — labelling it is done, sanitizing it is
a schema decision deliberately deferred out of a held release. See RC-040.

### Security — one fact could take the whole room down (RC-037, RC-038, RC-034)

> **Read this first. RC-037 and RC-038 are NOT closed.** Both fixes hold against
> an agent that names itself honestly. Both are bypassed by one flag: the gates
> compare `fact.tool` against the room lead, and `fact.tool` is self-asserted, so
> `--tool <lead-id>` satisfies them. Live-reproduced against the release binary
> after the fix — `rally say claim --tool <lead-id> --scope 'workspace:*'`
> restores the room-wide claim lockout, and `rally say blocker --tool <lead-id>`
> restores the room-wide deny. `rally lead assign --to <self>` additionally
> succeeded against a live incumbent, so the seat was not defended either.
>
> [Updated 2026-08-04: the seat IS now gated (ARP-R-01) — a transfer requires a
> leaderless room, an actor that is the incumbent, a stale incumbent, or an
> explicit `--force` that records the seizure. That closes the honest-name path
> and nothing beyond it: `--tool <incumbent>` still works, so the seat now shares
> the same single residual as the other two gates rather than sitting open
> beneath them. The paragraph above is left as written because it was true when
> written and the correction is worth more than a clean page.]
>
> The adversarial tests below are real and revert-proof, and they graded only the
> first move: every one of them posts the rogue's fact under the rogue's OWN id.
> That is the failure this repo's register names — a control tested against the
> bypass an attacker has no reason to choose. The accidental and honest cases are
> genuinely fixed; the adversarial case is not. See RC-037 and RC-038.

Three v0.2.0 release blockers. The first two were live-reproduced against the
release binary before the fix and again after it; each carries a test that fails
when the fix is reverted — subject to the bypass stated above.

- **A coarse claim no longer locks every agent out of claiming (RC-037,
  Critical).** One `rally say claim --scope workspace:zzz` made every later claim
  of every path by every other agent fail, permanently: a `workspace:` scope
  overlapped every scope regardless of identifier, `repo:` overlapped every path,
  and `append_fact` hard-errors on a claim conflict. Containment is now decided by
  identifier — the explicit wildcard `*`, or a path the finer scope sits beneath —
  so an opaque root contains nothing but itself. Room-wide breadth stays
  expressible as `workspace:*` / `repo:*` and is gated on the lead seat.
- **The rejection message named a scope its owner did not hold (RC-037).** It
  rendered the requested scope in both slots, so a rogue holding `workspace:zzz`
  produced `codex:99 already owns file:src/lib.rs`. It now reports what the owner
  holds and what you asked for, separately.
- **The hook stopped swallowing auto-claim failures (RC-037).** The PreToolUse
  auto-claim ended in `|| true`, so when claim registration broke room-wide every
  edit still proceeded, nothing was claimed, and nothing said so — deconfliction
  degraded to zero while the hook reported healthy. Failures now print the CLI's
  own message to stderr, once per session per failure class. Still advisory: a
  failed claim never blocks an edit.
- **An unscoped blocker no longer denies every write (RC-038, Critical).** One
  `rally say blocker --subject "everything is blocked"` flipped `check
  before-write` from `allow: true` to `allow: false` for every agent and every
  path, and `RALLY_HOOK_STRICT=1` turns that into a hard deny on every edit. A
  room-wide freeze is a real thing a lead needs, so it is gated rather than
  removed: the lead's unscoped blocker still stops the room (`room-freeze`);
  anyone else's is surfaced as a warning the agent reads and decides about
  (`unscoped-blocker`). Unscoped binding decisions are labelled `unscoped-decision`
  instead of being reported as applying to a path they never named.
- **The pre-push gate refuses, rather than warns, on the paths where the pin
  reviewed nothing (RC-034, ARP-R-05).** The gate pinned three dispatcher scripts
  from a trusted ref, then globbed and ran `tests/hooks/test_*.sh` from the pushed
  tree — so adding a new test file bypassed the pin while the hook printed a
  healthy pin message. Host tests absent from, or modified relative to, the pinned
  commit are now refused by name unless explicitly acknowledged. An env-supplied
  `RALLY_PREPUSH_GATE_PIN_REF` that RESOLVES now requires an operator ack, not only
  when it resolves to a commit in the push. Read this as narrowing the unreviewed
  surface, not closing it: the gate still compiles and runs pushed-tree code by
  design, and each refusal has an env-var override that runs it anyway.
  ARP-R-05 closed three ways the gate reported success on a path that reviewed
  nothing. (a) A DEFAULT pin resolving to a commit in the push — pushing `main`
  while the pin is `main`, which is this repo's ordinary path — used to warn and
  continue; it now refuses behind the same `RALLY_PREPUSH_ACK_VACUOUS_PIN=1` that
  already gated the env case. A check that passed on every normal push was
  certifying nothing. (b) The affirmative `gate scripts pinned to <ref> @ <sha>`
  line printed before any diff had run. It is replaced by a neutral resolve marker
  at that point and a post-comparison summary that names each path, which copy ran
  (pinned or pushed), and why; the closing line reports the SHA count and the two
  dispatchers that exited 0 instead of `all gates green`. (c)
  `hooks/ensure-rally-binary.sh` — `curl`, `chmod +x`, `cargo install` — is
  executed by two of the PINNED host test suites but could not be pinned itself,
  because the pin hardcoded a `scripts/` prefix. The pin is now keyed on
  repo-relative paths and that file is in the set as a compare-only entry.
  An env pin that does NOT resolve still takes the bootstrap fallback with a
  warning and no ack, which also disables the host-test check — recorded as
  RC-046, not fixed. The hook header states plainly what the pin does not cover.

Room-wide effects require the lead seat, and the lead seat is not authenticated:
`fact.tool` is self-asserted. The seat's own transfer is gated as of 2026-08-04
(ARP-R-01), so it is no longer takeable under a rogue's honest name — but
`--tool <incumbent>` still satisfies every one of these checks. They stop the
accidental and honest cases; they do not stop an adversary and they are not an
authorization boundary. See [`docs/security/TRUST-MODEL.md`](docs/security/TRUST-MODEL.md).

### Fixed — discoverability

- **`rally --help` names every command it accepts.** It listed 31 of 42; `doctor`,
  `risks`, `decisions`, `artifacts`, `claims`, `lead`, `ack`, `worktree`,
  `daemon`, `self-exit-check`, and `claims-refresh` were real and invisible, and
  the unknown-command handler routed users to that same incomplete list. A test now
  fails when a command is registered without a help line.
- **`rally check-ci` help advertises the real flag name** (`--receipt-threshold-secs`,
  not `--receipt-threshold`).
- **Documentation matches the CLI.** `--include-legacy` was documented and did not
  exist; `rally worktree-gc` is `rally worktree gc`; `rally run` needs its
  positional agent. README states the MSRV and the pinned toolchain as the two
  different things they are, and lists the runtime prerequisites (`git`, `tmux`,
  `node`, `python3`, `gh`).

### Security — issue #52 independent audit (Lattice)

Seven findings from the first genuinely independent security review of this repo:
3 Critical, 1 High, 2 Medium, 1 Low. Six are fixed with adversarial tests; one
(ARP-003) has a fail-safe and a registered redesign. Per-finding triage:
[`docs/security/AUDIT-2026-08-02-issue-52-triage.md`](docs/security/AUDIT-2026-08-02-issue-52-triage.md).
Register entries RC-013..RC-019.

**This supersedes the 0.1.2 "Binary auto-provision" behaviour below.** Lifecycle
hooks no longer provision anything.

- **Hooks no longer install software (ARP-001, Critical).** The SessionStart hook
  called `ensure-rally-binary.sh` on both the `.rally`-present and `.rally`-absent
  paths, so opening and trusting this repo could download a release binary,
  `chmod +x` it, run it, and write `~/.local/bin/rally` before you ran any project
  code — and could fall back to `cargo install` from repo source or execute an
  unverifiable shipped plugin binary. Both call sites are gone. The hook detects
  and advises; it never executes a candidate binary, even to probe it.
  Provisioning moved to `scripts/install-rally.sh`, run by a human, fail-closed on
  **both** SHA256 and client-side `gh attestation verify`. `ensure-rally-binary.sh`
  refuses (exit 3) unless `RALLY_EXPLICIT_INSTALL=1`, so re-wiring it into a hook
  later fails closed. The unverifiable shipped-prebuilt path is deleted, not gated.
- **Ledger prose is quoted before it reaches model context (ARP-004, High).** Peer
  subjects, evidence, intent, and paths flowed into `additionalContext` /
  `systemMessage` unsanitized, so anyone who could write `.rally/` could put
  instructions in a high-trust channel. All of it now passes one sanitizer:
  identifiers on a strict allowlist that excludes whitespace, prose flattened,
  length-capped, and wrapped in guillemets behind a fixed preamble telling the
  model the following is peer-authored and unauthenticated. Facts are still
  unsigned — see the trust model.
- **The workstream linter stopped claiming to be a safety proof (ARP-002,
  Critical).** `owns` permitted `;` `|` `&` `>` `(` `)`; `validation` needed only
  to be non-empty and was rendered verbatim into a `bash` block labelled "run
  these verbatim"; `runId`/`toolPrefix` were interpolated after a non-empty check.
  Now: positive allowlists, one shell-quoting helper on every rendered value,
  identifier validation at both CLI and library entry points, and descriptor
  `validation` rendered as non-executable prose. A new local recipe registry
  supplies real commands by name, so command text comes from this repo rather
  than the descriptor. Every "proves a plan is safe to fan out" claim is gone.
- **Cockpit binds sessions and approvals to an owner (ARP-005, Medium).**
  Constant-time token compare, a principal per authenticated connection, owner
  checks on send/steer/close/approve, `repo_path` canonicalized into a
  `COCKPIT_REPO_ALLOWLIST` (default `$HOME`), and non-loopback bind refused
  without an explicit risk-naming override. `hello` gained an optional
  `client_id` so a reconnecting client keeps control of its session.
- **Cockpit no longer claims to enforce tool authorization (ARP-003, Critical —
  fail-safe).** The approval gate pauses the event pump, not the child process:
  the tool has already run. Codex is spawned with stdin null so no denial can be
  delivered, and Claude's pre-execution hook needs an MCP server this workspace
  does not have — both are redesigns. Every enforcement claim is corrected,
  `tool_blocked` carries `advisory: true` / `enforced: false`, and the redesign's
  acceptance test is written down as an `#[ignore]`d test. **Not closed.**
- **The pre-push gate no longer runs the pushed commit's own gate scripts
  (ARP-006, Medium).** Gate code is pinned via `git show <ref>:scripts/<name>`;
  a differing pushed copy is refused with a diff unless explicitly acknowledged.
  Hook docs corrected — it is opt-in via `core.hooksPath`, and enabling it is a
  trust decision.
- **Watcher hardening (ARP-007, Low).** Malformed JSONL is quarantined and the
  cursor stops before it instead of silently advancing past a lost record;
  `ack-quarantine` resumes. macOS notifications pass title/body as `argv` to an
  `on run argv` script rather than interpolating them into script source.
  `watchfiles` pinned with a committed `uv.lock`. File sinks bounded by
  `AGENT_RALLY_WATCHER_SINK_ROOT`, rejecting symlinked leaves and opening
  `O_NOFOLLOW`.
- **`cockpit-cli` stopped printing success for refused commands.** It opens a new
  socket per subcommand, so owner binding made every invocation a different
  principal — and it printed `sent` without reading the reply. Same
  acknowledge-the-wrong-step shape as RC-001. Now sends a stable `client_id` and
  fails loudly on an error frame.
- **The release-parity gate runs every hook suite.** It ran a hardcoded list of
  three while seven existed, so the two adversarial suites closing RC-013 and
  RC-016 would have run in no gate at all. It now globs `tests/hooks/test_*.sh`
  and fails on an empty glob rather than passing zero tests vacuously.
- **New docs.** [`docs/DESIGN-TRADEOFFS.md`](docs/DESIGN-TRADEOFFS.md) (why hooks,
  why agents self-manage, why push-then-pull, and what is actually proven),
  [`docs/security/TRUST-MODEL.md`](docs/security/TRUST-MODEL.md) (what is and is
  not defended), and
  [`docs/rca-2026-08-02-security-findings-escaped.md`](docs/rca-2026-08-02-security-findings-escaped.md)
  (why every gate stayed dormant). `skills/agent-rally-point/SKILL.md` no longer
  claims Rally does not install host hooks — four committed registration files do,
  and the docs now say so plainly along with every off switch.

## v0.1.7 - 2026-07-30

Canonical Claude Code and Codex host integration, plus the daemon and handoff
work landed on `main` since v0.1.6.

- **One canonical contract now generates every host surface.** `config/host-integrations.json` plus the Cargo package version drive Claude, Codex, Cursor, marketplace, skill-frontmatter, packaged-artifact, and release-identity files. Release parity rejects any generated drift.
- **Installed hosts can be diagnosed and reconciled deterministically.** `scripts/sync_host_integrations.py` is read-only by default, compares a versioned content digest, detects stale caches and duplicate providers, and requires `--apply` before it removes noncanonical providers or updates the canonical marketplace.
- **Release identity replaces the dead manifest-version fallback.** First-session CLI provisioning reads `rally-release.json`; GitHub latest is now only the final fallback when packaged identity is absent.
- **Duplicate Claude hook registration no longer duplicates Rally side effects.** Locked per-source event counts collapse installed-plugin/project/global duplicates regardless of arrival order; repeated same-source events still run, including strict-mode denies.
- **Host differences are explicit.** Claude keeps edit-scoped `PreToolUse`; Codex intentionally runs unscoped `PreToolUse`. Global Claude hook installation now derives from the generated project template, including matchers and timeouts.
- **Rally now has a daemon-backed store path and hardened handover.** The `rallyd` thin-client route, warm-pool installation, bounded handover, burst robustness, and security fixes landed with direct-mode fail-open behavior preserved.
- **Handoffs acknowledge receipt before work begins.** Receiver-side flow now ACKs first, watches the room for follow-up, and surfaces the sender; injectability and ACK-wait diagnostics no longer hide delivery state.
- **Concurrent managed-session launches cannot acknowledge a collapsed reservation.** `rally run` and `rally adopt` serialize numbered-identity allocation across processes and positively read back the exact active-session event before returning a normal success envelope. The Linux CI regression now also validates every child response and rejects duplicate returned IDs before checking durable projection cardinality.
- **Direct-mode SQLite teardown cannot destroy committed WAL facts.** Rally pins a vendored `factstr-sqlite` 0.5.2 delta that closes sqlx pools synchronously, closes room-owned pools under the mutation lock, fingerprints both `facts.db` and its WAL, and remeasures post-append counts instead of incrementing a potentially stale sidecar. This closes the production data-loss mechanism behind duplicate managed-session identities under parallel launch.
- **Host provisioning lock and mtime checks are portable across macOS and Linux.** Timestamp fallback now discards failed BSD `stat` output before using GNU `stat`, and flock-capable workers also honor the portable PID lock so mixed-capability sessions cannot provision concurrently.
- **Doctor and maintenance surfaces expanded.** `doctor --compact-log`, `doctor --sweep-corrupt`, one-shot `claims-refresh`, and append-run provenance validation are included.

## v0.1.6 - 2026-07-07

Trustworthy room signal + read-surface ergonomics. Fable+Codex audited before release.

- **System-health facts no longer drown out real risks.** System-generated telemetry (`external-intake`, `unmanaged-agent`, `duplicate-active-squad-id`, `binary-drift`) now projects into a dedicated `system_health` bucket, deduped by subject, instead of `current_risks`. `rally room` shows only human coordination risks by default; telemetry stays auditable and resolvable in its own lane, and the count surfaces as `system_health=N`.
- **New read verbs:** `rally risks`, `rally decisions`, `rally artifacts`, `rally claims` — thin, discoverable projections of the room snapshot (`--json` gives `data.<verb>.rows`), so agents no longer hand-parse `rally room --json`.
- **Idempotency guards** for `duplicate-active-squad-id` and `binary-drift` (matching the existing `unmanaged-agent` guard): re-entering a room no longer appends duplicate telemetry facts.
- Resolve + enter-path guards updated to read `system_health`, so telemetry stays resolvable and non-duplicating.

## v0.1.5 - 2026-07-03

Hardening + Apple-Silicon-optimized release. **`rally backlog add/update` now
validate `--status`**: an unknown value (e.g. `wip`) fails loud with the valid
set instead of being stored silently and then dropping off the `rally next`
plan/status obligation radar. **Release binaries are ~32% smaller** — a tuned
`[profile.release]` (fat LTO + `codegen-units = 1` + `strip`) replaces the cargo
defaults; the arm64 macOS binary drops 7.1 MB → 4.8 MB, and startup benefits on
the hot hook path (`before-write`/`before-complete` run every commit). **The
Intel `x86_64-apple-darwin` binary now cross-compiles on the arm64 `macos-14`
runner** instead of the retiring native `macos-13` runner, which was starving in
the GitHub queue (a v0.1.4 build waited ~5h and never scheduled) — the Intel
target no longer depends on Intel-runner availability. CI hygiene: `release.yml`
pins its checkout to the dispatched tag so a re-published release matches its tag
commit, and all workflow actions move off the deprecated Node 20
(`checkout@v7`, `setup-node@v6`, `attest-build-provenance@v4`,
`action-gh-release@v3`).

## v0.1.4 - 2026-07-02

Ledger-integrity release. **Ends the duplicate-seq ledger-corruption class**: the
seq allocator now allocates from the canonical segment high-water mark (max+1)
under the room flock, instead of the derived `facts.db` row count — a count that
undercounts whenever the ledger has a seq gap and thus deterministically collided
with the live tail after any rebuild. Adds defense-in-depth: a last-line dup gate
(loud `seq allocation conflict` error instead of a silent duplicate that bricks
replay) and a fingerprinted fast path (the O(1) sidecar shortcut is taken only when
its segment fingerprint still matches on disk, so a stale cache can never hand out a
stale max regardless of caller order). Also: the **plan/status commitment bus** —
`rally backlog add/update` gain `--target`/`--status`/`--expected-by`, and `rally
next` surfaces targeted plan items as an actionable `update_plan_status` obligation
so peers forecast ETAs and plan/ETA requests cannot sit unconsumed; a stale-wait fix
(handoffs >24h or to takeover-eligible owners no longer force a wait); Codex⇄Claude
SessionStart cadence parity and advisory-prompt noise-trim; and gitignore hygiene for
runtime backups and local bundles.

## v0.1.3 - 2026-06-25

Coordination engine: recency decay + size-scaled auto-reclaim, in-room stale-state
reaper, codex⇄claude heartbeat parity, adaptive multi-signal liveness + squad-projection
decay, 3-layer tmux zombie prevention, and the orphan agent OS-process reaper. This is
the first release carrying the full liveness/reaper coordination layer; build-loop bundles
this binary (replacing its Python coordination mirror).

### Added — Orphan agent OS-process reaper (`rally sessions --reap-processes [--apply]`)

Closes the gap left by the tmux and worktree reapers: nothing previously killed orphan
OS processes (codex mcp-server, node .../bin/codex mcp-server, SkyComputerUseClient
post-turn zombies). This session manually killed 27 such processes aged 10-18 days.

- **New flag `--reap-processes`** on `rally sessions`: scans `ps -axo pid=,etime=,command=`
  for three candidate patterns and stages matches for removal. Dry-run by default —
  candidates are listed but nothing is killed.
- **New flag `--apply`** (requires `--reap-processes`): executes TERM then KILL on
  each staged process. Returns a count of killed PIDs in the text output.
- **Reuses the existing liveness model** (`liveness::{is_live, reapable, adaptive_window_secs}`)
  exactly as the orphan-tmux path does: single observable signal (process age from `etime`)
  mapped onto `code_progress_age`; `Unknown` promoted to `Stale` because a real process
  always has an observed age; `reapable(verdict, parent_alive)` is the single kill gate.
- **macOS BSD `etime` parsing** (`parse_etime_secs`): pure helper handles `mm:ss`,
  `hh:mm:ss`, and `dd-hh:mm:ss` (BSD `ps` has no `etimes`/`etime_secs` keyword).
  Malformed fields return `None` → the line is SKIPPED (fail-safe).
- **Fail-safe floor**: non-zombie candidates younger than 600 s (10 min) are never staged.
- **Post-turn zombie bypass**: `SkyComputerUseClient` processes with `turn-ended` in their
  command are staged at ANY age (the process is definitionally dead at turn end), bypassing
  the floor and liveness check.
- **Parent-liveness check**: for non-zombie candidates, the reaper resolves the process's
  parent PID via `ps -o ppid= -p <pid>` and calls `pid_is_alive(ppid)`. ppid == 1
  (reparented to launchd) or a dead ppid is `Some(false)` → staged as `"stale+parent-dead"`.
  Unresolvable ppid → `None` → falls back to window criterion alone (prior behavior).
- **No new crate dependencies**: uses `std::process::Command` for `ps` and `kill` exactly
  as the existing tmux path does. Zero additions to Cargo.toml.
- **Pure classifier** `classify_orphan_processes` is injected-`now` and injected-`parent_fn`
  for deterministic unit tests — 16 new tests covering parse_etime, staging, preservation,
  zombie bypass, idempotency, and codex-mcp-server parent-alive/dead/fresh scenarios.

### Added — Zombie-tmux prevention: three layers over one liveness model

Stops accreted zombie `rally-*` tmux sessions at the source instead of relying on a
clock. Root cause: rally `exec`s the agent, so a session auto-closes when its agent
EXITS — but agents that never exit (a disabled autonomy poller, idle detached panes)
leave the session forever, and tmux has no native idle/lifetime timeout. All three
layers REUSE the single `liveness::is_live` 4-signal model + adaptive window; none
adds a fixed idle clock.

- **Layer 1 — completion-scoped self-exit.** New `rally self-exit-check --tool
  <self> [--persistent] [--required-streak N]`: a task-scoped session that holds no
  active claims AND for which `rally next` is non-actionable for a SUSTAINED streak
  (default 2, persisted in the session's own `RALLY_SELFEXIT_STREAK` tmux env so it
  dies with the session) self-kills its own tmux session → `exec` auto-closes it.
  `--persistent` opts a deliberately-long-lived session out of the implicit "work
  done" path; `rally stop` remains the explicit path. Decision:
  `liveness::completion_self_exit_eligible`.
- **Layer 2 — event-driven liveness-lease safety net.** `rally enter` now
  opportunistically sweeps detached `rally-*` orphan tmux sessions (in addition to
  `rally sessions --reap`), via one shared actuator (`sweep_orphan_tmux`).
  Best-effort + fail-open: runs after presence, never blocks enter, never raises,
  never reaps a live / parent-alive session. No daemon/cron.
- **Layer 3 — parent-lifecycle binding.** `tmux_start_command` stamps
  `RALLY_PARENT_PID=<launcher pid>` into the new session's env in the same atomic
  `tmux new-session -e` call. The reaper reads it back, probes `kill -0 <pid>` (no
  new crate dependency), and feeds the result to the single shared
  `liveness::reapable(liveness, parent_alive)` authority.
- **One reaper-eligibility authority** `liveness::reapable` (mirrored Rust↔Python;
  `liveness_vectors.json` gains `reapable_cases` + `self_exit_cases`). Fail-safe:
  Live/Unknown are NEVER reaped; parent-dead reaps only a session ALSO `Stale` by
  liveness; missing parent info degrades to the window criterion alone (prior
  orphan behavior, unchanged); `kill -0` non-ESRCH failures read ALIVE.
- Tests: `liveness.rs` parity + dedicated `reapable`/`self_exit` cases;
  `backends.rs` Layer-3 classifier cases (dead-parent reaped, live-parent kept,
  code-progressing-with-dead-parent kept, missing-info window fallback, `kill -0`
  self/dead probe).

### Added — Adaptive, multi-signal session liveness (squad-projection decay + tmux orphan reaper)

Replaces fixed staleness cutoffs for the squad/presence projection with liveness
that ADAPTS to each session's planned heartbeat cadence and weighs four signals.

- **One liveness function** (`src/liveness.rs`, mirrored in build-loop's
  `scripts/rally_point/liveness.py`). Staleness is RELATIVE to the declared
  cadence: `window = planned_interval * MISS_MULTIPLIER + GRACE`. Defaults
  `DEFAULT_CADENCE_SECS=300`, `MISS_MULTIPLIER=6`, `GRACE_SECS=60` → a 5-min
  cadence is stale at ~31 min (≈6 missed beats); a 5-hour cadence not until
  ~30 h. Exposed as `.rally/config.json` `coordination{}` tunables
  (`default_cadence_secs`, `miss_multiplier`, `grace_secs`) + `RALLY_*` env.
- **Four signals — LIVE if ANY is fresh within the adaptive window:**
  (a) heartbeat/presence age, (b) inject/ack (receipt/wake/handoff naming the
  tool), (c) forward code progress (the tool's worktree branch HEAD moved
  between its two newest presence facts), (d) declared active work (a live claim
  or authored mission/handoff).
- **Squad-projection decay (the gap fix).** `snapshot_from_facts_with_policy`
  now DROPS a squad whose four signals are ALL provably stale from the default
  room view; `--include-archived` restores it (mirrors the message archive
  model). **FAIL-OPEN:** a Live OR Unknown (any absent/unparseable signal)
  verdict KEEPS the squad visible — hiding a still-alive peer is the dangerous
  direction (it could cause the very write-collision this system prevents). This
  is deliberately the OPPOSITE fail-direction from the reaper's fail-CLOSED
  removal path.
- **tmux orphan reaper.** `rally sessions --reap` now also detects DETACHED
  `rally-*` tmux sessions whose last activity is past the adaptive window and
  which are not tracked as managed sessions, kills them, and tombstones the
  reap. Closes the gap where `--reap` saw 0 of the real detached orphans.
- **`rally stop` self-kill.** On stop, if the stopping process is itself inside
  a `rally-*` tmux session distinct from the managed target, it kills that
  session too (contain at source — it can never become an orphan).
- Parity double-pinned by the byte-identical golden fixture
  `tests/fixtures/liveness_vectors.json` (identical copy in build-loop) +
  the `_provenance.json` drift manifest.

### Added — In-room stale-state REAPER + heartbeat parity + session-end self-release

Three new actuators that make coordination state self-cleaning:

- **`rally doctor --reap-stale` (REAPER).** New sub-command (with `--apply` to
  commit writes, dry-run by default) that physically removes over-TTL claims and
  stale squad-lead leases. Implemented in `crates/rally-cli/src/reaper.rs`.
  Composes existing eligibility functions (`claim_reclaim_eligible`,
  `takeover_eligible_owners`) without reimplementing staleness math.

  **Dual-signal eligibility (2026-06-22 fix):** a claim is now reaped when
  EITHER (1) its owner-squad is >timeout stale (`claim_reclaim_eligible`) OR
  (2) its own `lease_expires_at` evidence timestamp has provably passed
  (`claim_authority::expired_claims`). This closes the shared-tool-identity
  gap: a claim owned by an identity like `claude_code` (which appears "live"
  because the current session IS that identity) is still reaped when its
  individual lease has expired. Both signals are fail-closed: an unparseable
  owner timestamp keeps the claim, and an unparseable or missing
  `lease_expires_at` keeps the claim. The union preserves every
  future-dated-lease claim. Each reaped claim now carries a `reason` field
  in `ReapedClaim`: `"owner-stale"` | `"lease-expired"` |
  `"owner-stale+lease-expired"`.

  FAIL-CLOSED on any unparseable owner timestamp or lease (inherited
  guarantees from both composing functions). Race-safe: appends via
  `append_fact_verified` under the existing mutation lock. Idempotent: a
  re-run finds nothing eligible because `active_claims` projects only open
  claims. Output: `ReapReport { claims_reaped, squads_idle_cleared,
  lead_relinquished, preserved_future_or_active, applied }`.

- **Session-end self-release (LEVER 3).** `rally stop` now self-releases all
  active claims owned by the stopping tool before removing the session record.
  Self-release is authoritative (bypasses the 2h reclaim bar — the owner is
  declaring itself done), keeps SEC-001 dormant (no stale-owner evidence marker),
  and is best-effort (never blocks the stop path). Implemented inside the
  `SessionAction::Stop` arm in `lib.rs`.

- **Heartbeat parity fixture.**
  `crates/rally-cli/tests/fixtures/heartbeat_parity_vectors.json` — a new
  golden-vector file (byte-identical to the build-loop mirror at
  `scripts/rally_point/heartbeat_parity_vectors.json`) asserting that a
  `claude_code` session and a `codex` session that emit presence/heartbeat at the
  same age decay IDENTICALLY (heartbeat is tool-agnostic, curve is shared).
  Validated by a new `reaper::tests::heartbeat_parity_vectors_match_expected` test
  checking each vector against `decay::recency_weight` and the stale-at-15m
  verdict to 1e-4 precision.

### Test coverage (reaper.rs)

10 `#[cfg(test)]` cases inside `reaper::tests`:
(a) over-TTL claim is staged + leaves `active_claims` after apply;
(b) unparseable owner ts is never staged (fail-closed);
(c) fresh-owner claim is not staged;
(d) idempotent: second run finds nothing;
(e) stop self-release only releases the stopping tool's claims, not peers';
(f) heartbeat parity vectors match expected weight and stale verdict;
(g) **lease-expired claim with live owner IS reaped** (dual-signal fix, the 76-claim case);
(h) **future-lease claim with live owner is preserved** (the 9-claim keep case);
(+2) dry-run writes no facts; `squads_idle_cleared` enumerates stale owners.

Verified: `cargo build -p rally-cli && cargo test -p rally-cli` (349 pass, 0 fail).

---

### Added — Coordination recency decay + size-scaled lead/ownership auto-reclaim

A single shared coordination policy now governs two behaviors. All tunables live
under a `"coordination"` object in `.rally/config.json` (default → user → repo →
env precedence, mirroring `hooks`). The math is the single source of truth in the
new `crates/rally-cli/src/decay.rs`.

- **Time-based recency decay.** Every coordination message (fact) gets a
  continuously-computed weight from its age, `weight = 0.5 ^ (age_hours /
  half_life)` (exponential half-life, default 48h). `rally room` orders the
  decision / risk / artifact buckets fresh-first by weight; `rally recent` and
  `rally next` inherit recency ordering. A message whose weight falls below the
  archive floor (default `0.05`, ≈14d) is moved OUT of the active view into
  `stale_facts` (losslessly — the raw segments stay on disk). Re-include
  decayed messages with `rally room --include-archived` / `rally recent
  --include-archived`. Active state (claims, blockers, open handoffs) is never
  decayed — only historical message buckets. Tunables: `half_life_hours`,
  `archive_floor_weight` (env `RALLY_HALF_LIFE_HOURS`, `RALLY_ARCHIVE_FLOOR`).
- **Size-scaled lead/ownership auto-reclaim.** A stale owner's claim becomes
  reclaimable on a timeout that SCALES with the claimed work: a single-file
  claim after the small timeout (default 30m), a multi-file / directory / repo /
  task claim after the large timeout (default 2h — equal to the prior flat
  `TAKEOVER_STALE_SECS`, so coarse claims keep their existing grace window).
  Size is derived from the claim's existing `ResourceType` breadth + scope
  count (no new claim metadata). The size-scaled window also sets the claim's
  `lease_expires_at` evidence at claim time. The destructive reclaim path
  (`command_release_by_path`) records the reason + size class in the release
  fact's provenance (`reclaim-reason:stale-by-timeout;work-size=…`). Tunables:
  `reclaim_small_minutes`, `reclaim_large_minutes` (env
  `RALLY_RECLAIM_SMALL_MINUTES`, `RALLY_RECLAIM_LARGE_MINUTES`).
- **Preserved invariants.** Reclaim stays race-safe (the `.rally/mutation.lock`
  flock is untouched) and FAIL-CLOSED: an owner whose `last_seen_ts` is missing
  or unparseable is never reclaimable. Recency decay fails OPEN: a message with
  an unparseable `created_at` is treated as fresh and never hidden.
- **Behavior change to note.** A single-file claim that previously had the flat
  2h takeover grace is now reclaimable after 30m by default — an intentional
  tightening of narrow claims (multi-file / coarse claims are unchanged).

Verified: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
(workspace green; new unit + integration coverage for the reference decay ages,
the archive-floor boundary, and the small/large reclaim timing incl. fail-closed).

## 0.1.2 — Binary auto-provision hardening (2026-06-11)

Hardening of `hooks/ensure-rally-binary.sh` across five rounds of dual-vendor
adversarial review (Fable + Codex), all verified under stock macOS `/bin/bash`
3.2:

- **Verified downloads.** A downloaded binary is SHA256-verified against the
  release's `<asset>.sha256` before it is made executable; the download path is
  fail-closed (a mismatch OR an unverifiable download is rejected, never run —
  cargo-from-source is the fallback). Releases now publish per-asset `.sha256` +
  a sigstore build-provenance attestation (`gh attestation verify`).
- **Never blocks the session.** All network + compiler work runs in one
  backgrounded, fd-detached worker; local liveness probes are time-bounded
  (perl/setsid shim) so even a hung or crashing on-disk binary can't stall the
  hook. A signal-killed `version` probe is treated as failure on every
  acceptance path (PATH, `~/.local/bin`, shipped, cached, downloaded).
- **Concurrency.** A single atomic pid lock (noclobber + verify-after-write,
  parent writes the worker's real pid — `$BASHPID`-independent for bash 3.2);
  the `building` short-circuit is gated on worker-pid liveness so a crashed
  worker no longer wedges provisioning.
- **Charter robustness.** Unset `HOME`, a corrupt state file, and a missing
  script dir no longer abort under `set -euo pipefail` — exit 0 always. A
  checksum mismatch records a durable trace to `download-rejections.log`.

## 0.1.1 — Plugin auto-launch + hooks policy (2026-06-11)

- **Auto-launch on install.** `.claude-plugin/marketplace.json` makes the repo a self-hosting single-plugin marketplace, so `claude plugin marketplace add tyroneross/agent-rally-point` + `claude plugin install` work; hooks and skills activate on install.
- **Binary auto-provision.** `hooks/ensure-rally-binary.sh` provisions the `rally` CLI on first SessionStart (present-check → shipped prebuilt → GitHub-release download → backgrounded `cargo build` → advisory). `.github/workflows/release.yml` builds per-triple binaries on tag.
- **Offer-on-first-session.** In a git repo without `.rally/`, the SessionStart hook surfaces a one-time `rally init` offer instead of silently no-opping; it never auto-creates `.rally/` (no-litter charter preserved). Repos with `.rally/` keep full auto-coordination.
- **`rally hooks` policy command** (`status|on|off|prompt`) with session/repo/user/default resolution and `RALLY_HOOKS=off` opt-out.

- Hardened the fan-out path: `workstream-lint.mjs` now also rejects shell-unsafe `output` and `owns`/`id` chars; the empirical packet gate runs in CI (`rally-gate.yml` builds the release binary + runs the node suite); empty `--parent-step` values no longer write phantom DAG edges; and inject sanitization is hoisted to the `inject_commands` chokepoint so every backend (tmux + cmux) is covered.

### `packet.mjs` fan-out now generates CLI-executable rally commands (2026-06-09)

Fixes four findings where the emitted fan-out packet named rally markers that the
real CLI rejected — escaped review because the tests asserted marker *presence*,
not executability against the binary.

- **Repeated `--path` on `before-write` (HIGH).** `renderRallyLoop` emitted one
  `rally check before-write --tool <t> --path a --path b --strict` line, but
  `rally check` rejects repeated `--path`. A multi-owns task failed at its
  before-write step. Now emits one before-write line per owned path. The claim
  line is unchanged — `rally say --path` is repeatable.
- **Repeated `--parent-step` on `rally say` (HIGH).** A task with ≥2 `depends_on`
  emitted one `--parent-step` per dep, but `rally say` accepted at most one, so
  the task failed at its first command. Durable fix in the CLI:
  `SayArgs.parent_step_id` (`Option`) → `parent_step_ids` (`Vec`), one
  `parent-step:<id>` scope marker written per value, and `dag.rs` now extracts
  every marker (one DAG edge per parent). Zero/one value behaves exactly as
  before; existing ledger facts parse unchanged.
- **Test fixture didn't exercise the multi case (MEDIUM).** Added a
  2-path-owns/2-`depends_on` fixture and an empirical gate
  (`packet-empirical.test.mjs`) that dry-runs the emitted claim + before-write
  lines against the built release binary in a throwaway rally room and asserts
  `rally dag` shows two parent edges — so flag-arity drift fails tests.
- **Shell-unsafe descriptor fields (LOW).** `workstream-lint.mjs` now rejects
  `"`, `$`, or backtick in `intent` (break the emitted `--subject` quoting) and
  whitespace in any `owns` path (would split into multiple `--path` tokens).

### `rally inject` now actually submits + waits for an ACK (2026-06-09)

Fixes the long-recurring "inject delivered but never ACKed" signature (L5 /
`incident-rally-inject-not-acked`). Two independent root causes, both repaired:

- **Submit semantics (tmux fallback).** The tmux inject path built FOUR separate
  commands — `C-u`, `set-buffer`, `paste-buffer`, then a SEPARATE `send-keys
  Enter` — and that separate Enter never submitted against Codex's bracketed-
  paste TUI: the message landed in the input box and sat at the prompt. It now
  ships the whole frame as ONE atomic `send-keys -t <t> -H <hex…>` write —
  `ESC[200~ <text> ESC[201~` followed by a CR placed AFTER the close marker so
  it submits rather than pasting as literal text (ported from ptyd `frame_line`,
  `src/comms.rs` §4.1/§4.2, no path dependency). `C-u` stays a discrete clear.
  cmux keeps its separate-submit sequence (no raw-byte `send`); documented inline.
- **ACK wait never ran (watchdog pre-emption).** `inject --handoff
  --timeout-seconds 75` returned a bare `{ok:true,product:rally}` immediately —
  not because the `InjectData` envelope was missing (it was already built with
  `delivery_state`/`ack_state`/`fallback_plan` and polled the ledger via
  `wait_for_resolution`), but because the global 3s-default / 60s-max wall-clock
  watchdog killed the process before the 75s ACK poll could run, emitting the
  neutral fail-open payload. `inject` — the one deliberately-blocking interactive
  verb — now sizes its watchdog from `--timeout-seconds` + headroom (ceiling
  605s), bypassing the 60s hook cap. An explicit `--timeout-ms` /
  `RALLY_HOOK_TIMEOUT_MS` override still wins (clamped to the hook band); all
  other (hook-invoked) commands keep the 3s default unchanged.

Follow-up (spec only): [`docs/PLAN-daemon-first-inject-routing.md`](docs/PLAN-daemon-first-inject-routing.md)
describes the daemon-first routing this framed tmux write is the fallback for.

### Reliability & performance — store durability for scale (2026-06-04)

Foundation for durable coordination at thousands of agents. Commits `5c68dac`..`32d21be`.
See [`docs/SCALE-ROADMAP.md`](docs/SCALE-ROADMAP.md) for the measured roadmap.

- **facts.db corruption is now a non-event.** A malformed/missing `facts.db` is quarantined
  (`facts.db.corrupt.<ts>`) and the derived cache is rebuilt from the canonical JSONL ledger —
  zero history loss. Handles header (`SQLITE_NOTADB`), mid-page (`SQLITE_CORRUPT`), extended
  corruption codes, and torn trailing ledger lines (crash mid-append). Resolves the 2026-06-01
  easy-terminal `facts.db.corrupt` incident.
- **O(1) happy-path reconcile.** `reconcile_segments_and_db` no longer runs an O(N) segment scan +
  full SQLite load on every open/append; a disposable fingerprint sidecar
  (`.rally/.reconcile-cache.json`, deterministic FNV-1a) short-circuits when in sync, falling
  through to the authoritative scan+rebuild on any drift. Measured flat (150µs at n=200 and n=4000).
- **Active-segment-first R9 readback** and **thread-aware open jitter** (replaces constant `pid%17`),
  hardening concurrent opens and making the parallel test suite deterministic (flake 25% → 0%).

### Verification (2026-06-04)

- `cargo test --workspace` (0 failures); 12–30× parallel determinism runs green.
- `cargo clippy --package rally-cli --lib --no-deps -- -D warnings` clean.
- `independent-auditor` pass per integrity-critical change (caught a cross-process cache-key bug).
- End-to-end: real `rally room` recovers a corrupted store; `scripts/scale_reliability_test.sh`
  `SILENT_LOSS=0` at N≤128.

### Changed

- Cut the product architecture over to Rust. The user-facing command is `rally`.
- Removed the legacy Python runtime package, Python packaging metadata, and
  legacy discovery/migration documentation.
- Kept the durable product contract centered on `changes.jsonl`, portable
  events, signed trust, sync packets, and stable JSON command envelopes.

### Verification

- `cargo test`
- `cargo clippy --all-targets -- -D warnings`
- `git diff --check`
