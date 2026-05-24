# Goal & scoring criteria — α v0.3.0

## Goal
Ship agent-rally-point v0.3.0 implementing α1–α8 against the design brief, with all hard rules enforced and a clean PR against `main`.

## Acceptance criteria
1. **Canonical channel layout active** — `discover()` returns `channel_layout: "canonical"` with `channel_dir` under `~/.agent-rally-point/apps/<repo_id>/` when `policy=canonical`. Pytest covers it.
2. **Policy field round-trips** — `manifest.toml` `policy` ∈ {canonical, migration, legacy-only} parses, defaults to `migration`, and `discover()` surfaces it. Test covers each value.
3. **Dual-aware migration mode** — under `policy=migration`, `discover()` returns BOTH `canonical_channel_dir` and `legacy_channel_dir` plus `merged_view: true`. `sources.channel_dir == "migration-dual"`. Pytest covers it.
4. **Versioned discover envelope** — `protocol_version: "1.0"`, `last_resolved_at` (ISO8601 UTC), `repo_id`, `policy`, `channel_layout` all present in every `discover()` return.
5. **Migration subcommand works** — `agent-rally-migrate` (or `agent-rally-point migrate`) walks `~/.build-loop/apps/*` → `~/.agent-rally-point/apps/<repo_id>/`, writes the append-only log, places the advisory read-only marker. End-to-end pytest covers it.
6. **Cutover verifier refuses on fresh writes** — `agent-rally-point migrate verify-cutover` returns refuse when (a) legacy files newer than the marker exist OR (b) any of the 4 cutover conditions fail. Pytest covers refuse + accept paths.
7. **`agent-rally-discover` on PATH from fresh shell** — `python -m pipx install <path>` followed by `which agent-rally-discover` resolves to a pipx-managed path outside any `.venv`. Documented in README. (Manual verification step; pytest can't simulate pipx but documents the procedure.)
8. **Presence watcher liveness fix** — running `presence.run_refresh_loop(channel_dir, session_id, interval=60, parent_pid=<pid>)` refreshes the FULL envelope on every tick and exits when parent PID is dead. Pytest with a short interval covers full-envelope refresh + parent-dead exit.
9. **repo_id normalization stable** — same repo via clone, worktree, or remote URL with/without `.git` suffix produces the same `repo_id`. Different repos with same basename produce different `repo_id`. Pytest covers both.
10. **No silent fallback in discover** — under `policy=canonical` when canonical dir is unreadable, `discover()` returns `coordination_unavailable: true` and does NOT silently fall back to legacy. Pytest covers it.
11. **All 67 existing tests still pass** + new tests for α1–α8.
12. **Compatibility table written** — `~/.agent-rally-point/compatibility.json` materializes on first `discover()` with the documented shape.
13. **Branch `feat/canonical-substrate`** — no commits to `main`; PR opened at Phase 4.

## Scoring
Pass = all 13 criteria satisfied. Partial = ≥10/13 with documented gaps and follow-up tickets. Fail = <10/13.
