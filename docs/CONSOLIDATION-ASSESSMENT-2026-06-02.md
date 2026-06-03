<!--
SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Rally Point consolidation assessment - 2026-06-02

## Outcome

The one-folder consolidation is partially complete, but not ready to declare
finished.

The right end state is:

```text
/Users/tyroneross/dev/git-folder/agent-rally-point
```

as the single local folder for the Agent Rally Point product, with retired or
reference material preserved inside that folder under `archive/` or `tools/`.
Project-specific rally ledgers should not be centralized into this repo.
Rally's communication source of truth remains the owning repo's
`.rally/log/<engagement>.jsonl`.

The immediate blocker is repository divergence: local `main` is ahead of and
behind GitHub `main`. Do not push, delete sibling worktrees, or force-clean
archives until one terminal owns reconciliation.

## Current assessment

### Git state

| Item | Current state | Assessment |
|---|---|---|
| Canonical checkout | `/Users/tyroneross/dev/git-folder/agent-rally-point` | Correct folder. Keep this as the only product checkout. |
| Local `main` | `d709435` | Contains local-only merge/security/fleet work. |
| GitHub `main` | `500e42f` | `git ls-remote origin refs/heads/main` also reports `500e42f`; remote state is current. |
| Divergence | `main...origin/main [ahead 11, behind 9]` | This must be reconciled before any consolidation push. |
| Remote | `https://github.com/tyroneross/agent-rally-point.git` | Correct upstream. |

Current untracked files in the canonical checkout:

```text
.rally/log/test.jsonl
archive/bundles/herdr-lane-2026-06-01.bundle
archive/bundles/planf-functional-core-herdr-removal-20260602-101722.bundle
archive/bundles/planf-phases-p1-p2-p4-2026-06-02.bundle
archive/herdr-fix-and-harness-20260601T214555.bundle
```

The untracked `.rally/log/test.jsonl` is significant because `rally whoami`
currently reports `repo_id: test` even though `.rally/manifest.json` correctly
declares `"repo": "agent-rally-point"`. Treat that file as a cleanup/reconcile
item, not as a committed product artifact unless the lead confirms it is a real
engagement segment.

### Local folders and worktrees

| Folder | Current state | Should be | Why |
|---|---|---|---|
| `/Users/tyroneross/dev/git-folder/agent-rally-point` | Main worktree, branch `main`, HEAD `d709435` | Canonical product folder | This is the repo named by the consolidation rule and docs. |
| `/Users/tyroneross/dev/git-folder/agent-rally-point-cockpit` | Linked worktree, branch `feat/agent-cockpit`, clean | Temporary lane until merged or archived | Cockpit source belongs to Rally if it is part of the product, but the sibling folder should not remain permanent. |
| `/Users/tyroneross/dev/git-folder/agent-rally-point-sec-harden` | Linked worktree, branch `bl/security-harden-control-plane`, one untracked bundle | Temporary lane until branch is merged or archived | Security work may be product work, but it should land through the canonical repo and then the worktree can be retired. |
| `/Users/tyroneross/dev/git-folder/agent-rally-point-wt-runiso` | Linked worktree, branch `feat/rally-run-worktree-isolation`, clean | Temporary lane until merged or archived | Run isolation is product work; the lane should not remain as an extra product folder after merge. |
| `/Users/tyroneross/dev/git-folder/agent-rally-watcher` | Missing | Correct to be absent if fully imported | Watcher now lives under `tools/agent-rally-watcher/` as legacy reference. |
| `/Users/tyroneross/dev/git-folder/agent-rally-point-lane-a` | Missing | Correct to be absent if archived | No current folder to reconcile. |
| `/Users/tyroneross/dev/git-folder/agent-builder` | Separate repo | Remain separate | User explicitly excluded it from Rally consolidation. |

## Key files: where they are and where they should be

| File or area | Current location | Should be | Reason |
|---|---|---|---|
| Agent entry pointer | `AGENTS.md`, `CLAUDE.md` | Stay at repo root | Agents land here first; both contain the rally pointer block. |
| Quick guide | `RALLY.md` | Stay at repo root | This is the short human/agent entry guide. |
| Machine-readable rally manifest | `.rally/manifest.json` | Stay committed under `.rally/manifest.json` | It identifies docs, ledger path, and pointer markers for agents and tools. |
| Canonical ledger | `.rally/log/*.jsonl` | Stay repo-local under `.rally/log/` | This is the communication source of truth for this repo. |
| Rally archive ledger | `.rally/archive/**` | Stay repo-local under `.rally/archive/` | Rotated segments remain replayable and should not move to a global store. |
| Derived rally cache | `.rally/facts.db`, `.rally/cursors.json`, `.rally/log/index.json` | Stay gitignored/rebuildable | These are projections and fast paths, not canonical data. |
| Doctrine | `dynamic-workflows/COORDINATION.md` | Stay under `dynamic-workflows/` | This is the deeper coordination contract referenced by the entry docs. |
| Protocol | `dynamic-workflows/PROTOCOL.md` | Stay under `dynamic-workflows/` | This is the wire/descriptor contract. |
| Model tiers | `dynamic-workflows/MODEL-TIERS.md` | Stay under `dynamic-workflows/`; references should use this path or a relative path from that folder | It now exists in the module where `COORDINATION.md` expects it. Do not add a root duplicate unless needed as a compatibility shim. |
| Board/current lanes | `docs/ORCHESTRATION.md` | Stay under `docs/` | This is the operating board and current work map. |
| Handoff/launch guide | `docs/HANDOFFS-AND-LAUNCHING-AGENTS.md` | Stay under `docs/` | This tells leaders and workers how to launch, inject, and hand off. |
| Lead behavior spec | `docs/SPEC-lead-agent.md` | Stay under `docs/` and be linked from leader-facing docs | This is how the leader should coordinate: authority, dispatch, checks, and escalation. |
| Canonical checkout migration | `docs/CANONICAL-CHECKOUT-MIGRATION.md` | Stay under `docs/` | This is the one-folder migration policy and active-rally cutover rule. |
| Watch autonomy spec | `docs/SPEC-rally-watch-autonomy.md` | Stay under `docs/` | This is the target native watcher design. |
| Watcher legacy reference | `tools/agent-rally-watcher/` | Stay under `tools/` until `rally watch` reaches parity, then retire | It is no longer a standalone sibling repo; it is a preserved reference implementation. |
| Watcher migration note | `tools/agent-rally-watcher/MIGRATION.md` | Stay with the vendored watcher | It explains lineage, target replacement, and validation. |
| Watcher history bundle | `archive/bundles/agent-rally-watcher.bundle` | Stay under `archive/bundles/` | This preserves pre-import history without keeping a live sibling folder. |
| Other repo bundles | `archive/bundles/*.bundle` | Keep under `archive/bundles/` if intentionally preserved | Bundles are the right archive mechanism; review untracked bundles before committing. |
| Stray archive bundle | `archive/herdr-fix-and-harness-20260601T214555.bundle` | Move under `archive/bundles/` or remove after verification | Archive bundles should have one predictable location. |

## Current connections and where they should be

| Connection | Current state | Should be | Why |
|---|---|---|---|
| GitHub | `origin` points to `https://github.com/tyroneross/agent-rally-point.git`; local `main` diverged from `origin/main` | Reconcile local and remote before push | A forced or blind push could drop either the local 11 commits or the remote 9 commits. |
| Worktrees | Cockpit, security hardening, and run-isolation are linked worktrees from the same repo | Keep only while active; merge/archive one lane at a time | Worktrees are coordination lanes, not permanent product folders. |
| Repo communication | `.rally/log/<engagement>.jsonl` | Remain the canonical communication store | It is committed, append-only, and replayable. This is Rally's core design. |
| Cross-repo discovery | `~/.agent-rally-point/rooms/v1/index.json` | Pointer-only and opt-in via `RALLY_GLOBAL_INDEX=1` | It should help locate rooms, not become a second fact store. Code already treats this as opt-in; README wording should be updated to match. |
| Legacy global apps store | `~/.agent-rally-point/apps/<slug>/changes.jsonl` | Migration-only via `rally migrate-legacy`; no normal writes | This is the fragmentation source and should not be used for active coordination. |
| Native watcher | `crates/rally-cli/src/lib.rs` has `rally watch` code/spec surfaces | Make this the active watch/dispatch path | It reads repo-local `.rally/log` and avoids the legacy global apps store. |
| Python watcher | `tools/agent-rally-watcher/` still watches legacy `~/.agent-rally-point/apps/<app>/changes.jsonl` | Keep as reference only until native parity; do not use as active product path | It documents useful dispatch behavior but points at the wrong canonical store. |
| Build Loop embedded bridge | Local Build Loop still contains `scripts/rally_point/**`, `scripts/agent_rally.py`, `coordination_status.py`, and `coordination_rally.py` with legacy fallback references | Build Loop should resolve to the owning repo and shell to native `rally` for writes, or use a bridge that writes repo-local `.rally/log` | This prevents Build Loop-managed agents from becoming invisible to native-rally agents. |
| Easy Terminal | Easy Terminal docs require ET work to coordinate from `/Users/tyroneross/dev/git-folder/easy-terminal/.rally` | Keep ET coordination in Easy Terminal's repo, even when editing Rally or ptyd as support work | The owner of the work owns the ledger. Do not centralize project facts into Agent Rally Point. |
| Host runtime | `rally whoami` sees Easy Terminal's ptyd socket and `under_ptyd: true` | Treat as delivery/runtime context, not source-of-truth storage | ptyd can inject/capture sessions; it should not determine where facts are stored. |

## Recommendations

1. Assign one terminal as reconciliation owner before any push.

   Local `main` has local-only commits while GitHub has remote-only commits.
   The correct next action is to inspect both sides, choose merge or rebase,
   validate, then push. Do not let two terminals rewrite `main` concurrently.

2. Keep the one-folder product boundary strict.

   The only permanent local product folder should be
   `/Users/tyroneross/dev/git-folder/agent-rally-point`. Worktrees may exist
   during active work, but after merge they should be removed or archived.
   Historical source should be preserved with bundles in `archive/bundles/`.

3. Treat `tools/agent-rally-watcher/` as a legacy reference, not a second
   product.

   The import is in the right place. Its value is tests, dispatch behavior, and
   launchd/cursor design. The active future should be `rally watch` in the Rust
   CLI because it reads `.rally/log` and ships with the single rally binary.

4. Finish the Build Loop bridge migration separately and explicitly.

   Build Loop still has embedded Rally Point Python surfaces and legacy fallback
   references. That external bridge should stop writing
   `~/.agent-rally-point/apps/...` during active coordination. It should resolve
   the owning repo, then call native `rally` or a native-compatible writer.

5. Normalize docs around the global discovery index.

   `docs/RALLY_ARCHITECTURE.md` and code say the global room index is off by
   default and opt-in via `RALLY_GLOBAL_INDEX=1`. `README.md` still phrases it
   as a normal global hint disabled by `RALLY_NO_GLOBAL_INDEX=1`. Update README
   so agents do not infer that global discovery is always on.

6. Clean the untracked `test` engagement after coordination.

   `.rally/log/test.jsonl` is probably why `rally whoami` reports
   `repo_id: test`. Do not delete it blindly during an active run. Once the active
   terminal confirms it is only smoke-test residue, remove it or migrate it
   into a correctly named engagement segment.

7. Keep leader coordination discoverable from the first screen.

   `AGENTS.md` and `CLAUDE.md` already point agents to `RALLY.md`,
   `dynamic-workflows/COORDINATION.md`, `dynamic-workflows/PROTOCOL.md`, and
   `docs/ORCHESTRATION.md`. Add or strengthen a leader-facing pointer from the
   entry docs or `docs/ORCHESTRATION.md` to `docs/SPEC-lead-agent.md` and
   `docs/HANDOFFS-AND-LAUNCHING-AGENTS.md` so a lead knows how to dispatch,
   inject, capture, and close handoffs without rediscovering the process.

## Migration approach

Use this sequence for consolidation work:

1. Freeze destructive cleanup until the active terminal finishes its current
   run or posts a handoff.
2. Reconcile `main` with `origin/main` and run validation before pushing.
3. Review untracked bundles; move intentional bundles under
   `archive/bundles/`, then commit them or remove them.
4. For each linked worktree, decide one path: merge to `main`, keep as active
   lane, or bundle and remove the worktree.
5. Keep `tools/agent-rally-watcher/` until native `rally watch` has parity:
   per-consumer filtering, dispatch hooks, structured stream output, and
   launchd/systemd install output.
6. Patch Build Loop's bridge in the Build Loop repo/plugin so active writes go
   to the owning repo's `.rally/log`, not the legacy global apps store.
7. Post a final Rally decision that identifies this folder as the Agent Rally
   Point product folder and names external project ledgers as repo-owned.

## Final target shape

```text
/Users/tyroneross/dev/git-folder/agent-rally-point/
  AGENTS.md
  CLAUDE.md
  RALLY.md
  crates/
    rally-cli/
    rally-protocol/
  dynamic-workflows/
    COORDINATION.md
    MODEL-TIERS.md
    PROTOCOL.md
  docs/
    ORCHESTRATION.md
    HANDOFFS-AND-LAUNCHING-AGENTS.md
    SPEC-lead-agent.md
    SPEC-rally-watch-autonomy.md
    CANONICAL-CHECKOUT-MIGRATION.md
    RALLY_ARCHITECTURE.md
  tools/
    agent-rally-watcher/      # legacy reference until rally watch parity
  archive/
    bundles/                  # preserved histories and retired lanes
  .rally/
    manifest.json
    log/*.jsonl               # canonical facts for this repo only
    archive/**                # replayable rotated facts
```

The expected outcome is one Rally product folder, no live sibling product repos,
no hidden second communication store, and a clear rule that each project keeps
its own rally ledger while Agent Rally Point provides the common CLI,
protocol, docs, migration tools, and watcher behavior.
