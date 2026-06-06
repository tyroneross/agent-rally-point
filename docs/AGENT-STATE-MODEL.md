# Agent-State Coordination Model

A first-class, **liveness-aware** view of every agent currently coordinating in a Rally room.

## Why

Pre-this-PR Rally tracked agents via:

- `squads[]` — derived from "the latest fact of any kind per tool" (signals *someone showed up*, but nothing about what they're doing).
- `presence` facts whose `subject` followed a loose convention like `state=working | file=X | intent=Y` (used in practice, but unparsed and unprojected — a host reading the room couldn't query "who's working on `crates/rally-cli`?" without grepping subjects).
- `claim` / `release` / `resolve` facts (the ownership graph).
- A 15-minute "idle" threshold (`squad.status`).

Three concrete gaps surfaced as **lesson seq 1603**:

1. `rally say release` was easy to invoke as a silent no-op when the caller had `--path` but no `--ref`.
2. Claims never auto-expired by liveness. `codex:consolidation-01` carried an open `owns crates` claim 2 days after the session was gone.
3. There was no unified vocabulary for *what an agent is currently doing* — and no signal for *"my commit landed on a managed worktree branch, I'm done."*

This PR closes (1)–(3) with the smallest additive change that doesn't break the live fleet.

## Vocabulary

Each tool has at most one current `AgentState`:

| State | Required markers | Optional markers | Meaning |
|---|---|---|---|
| `idle` | — | `wake_after=<iso>` | Alive but not actively working. Optional wake hint (mirrors the `Standby` marker grammar). |
| `working` | `file=<path>` `intent=<one-line>` | — | Actively working on a file. The natural unit for path-overlap reasoning. |
| `blocked` | `ref=<event-id>` | — | Waiting on a fact (a blocker, a handoff, an unresolved risk). The `ref` is the actionable next step. |
| `done` | `committed_sha=<sha>` `worktree_branch=<branch>` | — | A commit landed on a managed worktree branch. **The done producer seam.** |
| `unknown` | — | — | Subject carried `state=<x>` for an unrecognised `x`. Surfaced so an older binary writer doesn't disappear from the board. |

## Marker grammar

Markers live in the presence fact's `subject` (and, for `done` only, optionally in the `summary`). Format: ` | `-separated `key=value` pairs.

```
state=working | file=crates/rally-cli | intent=agent-state model
state=blocked | ref=fact_1c63_18b6003369c3da28
state=idle | wake_after=2026-06-04T23:00:00Z
state=done | committed_sha=abc123def456 | worktree_branch=feat/agent-state
```

Parser rules (see `agent_state::parse_marker_string`):

- Whitespace around `|` and `=` is trimmed.
- A segment without `=` is recorded as a flag with empty value (lets the projection distinguish "marker absent" from "marker explicitly empty").
- Duplicate keys: last value wins (matches host-consumer reading-order intuition).
- Empty / whitespace-only string → empty map (no projection).

## Why markers, not a typed `Fact` column

`Standby` (`reason:`, `wake_after:`), `Mission` (`role:lead`), and `Presence` (`build_id:`) already use this additive-marker pattern. Reusing it:

- avoids a wire-schema bump that would require every in-flight binary to migrate before consumers can reason over the field (the fleet has logged ≥4 `binary-drift` risks recently);
- is forward-compat — a later PR can promote `state` to a typed `Fact` column and `project_agent_states` continues reading both shapes;
- keeps the writer path one `append_fact_verified` call (consistent with the rest of `rally say`).

## Projection

`agent_state::project_agent_states(facts, now_ts) -> Vec<AgentStateEntry>`:

1. Filter to `FactKind::Presence` facts only. A claim or artifact that *looks* like a presence subject must not be projected (a `claim` carries its meaning in different fields).
2. Group by `tool` (the reserved system author `"rally"` is excluded).
3. Keep the highest-`seq` presence per tool.
4. Parse markers via `parse_marker_string`. Missing `state=` marker → `AgentState::Idle{wake_after: None}` (back-compat with the legacy `agent presence: <tool>` shape `ensure_presence_tiered` writes).
5. Compute `stale = (now_ts - last_seen_ts) > IDLE_THRESHOLD_SECS` (15 minutes). Unparseable timestamps → not stale (conservative; matches the existing `Squad` projection).

The projection is pure — no I/O. Caller passes `now_ts` (RFC3339) explicitly so tests can pin the liveness clock.

## Surfaces

### `rally status post`

Writes ONE typed-heartbeat `presence` fact. Always append-only — never overwrites a prior heartbeat. Validates state-specific required args upfront (no silent malformed writes).

```bash
rally status post --tool claude_code:bl-agentstate --state working \
  --file crates/rally-cli --intent agent-state model

rally status post --tool codex:consolidation-01 --state idle \
  --wake-after 2026-06-04T23:00:00Z

rally status post --tool claude_code:bl-agentstate --state blocked \
  --blocked-ref fact_1c63_18b6003369c3da28

# Agent-agnostic done signal. If omitted, git metadata is inferred from the
# current checkout; explicit flags remain authoritative.
rally status post --tool codex:bl-committed --state done \
  --committed-sha abc123def456 --worktree-branch feat/agent-state

rally status post --tool claude_code:build-loop --state done
```

Validation rules:

| State | Required inputs | Error if unavailable |
|---|---|---|
| `idle` | — | — |
| `working` | `--file` `--intent` | "rally status post --state working requires --file <path> and --intent <one-line>" |
| `blocked` | `--blocked-ref` | "rally status post --state blocked requires --blocked-ref <event-id>" |
| `done` | `committed_sha` + `worktree_branch`, either explicit or inferred from git | "rally status post --state done could not auto-detect <field>; pass --<flag> explicitly" |

### `rally status read [--tool T]`

Reads the latest-per-tool typed state from the ledger. Returns `Vec<AgentStateEntry>`. With `--tool`, filters to one tool.

### `rally status --global`

Unchanged — multi-repo discovery aggregation, preserved as a flag for back-compat.

### `rally board --json` extensions

```jsonc
{
  "board": {
    "lanes":   [/* unchanged */],
    "backlog": {/* unchanged */},
    "delta":   [/* unchanged */],
    "max_seq": 1620,
    // NEW (additive, omitted when empty):
    "agent_states": [
      {
        "tool": "claude_code:bl-agentstate",
        "state": "working",
        "file":  "crates/rally-cli",
        "intent":"agent-state model",
        "last_seen_seq": 1617,
        "last_seen_ts":  "2026-06-04T22:14:00Z",
        "stale": false
      },
      {
        "tool": "codex:consolidation-01",
        "state": "working",
        "file":  "crates",
        "intent":"consolidation",
        "last_seen_seq": 1432,
        "last_seen_ts":  "2026-06-02T10:00:00Z",
        "stale": true
      }
    ],
    "auto_releasable_claims": [
      {
        "status": "in_flight",
        "owner":  "codex:consolidation-01",
        "subject":"owns crates",
        "event_id":"fact_1432_…",
        "seq": 1432,
        "scope":["file:crates"]
      }
    ]
  }
}
```

### Liveness gating

A claim is **auto-releasable** iff:

- it is currently `LaneStatus::InFlight` (not closed or landed-unverified), AND
- its owner is in the stale set (`last_seen_ts` older than `IDLE_THRESHOLD_SECS`).

Per the charter, Rally **records and surfaces** — it never auto-executes the release. The host (or a lead) reads `auto_releasable_claims[]` and decides. The natural follow-on is `rally say release --tool <lead> --path <path>` to action the auto-release surface (now safe — see "Release fix" below).

## Release fix (`rally say release`)

Pre-this-PR: `rally say release --tool T --path P` (no `--ref`) erred at the `append_state_transition_verified` layer with `release requires --ref <event-id>`. The error was correct but unactionable — the operator had to find the event_id manually and retry.

Post-this-PR: that exact invocation **resolves to the calling tool's own active claims overlapping any of the paths**, and emits ONE release fact whose `ref_id` is the first match. The projection's released-scopes filter already closes every claim whose scope overlaps the release's scope, so a single release fact correctly closes all matched claims; per-claim audit lives in the response's `warnings[]`.

Loud-error rules:

- No path-only match found → error names the tool's currently-open claims so the operator's next step is in hand.
- Neither `--ref` nor `--path` provided → unchanged ("release requires --ref").

## Done Producer Seam

The `done` state is authored by whichever agent owns the committing lane. In the original split this was Codex; the contract is now shared by Codex, Claude Code, and any other Rally participant:

1. Finish work on a managed worktree or checkout.
2. Emit:
   ```bash
   rally status post --tool <tool-id> --state done
   ```
   The CLI resolves `committed_sha` from `git rev-parse --verify HEAD` and `worktree_branch` from `git symbolic-ref --quiet --short HEAD`.
3. If the checkout is detached, has no commit, or git is unavailable, pass explicit values:
   ```bash
   rally status post --tool <tool-id> --state done \
     --committed-sha <sha> --worktree-branch <branch>
   ```

Consumer responsibility:

- Consume the fact unchanged in `project_agent_states` → `AgentState::Done { committed_sha, worktree_branch }`.
- Surface in `rally board --json` `.board.agent_states[]`.

Schema lock (unit-tested in `agent_state::tests::project_agent_states_recognises_done_from_synthetic_codex_fact`):

- `subject`: `state=done | committed_sha=<sha> | worktree_branch=<branch>`
- OR `subject`: `state=done`, with the markers in `summary` (subject-wins-summary-fills tolerance).

A future revision can promote `committed_sha` + `worktree_branch` to typed `Fact` fields — the projection accepts both shapes.

## Stability + forward compat

- All new `BoardOutput` fields are `#[serde(skip_serializing_if = "Vec::is_empty")]` + `default` — a host that ignores them sees the legacy wire shape unchanged.
- `AgentState` is a tagged enum (`#[serde(tag = "state", rename_all = "snake_case")]`) — adding a new variant is forward-compat only for variants the host accepts; an old host parsing a new variant sees `AgentState::Unknown { raw }`.
- The `agent_state` module exposes nothing publicly outside the crate (`pub(crate)`); the JSON surface is the only stable contract.

## Open follow-ons (out of scope)

- Auto-release of stale claims (operator/lead triggered; surfaced not actioned).
- Promoting `state` to a typed `Fact` column once the fleet has migrated.
- Wake-on-done: when a tool transitions to `done`, optionally trigger waiting downstream tools' `wake_after` if their `Standby` fact's `ref_id` named the done-er's worktree branch.

## File map

| File | Role |
|---|---|
| `crates/rally-cli/src/agent_state.rs` | Vocabulary, marker parser, projection. Pure functions. |
| `crates/rally-cli/src/board.rs` | Adds `agent_states[]` + `auto_releasable_claims[]` to `BoardOutput`. |
| `crates/rally-cli/src/cli.rs` | `rally status post` + `rally status read` parsers; `--global` preserved. |
| `crates/rally-cli/src/lib.rs` | `command_status_post`, `command_status_read`, `command_release_by_path`. |
| `docs/AGENT-STATE-MODEL.md` | This file — the vocabulary contract + the done producer seam. |
