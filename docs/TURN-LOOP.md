# Rally Turn-Loop Contract

Rally coordinates a turn; it does not execute the turn. The coding host owns
the edit, test, review, and any decision that needs human authority.

```text
whoami -> enter -> ack -> next -> claim -> check before-write -> edit
       -> verify -> say artifact|handoff|resolve|release -> next
```

Stop rather than inventing work when `next` reports `actionable: false` or
`requires_human: true`.

## Record boundary

`.rally/log/<engagement>.jsonl` is the canonical append-only coordination
record. `facts.db` is derived from it. The table distinguishes a durable fact
from a read or from work that Rally deliberately leaves to the host.

| Step | Command shape | Durable coordination effect |
|------|---------------|-----------------------------|
| Self-locate | `rally whoami --tool <unique-tool> --json` | Reads local room/host state; does not append a durable fact. Stop if `host_runtime.ambiguous` is true. |
| Join | `rally enter --tool <unique-tool> --json` | Writes presence for the session. |
| Acknowledge | `rally ack --tool <unique-tool>` | Writes acknowledgement of the room's rules, lead, and mission. |
| Ask | `rally next --tool <unique-tool> --json` | Reads the current room and records the wake intent for the check. Treat its result as a recommendation, not an execution order. |
| Reserve | `rally say claim --tool <unique-tool> --path <path> ... --json` | Appends a scoped claim before the host changes shared work. Save its returned event ID. A claim can cover files, services, ports, branches, tasks, and other supported resources. |
| Check | `rally check before-write --tool <unique-tool> --path <path> --strict --json` | Reads overlapping file claims. Default mode advises; `--strict` returns exit 4 on a stop finding so the calling harness can abort the edit. |
| Edit | host editor/tool | No Rally fact. The host performs the change. |
| Verify | host test/build/review | No Rally fact. The host determines the evidence. |
| Record outcome | `rally say artifact|handoff|resolve|release ... --json` | Appends a durable fact for peers and a resumed session. A successful `inject` is delivery only; the receiver's own acknowledgement proves receipt. |

## Minimum safe manual loop

Use a distinct tool ID for every concurrent terminal. The commands below show a
file change; replace the path and subject with the work actually being done.

```bash
rally whoami --tool codex:parser-01 --json
rally enter --tool codex:parser-01 --json
rally ack --tool codex:parser-01
rally next --tool codex:parser-01 --json
# Save the claim response's event_id as <claim-id>.
rally say claim --tool codex:parser-01 --subject "edit parser" \
  --path crates/rally-cli/src/main.rs --json
rally check before-write --tool codex:parser-01 \
  --path crates/rally-cli/src/main.rs --strict --json
# The host edits and verifies here.
rally say artifact --tool codex:parser-01 --subject "parser hardened" \
  --uri crates/rally-cli/src/main.rs --evidence "cargo test" --json
rally say release --tool codex:parser-01 --ref <claim-id> \
  --subject "parser lane complete" --json
rally next --tool codex:parser-01 --json
```

## If the boundary blocks the edit

When `rally check before-write --strict` returns exit 4, do not edit. Read the
finding, coordinate with the current holder, and keep the claim only while this
lane still needs the resource. If you stop or switch lanes, record
`rally say release --ref <claim-id> ...` before moving on. Do not use a blind
`|| release` shell shortcut: a timeout or another command failure needs
diagnosis, not automatic abandonment of the claim.

## What the loop does not promise

- Advisory hooks do not make unclaimed edits safe.
- A claim serializes overlapping coordination scopes; it is not an operating-system permission boundary.
- A handoff exists when the sender records it; it is complete when the target writes its own acknowledgement.
- Rally does not schedule work, run commands, choose code changes, or approve external actions.

See [RALLY.md](../RALLY.md) for the 60-second operating guide,
[COMMAND-SEMANTICS.md](COMMAND-SEMANTICS.md) for command behavior, and
[RALLY_ARCHITECTURE.md](RALLY_ARCHITECTURE.md) for the product boundary.
