<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> | SPDX-License-Identifier: Apache-2.0 -->

# Rally Hook + CLI Issues — 2026-07-06

Context: filed from a live `claude_code` session (`claude_code:1ad7c71b`) doing a
multi-agent assessment in `agent-builder-studio` alongside a Fable peer and
several codex agents. Rally hooks (SessionStart presence, before-write,
external-intake, stop) fired throughout. Six issues surfaced — one is a
correctness footgun against the operator-set identity model, the rest are
signal-to-noise and discoverability. Binary: `0.1.5` (installed via marketplace
cache `0.1.3`).

Rally-room risk fact for this report: posted after write (see BACKLOG note).

## Issue 1 — `external-intake` risk facts flood `current_risks` with non-actionable telemetry

**Problem.** A hook auto-posts `kind=risk / subject="external-intake: <path>"`
for every file the agent reads or writes **outside the repo root**. These are
path-touch telemetry, not risks, yet they land in `room.current_risks`
alongside real risks. An agent (or human) scanning `current_risks` to decide
what needs attention cannot separate genuine risk facts from intake noise.

**Evidence (this session).** In the `agent-builder-studio` room, `current_risks`
= 10. Exactly **one** was a real, actionable risk (a C0 commit-hygiene finding).
The other nine were `external-intake` / `unmanaged-agent` /
`duplicate-active-squad-id`. My session alone minted `external-intake` facts for
`~/.claude/plans/flickering-nibbling-marshmallow.md`,
`~/.claude/projects/.../memory/MEMORY.md`,
`~/.claude/projects/.../memory/reference_extension_adapter_contract_before_repo_split.md`,
and a scratchpad `research-entry.md` — none of which are repo risks.

**Options.**
1. **Route intake to its own fact kind** (`intake`/`telemetry`), excluded from
   `current_risks` by default; surface under a separate `room.external_intakes`
   only when asked. *(recommended)*
2. Keep the kind but **default-filter** `subject^=external-intake` out of
   `current_risks` in `rally room` unless `--include-intake`.
3. Suppress intake facts for well-known agent-config trees
   (`~/.claude`, scratchpad/tmp) entirely — they are never repo risks.

## Issue 2 — bare host-family `--tool` is accepted silently and mints collision risk facts

**Problem.** The identity model (binding decisions `fact_f337…`, `fact_fd5c…`)
says every working agent posts under a UNIQUE id (`claude_code:<uuid>`), not the
bare host family (`claude_code`). But `rally say --tool claude_code …` is
**accepted without error or normalization**, and the hooks then emit
`unmanaged-agent: claude_code` + `duplicate-active-squad-id: claude_code` risk
facts. The CLI silently does the wrong thing and turns it into room noise.

**Evidence.** My first three `rally say` posts used `--tool claude_code` (host
family) before I loaded the skill. Result: `duplicate-active-squad-id` +
`unmanaged-agent` risk facts under bare `claude_code`, now stale-facts clutter.
The correct `claude_code:1ad7c71b…` posts were clean.

**Fix.** `rally say` / `rally enter` should **reject or hard-warn** when `--tool`
is a bare registered host family (`codex`, `claude_code`, `cursor`, `gemini`)
that collides with one or more active session ids in the room — exit non-zero
with the "use a unique agent id" guidance, rather than accepting it and
back-filling risk facts. A bare family id is almost always a mistake under the
operator-set model.

## Issue 3 — no read-only per-kind list verbs; only `rally room --json` + nested parse

**Problem.** The natural read verbs do not exist. All of these return
`rally: unknown Rally command`:
`rally artifacts`, `rally decisions`, `rally risks`, `rally log`,
`rally artifact list`, `rally room --full`. `rally next` also errors without
`--tool`. The only way to list facts of one kind is `rally room --json` then
hand-parse `data.room.{recent_artifacts,current_decisions,current_risks,active_claims}`
(note the double-nested `data.room`).

**Impact.** Every agent re-implements the same JSON parser; discovery-by-trying
wastes turns hitting "unknown command." A local-first coordination tool should
make its own room trivially greppable.

**Fix.** Add thin read-only aliases `rally risks|decisions|artifacts|claims
[--json]` that project the corresponding `room.*` array (and let `rally room`
accept `--kind risk|decision|artifact`). Zero new state — pure views over the
existing room payload.

## Issue 4 — `rally whoami --tool <id> --json` returns all-null inside a valid repo

**Problem.** `rally whoami --tool claude_code:1ad7c71b… --json` returned
`{repo_id: null, worktree: null, build_id: null, cwd: null}` while run from
inside the `agent-builder-studio` git worktree. The help text advertises these
fields; getting all-null makes `whoami` useless for the identity/liveness
self-check the skill recommends.

**Fix.** Populate `repo_id`/`worktree`/`cwd` from the resolved repo root (the
same resolver `rally enter` uses — enter worked from the same cwd), or document
why they can be null and what to pass to populate them.

## Issue 5 — `rally sessions --json` double-nests `{"sessions":{"sessions":[]}}`

**Problem.** `rally sessions --json` returns
`{"data":{"sessions":{"sessions":[]}}}` — the doubled `sessions` key breaks
naive `data.sessions` iteration (it's a dict, not the list you expect).

**Fix.** Flatten to `data.sessions: []`, or rename the inner container
(`data.sessions.items` / `data.sessions.managed`).

## Issue 6 — stale codex peer + binary-drift risk facts accumulate instead of aging out

**Problem.** Repeated `unmanaged-agent: codex:…`, `duplicate-active-squad-id:
codex:…`, and `binary-drift: 0.1.5+890aefc vs 0.1.5+5e90f0b` risk facts from
codex sessions pile up in `current_risks` rather than deduping or aging into
`stale_facts`. Binary-drift across agents in one room is a real signal, but it
should be **one deduped fact per distinct drift pair**, not one per re-detection.

**Fix.** Dedup identity/drift risk facts by `(kind, subject)` and let the reaper
(`rally sessions --reap`) expire `unmanaged-agent`/`duplicate-active-squad-id`
for sessions no longer present. Collapse repeated `binary-drift` into a single
current fact updated in place.

## Resolution (2026-07-07, branch `improve/room-signal-and-read-surface`)

Durable design improvements, not point-fixes. All land behind the existing test gate (424 lib + all integration tests green, clippy clean).

- **Issues 1 + 6 → DI-1 (fact class).** New `RoomSnapshot.system_health` bucket: risk facts with a known system subject prefix (`external-intake:`, `unmanaged-agent:`, `duplicate-active-squad-id:`, `binary-drift:`) project there instead of `current_risks`, deduped by subject. One predicate (`SYSTEM_HEALTH_SUBJECT_PREFIXES` in `store.rs`) is the single source of truth. `current_risks` now shows only human coordination risks; telemetry stays auditable. `rally room` text line surfaces `system_health=N`.
- **Issue 2 → DI-1 makes it harmless + DI-4 stops accumulation.** Bare-family noise is reclassified as telemetry (out of the risk view) and idempotency-guarded. Hard-reject deliberately NOT added (fails-closed against hooks; DI-1 removes the actual pain). Bare-family *normalization* flagged as a separate future decision.
- **Issue 3 → DI-2 (read verbs).** `rally risks|decisions|artifacts|claims [--json]` added — thin projections of the existing snapshot at `data.<verb>.rows`.
- **Issue 4 → not a code bug.** Current source resolves all `whoami` fields; the null output was a stale pre-`fcb81ca` binary. Rebuild/reinstall. Invariant already guarded by `json_envelope_contract.rs`.
- **Issue 5 → DEFERRED (not in this branch).** Flattening `data.sessions.sessions` → `data.sessions.rows` is a breaking wire change to the *shipped* `agent-rally.command.sessions.v1` schema. A Fable+Codex audit convicted the first attempt (9 user_journey failures + unregenerated schema) — doing it correctly requires a `v2` schema bump + consumer migration, which is out of scope for this change. Tracked as a proper follow-up.
- **Issue 6 → DI-4 (idempotency guards).** `duplicate-active-squad-id` + `binary-drift` now dedup at generation (matching the `unmanaged-agent` guard) AND collapse by subject in the `system_health` projection.

## Priority

- **P1 (correctness):** Issue 2 — silent bare-family acceptance violates the
  identity model and generates the very noise of Issues 1/6.
- **P2 (signal-to-noise):** Issues 1, 6 — the room's risk view is ~90% noise in
  a live multi-agent session, which defeats "rally as the source of
  coordination truth."
- **P3 (ergonomics):** Issues 3, 4, 5 — read-path discoverability and JSON shape.
