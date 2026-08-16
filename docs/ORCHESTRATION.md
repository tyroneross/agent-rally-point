# Room and Board Projections

Rally derives the current coordination view from the room ledger. It does not
use a hand-maintained project plan as coordination truth.

Use these read-only commands to understand a busy repository:

```bash
rally room --json
rally next --tool <you> --json
rally board --json
```

- `room` returns current presence, claims, handoffs, blockers, decisions, and
  recent artifacts.
- `next` returns the highest-priority action for one agent after applying the
  current room rules and wake state.
- `board` projects lanes and work status from recorded facts without changing
  the ledger or writing a Markdown board.

The append-only `.rally/log/<engagement>.jsonl` record is canonical. Database,
dashboard, room, and board views are derived projections and can be rebuilt.
An acknowledgement proves that an agent consumed a coordination request; it
does not prove that the requested work was completed or accepted.

See [Command Semantics](COMMAND-SEMANTICS.md), [Agent State Model](AGENT-STATE-MODEL.md),
and [Handoffs and Launching Agents](HANDOFFS-AND-LAUNCHING-AGENTS.md) for the
underlying contracts.
