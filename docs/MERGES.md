<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Merge log — context for every merge to `main`

Now that **commit + merge authority is delegated to frontier agents** (COORDINATION.md field 4), every
merge to `main` MUST append an entry here with **context** — what, *why*, evidence, who, and the lead's
post-hoc audit. This is the durable context record (bookmark-style): a fresh agent or reviewer
understands the history without re-reading diffs, and the lead's audit trail is explicit.

**Entry format (append newest-first):**

```
## <date> · <commit-range> · <subject> — merged by <rally tool-id>
- **Context (why):** <the problem/intent this merge serves>
- **Evidence:** <tests/clippy/fmt/lint or host-equivalent that passed>
- **Lead audit:** ✅ verified <how> | ⚠️ pending | ❌ reverted <reason>
- **Blast radius / reversibility:** <files/systems touched; how to revert>
```

**Rule:** a merge without a MERGES.md entry is incomplete — the lead's post-hoc audit will flag it.
For routine fast-forward pushes of an agent's *own* committed lane, a one-line entry is fine; a
branch→`main` merge (e.g. L4) gets the full entry above.

---

## Backfill — this session's notable landings (summary level)

- **`8a74f60`** — consolidate the divergent rally lines into one `main` (lean line won; attuned line removed). Context: two structurally divergent architecture lines existed; `main` is now the single canonical superset. Evidence: integration was 100% merged. Audit: ✅ (the line we built on).
- **`6d76780`…`d52a184`, `e1589dd`, `ba1f93c`** — **Rally Flow** (dynamic-workflows module: L1 protocol/lint/skills + L6 route.mjs/workstream-status resume). Context: agent-rally-point's take on dynamic workflows, host-side protocol + guardrail + durable resume. Evidence: 35 module tests green. Audit: ✅ lead-built + verified. Reversibility: net-new `dynamic-workflows/` dir.
- **`12468f9`,`6ec1042`,`e7b87aa`,`3bdc040`,`c72328f`/`d39aab7`,`4e1a329`,`ea6aee9`** — **rally-cli hardening** (audit HIGHs B4–B9 + B8). Context: the scaled-audit + A/B surfaced real correctness/security bugs (gate bypass, NUL panics, index clobber, cli guardrails, lib residuals). Evidence: `cargo test`/`clippy -D warnings`/`fmt` green per commit. Audit: ✅ lead source-verified each (caught dropped B5/B7, re-delegated). Codex lane.
- **L4/PR46** — *pending* — Claude #2 merges `claude2/l4-audit-folds` → `main` itself (first formal entry under the delegated-merge rule). Will document: contract-claims + receipts + check-ci + folded B7-store/B8-next findings; evidence `cargo test`/clippy/fmt; lead audits post-hoc.
