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
| Cursor | `plugins/cursor` / `.cursor-plugin/plugin.json` | Bundle `hooks/cursor.json` |
| Antigravity | `plugins/antigravity` / `plugin.json` (Agent Plugins 1.0.0) | Skills only |

Muse, Cursor, and Antigravity provider records describe generation, not reconciliation.
The installed-provider reconciler manages Claude Code and Codex only.
All three bundles reuse the Muse copy path and the canonical contract's three
skills, local references, and workflow runtime. They contain regular files only.
Their complete payloads participate in the shared release digest, excluding
`rally-release.json` itself. `--check` detects edited, missing, extra, or symlinked
bundle files. Generation does not install bundles or change host configuration.

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

## Cursor bundle

Use `plugins/cursor` as the complete folder to copy for a local Cursor plugin.
Its 25 regular files include `.cursor-plugin/plugin.json`, `hooks/cursor.json`,
`hooks/rally-coordination-hook.sh`, all three skills with their references and
workflow runtime, and `rally-release.json`. The repository-root Cursor manifest
and hook declarations remain generated for existing consumers.

Cursor's [local installation documentation](https://cursor.com/docs/plugins)
says copying a plugin skips symlinks that point outside its folder. The bundle
materializes the shared payload so the workflow modules and references survive
that copy.

Read-only comparison on 2026-11-09 against
`~/.cursor/plugins/local/agent-rally-point`: the installed folder has 26 files;
the generated bundle has the same file list except `.install-source.json`.
That file records the installation source and is not a shipped capability.
Among the 25 shared files, only `rally-release.json` differs because its digest
now covers the new bundles. No installed file was changed.

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

## Antigravity bundle

Use `plugins/antigravity` directly. Its 23 regular files contain a root
`plugin.json` targeting [Agent Plugins 1.0.0](https://agent-plugins.org/plugin-authors/manifest),
all three skills, their local references and workflow runtime, and
`rally-release.json`. The portable manifest discovers the fixed `skills/`
directory; it does not declare a nonstandard `skills` list. No conversion or
installation is needed for validation.

Antigravity 1.5.3 accepts both native and Agent Plugins root manifests in
read-only validator probes. Its [plugin authoring guide](https://antigravity.google/docs/build-plugins)
also documents root `plugin.json` and `.antigravity-plugin/plugin.json`.
The repository root remains Claude-format, and validating it on 2026-11-09
returned exit 1 with one error:

```text
agy plugin validate . --json
no plugin.json found at the plugin root or in .antigravity-plugin or
.gemini-plugin. This looks like a Claude Code plugin: `plugin install`
converts it, but it doesn't load as is
```

That output identifies install-time conversion as an alternative for the
Claude-format source. No `plugin install <directory>` was run; conversion
behaviour was not exercised.

This bundle ships skills only. Antigravity itself **does support native hooks**:
its [hook documentation](https://antigravity.google/docs/hooks) lists SessionStart,
PreToolUse, PostToolUse, PreInvocation, PostInvocation, and Stop. The portable
Agent Plugins core covers skills and MCP, not native lifecycle hooks. Rally's
current wrapper does not adapt Antigravity's `conversationId`, `workspacePaths`,
`toolCall`, and `injectSteps` envelope. No Claude hook declarations or guessed
event names are carried into this bundle. Automatic coordination hooks are not
provided for Antigravity; use the skill's CLI loop.

Validation on 2026-11-09, Antigravity CLI 1.5.3:

```text
agy plugin validate plugins/antigravity --json
exit: 0; manifest: plugin.json; agentPluginsSpec: true
components: skills=3; hooks=0; all other components=0
findings: 0 errors, 1 warning, 2 notes
warning: skills/agent-rally-point/SKILL.md body is 25481 bytes;
         only the first 24000 are loaded
note: Agent Plugins schema loads in spec mode with enable-agent-plugins-spec
      enabled, otherwise as a native plugin
note: no logo; the host uses a generic icon
```

The full skill file is shipped, but this host's body limit can omit its tail
from model context. Strict validation would reject the warning; only the
requested non-strict validation passed. Installed discovery, complete skill
loading, and live session behaviour remain unverified.

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
