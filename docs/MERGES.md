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

## 2026-05-29 · B14 doc-citation sweep · merged by claude_code:lead(build-loop)
- **Context (why):** the `wor9rkkhp` assessment found 8 stale/dangling doc-citation rows — docs/manifests citing files never ported from the prior pi repo, plus 5 implemented CLI subcommands (`sessions`/`attach`/`stop`/`locate`/`recent`) absent from the user-facing docs. Integrity fix: every cited path must resolve or be explicitly marked aspirational/external.
- **Evidence:** `cargo build` (workspace) finished clean — unaffected by the doc/manifest/docstring edits. `rally_wake.py` parses (`ast.parse`), `examples/manifest.toml` parses (`tomllib`). Verified each formerly-dangling path: `manifest.toml` `cli_entry=rally` (only `rally` binary exists), `discover_module` commented + marked aspirational; RALLY.md + README.md now document all 13 subcommands (confirmed wired in `cli.rs:158-159,251`); schema shorthand in PLAN-take-best-pr46 corrected to actual `agent-rally.fact.v1.json` / `agent-rally.command.*.v1.json` names (confirmed via `ls docs/schemas/`); WAKE_COORDINATION_PLAN + pi-dynamic-assessment-handoff dangling files marked external/from-prior-pi-repo (confirmed missing via `ls`); PLAN-pi-dynamic-seam B6 heartbeat row marked future (only `rally_wake.py` present); `rally_wake.py` docstring now notes its relationship to `rally inject`.
- **Lead audit:** ⚠️ pending — lead audits post-hoc from this evidence.
- **Blast radius / reversibility:** docs + `examples/manifest.toml` + `scripts/rally_wake.py` docstring only. No `crates/**`, no behavior change. Revert: `git revert <hash>`.

## Backfill — this session's notable landings (summary level)

- **`8a74f60`** — consolidate the divergent rally lines into one `main` (lean line won; attuned line removed). Context: two structurally divergent architecture lines existed; `main` is now the single canonical superset. Evidence: integration was 100% merged. Audit: ✅ (the line we built on).
- **`6d76780`…`d52a184`, `e1589dd`, `ba1f93c`** — **Rally Flow** (dynamic-workflows module: L1 protocol/lint/skills + L6 route.mjs/workstream-status resume). Context: agent-rally-point's take on dynamic workflows, host-side protocol + guardrail + durable resume. Evidence: 35 module tests green. Audit: ✅ lead-built + verified. Reversibility: net-new `dynamic-workflows/` dir.
- **`12468f9`,`6ec1042`,`e7b87aa`,`3bdc040`,`c72328f`/`d39aab7`,`4e1a329`,`ea6aee9`** — **rally-cli hardening** (audit HIGHs B4–B9 + B8). Context: the scaled-audit + A/B surfaced real correctness/security bugs (gate bypass, NUL panics, index clobber, cli guardrails, lib residuals). Evidence: `cargo test`/`clippy -D warnings`/`fmt` green per commit. Audit: ✅ lead source-verified each (caught dropped B5/B7, re-delegated). Codex lane.
- **L4/PR46** — *pending* — Claude #2 merges `claude2/l4-audit-folds` → `main` itself (first formal entry under the delegated-merge rule). Will document: contract-claims + receipts + check-ci + folded B7-store/B8-next findings; evidence `cargo test`/clippy/fmt; lead audits post-hoc.
