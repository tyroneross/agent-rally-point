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

Auto-created on first `agent-rally discover` invocation (or any package operation that requires the global root). Idempotent.

```toml
schema_version = "1.0"

[package]
name = "agent-rally-point"
version = "0.2.0"
installed_at = "2026-05-23T12:00:00Z"

[paths]
apps_root = "~/.agent-rally-point/apps"
legacy_apps_root = "~/.build-loop/apps"

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

| Key                | Type     | Description |
|--------------------|----------|-------------|
| `installed`        | bool     | True if any layer (canonical or legacy) resolved. |
| `version`          | string   | Package version (from `agent_rally_point.__version__`). |
| `schema_version`   | string   | Manifest schema version (currently `"1.0"`). |
| `channel_dir`      | string   | Absolute path to this repo's channel directory. |
| `channel_layout`   | string   | `"canonical"` (under `~/.agent-rally-point/apps/`) or `"legacy"` (under `~/.build-loop/apps/`). |
| `app_slug`         | string   | The resolved slug for this cwd. |
| `apps_root`        | string   | Resolved apps root (after overlays + env). |
| `active_revision`  | integer  | Current revision counter for this channel (0 if absent). |
| `active_peers`     | list     | List of session dicts (`session_id`, `tool`, `heartbeat_ts`, `branch_name`, `cwd`) — see `presence.read_active_presence`. |
| `schema_doc_url`   | string   | URL to `docs/SCHEMA.md`. |
| `apis`             | dict     | `{"post": "agent_rally_point.post:post", "checkpoint_read": "agent_rally_point.checkpoint:checkpoint_read"}` |
| `sources`          | dict     | Per-field source provenance: `{"channel_dir": "repo" | "global" | "default", ...}` |

When `installed: false` (no manifest, no legacy channel, not in a git repo), the dict still returns these keys with sensible defaults (`channel_dir` resolves to where it *would* live under canonical paths; `active_revision: 0`; `active_peers: []`). This keeps callers branch-free: they can always read the structure, then check `installed`.

## Resolution order

For `channel_dir` and `apps_root`:

1. **Repo-level `.agent-rally.toml`** at the canonical repo root (from `git rev-parse --show-toplevel`). If `paths.apps_root` is set, use it. If `channel.slug` is set, use it instead of auto-deriving.
2. **Global `~/.agent-rally-point/manifest.toml`**. If absent, auto-create with defaults.
3. **Canonical default**: `~/.agent-rally-point/apps/<slug>/`.
4. **Legacy fallback**: if the canonical channel directory does not exist on disk AND `~/.build-loop/apps/<slug>/` does exist, return the legacy path with `channel_layout: "legacy"`. This preserves backward compatibility for sessions that started before the package was extracted from build-loop.

Step 4 is read-only — discovery does not migrate legacy channels to canonical. Use `agent-rally-point migrate` (future) or move them manually.

## Versioning

`schema_version = "1.0"` is the initial manifest contract. Backward-compatible additions (new optional fields) keep `"1.0"`; breaking changes bump the major. Consumers should read `schema_version` and reject or gracefully degrade on a major they don't recognize.

## See also

- [`ARCHITECTURE.md`](../ARCHITECTURE.md) — three-layer model and channel layout
- [`docs/SCHEMA.md`](SCHEMA.md) — record format consumed via `checkpoint_read`
- [`agent-rally-watcher`](https://github.com/tyroneross/agent-rally-watcher) — push-based daemon that uses discovery to find the channel
