# Worktree closeout projection

`rally worktree closeout --json` returns read-only evidence for a closeout
decision. It does not remove a checkout, branch, or Rally fact. The envelope
schema is `agent-rally.command.worktree-closeout.v1`; the payload is
`data.worktree_closeout`.

```json
{
  "schema_version": 1,
  "repository": {"root": "/canonical/repo", "git_common_dir": "/canonical/repo/.git"},
  "generated_at": "2026-09-27T00:00:00Z",
  "sources": {"git": "current", "rally": "current"},
  "worktrees": [{
    "path": "/canonical/repo/.rally/worktrees/task",
    "head": "<git-commit>",
    "branch": "refs/heads/rally/task",
    "git_status": "clean",
    "branch_state": "attached",
    "merge_target": {"ref": "main", "commit": "<creation-tip>", "status": "unmerged"},
    "ownership": {
      "status": "live_owner", "source": "rally_managed_session",
      "session_id": "task", "tool": "codex:task", "liveness": "live",
      "claims_status": "active", "claim_count": 1, "availability": "current"
    },
    "disposition": {"action": "retain", "reasons": ["merge_status_unproven"]}
  }]
}
```

The example illustrates field shape; `disposition.reasons` is computed from
the actual row and may differ from the shown values. Paths are canonicalized
when they exist. `branch` is the full Git ref. `head` is null for a session
path missing from Git's worktree list. `branch_state` is `attached`,
`detached`, `mismatch`, or `missing`; `git_status` comes from Git's existing
worktree status check, or `missing` for an unregistered session path.

Rally records `merge_target.ref` and its `commit` at worktree creation. The
commit is provenance; mergedness checks the current tip of that **recorded
ref**, independent of which branch is currently checked out in the canonical
workspace. Legacy sessions lack this evidence and report `unknown`. The
possible `merge_target.status` values are `merged`, `unmerged`, and `unknown`.

Ownership matches the exact canonical worktree path to active managed-session
records. A single match must also agree on branch. A live match yields
`live_owner`; a verified empty active-session set yields
`no_live_owner_observed`. Unavailable, stale, unknown, conflicting, or
branch-mismatched evidence yields `unknown`. The `availability` field is
`current`, `stale`, `unavailable`, or `ambiguous`. Claim counts join on both
exact `tool` and `from_session_id`; shared tool labels alone cannot join.
If Git and Rally disagree about the path for the same branch, both paths are
`ambiguous`. The Rally snapshot reads canonical JSONL segments without opening
or reconciling the room. A room with only derived SQLite or legacy facts is
`unavailable` until an explicit maintenance operation restores canonical
segments.

`disposition.action` is `retain` or `review_removal`. Even
`review_removal` only invites a separate cleanup review. The CLI never turns
absence of a managed session into proof that an unrelated process or external
agent has stopped. Consumers must re-read Git and ownership immediately before
removing anything.

Implicit `rally stop`, task completion, and session reaping now retain dirty,
unmerged, and merge-unknown worktrees. They also retain any worktree that holds
gitignored files (reason `ignored_files`), because agents keep local memory,
run state and env files there; the warning lists those paths for review, and
Rally never deletes them. Stop confirms backend death before any
source cleanup. `rally worktree gc --apply` requires an available exact
managed-session ledger, no active owner, a clean checkout, and a merged branch;
its report marks `reaped` only after removal succeeds. Worktree paths remain
repo-local.
