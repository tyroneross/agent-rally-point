<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Decomposition — goal → descriptor (host-neutral)

This is the procedure a strong orchestrator runs in its head to turn a vague goal into a
*linted workstream descriptor*. Written down so any host — any model tier, Claude / Codex /
Cursor / a bare agent — produces the same descriptor instead of relying on frontier intuition.
Output of this procedure feeds `workstream-lint.mjs` (boundary proof) then `packet.mjs`
(per-task prompts). Rally records and lints; it never derives the tasks for you — that is this
document's job.

## The five moves

### 1. Find the partition axis

A good fan-out splits the goal along ONE axis so the pieces are independent. Common axes:

| Axis | Use when the goal is… | Example |
|------|-----------------------|---------|
| one-per-persona | "review X from N perspectives" | 10 persona reviewers |
| one-per-file | "apply the same change across these files" | rename a symbol in 8 modules |
| one-per-route | "build/audit each page/endpoint" | 6 API routes |
| one-per-dimension | "evaluate X on these N criteria" | perf / a11y / security / cost audit |

Pick the axis that yields **disjoint `owns`** (move 2). If two candidate axes both fit, prefer
the one whose tasks write to clearly separate paths — that is what lets them run in parallel and
what the linter's MECE rule enforces. If no axis yields disjoint writes, the work is not
parallelizable as framed: either narrow it to a read-only review (every task `owns: "read-only"`,
each writing one result file) or sequence it with `depends_on`.

### 2. Derive `owns` from each task's OUTPUT artifact path

Do not hand-assign write boundaries — **read them off the output**. Each task produces one
artifact; its path IS its `owns`. When every task writes a distinct output file, the boundaries
are MECE by construction and the linter passes on the first try.

- Give each task its own output file under a run-scoped directory:
  `…/runs/<run_id>/<task-id>-<slug>.json`. One writer per file → zero overlap.
- A task that only *reads* (a review, an audit) still produces one output file — make that file
  its `owns`, so even read-heavy work has a disjoint write boundary.
- Never let two tasks share a directory prefix they both write into (`src/` and `src/foo.js`
  overlap — the linter rejects it). Push each task down to its own leaf path.

### 3. Make `validation` a deterministic command

`validation` is the command a reviewer (or the orchestrator) runs to confirm the task landed.
It must be **reproducible** — no wall-clock, no randomness. The linter blocks `Date.now()`,
`Math.random()`, and `new Date()` for exactly this reason.

- Prefer a parse/build/test the output must pass: `node -e "JSON.parse(require('fs').readFileSync('<out>','utf8'))" && echo VALID`, `node --check <file>`, `pytest <test>`, `cargo build`, a JSON-schema check.
- The command should exit non-zero on failure and print a stable success token (`echo VALID`)
  so evidence is greppable.
- No timestamps, no random fixtures, no `$(date)` — two agents running it must get the same
  result.

### 4. When the target repo has its own domain skill, DEFER to it

If the repo you are fanning out *into* ships a skill that already defines the work pattern, that
skill owns the **task content**; this procedure only imposes the descriptor's **structural
contract** (id / intent / owns / validation / output, disjoint writes, deterministic validation).

Example: the AI User Personas repo ships `persona-lab` (`plugins/persona-lab/skills/persona-lab/SKILL.md`).
Its §2 "Select Persona Perspectives" is the authority on *which* personas and *how many*
(4–7 unless specified; distinct lenses; at least one skeptic; a11y when the artifact is a UI).
Decomposition does NOT re-invent persona selection — it asks persona-lab for the roster, then
wraps each selected persona as one descriptor task with its own result-file `owns` and a JSON-parse
`validation`. Same rule for any repo with a domain skill: borrow the *what*, impose the *shape*.

### 5. Lint before you generate packets or fan out

```bash
node dynamic-workflows/core/workstream-lint.mjs my.workstream.json   # exit 0 → safe
```

A clean lint is the gate. Only after exit 0 do you generate packets (`packet.mjs`, §2.5 of the
skill) and dispatch.

## Worked example — the haiku-scale-20260609-01 persona run

**One-line goal (what the user said):**
> "Fan out 10 Haiku agents to review whether the AI User Personas workspace produces useful,
> trustworthy synthetic-user feedback."

**Move 1 — axis.** "Review from N perspectives" → **one-per-persona**. Ten personas → ten tasks.
Each persona is an independent lens, so the tasks are independent.

**Move 4 — defer to the domain skill (done before move 2 here).** The target repo ships
`persona-lab`. Ask its §2 selection rules for the roster instead of inventing personas: distinct
lenses (operator, design, support, competitive intel, product marketing, field, accessibility,
privacy, novice, research methodology), at least one skeptic (Morgan Lee, ops skeptic), a11y
included (Asha Patel) because the artifact is a UI. That yields the 10 named personas — persona-lab
chose the *content*; decomposition imposes the *structure*.

**Move 2 — owns from output.** Each persona writes exactly one result file under the run dir:
`councils/runs/haiku-scale-20260609-01/p01-morgan-lee.json`, `…/p02-priya-shah.json`, … Distinct
leaf path per task → disjoint `owns` → MECE by construction.

**Move 3 — deterministic validation.** Each result is JSON, so validation is a JSON parse:
`node -e "JSON.parse(require('fs').readFileSync('…/p01-morgan-lee.json','utf8'))" && echo VALID`.
No clock, no randomness.

**The resulting task (one of ten):**

```json
{
  "id": "p01",
  "intent": "Persona review as Morgan Lee (Operations skeptic, RevOps lead): audit the persona workspace for operational trustworthiness and auditability of persona-derived feedback",
  "owns": ["councils/runs/haiku-scale-20260609-01/p01-morgan-lee.json"],
  "validation": "node -e \"JSON.parse(require('fs').readFileSync('councils/runs/haiku-scale-20260609-01/p01-morgan-lee.json','utf8'))\" && echo VALID",
  "output": "JSON: {task, persona, top_concern, findings[{severity, evidence, why_it_matters, suggested_change, confidence}], effectiveness_suggestions[], access_gaps[]}"
}
```

Repeat for p02…p10 (same shape, different persona + leaf path), wrap in the descriptor envelope
(`workstream`, `description`, `tasks`), and `run_id: haiku-scale-20260609-01`. Lint → exit 0 →
generate packets (`packet.mjs --run haiku-scale-20260609-01`) → fan out. The real descriptor that
ran lives at `AI User Personas/councils/runs/haiku-scale-20260609-01/workstream.json`; all 10 agents
completed with zero retries and disjoint writes held under `before-write --strict`.

The full descriptor is what §1 of the skill links here from: derive it with these five moves, lint
it, then hand each task to `packet.mjs` for a ready-to-paste prompt.
