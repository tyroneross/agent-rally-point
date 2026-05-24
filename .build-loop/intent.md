# Intent — agent-rally-point v0.3.0 canonical substrate

## North star
Ship a coordination substrate where every consumer (build-loop, codex, claude_code, watcher) writes to ONE canonical path per repo, with a versioned discovery handshake that fails loud on mismatch instead of silently splitting writes across two universes.

## Why now
v0.2.1 advertises `channel_layout: "canonical"` while in practice every peer writes to `~/.build-loop/apps/<slug>/`. `~/.agent-rally-point/apps/` does not exist on any machine that has used build-loop. The discover layer's fallback semantics (canonical-absent → legacy) are silently effective in 100% of real installs. v0.12.16 was the lesson: one missing audit + one silent fallback creates a second universe.

## Scope (α1–α8)
| Item | Surface | Hard rule |
|------|---------|-----------|
| α1 | `channel_layout="canonical"` mode resolving to `~/.agent-rally-point/apps/<repo-id>/` | repo-id (not slug) becomes the per-repo channel name |
| α2 | `manifest.toml` `policy` field {canonical \| migration \| legacy-only}, default `migration` | migration is DUAL-AWARE — discover returns both paths + merged view; writes mirror to both |
| α3 | `discover()` envelope: `protocol_version: "1.0"`, `channel_layout`, `last_resolved_at`, `policy`, `repo_id` | When `policy=migration`, also returns `legacy_channel_dir` and `merged_view: true` |
| α4 | `agent-rally-migrate` subcommand walking `~/.build-loop/apps/*` → `~/.agent-rally-point/apps/<repo-id>/` | Append-only log at `~/.agent-rally-point/migration.log` with sha256 manifest; legacy kept + marker |
| α5 | Cutover check — only promote `policy: canonical` after ALL 4 conditions | Verifier scans legacy mtimes; refuses promotion if writes detected within one TTL |
| α6 | Console script `agent-rally-discover` installed to user PATH via pipx | Verifiable from a fresh shell |
| α7 | Presence-refresh watcher fix — 60s refresh preserving full envelope + parent PID liveness check | Watchers don't keep dead sessions falsely alive |
| α8 | `repo_id` from normalized git remote URL + 8-char content hash | Falls back to repo-root path hash when no remote; frozen as `protocol_version 1.0` |

## Hard rules (non-negotiable)
1. **`coordination_unavailable` is loud-and-degraded, never silent.** If canonical unreachable under `policy=canonical`, return `coordination_unavailable` AND proceed without coordination. No silent legacy fallback.
2. **Channel-consistency invariant per policy.** `canonical`/`legacy-only` → one `channel_dir`, all subcounts from that path. `migration` → both paths + merged view with revision-based dedup.
3. **Legacy read-only marker is advisory only.** Migration verifier must detect fresh legacy writes and refuse cutover until writes cease for one TTL (15 min).
4. **Workstreams as path-scoped children**, not flat-inbox tags.
5. **`repo_id` frozen as part of `protocol_version 1.0`.** Backward-incompatible change requires protocol bump.
6. **No direct-to-main commits.** Branch + PR only. Precedent: b6eff45 on build-loop.

## Compatibility table
Written to `~/.agent-rally-point/compatibility.json` on install:
```json
{
  "agent_rally_point": "0.3.0",
  "protocol_version": "1.0",
  "supported_build_loop_range": ">=0.12.17,<0.14.0",
  "deprecation_notices": []
}
```

## Out of scope
- Build-loop changes (Phase β, separate repo, separate PR).
- agent-rally-watcher changes.
- Removing the 5 legacy `_resolve_channel` call sites in `build-loop/scripts/agent_rally.py` (β2).

## Integration checkpoint (what α must deliver to unblock β)
- `~/.agent-rally-point/apps/` exists with at least one repo's state migrated.
- `which agent-rally-discover` resolves from a fresh shell (no .venv preconditioning).
- discover envelope returns `{protocol_version, policy, repo_id, channel_layout, last_resolved_at}`.
- `agent-rally-migrate` works end-to-end against the build-loop app.
- No-fresh-legacy-writes check operational.

## Stop conditions
- C5 architectural-class novel decision the brief doesn't resolve.
- Need to modify build-loop instead of just agent-rally-point.
- Repo-wide test failure that suggests the design itself is wrong.
