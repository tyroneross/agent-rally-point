<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Discovery

How sibling tools (build-loop, agent-rally-watcher, codex, custom integrations) find an installed Rally Point and resolve its active channel without hardcoding paths.

## Why a discovery layer

Before this layer, every consuming tool hardcoded `~/.build-loop/apps/<slug>/` and re-derived the slug from `git rev-parse --git-common-dir`. That coupled every consumer to the legacy path, prevented relocation, and forced new tools to duplicate path-resolution logic. Discovery centralizes:

- "Is Rally Point installed?" → one call.
- "Where is this repo's channel?" → one call.
- "What's the active state (revision, peers)?" → one call.
- "Can I rely on this being stable as the package moves?" → yes, via the manifest contract.

## Manifest

### Global manifest

Path: `~/.agent-rally-point/manifest.toml`

Auto-created on first `agent-rally-discover` invocation (or any package operation that requires the global root). Idempotent.

```toml
schema_version = "1.0"
protocol_version = "1.0"

[package]
name = "agent-rally-point"
version = "0.3.0"
installed_at = "2026-05-24T12:00:00Z"

[paths]
apps_root = "~/.agent-rally-point/apps"
legacy_apps_root = "~/.build-loop/apps"

[policy]
# canonical | migration | legacy-only. Default: migration (dual-aware).
mode = "migration"

[api]
discover_module = "agent_rally_point.discover"
cli_entry = "agent-rally"
schema_doc = "https://github.com/tyroneross/agent-rally-point/blob/main/docs/SCHEMA.md"

[defaults]
heartbeat_minutes = 15
stale_session_seconds = 3600
```

Fields:

| Field                      | Type     | Description |
|----------------------------|----------|-------------|
| `schema_version`           | string   | Manifest format version. Currently `"1.0"`. |
| `package.name`             | string   | Always `agent-rally-point`. |
| `package.version`          | string   | Installed version (`__version__`). |
| `package.installed_at`     | string   | ISO 8601 timestamp the manifest was first written. |
| `paths.apps_root`          | string   | Canonical channels root (overridable via `$BUILD_LOOP_APPS_ROOT`). |
| `paths.legacy_apps_root`   | string   | Legacy channels root (read-only fallback). |
| `api.discover_module`      | string   | Python module exposing `discover()`. |
| `api.cli_entry`            | string   | CLI entry point (`agent-rally`). |
| `api.schema_doc`           | string   | URL to the record schema documentation. |
| `defaults.heartbeat_minutes` | integer | Default presence heartbeat staleness window. |
| `defaults.stale_session_seconds` | integer | Default closeout reaper threshold. |

### Repo-level overlay (opt-in)

Path: `<repo-root>/.agent-rally.toml`

Per-repo override. Useful for monorepos where the canonical slug derivation doesn't match the desired channel split, or for pinning a non-default apps root.

```toml
schema_version = "1.0"

[channel]
# Override the auto-derived slug. Optional; absent → use git-common-dir.
slug = "my-monorepo/web"

[paths]
# Override the apps root for this repo. Optional.
apps_root = "/tmp/test-channels"

[tool]
# Per-tool identification overrides. Optional.
tool_id = "claude_code"
```

When both global and repo-level exist, repo-level fields override global on a per-field basis. Missing fields fall through to global. The discovery resolver records `source: "repo" | "global" | "default"` per field so consumers can audit.

## CLI

```
agent-rally-discover [--field NAME] [--json] [--quiet]
```

Or invoke via `python3 -m agent_rally_point.discover` if the console script is not on `$PATH`.

Default behavior prints the resolved manifest as pretty JSON. Exit code 0 if Rally Point is installed (manifest present OR legacy channel exists for this cwd), 1 otherwise.

### Flags

| Flag         | Effect |
|--------------|--------|
| `--json`     | Emit JSON (default; explicit flag is a no-op). |
| `--field N`  | Print only the value of field `N` (e.g. `--field channel_dir` → bare path). Exit 1 if the field is absent. |
| `--quiet`    | Suppress output entirely. Exit 0 if installed, 1 if not. Use for shell scripting. |

### Examples

```bash
# Full discovery as JSON
agent-rally-discover

# Just the channel directory for the current cwd
agent-rally-discover --field channel_dir

# Probe in a shell script
if agent-rally-discover --quiet; then
    echo "Rally Point installed"
fi

# Module form
python3 -m agent_rally_point.discover --json
```

## Programmatic API

```python
from agent_rally_point.discover import discover

info = discover()  # cwd defaults to os.getcwd()
# info is a dict; keys documented below
```

`discover(cwd=None)` returns a dict with these top-level keys:

| Key                          | Type     | Description |
|------------------------------|----------|-------------|
| `installed`                  | bool     | True if any layer (canonical or legacy) resolved. |
| `version`                    | string   | Package version (from `agent_rally_point.__version__`). |
| `schema_version`             | string   | Manifest schema version (currently `"1.0"`). |
| `protocol_version`           | string   | Discovery-envelope contract version. Frozen at `"1.0"`. |
| `policy`                     | string   | Active substrate policy: `"canonical"` \| `"migration"` \| `"legacy-only"`. |
| `last_resolved_at`           | string   | ISO8601 UTC timestamp of this `discover()` call. |
| `repo_id`                    | string   | Worktree-stable, clone-stable repo identifier. `<slug>-<8hex>`. |
| `channel_dir`                | string   | Absolute path to this repo's primary channel directory. |
| `channel_layout`             | string   | `"canonical"` or `"legacy"`. Under `policy: migration` this is `"canonical"` (write target). |
| `app_slug`                   | string   | The resolved slug for this cwd (basename-derived, not repo_id). |
| `apps_root`                  | string   | Resolved apps root (after overlays + env). |
| `active_revision`            | integer  | Current revision counter. Under `policy: migration` this is `max(canonical, legacy)`. |
| `active_peers`               | list     | List of session dicts. Under `policy: migration` this is the union of both channels deduped by `session_id`. |
| `coordination_unavailable`   | bool     | True iff substrate is in loud-degraded mode. Consumer MUST check before treating `channel_dir` as writeable. |
| `coordination_unavailable_reason` | string | Present only when `coordination_unavailable: true`. Currently: `"canonical_unreadable"`. |
| `schema_doc_url`             | string   | URL to `docs/SCHEMA.md`. |
| `apis`                       | dict     | Resolution map for callable surfaces. |
| `sources`                    | dict     | Per-field source provenance: `{"channel_dir": "canonical" \| "migration-dual" \| "legacy-only" \| ..., "policy": ..., ...}` |

#### Migration-policy extras

When `policy == "migration"` (the default), the envelope additionally carries:

| Key                       | Type     | Description |
|---------------------------|----------|-------------|
| `canonical_channel_dir`   | string   | The canonical write target (`~/.agent-rally-point/apps/<repo_id>/`). Same as `channel_dir`. |
| `legacy_channel_dir`      | string   | The legacy read target (`~/.build-loop/apps/<slug>/`). Mirror-write target. |
| `merged_view`             | bool     | Always `true` under migration policy. Reads merge both channels. |

These keys are absent under `policy: canonical` or `policy: legacy-only` (single channel).

When `installed: false` (no manifest, no legacy channel, not in a git repo), the dict still returns these keys with sensible defaults (`channel_dir` resolves to where it *would* live under canonical paths; `active_revision: 0`; `active_peers: []`). This keeps callers branch-free: they can always read the structure, then check `installed`.

## Resolution order

`apps_root` (per-process override → repo overlay → global manifest → built-in default).

`app_slug` (repo overlay → derived from `git rev-parse --git-common-dir` basename).

`repo_id` (normalized git remote URL + 8-hex sha256; falls back to path hash when no remote). See `agent_rally_point/repo_id.py`.

`policy` (`$AGENT_RALLY_POLICY` env → repo overlay `[policy].mode` → global manifest `[policy].mode` → default `"migration"`).

`channel_dir` resolution then depends on the resolved policy:

| Policy        | `channel_dir`                                            | Reads                              | Writes                                  |
|---------------|----------------------------------------------------------|------------------------------------|-----------------------------------------|
| `canonical`   | `<apps_root>/<repo_id>/`                                 | canonical only                     | canonical only                          |
| `migration`   | `<apps_root>/<repo_id>/` (primary write target)          | merged union of canonical + legacy | canonical AND mirror-write to legacy    |
| `legacy-only` | `~/.build-loop/apps/<slug>/`                             | legacy only                        | legacy only                             |

### Hard rule: no silent fallback

Under `policy: canonical`, if the canonical channel is unreadable, `discover()` returns `coordination_unavailable: true` and a non-empty `coordination_unavailable_reason`. It does **not** silently serve legacy data. Consumers in canonical-policy mode must check `coordination_unavailable` before treating `channel_dir` as a working write target. Rally Point is awareness, not enforcement — the build proceeds in degraded mode without coordination.

Source: `coordination-substrate-canonical.md` (the v0.12.16 silent-second-universe defect class is exactly what this rule prevents).

### Cutover from migration to canonical

The package ships defaulting to `policy: migration`. Promote to `policy: canonical` only after `agent-rally-point migrate verify-cutover` confirms all four conditions:

1. Legacy state fully copied to canonical.
2. Migration verifier shape + integrity checks pass.
3. No fresh legacy writes detected within one heartbeat TTL (15 min).
4. Downstream callers (build-loop ≥ 0.12.17) installed or retired.

The promotion is a manual edit to `[policy] mode` in the manifest, *after* `verify-cutover` returns `can_promote: true`. The verifier itself never writes the manifest.

## Versioning

`schema_version = "1.0"` is the initial manifest contract. Backward-compatible additions (new optional fields) keep `"1.0"`; breaking changes bump the major. Consumers should read `schema_version` and reject or gracefully degrade on a major they don't recognize.

## See also

- [`ARCHITECTURE.md`](../ARCHITECTURE.md) — three-layer model and channel layout
- [`docs/SCHEMA.md`](SCHEMA.md) — record format consumed via `checkpoint_read`
- [`agent-rally-watcher`](https://github.com/tyroneross/agent-rally-watcher) — push-based daemon that uses discovery to find the channel
