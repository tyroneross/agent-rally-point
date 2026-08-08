<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Position — Rally as a federated coordination plane

> **Living document.** It records a position, how that position changed, and why. Revisions
> append to §2; they do not silently rewrite §1. If you disagree with §1, the fastest way to
> move it is to falsify a row in §4.
>
> **Status:** working position, not ratified. **Last revised:** 2026-08-08 (v5).
> **Supersedes:** nothing. **Superseded by:** nothing.
> **Room:** `federated-coordination-plane`, facts seq 8976–9032.

---

## 1. Current position

**Rally is a federated coordination plane. Its protocol is canonical and internal; every
external protocol is a codec at the edge, and several codecs run at once.**

Three things are defensible. Everything else belongs to the host or to a later commercial layer.

| # | Defensible core | Why nobody else holds it |
|---|---|---|
| 1 | **Deconfliction enforced below the model, in single-digit milliseconds** | 11 ms cold start (§4.1). A hook fires whether or not the agent remembered to check. Node/Python implementations pay ~116 ms before doing any work; Anthropic's layer is in-process TS and has no file-ownership concept at all. |
| 2 | **A durable, replayable record of who owns what and who acknowledged** | MCP, A2A, and ACP each decline to express audit trails, delegation, consent lifecycle, and conflict resolution (§4.4). The standards are converging on transport and capability. Nobody is claiming this. |
| 3 | **Simultaneous heterogeneous reach** | A Claude peer on `SendMessage`, a Codex peer on `app-server`, a CI job on webhook, all in one room at once. Anthropic's native layer coordinates Claude sessions only, by construction. |

**What Rally is not.** Not a session runtime (that is Easy Terminal). Not a scheduler. Not a
judge of whether code is correct. Not a privilege boundary — see `docs/security/TRUST-MODEL.md`.

**The single sentence.** *Rally delivers the fastest correct coordinated outcome across
heterogeneous agents, with durable proof and no human referee.*

### 1.1 The two constraints that decide whether the codec model works

1. **The internal model must be a provable superset.** A codec over an under-specified model
   drops fields silently — the same failure shape as an unannounced capability downgrade.
   Prove it with round-trip tests (`A2A AgentCard → internal → AgentCard`), not assertion.
2. **Identity is internal-first.** Codecs map *into* a stable Rally id; they never store a
   per-protocol id as the identity. With one codec Rally already produced **140 distinct
   `tool` ids** for roughly a dozen real agents (§4.2). Each additional codec multiplies that
   unless resolution lives above the codec layer.

### 1.2 The naming question, settled

Do not adopt A2A's or ACP's vocabulary. **Keep capability vocabulary in the runtime
descriptor and out of durable facts.** In a runtime descriptor a rename costs one
`#[serde(alias = …)]` — precedent already in-tree at `crates/rally-cli/src/store.rs:171`.
Written into an append-only fact it is immutable, and every projection carries a
compatibility shim for the life of the ledger.

---

## 2. Position log

Each revision states what changed and what forced the change. Corrections are kept, not edited away.

### v1 — Adopt the native transport (2026-08-07)
Claude Code 2.1.224/225 shipped cross-session `SendMessage` + `ListAgents`, incl. cross-machine.
Initial read: use it as a delivery backend for `rally inject`; Rally's moat is claims + ledger.

### v2 — Corrected: cross-host is already the charter, and it is failing
Reading the repo rather than the summary changed three premises:
- "Rally should coordinate across hosts" is not a new strategy — `NORTH_STAR.md` already says
  it. The gap is execution: **4.4 % wake delivery, 25 % of handoffs never acknowledged** (§4.2).
- **There is no receive side.** `FileInbox::read_since` has zero call sites outside its own module.
- The presence registry with `capabilities` is **already specced** in `PROTOCOL-NORTH-STAR.md`
  and dead-coded in `session_identity.rs`.

→ The work was reclassified from *design* to *wire what exists*.

### v3 — Found the contract already written, then over-corrected on naming
`docs/schemas/agent-rally.session-backend.v1.json` already defines `surfaces{run, inject,
capture, stop, wake_signal}` and a `delivery` enum (`native_wake`, `managed_session_injection`,
`resume_or_prompt`, `automation_policy`) that maps 1:1 onto Claude / tmux / Codex / CI. Zero
call sites. `rally_owns_daemon` is a `const: false` — the charter answer, written and unenforced.

**Recommended aligning field names with A2A/ACP "before it gets expensive."** That was wrong,
and the challenge that broke it was correct: *why can't we just rename later?*

### v4 — Corrected: renaming is cheap; the real rule is where the vocabulary lives
Evidence against v3: `serde(alias)` precedent in-tree, `CompatMode::{Lenient,Strict}`, versioned
`.v1` schemas, a documented forward-compat stance, and `wake_signal`/`backend` **absent from
`ledger.rs`** — the vocabulary is not persisted today. Renaming costs one alias. See §1.2.

Two further findings in this pass:
- **"ACP" names three different protocols.** IBM's Agent Communication Protocol merged into A2A
  and was archived 2025-08. Zed's Agent Client Protocol is the live one. Any source saying "ACP"
  needs disambiguation before it is acted on — including the arXiv governance-gaps paper (§4.4),
  which most likely means IBM's.
- **MCP dwarfs everything** (§4.4) and Rally exposes none of it.

### v5 — The federation framing, and the measurement that reorders the plan (2026-08-08)
Operator direction: *Rally keeps its own protocol; external protocols are decoders; multiple
run simultaneously.* Adopted as §1. This demotes MCP from a strategic question to **codec #1**.

Then the Rust engine was measured, and it reordered the sequence: **the 11 ms advantage is real
and is being discarded by an 830 ms hook** (§4.1). No codec work can precede fixing that — a fast
codec bolted to an 830 ms hook is still an 830 ms product.

**Also recorded against my own earlier framing:** Rust is not the advantage on the room path.
`rally room` is 132 ms of which ~121 ms is ledger work. RC-042 (quadratic projection) and RC-058
(write path re-reads the whole ledger ~5× per append) survived to a 6.9 MB ledger *because* Rust
made them fast enough not to notice. Speed substituting for the right data structure is a trap
already entered.

### v6 — Rally bills in tokens too, and its advisory is not human-readable (2026-08-08)
Operator question — *"does this consume tokens, and what is it even telling me?"* — exposed a cost
axis the position had ignored. Measured: **236 tokens per user prompt, 101 of them byte-identical
boilerplate**, accumulating to ~11.8 k tokens over a 50-prompt session (§4.5).

Two consequences, both folded into Phase 1:
- **Cost is two-currency.** "Fast" has meant milliseconds. Rally also spends the scarcest resource
  an agent has — context. A coordination layer that costs 10 k tokens a session to say mostly
  nothing will be turned off, exactly as an 830 ms hook will be skipped.
- **The advisory is written for the model, not the operator.** It leads with a defensive disclaimer
  and never states outcome, cause, or next action. A human cannot tell from it whether anything
  happened. That is a product defect, not a formatting preference.

---

## 3. The plan

Phases are gated. **A gate is a measured number or an executed check, never a review opinion.**

### Phase 0 — Make the room legible (prerequisite, not optional)
`rally room` in this repo currently returns `system_health=161` against 5 coordination facts.
RC-067 / RC-068. Every fact this programme writes lands in a channel that truncates it.

**Gate:** coordination facts are never displaced by `system_health` in a default `rally room`.

### Phase 1 — Reclaim the cold-start advantage
1. `rally hook <phase> <host>` as a **Rust binary subcommand** emitting the host envelope.
   Already specified as backlog **B19-(a)-universal**. Removes Node from the hook path and
   collapses N invocations to one. Also closes **RC-033**.
2. Make `check before-write` read the active claim index only — never replay the ledger.

**Gate:** `before-write` p95 **< 20 ms**; `start` p95 < 150 ms; `node` absent from every hook path.
Baseline to beat: 830 ms / 1450 ms (§4.1).

> Why this is first: at sub-20 ms, coordination stops being a tradeoff an agent can rationally
> skip. That is the product goal, not a performance nicety.

3. **Stop paying context rent.** The `UserPromptSubmit` advisory costs ~236 tokens **per prompt**,
   of which ~101 is a byte-identical security preamble (§4.5). Three fixes, in order of return:
   emit the untrusted-data boundary **once per session** rather than per prompt; emit nothing when
   the payload carries no peer-authored prose; emit a **delta**, not full room state, when nothing
   changed. Same family as RC-068 — re-emitting fixed text per entry until it crowds out the signal.
4. **Make the advisory human-readable: outcome first.** Current output leads with a defensive
   disclaimer written for the model and buries what happened. It must instead answer, in this order:
   **what happened · why · what next.**

   ```
   Rally: PROCEEDING — docs/POSITION-….md is unclaimed.
     Peer: claude_code:term-7c41 is editing README.md (claimed 6m ago, live).
     Next: 1 item awaiting review (fact_1510c). 2 peers idle.
   ```
   ```
   Rally: STOPPED your edit to src/auth.rs.
     Why: codex:019fdfe3 claimed it 4m ago and is live.
     Next: work elsewhere, or negotiate — their claim expires 14:32.
   ```
   The operator's own framing is the acceptance test: *"maybe the file is being edited by agents not
   on rally; we'll proceed in a new worktree while we investigate"* — a reader must be able to reach
   that sentence from the output alone.

**Gate (added):** median advisory ≤ 60 tokens per prompt; zero tokens on a no-change tick; every
advisory states outcome, cause, and next action in that order.

### Phase 2 — Presence and identity
Take `session_identity.rs` out of `allow(dead_code)`; make `endpoint_id` and `capabilities` real;
resolve identity internally so one agent is one id across codecs.
Closes the writer gap behind **RC-030** (`branch_head_sha` / `planned_heartbeat_secs`: 0 occurrences
in the ledger) and therefore **RC-059**.

**Gate:** distinct `tool` ids observed over a week ≤ 1.2 × the number of real agents. Baseline: 140.

### Phase 3 — Receive side, and codec #1
A registered, always-on receive path. Claude `SendMessage` as the first delivery codec.
Closes OC **`b82292bc`** — open, `needs_input`, surfaced in 39 sessions over 18+ days.

**Gate:** delivery rate ≥ 95 % (baseline 4.4 %); every non-delivery produces a typed failure fact.
No "message sent" without transport success.

### Phase 4 — Formalize the codec seam
Extend `session-backend.v1` → `v2` with `supports_live_interrupt`, `supports_positive_ack`,
`on_downgrade`. One codec trait, one conformance suite, plus the round-trip superset test (§1.1).
Closes **RC-061**'s pattern by giving the contract a consumer.

**Gate:** conformance suite passes for codec #1; round-trip test passes for at least two external
vocabularies; a codec that cannot carry a required field **refuses** rather than degrading silently.

### Phase 5 — Intent at checkout
The missing primitive. A claim says *what path*. Intent says *what done looks like*: repo, intended
files, estimated region, system area, why, self-directed vs instructed-by-whom, expected outcome,
and the done-criteria. Cross-check declared downstream impact against NavGator's computed blast
radius; **divergence is a first-class coordination risk.**

**Gate:** a peer can decide *work elsewhere / prepare downstream / audit on landing* from the room
alone, without messaging the owner. Measure: unanswered-handoff rate (baseline 25 %).

### Phase 6 — Codec #2 and the MCP codec
Codex via `app-server` (bind against `generate-json-schema`), `exec resume` as the queued fallback,
tmux demoted to last resort. MCP server as the universal read/write surface — **not** a delivery
transport: per MCP 2026-07-28 servers never initiate requests, so MCP cannot wake an idle agent.

**Gate:** one workflow passes end-to-end across a mixed Claude + Codex fleet.

### Later — authenticated identity
Signed facts, `principal_id` enforced. **Not before a second writer exists.** See §5.

---

## 4. Evidence register

Markers: `executed:` = run on this machine, 2026-08-07/08 · `cited:` = from a repo doc that states
its own verification · `T1/T2/T3` = source tier.

### 4.1 Latency — `executed:`, 5-run means, this machine

| Measurement | Value |
|---|---|
| `rally version` (pure Rust startup) | **11 ms** |
| `rally claims` | 96 ms |
| `rally whoami` | 103 ms |
| `rally room` (6.9 MB / 7,872-line ledger) | 132 ms |
| `node -e 0` (bare Node, does nothing) | **116 ms** |
| Hook `before-write`, end to end — fires on **every** edit | **830 ms** |
| Hook `start`, end to end | **1450 ms** |

Binary: 5.5 MB static arm64 Mach-O, no runtime. `ptyd server` resident (pid 43604); `rallyd` not.
Hook source states the cause at `hooks/rally-coordination-hook.sh:49` — *"NODE REQUIRED FOR HOOK OUTPUT."*

### 4.2 Delivery — `cited:` `docs/assessment-2026-07-30-delivery-architecture.md`

| Metric | Value |
|---|---|
| Wake facts delivered | **32 of 734 — 4.4 %** |
| Handoffs with no target-authored response | **25 %** (44 of 174) |
| Wakes aimed at presence >15 min stale or never posted | **80 %** |
| Distinct `tool` ids ever seen | **140** |
| Time-to-first-response on answered handoffs | median 17.4 min, p90 63.3 h |

⚠️ One figure in that assessment is now stale: it found no `ptyd` running. One is running today.

### 4.3 Designed-and-unwired — `executed:` greps

| Artifact | State |
|---|---|
| `session_identity.rs` | `#![allow(dead_code)]` |
| `event_envelope::authorize` | no production call site (**RC-061**) |
| `docs/schemas/agent-rally.session-backend.v1.json` | no call site; flagged in **B13** |
| `FileInbox::read_since` | no call site outside its own module |
| Crypto in `rally-protocol` / `rally-cli` | **zero primitives**; only `cockpitd/src/crypto.rs` exists |
| MCP server | none; Rally only pattern-matches `codex mcp-server` in `ps` for liveness |
| Command schemas already written | **14** under `docs/schemas/agent-rally.command.*.v1.json` |

### 4.5 Context cost — `executed:`, measured against a live `UserPromptSubmit` payload

Rally bills every agent in tokens as well as milliseconds. The advisory is injected on **every**
user prompt and accumulates in history.

| | chars | ~tokens |
|---|---|---|
| Full advisory | 944 | **236** |
| Fixed security preamble (`UNTRUSTED_PREAMBLE`, `hooks/rally-coordination-hook.sh:693`) | 405 | **101** — byte-identical every prompt |
| Actual coordination signal | 539 | 134 |

| Session length | Total | Of which boilerplate |
|---|---|---|
| 20 prompts | ~4,700 | ~2,000 |
| 50 prompts | ~11,800 | ~5,100 |
| 100 prompts | ~23,600 | ~10,100 |

⚠️ Token counts are `chars/4`, an estimate, not a tokenizer run.
The preamble is load-bearing — it is the ARP-004 injection boundary — so the fix is *frequency and
placement*, never deletion. Companion latency numbers in `docs/ASSESSMENT-2026-08-03-efficiency.md`
(0-edit turn 0.92 s, 10-edit turn 5.6 s) measure the same tax in the other currency.

### 4.4 External protocol landscape — T1/T2 unless noted

| Protocol | Adoption | Relevance |
|---|---|---|
| **MCP** | 97 M monthly SDK downloads / 16 mo; registry ~1,200 → **9,400+** servers (Q1 2025 → Apr 2026); 78 % of enterprise AI teams run ≥1 in production | Universal read/write surface. Spec **2026-07-28**: stateless, **no `initialize` handshake**, per-request version in `_meta`, `server/discover` mandatory, **servers never initiate requests** |
| **A2A** | v1.0, Linux Foundation, 150+ orgs | Horizontal peer coordination; AgentCard is the capability-declaration reference |
| **ACP (Zed)** | Zed 1.0 headline Apr 2026; JetBrains built-in since Dec 2025; registry Jan 2026 | Editor↔agent transport. Client spawns agent — wrong topology for Rally, right topology for Easy Terminal |
| ACP (IBM) | **archived 2025-08**, merged into A2A | Dead. Source of the acronym trap |

Consensus stack: **MCP vertical (agent↔tools), A2A horizontal (agent↔agent).**
⚠️ T3: arXiv *"What MCP, A2A and ACP Cannot Express"* finds none express audit trails, delegation,
consent lifecycle, or conflict resolution. Preprint; also ambiguous on which ACP. Directionally
supports §1 row 2; do not cite externally without verifying.

---

## 5. Decisions owed by the operator

| # | Decision | Why it cannot be made by an implementer |
|---|---|---|
| D1 | **RC-026** — charter says never spawns; the CLI spawns. Proposed resolution: amend to *"delivers, does not schedule"*; Easy Terminal owns process and human observation; `rally_owns_daemon: false` stays true because the daemon serves state, not schedule | Product-boundary change |
| D2 | Does an MCP server ship? Standing preference is CLI/API over MCP; that preference is about *you driving tools*, and this is *other agents discovering Rally* | Reverses a standing default |
| D3 | Multi-user — needs signed facts, enforced `principal_id`, and a ledger commit policy. Multi-**org** additionally needs authoritative `workspace_id`, cross-domain key distribution, and a network transport, which **breaks the no-server property**. Recommend treating multi-org as a separate product | Changes the trust model and the adoption story |
| D4 | Whether an independent judge/auditor agent is needed for consistency. Current position: **no** — declared done-criteria (Phase 5) make verification deterministic and any agent can audit; reserve an LLM auditor for prose-only criteria | Adds latency to the top-priority axis |

---

## 6. Standing risks

**R1 — The design-without-wiring pattern.** Four instances in §4.3. This programme adds three new
contracts. **Rule: no new contract merges without a consumer in the same change.**

**R2 — Speed masking structure.** RC-042 and RC-058 survived because Rust made them tolerable.
Phase 1's gate must be measured *at the hook boundary*, not inside the CLI, where 830 ms was invisible.

**R3 — Remediation shipping its own defect class.** RC-024 records that the issue #52 remediation
shipped seven defects of the class it was fixing; RC-029's `controlled` mark was withdrawn after six
mutation-validated tests reported health over a half-covered surface. Treat every gate above as a
target, not a predicted closure.

**R4 — Identity is descriptive, not authoritative (RC-063).** Structural, not patchable. It bounds
every authority claim in this document. Nothing here should be described as *enforced* until D3 lands.

**R5 — Someone else is building the transport.** ACPX is already pitching multi-agent orchestration
over ACP as *"beyond PTY scraping."* The transport is commoditising. The ledger is not.

---

## 7. How to revise this document

Append a `v<n>` block to §2 naming what changed and the evidence that forced it. Update §1 only
after §2 records why. Add measurements to §4 with an `executed:` or `cited:` marker and a date.
Never delete a superseded position — the record of being wrong is the useful part.
