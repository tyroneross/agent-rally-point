<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Build-loop plan A — "take the best from PR46" onto the main foundation

> **Target line:** `main` (LEAN, single `rally-cli` crate, flat `Fact` model). Confirmed canonical
> 2026-05-28: integration was merged into main (`8a74f60 merge: consolidate rally integration into local main`),
> so main now carries all of origin/main + the integration work as the single source of truth.
> **Why this is the easy path:** PR46 was built on the same LEAN line. `main` already contains
> `check.rs` / `next.rs` / `store.rs`, so PR46's two features largely **apply** rather than needing the
> cross-architecture re-port the context brief §4 described (that was for the abandoned attuned line — ignore here).
> **Run via:** `/build-loop:run` from `~/dev/git-folder/agent-rally-point` on branch `main`.
> **Hosts:** Claude + Codex, coordinating through the rally channel itself (dogfood). ≤4 parallel agents per host per phase.
> **Branch hygiene:** any chunk branches collapse back onto `main` at close; no leftover branches/worktrees.

---

## 0. Goal / Deliverables / Unknowns

**Goal.** Land PR46's two coordination capabilities on `main`, fixing the two known nits, with the full
schema/test chain green.

**Deliverables.**
1. **Predictive contract claims** — `--produces` / `--depends` on claims, cross-agent stale-base detection,
   soft gate (`coordination_required`), `suggested_command`, boundary `validate_contracts`.
2. **Handoff receipts** — derived lifecycle, **upgraded to a transitive ref-chain walk** (fixes the CodeRabbit
   Major), `self_reported`, evidence from latest chain artifact.
3. **CI gate** — `check ci --strict` (exit 4 on confirmed-stale-base + active-blocker), `declare-contracts`
   before-write nudge.
4. **Schema + test chain** — `fact`/`next`/`room`/`check` schemas, golden-contract validation, user-journey tests.
5. **Doc fix** — RALLY.md evidence wording (evidence is from the artifact fact, not "the producer's claim").

**Unknowns to resolve in Phase 1 (Assess).**
- **U1 — overlap with PR43/44/45 + the integration merge.** main now descends from origin/main (merged
  PR#43/44/45, same head branch as PR46) **plus** the integration merge. **Some of PR46 may already be present.**
  First chunk diffs `main` against PR46 to find what is genuinely missing. Plan scope = the delta only.
- **U2 — `say` vs `claim` surface.** Confirm main's write command verbs (lean line uses `rally say claim`).
- **U3 — receipt transitive walk** interacts with `resolve` vs `release` semantics; confirm on main's model.

**Risks.** (a) PR46 content partially landed → wasted re-implementation (mitigated by U1 delta chunk).
(b) Transitive receipt walk could create cycles → use a visited-set BFS (CodeRabbit's pattern). (c) Schema
drift if any layer is missed → the golden-contract test is the backstop gate.

---

## 1. MECE work split (file-ownership = no write conflicts)

| Owner | Chunk | Owned files | `--produces` contract |
|---|---|---|---|
| **build-loop (Assess)** | A0 reconciliation | *(read-only)* | `delta.pr46-vs-main` |
| **Codex** | A1 contract claims | `crates/rally-cli/src/next.rs`, `crates/rally-cli/src/cli.rs`, `crates/rally-cli/src/lib.rs` (validate_contracts) | `feature.stale-base`, `cli.produces-depends` |
| **Codex** | A3 CI gate + nudge | `crates/rally-cli/src/check.rs`, `.github/workflows/rally-gate.yml` | `feature.ci-gate` |
| **Claude** | A2 receipts (transitive) | `crates/rally-cli/src/store.rs` | `feature.receipts`, `model.fact-fields` |
| **Claude** | A4 schema + tests + doc | `docs/schemas/*.json`, `crates/rally-cli/tests/*.rs`, `RALLY.md` | `schema.contracts`, `tests.contracts` |

**The one shared edge (dogfood it):** `store.rs::Fact` must gain `produces`/`depends` fields (Claude owns
`store.rs`) **before** Codex's `next.rs` matcher can read them. → Claude lands `model.fact-fields` first and
posts a **handoff** to Codex via rally; Codex's A1 claim `--depends model.fact-fields@<hash>`. If Claude
re-pins the fields, Codex sees a `stale_base` finding on its own claim — the feature validating itself.

---

## 2. Chunks (each: change → verify → commit)

### A0 — Reconciliation (build-loop Assess, read-only)
- `git fetch`; diff `main` against PR46's tree (`gh pr diff 46` or the archived bundle). Produce `delta.md`:
  which of {produces/depends fields, stale_base, receipts, check ci, schemas} already exist on main vs missing.
  **Scope the rest of the plan to the missing delta.**
- F-criteria: `delta.md` lists each of the 5 deliverables as present | partial | absent with file:line evidence.

### A1 — Predictive contract claims (Codex)
- `Fact` already has `produces`/`depends` after A2's field add (see shared edge). Add `--produces`/`--depends`
  repeatable flags (`cli.rs`), `validate_contracts` at the `say` boundary (`lib.rs`: reject empty name / >1 `@`).
- Port `stale_base_findings()` into `next.rs`: cross-agent `depends`×`produces` name match; `confirmed` iff both
  pinned and hashes differ; `suggested_command` = blocker-referencing-producer; soft gate `coordination_required`
  = any confirmed; surface in `next.stale_bases[]` and `enter` attention (`reason:"stale_base"`, every loop, even
  if producer predates cursor). **Advisory — never set `requires_human`.**
- F-criteria: unit test — confirmed (pins differ) trips gate; unpinned overlap does not; producer's own `next`
  carries no finding; malformed token → exit 2.

### A2 — Handoff receipts, transitive (Claude)
- Add `produces`/`depends` to `Fact` (`#[serde(default, skip_serializing_if=Vec::is_empty)]`) + `Receipt`/
  `ReceiptStep` structs; `RoomSnapshot.receipts`.
- `build_receipts()`: **transitive ref-chain walk** (BFS over `ref_id` with a visited-set — fixes the one-hop
  Major). State precedence: `completed` (chain artifact) > `blocked` (chain blocker ∉ resolve_refs) >
  `acknowledged` (handoff ∈ resolve_refs) > `acted` (chain claim) > `delivered`. `resolve_refs` from `kind==resolve`
  only (a `release` does not acknowledge). `evidence` = latest (max-seq) chain artifact's evidence.
  `self_reported = true` always.
- **Land `model.fact-fields` first + handoff to Codex** (shared edge).
- F-criteria: tests — delivered→acted→completed; blocked; release≠resolve→acknowledged; **a 2-hop chain
  (blocker refs a claim that refs the handoff) renders `blocked` not `delivered`** (the regression the Major would cause).

### A3 — CI gate + nudge (Codex)
- `check ci` phase in `check.rs`: `confirmed-stale-base` + `active-blocker` = `stop`; `open-handoff` = `warn`.
  Exit 4 iff `strict && any stop`. `before-write` always appends `declare-contracts` (severity `info`).
- Switch `.github/workflows/rally-gate.yml` to `check ci --tool ci --strict --json`.
- F-criteria: tests — `check ci --strict` exit 4 on confirmed-stale + active-blocker; exit 0 on clean room;
  `before-write` emits `declare-contracts`.

### A4 — Schema chain + golden tests + doc (Claude)
- Update `docs/schemas/agent-rally.fact.v1.json` + `docs/schemas/agent-rally.command.{next,room,check}.v1.json` (+produces/+depends; +stale_bases/+coordination_required;
  required +receipts; phase enum +ci). Thread through the kind→schema source. Extend golden-contract test to
  validate the new fields. Fix RALLY.md evidence wording.
- F-criteria: `cargo test` golden-contract suite green; every command's JSON validates against its schema.

---

## 3. Exit gates (all must pass before "done")
```
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all                       # incl. new A1–A4 cases + the 2-hop receipt regression
cargo run -p rally-cli -- check ci --tool ci --strict --json   # exit 0 on clean room
```
Plus build-loop's own: plan-verify + plan-critic on this plan; independent-auditor on the final diff;
code-simplifier pre-commit (durable-minimum: PR46 was ~537 LoC — aim ≤ that since the transitive walk
replaces, not adds to, the one-hop walk). **At close: merge the chunk work onto `main`, delete any temp branches.**

## 4. Coordination protocol (dogfood)
- Both hosts `rally enter --tool <claude_code|codex>` at start; `rally next` each loop.
- Each chunk: `rally say claim --produces <contract> [--depends <c>@<hash>]` before writing owned files.
- Shared edge: Claude `rally say handoff --target codex --subject "fact-fields ready" --ref <claim>`; Codex
  acts → receipt goes `delivered→acted→completed`. **This run is the feature's first live test.**
- `rally check before-write` before each edit; resolve any `stop` (confirmed stale-base) before proceeding.
