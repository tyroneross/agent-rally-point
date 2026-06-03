<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Assessment — codex rally hook desync + missing wall-clock bound (2026-05-31)

> **Historical (2026-05-31).** References to `--backend herdr` and herdr lifecycle commands describe the state of the world before Plan F removed the herdr backend. Easy Terminal's app daemon socket has since been renamed `herdr.sock` → `ptyd.sock`.

Field report from dogfooding parallel agents in Easy Terminal. Surfaced while
diagnosing orphaned, unkillable `rally hook` processes. Ties directly to **B17**
(one-store retirement) and the **lazy-auto-enter / no-hook** design principle.

## What was observed

1. **The installed codex wrapper calls commands that no longer exist.**
   `~/.codex/rally-hook.sh` invokes `rally hook before-write --tool codex
   --session-id <id> --json --fail-open --auto-claim`. The `hook` and `start`
   subcommands — and the `--fail-open` / `--auto-claim` / `--session-id` flags —
   were deleted in commit **`0d5024b`** ("remove legacy rally implementation").
   Today the call returns immediately:
   ```
   $ echo '{}' | rally hook before-write --tool codex --session-id test --json --fail-open
   {"error":"unknown Rally command hook","exit_code":2,"ok":false,"product":"rally"}
   ```
   The wrapper's `|| true` fail-open swallows this, so the shell sees rc=0 and
   nothing looks wrong.

2. **Codex coordination has therefore been a silent no-op.** Every codex
   before-write hook resolves to "unknown command" and records nothing — no
   presence, no claim, no handoff. The earlier "rally only tracked 2/19 agents"
   observation is partly explained by this: the *Python* `agent_rally.py presence`
   calls an orchestrator drives by hand work, but the *automatic* codex path is dead.

3. **Four orphaned, unkillable processes.** The *previous* `rally` binary's
   `hook` path did unbounded filesystem work on store open (segment replay + DB
   reconcile in `store.rs`; `Path::canonicalize` in `repo_root`/`git_common_dir`).
   On a wedged/slow vnode that blocks indefinitely. Four `rally hook before-write`
   procs were stuck in `UE` (uninterruptible kernel I/O wait), `ppid=1`, etime
   ~7h45m. **A process in uninterruptible sleep cannot be killed by any signal,
   including SIGKILL** — only a reboot clears them.

4. **No wall-clock bound anywhere in the chain.** `--fail-open` only governs the
   *exit decision* ("don't block the tool on error"); it does nothing for a hang.
   The bash wrapper called `rally` with no `timeout`. So any blocked invocation
   ran forever.

5. **Claude has no rally integration at all.** There is no `~/.claude` rally hook
   and no `rally` reference in Claude settings. Claude coordination is entirely
   manual `agent_rally.py` calls. Two coordination surfaces coexist: the Python
   legacy store (`~/.agent-rally-point/apps/<slug>/changes.jsonl`) and the Rust
   per-repo ledger — exactly the split B17 is retiring.

## What the issue is

- **Correctness (primary):** the externally-installed codex wrapper desynced from
  the CLI when `0d5024b` removed the legacy subcommands. The wrapper was never
  updated, so codex's automatic coordination is inert. This is the "external
  wrapper / build-loop-side legacy writer" cleanup B17 notes is "tracked
  separately" — here is the concrete instance.
- **Robustness (secondary):** no layer bounded execution time, so the legacy
  binary's blocking store-open could orphan unkillable procs. (The current binary
  fails fast, but a future blocking path would have repeated this.)
- **Coverage gap:** Claude is unintegrated; the bespoke per-tool hook is the
  legacy pattern the "lazy auto-enter (no hook)" model is meant to replace.

## Recommendation

Make the Rust `rally` CLI the single canonical surface (user decision, 2026-05-31)
and retire the desynced external wrapper rather than patch it in place:

1. **Retire / re-point the codex wrapper** to the live model. Prefer **lazy
   auto-enter** — have codex call `rally check before-write --tool codex:NN
   --path <p>` (and `rally enter` on session start), which registers presence with
   no bespoke hook logic. Update the wrapper's node post-processing to the
   `check`/`room` envelope shape (it currently parses the removed `hook`
   envelope's `data.hook.agent_visible`).
2. **Add a Claude integration** mirroring it (`~/.claude/settings.json` PreToolUse
   → `rally check`/`rally enter`). ⚠️ Live-session impact — fires on every tool
   call; land deliberately and separately.
3. **`rally migrate-legacy`** to fold the Python `~/.agent-rally-point/...`
   ledger into the Rust store, then retire the Python writers (B17 follow-through).
4. **Adopt `rally run`/`inject`/`capture`/`stop`** (with `--backend herdr`) as the
   agent lifecycle, retiring hand-rolled herdr + `agent_rally.py` orchestration.
5. **Keep the wall-clock watchdog** (already landed — see below) as the permanent
   safety net so no hook can ever hang again, regardless of which path blocks.

## Benefit

- Codex **and** Claude actually coordinate — handoffs, path claims, no duplicate
  work — instead of a silent no-op.
- **One canonical ledger**, ending the two-store contamination/trust-drift B17
  targets.
- **No hung or orphaned hook processes**, ever (timeout is cause-agnostic).
- **Native multi-agent Easy Terminal orchestration** — `rally run codex
  --backend herdr`, `rally inject` (mid-run steering), `rally capture`, `rally
  stop` — replaces the hand-rolled herdr+Python stack with a first-class,
  ledger-backed feature.
- **Richer handoffs** than the Python MECE quartet: a DAG of
  `scope`/`resource`/`produces`/`depends` with `run`/`step` checkpoints.

## Next steps (sequenced)

1. **Merge the watchdog fix** — branch `fix/rally-hook-walltime-timeout`, commit
   `29e263f` (already built + installed locally; 4 new tests + suite green).
2. **Re-point the codex wrapper** to `rally check before-write` + map the envelope.
   Verify end-to-end that a before-write records a fact. *Low risk.*
3. **Claude rally hook** (PreToolUse). *Deliberate, live-session impact.*
4. **`rally migrate-legacy`** one-shot import; retire Python writers (B17).
5. **Adopt the `rally run/inject/capture/stop` lifecycle** in the ET workflow.
6. **Reboot** to clear the 4 inert `UE` procs (pid 24744/24748/24749/24750).

## Relevant files

| File | Role |
|------|------|
| `~/.codex/rally-hook.sh` (+ `.bak`) | The desynced external wrapper; calls removed `rally hook`; now has a `rally_timeout` shim (perl-alarm backend on this host). |
| `~/.local/bin/rally` (+ `.bak`) | Installed binary. Before `0.1.0+babc9b0` (16.5 MB) → after `0.1.0+d0e3715` (6.2 MB). |
| `crates/rally-cli/src/lib.rs` | `run_with_watchdog` (3 s default, `--timeout-ms`/`RALLY_HOOK_TIMEOUT_MS`); `repo_root`/`git_common_dir` canonicalize (a blocking-syscall site). |
| `crates/rally-cli/src/output.rs` | `Output::render()` / `RenderedOutput` (cross-thread result for the watchdog). |
| `crates/rally-cli/src/store.rs` | `reconcile_segments_and_db`, `read_segment_files` (unbounded store-open FS work — the legacy hang site). |
| `crates/rally-cli/tests/watchdog_timeout.rs` | New: returns-within-budget, exit 0, neutral envelope, no surviving worker. |
| commit `0d5024b` | Removed the legacy `hook`/`start` subcommands the wrapper still calls. |
| commit `29e263f` / branch `fix/rally-hook-walltime-timeout` | The wall-clock watchdog fix. |
| `build-loop/scripts/agent_rally.py` · `~/.agent-rally-point/apps/<slug>/` | The Python legacy surface to migrate + retire (B17). |

## Resolution (2026-05-31)

**Core landed (merge `40d641d`, on `origin/main`):** steps 1–2 above shipped — the
wall-clock watchdog plus the codex wrapper repointed off the removed `rally hook`
to the lazy model (`rally enter` + `rally check before-write`, envelope parser →
`data.check.*`). The field no-op is fixed: codex's automatic before-write now
records presence/claims again. Auto-merged conflict-free with main's
`worktree_guard` (`763be1c`); full suite green (275 tests; the one parallel-launch
flake is pre-existing — see BACKLOG `B-test-flake`).

**Hook made advisory-only (charter fix, 2026-05-31).** Review of the repointed
wrapper found a charter violation: its envelope translator mapped
`data.check.allow == false → permissionDecision: "deny"` (and `Stop → decision:
"block"`), i.e. it **blocked** the write. That contradicts the never-block charter
(*coordination is never blocked — collisions warn + record a durable audit fact*)
and `rally mission`'s own *"records and exposes only — never enforces."* Fix: the
wrapper's translator now forces `stop = false`, so every `PreToolUse`/`Stop` branch
emits **advisory** output (`additionalContext` / `systemMessage`) — high-severity
collisions are surfaced with a visible `⚠️` prefix but **never deny or block**.
`rally check`/`say` still record the durable collision fact, so nothing is lost;
the agent decides. Verified: synthetic stop-severity envelope → `additionalContext`,
no `deny`/`block`. (Decision: *strip-deny, keep advisory* — user, 2026-05-31.)

**Deferred (deliberately separate):** step 3 Claude PreToolUse hook is **on hold** —
under the advisory-only + lazy-auto-enter model a bespoke per-tool Claude hook may
be unnecessary; revisit after the `run/inject/capture/stop` lifecycle (step 5)
lands. Steps 4 (`migrate-legacy` + retire Python writers) and 5 (ET/herdr
lifecycle) are tracked as `LANE-C` / `LANE-D` on the rally backlog board.

> ⚠️ **Recurrence risk — wrapper is not version-controlled.** `~/.codex/rally-hook.sh`
> is a hand-installed external file with no repo source; that is exactly what let it
> desync from the CLI (the root cause here). Recommended follow-up: vendor the
> canonical advisory wrapper into the repo (e.g. `integrations/codex/`) with an
> installer, so the CLI and its host adapters can't drift silently again.

## Cross-refs

- **B17** (one-store retirement) — this is the concrete external-wrapper instance
  of the "build-loop-side legacy writers retired separately" follow-through.
- **Lazy auto-enter (no hook)** — the directional fix for step 2/3: agents call
  tool-scoped `rally` commands; no bespoke per-tool hook script. The advisory-only
  hook above is the interim model until lazy-auto-enter fully replaces it.
- build-loop-memory lesson: `lessons/2026-05-31-rally-wrapper-desync-and-watchdog.md`.
