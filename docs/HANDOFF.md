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

## What The Current Branch Adds

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
- Adds `rally packet --tool <agent> --json`: a read-only, role-shaped work
  brief derived from `ContextBrief.attuned_items`.
- Adds machine-readable recommendation trust assessment so agents can see
  whether a recommended action satisfies its automation threshold.
- Adds a strict `rally herdr inject` trust gate that surfaces handoff trust and
  requires `--force` for unsigned/untrusted input.
- Adds `rally adapter contract`, `rally cmux packet`, and `rally herdr packet`
  as side-effect-free adapter JSON surfaces over work packets.
- Adds rebuildable `rally.checkpoint.json` hot-read cache support plus
  `rally checkpoint status|rebuild`.
- Adds `rally <tool>` and `rally start <tool>` as the canonical agent startup
  surface, returning preflight/context/packet/checkpoint/cursor state and next
  watch command as JSON by default.
- Adds `rally doctor` and `rally setup` for agent-product readiness: harness
  discovery, enforcement mode, adapter install notes, anonymous coordination
  detection, and startup health findings.
- Extends `rally setup install <cmux|herdr>` to install wrapper scripts and patch
  harness-local config files, while keeping all Rally state in the channel.
- Enforces `setup enforcement strict` on write commands by rejecting new
  anonymous tool/from_tool/owner_tool writes.
- Adds initial formal JSON schema files in `docs/schemas/` and a dogfood report
  in `docs/DOGFOOD_REPORT.md`.
- Keeps urgent coordination obligations ahead of specialization:
  - required handoffs
  - active tasks
  - blockers
  - claim conflicts

## Important Files

- `crates/rally-core/src/context.rs`: role-aware ranking, recommendations, and
  work packet shaping.
- `crates/rally-core/src/event.rs`: profile payload includes `role`.
- `crates/rally-core/src/query.rs`: projected agent profile includes `role`.
- `crates/rally-cli/src/args.rs`: accepts `--role`.
- `crates/rally-cli/src/args/commands.rs`: parses profile role.
- `crates/rally-cli/src/write_commands.rs`: writes profile role.
- `crates/rally-core/src/store.rs`: checkpoint cache read/rebuild/status.
- `crates/rally-cli/src/query_commands.rs`: renders context, packet,
  checkpoint, adapter, cmux, and Herdr outputs.
- `crates/rally-core/tests/kernel.rs`: reviewer specialization and packet tests.
- `crates/rally-cli/tests/verify.rs`: CLI profile, packet, and Herdr gate coverage.
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

The next clean slice is hardening and documentation around packet consumption:

- add more fixture-style JSON contract coverage as packet fields stabilize;
- dogfood packet output from the Rally skill before installing `rally` globally;
- keep packet shaping read-only and derived from context, not scheduler-y.
