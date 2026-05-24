# Plan — agent-rally-point v0.3.0 canonical substrate

**Branch:** `feat/canonical-substrate` (already cut from main)
**Tier:** thinking — `risk_reason: persistence contract` (substrate redesign + protocol_version freeze)

## Synthesis dimensions
1. Canonical layout primitive
2. Dual-aware migration
3. Discover envelope versioning
4. Migration tool + verifier
5. Cutover gate
6. Distribution (pipx PATH)
7. Watcher liveness
8. repo_id normalization

Count = 8. Routes to `tier: thinking` per orchestrator §"Synthesis-density routing" (count > 5 escalates).

## Chunks (sequential — each chunk a commit on `feat/canonical-substrate`)

### Chunk 1 — `repo_id` normalization (α8)
**files_owned:** `agent_rally_point/repo_id.py` (new), `agent_rally_point/test_repo_id.py` (new), `agent_rally_point/__init__.py` (export `repo_id`).
**files_not_owned:** every other file.
**interface contract:** `repo_id(cwd: Path | None = None) -> str` returns lowercased-normalized-remote + `-` + 8-char sha256(remote). Falls back to `_unscoped-` + 8-char path hash when no remote. Frozen as `protocol_version 1.0`.
**integration checkpoint:** test cases — same repo via two worktrees → same repo_id; same repo with/without `.git` suffix on remote → same repo_id; two different repos with same basename → different repo_id; no-remote repo → path-hash fallback.
**risk_reason:** persistence contract.
**modifies_api:** false (new public symbol; nothing called this before).

### Chunk 2 — manifest `policy` field + compatibility table (α2 + compat)
**files_owned:** `agent_rally_point/discover.py` (extend `_ensure_global_manifest` to write `policy = "migration"` + `protocol_version = "1.0"`; add `_ensure_compatibility_table`), `agent_rally_point/test_discover.py` (extend), `examples/manifest.toml` (refresh).
**files_not_owned:** channel_paths.py, presence.py, post.py, repo_id.py.
**interface contract:** new manifest top-level keys `protocol_version` and a `[policy]` section with `mode = "canonical" | "migration" | "legacy-only"` (default `migration`). Compatibility table at `~/.agent-rally-point/compatibility.json` auto-written on first discover.
**integration checkpoint:** test — manifest auto-creates with `policy.mode = "migration"`, `protocol_version = "1.0"`; compatibility.json materializes with correct shape.
**risk_reason:** persistence contract.
**modifies_api:** false (additive fields).

### Chunk 3 — discover envelope versioning + policy-aware resolution (α1 + α3)
**files_owned:** `agent_rally_point/discover.py` (resolution logic + envelope), `agent_rally_point/test_discover.py` (envelope tests), `docs/DISCOVERY.md` (refresh).
**files_not_owned:** channel_paths.py, repo_id.py (consumed only).
**interface contract:** `discover()` returns new fields `protocol_version`, `policy`, `repo_id`, `last_resolved_at` always. Under `policy=migration`, additionally returns `legacy_channel_dir` + `merged_view: true` + `sources.channel_dir = "migration-dual"`. Under `policy=canonical` with canonical unreadable, returns `coordination_unavailable: true` (NEW key) + does NOT fall back to legacy. Under `policy=legacy-only` returns legacy as the single channel.
**integration checkpoint:** tests — each policy returns the expected envelope shape; `coordination_unavailable: true` test asserts no silent legacy fallback.
**risk_reason:** persistence contract.
**modifies_api:** true (envelope contract change).
**caller-audit:** `discover()` is called by tests (in this repo) and (out of scope) build-loop's β bridge. Build-loop callers are deliberately blocked behind β work; no callers in agent-rally-point break because all callers thread the dict through and the new keys are additive (existing keys preserved).

### Chunk 4 — migration tool `agent-rally-migrate` (α4)
**files_owned:** `agent_rally_point/migrate.py` (new), `agent_rally_point/test_migrate.py` (new), `agent_rally_point/__init__.py` (export), `pyproject.toml` (add `agent-rally-migrate` console script).
**files_not_owned:** discover.py, channel_paths.py, presence.py.
**interface contract:** `python -m agent_rally_point.migrate` (and console script `agent-rally-migrate`). Subcommands: `scan` (dry-run list of source dirs), `apply` (copy + log + marker), `verify-cutover` (4-condition check + fresh-write detection). Append-only log at `~/.agent-rally-point/migration.log` JSONL with `{ts, source_path, dest_path, file_count, sha256_manifest, operation}`. Legacy dir kept; advisory marker at `<legacy>/.RALLY_LEGACY_READONLY` with timestamp.
**integration checkpoint:** test — `apply` against tmp legacy → canonical, log entries verified, marker present, sha256 verified, idempotent (re-run no-ops with logged "already-migrated" entries).
**risk_reason:** persistence contract.
**modifies_api:** false.

### Chunk 5 — cutover verifier (α5)
**files_owned:** `agent_rally_point/migrate.py` (extend `verify-cutover`), `agent_rally_point/test_migrate.py` (extend).
**files_not_owned:** everything else.
**interface contract:** `verify-cutover` returns `{can_promote: bool, conditions: {legacy_fully_copied, integrity_verified, no_fresh_writes_within_ttl, downstream_ready}, fresh_writes: [...]}`. Refuses promotion (`can_promote: false`) if any condition fails. `--ttl-minutes` override for testing.
**integration checkpoint:** tests — refuses on fresh legacy write within TTL; refuses on integrity mismatch; accepts when all 4 conditions hold; `--ttl-minutes 1` accelerates test.
**risk_reason:** persistence contract.
**modifies_api:** false.

### Chunk 6 — presence watcher liveness fix (α7)
**files_owned:** `agent_rally_point/presence.py` (add `run_refresh_loop`), `agent_rally_point/test_presence.py` (extend).
**files_not_owned:** discover.py, migrate.py.
**interface contract:** `run_refresh_loop(channel_dir, *, session_id, tool, model, run_id, app_slug, phase_provider, files_provider, interval=60, parent_pid=None, cwd=None) -> None`. Calls `write_presence()` every `interval` seconds with the FULL envelope (callbacks supply current phase + files). Exits when `parent_pid` is dead (`os.kill(pid, 0)` raises ProcessLookupError) — does NOT keep refreshing on a dead parent. Catches and swallows all exceptions per fire-and-forget contract.
**integration checkpoint:** test with `interval=0.1`, fake parent_pid that we kill mid-loop → loop exits within one tick after parent dies; presence file disappears after TTL elapses.
**risk_reason:** runtime protocol.
**modifies_api:** false (additive public function).

### Chunk 7 — pipx distribution + README refresh (α6)
**files_owned:** `pyproject.toml` (version bump to 0.3.0), `README.md` (pipx install section), `docs/DISCOVERY.md` (envelope refresh, policy section), `ARCHITECTURE.md` (canonical/migration/legacy-only policy explainer).
**files_not_owned:** all .py source.
**interface contract:** README documents `pipx install <path>` AND `pipx install agent-rally-point` (once published). Manual verification step: `which agent-rally-discover` resolves from a fresh shell outside any .venv.
**integration checkpoint:** doc content present; lints clean.
**risk_reason:** deployment.
**modifies_api:** false.

### Chunk 8 — integration test + acceptance gate (covers α1-α8 end-to-end)
**files_owned:** `agent_rally_point/test_acceptance_alpha.py` (new).
**files_not_owned:** all source (read-only).
**interface contract:** one pytest module asserting all 13 acceptance criteria from `goal.md`. No new production code.
**integration checkpoint:** all 13 criteria green; baseline 67 + new tests all green.
**risk_reason:** none (verification only).
**modifies_api:** false.

## Dependency graph
```
1 (repo_id) ──┐
              ├──▶ 3 (discover envelope) ──▶ 4 (migrate) ──▶ 5 (cutover) ──▶ 8 (acceptance)
2 (policy) ──┘                              │
                                            └──▶ 6 (watcher) ──▶ 8
                                            
7 (docs/pipx) ──▶ 8 (after all source chunks land)
```
1, 2 are independent.
3 depends on 1+2.
4 depends on 3.
5 depends on 4.
6 is independent of 4/5 — could run after 3.
7 is documentation only; runs after 1–6.
8 runs last.

**parallel_skipped_reason:** Working solo as the implementer. Each chunk is small enough that serial execution keeps the build-loop coherent + each commit verifiable. Substrate package — no token-cost gain from fan-out warrants the coordination overhead.

## Pre-commit gate per chunk
1. `python -m pytest agent_rally_point/ -q` — full suite green.
2. `git diff --stat` — only files declared in `files_owned`.
3. Commit on `feat/canonical-substrate`. No direct-to-main.

## Caller audit (modifies_api chunks)
Chunk 3 is the only `modifies_api: true` chunk.
- **In-repo callers of `discover()`**: only tests (`test_discover.py`). All tests are updated in the same chunk.
- **Out-of-repo callers**: build-loop's β bridge (separate repo, separate PR). β is deliberately blocked on α — no agent-rally-point callers break because the envelope change is additive (existing keys preserved, new keys added).
- **Scope acceptance**: no out-of-repo callers in scope for this PR. Build-loop side will adopt the new envelope in β.
