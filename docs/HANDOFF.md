<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Handoff

## Current Branch

- Branch: `codex/rally-role-specialization`
- PR: https://github.com/tyroneross/agent-rally-point/pull/40
- Status: open, ready for review
- Base: stacked on the Rust greenfield PR series through #39

## PR Stack

- #34 `refactor(rust): project query state for attuned context`
- #35 `feat(rust): add agent context brief`
- #36 `feat(rust): add attuned coordination facts`
- #37 `docs: add Rally agent coordination skill`
- #38 `test(rust): add agent user journeys`
- #39 `feat(rust): rank attuned context for agents`
- #40 `feat(rust): specialize context by agent role`

## What #40 Adds

- Adds optional `role` to agent profiles.
- Wires `rally profile --role <name>` through the CLI.
- Supports role-aware context specialization for:
  - `reviewer`
  - `architect`
  - `builder`
  - `qa`
- Infers lightweight specialization from capabilities when no explicit role is
  declared, for example `review`, `architecture`, `qa`, or `implementation`.
- Uses specialization to shape `attuned_items` scoring.
- Adds advisory role-specific recommendations such as `review_artifact`.
- Keeps urgent coordination obligations ahead of specialization:
  - required handoffs
  - active tasks
  - blockers
  - claim conflicts

## Important Files

- `crates/rally-core/src/context.rs`: role-aware ranking and recommendations.
- `crates/rally-core/src/event.rs`: profile payload includes `role`.
- `crates/rally-core/src/query.rs`: projected agent profile includes `role`.
- `crates/rally-cli/src/args.rs`: accepts `--role`.
- `crates/rally-cli/src/args/commands.rs`: parses profile role.
- `crates/rally-cli/src/write_commands.rs`: writes profile role.
- `crates/rally-core/tests/kernel.rs`: reviewer specialization test.
- `crates/rally-cli/tests/verify.rs`: CLI profile role coverage.
- `docs/CONTEXT_BRIEF_SCHEMA.md`: context contract updates.
- `skills/agent-rally-point/SKILL.md`: agent skill guidance for roles.

## Verification

The branch was verified with:

```bash
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
git diff --check
```

The commit hook also ran `cargo test` successfully during commit.

## Product Intent

Rally should stay agent-first without becoming a heavyweight scheduler or
role/task framework. Roles are declared, inspectable coordination facts that
shape context and recommendations. They are not hidden prompt magic and do not
override urgent coordination state.

The desired behavior is:

- Reviewer agents see review packets, decisions, lessons, and artifacts rise.
- Architect agents see decisions and durable design knowledge rise.
- Builder agents see tasks and claims rise.
- QA agents see verification artifacts and lessons rise.
- All agents still handle handoffs, blockers, and conflicts first.

## Next Good PR

The next clean slice is a compact "review packet" output or command surface:

- produce a bounded review-oriented context packet from `attuned_items`;
- include changed files, artifacts, decisions, test evidence, and trust labels;
- keep it read-only and source-linked;
- avoid adding orchestration or scheduler behavior.

That would make role-aware specialization more directly usable by reviewer
agents without bloating #40.
