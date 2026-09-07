# Coordination optimization: capabilities and validation

Rally remains a local, durable coordination protocol. Host names are metadata;
handoff state and receiver identity determine receipt. A terminal transports a
notification. Only a receiver-authored response proves acknowledgement.

## Available interfaces

| Capability | Interface | Reliability boundary |
| --- | --- | --- |
| Compact observation | `rally room --compact --tool <id> --path <path> --json` | Advisory. Retains applicable claims, blockers, decisions and addressed obligations. Explicitly reports omitted inventory and critical overflow. Use `rally check` before writing. |
| Incremental observation | Add `--since <max_seq>` | Reports whether state changed; old active obligations remain visible. Reads append no presence or read facts. |
| Durable context | `node dynamic-workflows/core/checkpoint.mjs put <directory> <context.json>` | Immutable, versioned, content-addressed JSON; bounded to 64 KiB. Preserves supplied context exactly; does not invent or summarize missing context. |
| Context consumption | `packet.mjs ... --task <id> --checkpoint <file> --revision <full-sha>` | Verifies hash, run, task and revision before rendering. Source and packaged Codex runtime include the reader. |
| Custom host launch | `rally run <host-label> --command-json '["/path/to/host"]' ...` | Explicit argv; no invented vendor flags. Custom commands cannot use the built-in `--task` lifecycle. CLI fallback works without a host hook. |
| Safe stock-tmux delivery | Normal managed `run` / `adopt` / `inject` | Captures pane, process, server and socket identity. Rejects observed copy mode or replacement. Small frames share one send command; large frames use a unique stdin buffer. |
| Receipt reuse | Inject a previously acknowledged handoff | Returns the original exact receiver receipt without another directive or terminal send. Uncertain, unacknowledged sends are not automatically replayed. |
| Storage inspection | `python3 scripts/rally_storage.py inventory --rally-dir <directory>` | Streaming inventory. No database or ledger mutation. |
| Explicit archive compression | `rally_storage.py compress --rally-dir <directory> --file facts.db.corrupt.<stamp>` | Verifies a lossless gzip round trip before replacing one quarantine snapshot. Protects live DB/WAL, ledger, task results and recovery bundles. Restore refuses overwrite. |

For launch examples, ACK variants and checkpoint structure, see
[any-agent onboarding](ANY-AGENT-ONBOARDING.md).

## Measured evidence

Local measurements on 2026-09-07 used macOS and stock tmux 3.6a.

| Probe | Result | Scope |
| --- | --- | --- |
| Crowded live room: six alternating reads per mode, same release executable and path scope | Full median 261,210 bytes / 0.224 s; compact median 49,903 bytes / 0.119 s | 80.9% smaller output and 47.1% lower median CLI read time in this sample. Views intentionally expose different detail. Not a billed-token or end-to-end model latency comparison. |
| Routine 800-peer fixture | Compact fits 6,000 bytes and saves over 50% of serialized bytes | Critical context is never cut to force the byte target. The crowded live room above exceeded the target and reported overflow. |
| Live Claude ↔ Codex pilot | 20/20 synthetic tasks: ten each direction, exact capsule content, correct answers, receiver-authored ACKs | Four provider calls. Tests CLI handoff and receipt semantics; does not test interactive model TUIs or production task generality. |
| Real stock-tmux fault probe | 7/7 checks passed | Unicode/control sanitization, honest receipt state, 15 KB payload, concurrent frames, active-pane switch, copy mode, and process replacement with durable directive retained. |
| Lossless compression of a copied quarantine snapshot | 7,749,632 input bytes; 6,723,103 bytes saved (86.8%); restored SHA-256 matched | Original repository snapshot remained unchanged. This is potential space reduction, not space already reclaimed from the user's store. |

Deterministic tests cover Codex, Claude, Gemini, Cursor and RossLabs host labels
using the same command and capsule contracts. Live model execution is verified
for Claude and Codex only. Other harnesses still require their own executable,
permissions and lifecycle integration tests.

## Reproduce the acceptance probes

```sh
cargo build --release -p rally-cli --bin rally
python3 scripts/rally_optimization_probe.py --binary target/release/rally --output /tmp/rally-transport.json
python3 scripts/rally_model_handoff_probe.py --binary target/release/rally --output /tmp/rally-models.json
node --test dynamic-workflows/tests/checkpoint.test.mjs
python3 -m unittest discover -s tests/scripts -p test_rally_storage.py
bash scripts/run-quality-gate.sh
bash scripts/run-release-auxiliary-gate.sh
```

The model probe uses authenticated local Claude and Codex CLIs and their configured
models. `--codex-binary <path>` selects a compatible installed executable without
changing the global CLI. Fixtures use private temporary directories and tmux
sockets; traces contain synthetic tasks. Record executable hashes with results
when comparing builds.

## Design limits and the tmux extension decision

The tested stock adapter meets the current transport baseline, so this change
does not introduce a maintained tmux fork. An extension would need a reproduced
stock limitation and a passing comparison on the same acceptance probes before
its maintenance cost is justified. The implementation follows tmux's
[load-buffer](https://github.com/tmux/tmux/blob/3.6a/cmd-load-buffer.c),
[paste-buffer](https://github.com/tmux/tmux/blob/3.6a/cmd-paste-buffer.c) and
[send-keys](https://github.com/tmux/tmux/blob/3.6a/cmd-send-keys.c) mechanisms.

Pane echo does not prove that an LLM accepted a prompt. Identity checks cannot
eliminate the race between checking a process and writing to its terminal.
Legacy sessions without a stored binding remain unbound. An application may
ignore a valid paste or be busy; missing capture evidence stays unverified.
No universal exactly-once or never-fails guarantee is made.

Checkpoint hashes detect corruption, not authenticity of peer content. Evidence
URIs remain references to verify. Compression applies only to explicitly selected
quarantine snapshots; it adds no automatic memory eviction, retention deletion or
database migration. Existing durability settings remain intact.
