# Host plugin surfaces

`config/host-integrations.json` owns generated host metadata, skill frontmatter,
events, matchers, and timeouts. Run `python3 scripts/generate_host_surfaces.py`
after changing it; `--check` reports drift without writing host settings.
The generator skips writes when a surface already has the expected bytes.

| Host | Plugin root / manifest | Hook surface |
| --- | --- | --- |
| Claude Code | Repository root / `.claude-plugin/plugin.json` | `hooks/hooks.json` |
| Codex | `plugins/codex` / `.codex-plugin/plugin.json` | Project `.codex/hooks.json`; packaged plugin supplies skills |
| Muse | `plugins/muse` / `.claude-plugin/plugin.json` | Bundle `hooks/hooks.json` |
| Cursor | Repository root / `.cursor-plugin/plugin.json` | `hooks/cursor.json` |

Muse and Cursor provider records describe generation, not reconciliation.
The installed-provider reconciler manages Claude Code and Codex only.

## Muse bundle

Use the separate `plugins/muse` bundle. It has exactly one host manifest and
regular files, including all three contract skills, their workflow runtime and
local references, and the shared coordination script. Copying just a skill folder
loses references outside that folder. The bundle carries `rally-release.json`;
the shared release digest covers its complete payload except the identity itself.

The Claude-compatible format preserves SessionStart, UserPromptSubmit,
PreToolUse with its mutation matcher, and async Stop. Commands use
`CLAUDE_PLUGIN_ROOT` and pass `muse` as the Rally family; the wrapper uses its
Claude-shaped fallback envelope for that family. No runtime adapter or Rust wire
format changes are implied.

Do not add `.muse-plugin` at the repository root: Easy Terminal's
`PluginAgentSupport.swift` requires exactly one Muse/Codex/Claude manifest when
there is no root `plugin.json`. The root already has Claude and Codex manifests.
Muse 1.4.14's validator is more permissive about multiple manifests but warns
that symlinks prevent installation. A root portable manifest would also change
Codex discovery precedence. The separate bundle avoids both changes.

Native Muse declarations were evaluated with its validator: reusing one script
for several hooks fails with `duplicate-hook-source`, and native `matcher`
fields are rejected. The compatible format supports both without extra wrappers.

Validation on 2026-11-09, Muse Code 1.4.14 (1.4.14-R7309.1):

```text
MUSE_NO_AUTO_UPDATE=1 muse plugins validate plugins/muse --json
valid: true; manifest_family: claude-compatible
skills: 3; hooks: 4; compatibility.summary: full; diagnostics: []
```

The environment variable suppresses the launcher's automatic update check.
Validation does not install, trust, or execute the hooks. Runtime activation,
prompt delivery, and mutation checks still require a host session check.

## Cursor plugin hooks

The manifest explicitly selects `hooks/cursor.json` so Cursor does not consume
Claude's `hooks/hooks.json`. Its four events and mutation matcher come from the
same contract as `.cursor/hooks.json`. Plugin commands use the quoted
`${CURSOR_PLUGIN_ROOT}/hooks/rally-coordination-hook.sh`, including Stop, which
may execute from the workspace root. They do not require the consumer repo to
contain Rally's script. These paths and variables follow the official
[plugin reference](https://cursor.com/docs/reference/plugins) and
[hook environment contract](https://cursor.com/docs/hooks).

Cursor Agent 2026.11.05-86047df exposes `--plugin-dir <path>` in
`cursor-agent --help`. Its `cursor-agent plugin --help` lists only marketplace
management; it exposes no local plugin validator. Generator tests execute every
plugin command from an unrelated consumer directory with a stub script and a
plugin path containing spaces. This verifies path resolution, not host loading.

The user-level alternative is `~/.cursor/hooks.json` with schema `version: 1`,
the same event entries, and commands pointing to an absolute, stable copy of the
script. User hook relative paths resolve from `~/.cursor`, so the project-only
git-top-level launcher is unsuitable there. User skills can live in
`~/.agents/skills` or `~/.cursor/skills`; preserve the full skill/reference layout.
No user configuration is created by generation.

## Codex skill discovery and symlink caches

Observed on 2026-11-09 with Codex CLI 0.167.0: this session advertised only
`agent-rally-point` from `~/.agents/skills/agent-rally-point/SKILL.md`.
Neither `mini-loop` nor `rally-workflows` appeared. The plugin was enabled in
`~/.codex/config.toml`, and its cached manifest declared
`"skills": "./.codex-plugin/skills"`, containing three regular skill files.

The cache's `.codex-plugin` directory was itself a symlink to the source
checkout. Codex's local `logs_2.sqlite` recorded
`failed to load plugin: missing or invalid plugin.json` for
`agent-rally-point@canonical-local`, including `skills/list` requests.
The pinned [0.167.0 manifest discovery source](https://github.com/openai/codex/blob/rust-v0.167.0/codex-rs/utils/plugins/src/plugin_namespace.rs#L57-L64)
uses `symlink_metadata` and rejects a manifest parent that is not a regular
directory. That explains why the skills exist but are never discovered.

The generated package uses a regular manifest directory. Its declared skill and
icon paths resolve from the marketplace plugin root, matching the
[0.167.0 resolver](https://github.com/openai/codex/blob/rust-v0.167.0/codex-rs/core-plugins/src/manifest.rs#L570-L618)
and [official manifest documentation](https://developers.openai.com/plugins/deploy/submission#package-identity-and-components).
No Codex manifest correction is needed. Repairing that local cache belongs to
an explicitly authorized host-maintenance operation; generation does not repair
or modify installed caches. Discovery after such a repair remains unverified.
