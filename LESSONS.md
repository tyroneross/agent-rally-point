<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Lessons learned — agent-rally-point

Curated process lessons (how we worked) from the multi-agent rally sessions. Each is insight about
the *process*, with **why** it bit and **how to apply** it — distinct from the forward work items in
[`BACKLOG.md`](BACKLOG.md). Source detail: the hand-written retro in
[`docs/assessment-2026-05-30-session-rallyflow.md`](docs/assessment-2026-05-30-session-rallyflow.md)
(§Lessons Learned); the auto-generated fact ledger is [`.rally/RETROSPECTIVE.md`](.rally/RETROSPECTIVE.md)
(`rally retrospective` — do not hand-edit).

**Headline:** the session's dominant defect was the **wrong negative** — 5× a "X doesn't exist / is
done / is a false positive" conclusion drawn from one cheap signal was reversed by a deeper probe. A
wrong negative is self-sealing (it stops investigation); a wrong positive gets caught downstream.
Everything below radiates from that.

## The lessons

1. **Verify before asserting a negative.** Any claim that "X doesn't exist / didn't work / is a
   false positive / is done / not a bug" must cite the *exhaustive* check that ruled it out. A
   closing claim touching another squad's lane enters as **PROPOSED** (peer-confirm), not BINDING.
   *Why:* the dominant defect class; self-sealing, and with merge authority delegated it can suppress
   a real fix. *How:* a machine-stored `verified_by` field (literal command + output) on every
   closing/negative fact; a disprove-by-source gate (grep ALL callers, read the actual field, check
   BOTH binaries, read back state); cross-lane closings default to peer-confirm.

2. **Trust the effect, not the call.** An exit code / `--help` success / "delivered" status proves
   the command *ran*, not that state *changed* — read back ground truth before reporting success.
   *Why:* the release no-op and stale-binary write-drop corrupt shared truth without erroring, so
   nothing downstream catches them. *How:* automatic readback after mutations (assert new fact
   present / `max_seq` advanced); after install/rebuild confirm *which* binary on PATH ran. → this is
   the lesson that becomes **R9-readback** in the backlog.

3. **Make ground truth self-declaring, not inferred.** Put the fact IN the identifier instead of
   extrapolating from a weak signal. *Why:* model attribution was over-generalized from one
   self-report (both Codex squads tagged "gpt-5.4-mini"; one was 5.5). *How:* `host-llm-role-number`
   squad ids encode the model so the lead never infers it; generalize — readback (ledger declares the
   write landed), session-bound ack (the session declares it woke).

4. **A context-free read-only verifier is the highest-leverage role in a facilitator model.**
   *Why:* the read-only Codex coordinator caught exactly the three silent-corruption defects the
   acting lead was blind to — precisely because it had no lane to defend and re-derived state each
   heartbeat. *How:* standing rule for any 2+ agent build — one persistent read-only squad, full room
   visibility, zero edit authority, re-derives ground truth each heartbeat, posts divergences as
   risks; pair with the peer-confirm gate.

5. **A consistent handoff shape is a protocol, not an agent-quality issue.** *Why:* free-text
   handoffs made Codex squads request a contract and stand by; the 11-field contract removed the
   round-trips. "What next?" is a protocol gap, not a slow agent. *How:* issue every cross-agent
   handoff in the fixed 11-field shape from the first handoff; keep tier instructions host-relative
   (don't tell another host to use *your* model tier).

6. **A dependency's schema is not your feature.** *Why:* `facts.db.subscriber_cursors` exists but is
   created by the `factstr-sqlite` dependency, holds zero rows, and is referenced nowhere in rally —
   reasoning it backed read-tracking would be wrong (rally's read state is the separate
   `cursors.json`). Same shallow-claim root as #1, reversed: presence-of-marker ≠ behavior. *How:*
   grep the project's *own* source+tests for a table/field before asserting it does (or doesn't)
   implement a behavior.

7. **Multiple working trees + a gitignored source-of-truth ledger is a handoffs/simplicity gap.**
   *Why:* the two-clone split caused a route.mjs commit on the wrong branch (cherry-picked,
   e1589dd) and kept the resume-ledger gitignored so the coordination story didn't travel with
   commits until R1 made it tracked. *How:* pin the primary checkout to `main` + branch work in
   dedicated worktrees; make the coordination ledger canonical + tracked from day one; `rally whoami`
   so identity is queried, not inferred.

8. **A reversible, authorized, clearly-instructed action must be executed on first instruction —
   never re-surfaced as an "open call."** *Why:* the lead identified the two-clone consolidation as
   the right fix, then deferred it across *multiple turns* as "your open call / say the word," forcing
   the user to repeat the directive. It was reversible (re-clone from origin), authorized (a direct
   instruction), and determinate (one outcome) — exactly the profile that should auto-execute.
   **Re-presenting a directive the user has already given is itself the violation.** *How:* when about
   to write "your open call" / "say the word" / "want me to…" about something reversible the user
   already asked for, treat that phrase as the stop signal and execute that turn instead.

9. **Repo scope is a truth boundary, not just a path prefix.** *Why:* a different repo's assessment
   briefly entered the agent-rally-point coordination surface because it looked useful — creating
   ledger/backlog contamination. *How:* every artifact/handoff/assessment carries `repo_root`,
   `repo_id`, `ledger_scope`; external material routes to its owning repo or a neutral intake
   surface; only distilled repo-local actions enter this backlog. → becomes **B18**.

10. **A confusing-to-parse output contract is a product defect, not a parsing-discipline problem.**
    *Why:* the `--json` envelope had three inconsistent nesting patterns (some results under
    `data.<command>`, some flat, `wake-due` under `data.due`), so every consumer — and a verifier
    ~5× in one session — guessed the path wrong. No amount of "dump raw first" discipline fixes an
    output with no rule. *How:* standardize so `data[command]` ALWAYS holds the result, and enforce
    it with a **COMMANDS-driven contract test** (`json_envelope_contract.rs`) so a new command can't
    skip the contract. Generalize: ad-hoc `bash | python -c` parsing is itself a defect surface —
    the in-repo test suite (correct args, correct fields) is the real verification, not a one-liner.

11. **Async deliver-then-ack: an ack-timeout is not a failure of the primary action.** *Why:*
    `inject --require-ack` recorded the content fact and delivered the message *before* waiting for
    the ack, then on timeout returned `ok:false`/exit-1 — which a caller reads as "the whole inject
    failed" and retries → **duplicate delivery**. *How:* the durable primary action (record +
    deliver) reports its own success; the downstream ack is separate metadata. On timeout return
    `ok:true, delivered:true, ack:{resolved:false, timed_out:true}` so the caller sees the message
    landed and checks `ack.resolved` rather than re-sending. Record-then-deliver-then-ack is the
    smooth shape; the timeout response must distinguish "not acked" from "not delivered".

12. **A backlog item's prescribed FIX can be stale, or now wrong against the charter — re-validate
    both before building.** *Why:* most "open" rows had already shipped (verify-the-negative again),
    and B18's written prescription ("hard-reject foreign-repo writes") would have *violated* the
    never-block charter — the shipped quarantine-and-filter was the correct approach. Building the
    backlog literally would have regressed a core principle. *How:* Phase-1 assessment validates each
    item twice — (a) is it still an issue, against current code; (b) is the prescribed approach still
    right, against the charter/invariants. A backlog is a hypothesis, not an instruction.

13. **The canonical record must survive the cache.** *Why:* `facts.db` is a derived sqlite cache;
    `.rally/log/<engagement>.jsonl` is the canonical record. On 2026-06-01, easy-terminal saw
    `facts.db` corrupt and every `rally` command failed with `database disk image is malformed`; the
    surviving cache later restarted at `sequence_number 1`, leaving orphan `facts.db.corrupt.bak`
    files on disk. Root cause: `read_db_event_count` propagated SQLite's corruption error instead of
    treating malformed-cache identically to missing-cache, so the existing `rebuild_db_from_segments`
    replay path never fired. *How:* opens that touch a derived cache must (a) recognise corruption
    sentinels (sqlite codes 11/26: `SQLITE_CORRUPT`, `SQLITE_NOTADB`), (b) quarantine the bad bytes
    for forensics (`facts.db.corrupt.<UTC_NS>` + WAL/SHM siblings), (c) fall through to the existing
    rebuild path. The cache-false-pass invariant in `docs/ORCHESTRATION.md §116` is now load-bearing:
    every reader hits this path on open, so corruption is a non-event. Empirical test:
    `malformed_facts_db_is_rebuilt_from_segments_on_open` in `crates/rally-cli/src/store.rs`.

## Top systemic fixes (ranked)

1. **Verify-before-asserting-a-negative** — machine-stored `verified_by` on every closing fact +
   cross-lane closings peer-confirm before binding. Retires the dominant defect class.
2. **R9 / R9-readback** — post-mutation readback + stale-binary guard. Kills release-no-op and
   stale-binary-drop at one anchor; charter-safe (rally still only facilitates).
3. **R10** — deterministic read+write path; `rally room --readers` surfaces receipts.
4. **Standing read-only verifier squad** doctrine for every 2+ agent build, paired with peer-confirm.
