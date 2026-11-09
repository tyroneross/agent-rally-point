#!/usr/bin/env python3
"""Generate every shipped coding-host integration from one canonical contract."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any


CONFIG_PATH = Path("config/host-integrations.json")
# Separate Muse payload avoids ambiguous root manifests and install-rejected symlinks.
MUSE_ARTIFACT = Path("plugins/muse")
# Cursor overrides Claude's default hooks/hooks.json with its native event schema.
CURSOR_PLUGIN_HOOKS = Path("hooks/cursor.json")
IDENTITY_PATHS = (
    Path("rally-release.json"),
    Path("plugins/codex/rally-release.json"),
    Path("plugins/codex/.codex-plugin/rally-release.json"),
    MUSE_ARTIFACT / "rally-release.json",
)
JSON_SURFACES = (
    Path(".claude-plugin/plugin.json"),
    Path(".claude-plugin/marketplace.json"),
    Path(".codex-plugin/plugin.json"),
    Path(".agents/plugins/marketplace.json"),
    Path("hooks/hooks.json"),
    Path(".claude/settings.json"),
    Path(".codex/hooks.json"),
    Path(".cursor/hooks.json"),
    Path(".cursor-plugin/plugin.json"),
    CURSOR_PLUGIN_HOOKS,
)
GENERATED_DIRS = (Path("plugins/codex/.codex-plugin"), MUSE_ARTIFACT)
SKILL_ROOT = Path("skills")
# Claude Code accepts a path or an array of paths for `skills`; Muse accepts only an array.
CLAUDE_MANIFEST_SKILL_PATHS = ["./skills"]
CODEX_ARTIFACT = Path("plugins/codex/.codex-plugin")
CODEX_WORKFLOW_RUNTIME_FILES = (
    "is-main.mjs",
    "checkpoint.mjs",
    "fanout.mjs",
    "limiter.mjs",
    "packet.mjs",
    "workstream-lint.mjs",
    "workstream-status.mjs",
)
CODEX_WORKFLOW_RUNTIME_SOURCE = Path("dynamic-workflows/core")
CODEX_WORKFLOW_RUNTIME_DEST = Path("skills/rally-workflows/core")
CODEX_WORKFLOW_NOTICE_SOURCE = Path("dynamic-workflows/NOTICE")
CODEX_WORKFLOW_NOTICE_DEST = Path("skills/rally-workflows/NOTICE")
CODEX_PACKAGED_REFERENCE_FILES = (
    Path("dynamic-workflows/PROTOCOL.md"),
    Path("dynamic-workflows/COORDINATION.md"),
    Path("dynamic-workflows/MODEL-TIERS.md"),
    Path("docs/ORCHESTRATOR_SEAM.md"),
    Path("docs/SPEC-lead-agent.md"),
    Path("docs/JSON_ENVELOPE.md"),
    Path("docs/schemas/agent-rally.command.inject.v1.json"),
    Path("docs/schemas/agent-rally.fact.v1.json"),
    Path("docs/security/TRUST-MODEL.md"),
)
CODEX_REFERENCE_SKILLS = {"agent-rally-point", "rally-workflows"}


class GenerationError(RuntimeError):
    """A canonical input or generated surface is invalid."""


def json_text(value: Any) -> str:
    return json.dumps(value, indent=2, ensure_ascii=False) + "\n"


def load_config(source_root: Path) -> dict[str, Any]:
    path = source_root / CONFIG_PATH
    try:
        config = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise GenerationError(f"cannot load {path}: {exc}") from exc
    if config.get("schema") != "agent-rally.host-integrations.v1":
        raise GenerationError(f"{path}: unsupported schema {config.get('schema')!r}")
    return config


def cargo_version(source_root: Path) -> str:
    path = source_root / "crates/rally-cli/Cargo.toml"
    in_package = False
    for line in path.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if stripped == "[package]":
            in_package = True
            continue
        if in_package and stripped.startswith("["):
            break
        if in_package:
            match = re.match(r'^version\s*=\s*"([^"]+)"', stripped)
            if match:
                return match.group(1)
    raise GenerationError(f"{path}: no [package] version")


def merged_keywords(config: dict[str, Any], host: str) -> list[str]:
    plugin = config["plugin"]
    return [*plugin["keywords"], *plugin["host_keywords"].get(host, [])]


def render_plugin_surfaces(config: dict[str, Any], version: str) -> dict[Path, str]:
    plugin = config["plugin"]
    name = plugin["name"]
    author = plugin["author"]
    homepage = plugin["homepage"]
    repository = plugin["repository"]
    license_name = plugin["license"]
    claude_description = plugin["descriptions"]["claude_code"]
    codex_description = plugin["descriptions"]["codex"]
    interface = plugin["codex_interface"]

    claude_manifest = {
        "name": name,
        "description": claude_description,
        "author": author,
        "homepage": homepage,
        "repository": repository,
        "license": license_name,
        "keywords": merged_keywords(config, "claude_code"),
        "skills": list(CLAUDE_MANIFEST_SKILL_PATHS),
    }
    claude_marketplace = {
        "name": name,
        "owner": author,
        "metadata": {
            "description": (
                "Single-plugin marketplace hosting agent-rally-point — "
                "repo-local coordination surface for parallel coding agents"
            ),
            "version": version,
        },
        "plugins": [
            {
                "name": name,
                "description": claude_description,
                "author": author,
                "source": "./",
                "homepage": homepage,
                "repository": repository,
                "license": license_name,
                "category": plugin["category"],
                "keywords": merged_keywords(config, "claude_code"),
            }
        ],
    }
    codex_manifest = {
        "name": name,
        "description": codex_description,
        "author": author,
        "repository": repository,
        "license": license_name,
        "keywords": merged_keywords(config, "codex"),
        "skills": "./.codex-plugin/skills",
        "interface": {
            "displayName": plugin["display_name"],
            "shortDescription": interface["short_description"],
            "longDescription": interface["long_description"],
            "developerName": plugin["developer_name"],
            "category": "Coding",
            "composerIcon": interface["composer_icon"],
            "logo": interface["logo"],
            "capabilities": interface["capabilities"],
            "defaultPrompt": interface["default_prompt"],
        },
    }
    codex_marketplace = {
        "name": name,
        "version": version,
        "plugins": [
            {
                "name": name,
                "source": {"source": "local", "path": "./plugins/codex"},
            }
        ],
    }
    cursor_manifest = {
        "name": name,
        "version": version,
        "description": plugin["descriptions"]["cursor"],
        "author": author,
        "repository": repository,
        "license": license_name,
        "keywords": merged_keywords(config, "cursor"),
        "skills": [f"./skills/{skill}" for skill in sorted(config["skills"])],
        "hooks": "./" + CURSOR_PLUGIN_HOOKS.as_posix(),
    }
    # Muse's Claude adapter supports shared hook scripts, matchers, and async Stop.
    muse_manifest = {
        "name": name,
        "version": version,
        "description": plugin["descriptions"]["muse"],
        "skills": [f"./skills/{skill}" for skill in sorted(config["skills"])],
    }
    return {
        Path(".claude-plugin/plugin.json"): json_text(claude_manifest),
        Path(".claude-plugin/marketplace.json"): json_text(claude_marketplace),
        Path(".codex-plugin/plugin.json"): json_text(codex_manifest),
        Path(".agents/plugins/marketplace.json"): json_text(codex_marketplace),
        Path(config["providers"]["cursor"]["manifest"]): json_text(cursor_manifest),
        Path(config["providers"]["muse"]["manifest"]): json_text(muse_manifest),
    }


def command_for(host: str, source: str, phase: str, timeout_ms: int) -> str:
    allowance = hook_timeout_seconds({"phase": phase, "timeout_ms": timeout_ms}) * 1000
    declaration = f"RALLY_HOOK_HOST_TIMEOUT_MS={allowance} "
    if host in {"claude_code", "muse"} and source == "plugin":
        return (
            "RALLY_HOOK_SOURCE=plugin "
            + declaration
            + f'"${{CLAUDE_PLUGIN_ROOT}}/hooks/rally-coordination-hook.sh" {phase} {host}'
        )
    if host == "claude_code":
        return (
            "RALLY_HOOK_SOURCE=project "
            + declaration
            + f'"${{CLAUDE_PROJECT_DIR}}/hooks/rally-coordination-hook.sh" {phase} claude_code'
        )
    if host == "codex":
        return (
            "RALLY_HOOK_SOURCE=project "
            + declaration
            + 'bash "$(git rev-parse --show-toplevel 2>/dev/null || pwd)'
            f'/hooks/rally-coordination-hook.sh" {phase} codex'
        )
    if host == "cursor":
        if source == "plugin":
            return (
                "RALLY_HOOK_SOURCE=plugin "
                + declaration
                + f'bash "${{CURSOR_PLUGIN_ROOT}}/hooks/rally-coordination-hook.sh" {phase} cursor'
            )
        return (
            "RALLY_HOOK_SOURCE=project "
            + declaration
            + 'bash "$(git rev-parse --show-toplevel 2>/dev/null || pwd)'
            f'/hooks/rally-coordination-hook.sh" {phase} cursor'
        )
    raise GenerationError(f"unsupported hook host {host!r}")


def claude_hook_group(
    phase: dict[str, Any], source: str, *, include_timeout: bool, host: str = "claude_code"
) -> tuple[str, dict[str, Any]]:
    event = phase["events"][host]
    hook: dict[str, Any] = {
        "type": "command",
        "command": command_for(host, source, phase["phase"], phase["timeout_ms"]),
    }
    if include_timeout:
        hook["timeout"] = hook_timeout_seconds(phase)
    # Stop never blocks and its message is advisory, so Claude runs it in the
    # background instead of making every turn end wait on Rally reads.
    if phase.get("async", {}).get(host) is True:
        hook["async"] = True
    group: dict[str, Any] = {"hooks": [hook]}
    matcher = phase.get("matchers", {}).get(host)
    if matcher:
        group["matcher"] = matcher
    return event, group


def codex_hook_group(phase: dict[str, Any]) -> tuple[str, dict[str, Any]]:
    event = phase["events"]["codex"]
    hook = {
        "type": "command",
        "timeout": hook_timeout_seconds(phase),
        "command": command_for("codex", "project", phase["phase"], phase["timeout_ms"]),
    }
    group: dict[str, Any] = {"hooks": [hook]}
    matcher = phase.get("matchers", {}).get("codex")
    if matcher:
        group["matcher"] = matcher
    return event, group


def hook_timeout_seconds(phase: dict[str, Any]) -> int:
    """Convert the canonical millisecond value to exact host-schema seconds."""
    timeout_ms = phase.get("timeout_ms")
    if (
        isinstance(timeout_ms, bool)
        or not isinstance(timeout_ms, int)
        or timeout_ms <= 0
        or timeout_ms % 1000 != 0
    ):
        raise GenerationError(
            f"hook phase {phase.get('phase')!r} timeout_ms must be a positive whole second"
        )
    return timeout_ms // 1000


def native_effects(config: dict[str, Any]) -> dict[str, list[str]]:
    """Validate and return the wrapper's named native-tool effect registry."""

    effects = config.get("hooks", {}).get("native_effects")
    required = ("pure_read", "opaque_shell", "mutation")
    if not isinstance(effects, dict) or set(effects) != set(required):
        raise GenerationError(
            "hooks.native_effects must define exactly pure_read, opaque_shell, mutation"
        )

    normalized: dict[str, list[str]] = {}
    seen: dict[str, str] = {}
    for effect in required:
        tools = effects.get(effect)
        if not isinstance(tools, list) or not tools:
            raise GenerationError(f"hooks.native_effects.{effect} must be a non-empty list")
        if any(not isinstance(tool, str) or not tool.strip() for tool in tools):
            raise GenerationError(
                f"hooks.native_effects.{effect} contains an empty or non-string tool"
            )
        normalized[effect] = list(tools)
        for tool in tools:
            key = tool.lower()
            if key in seen:
                raise GenerationError(
                    f"native tool {tool!r} appears in both {seen[key]} and {effect}"
                )
            seen[key] = effect
    return normalized


def render_hook_surfaces(config: dict[str, Any]) -> dict[Path, str]:
    phases = config["hooks"]["phases"]
    effects = native_effects(config)
    claude_plugin_hooks: dict[str, list[dict[str, Any]]] = {}
    claude_project_hooks: dict[str, list[dict[str, Any]]] = {}
    codex_hooks: dict[str, list[dict[str, Any]]] = {}
    cursor_hooks: dict[str, list[dict[str, Any]]] = {}
    cursor_plugin_hooks: dict[str, list[dict[str, Any]]] = {}
    muse_hooks: dict[str, list[dict[str, Any]]] = {}

    for phase in phases:
        events = phase["events"]
        if "claude_code" in events:
            event, group = claude_hook_group(phase, "plugin", include_timeout=True)
            claude_plugin_hooks.setdefault(event, []).append(group)
            event, group = claude_hook_group(phase, "project", include_timeout=True)
            claude_project_hooks.setdefault(event, []).append(group)
        if "muse" in events:
            event, group = claude_hook_group(phase, "plugin", include_timeout=True, host="muse")
            muse_hooks.setdefault(event, []).append(group)
        if "codex" in events:
            event, group = codex_hook_group(phase)
            codex_hooks.setdefault(event, []).append(group)
        if "cursor" in events:
            cursor_event = events["cursor"]
            entry: dict[str, Any] = {
                "command": command_for("cursor", "project", phase["phase"], phase["timeout_ms"]),
                "timeout": hook_timeout_seconds(phase),
            }
            matcher = phase.get("matchers", {}).get("cursor")
            if matcher:
                entry["matcher"] = matcher
            cursor_hooks.setdefault(cursor_event, []).append(entry)
            cursor_plugin_hooks.setdefault(cursor_event, []).append({
                **entry,
                "command": command_for("cursor", "plugin", phase["phase"], phase["timeout_ms"]),
            })

    plugin_comment = (
        "GENERATED from config/host-integrations.json. Claude plugin hooks are "
        "auto-loaded at this standard path. Project and plugin registration may "
        "both be active; the canonical hook counts registration sources for an "
        "identical event while preserving same-source repeats. Claude keeps edit-scoped "
        "PreToolUse. Codex keeps its matcher unset pending captured native matcher "
        "evidence; the wrapper classifies named reads, opaque shell tools, and mutations "
        "before repo or Rally resolution."
    )
    project_comment = (
        "GENERATED from config/host-integrations.json. Portable project hooks "
        "work without global settings. Project and installed-plugin registration "
        "may overlap; the canonical hook counts registration sources for an "
        "identical event while preserving same-source repeats. "
        "Claude keeps edit-scoped PreToolUse. Codex keeps its matcher unset pending "
        "captured native matcher evidence; the wrapper classifies named reads, opaque "
        "shell tools, and mutations before repo or Rally resolution. See "
        "docs/AUTO-COORDINATION-HOOKS.md."
    )
    codex_comment = (
        "GENERATED from config/host-integrations.json. Codex PreToolUse has no matcher "
        "until captured native matcher evidence proves a stable filter. The wrapper "
        f"classifies {sum(len(tools) for tools in effects.values())} named tools before "
        "repo or Rally resolution: pure reads and opaque shell tools return {}, while "
        "mutations receive path-scoped checks. See docs/AUTO-COORDINATION-HOOKS.md."
    )
    cursor_comment = (
        "GENERATED from config/host-integrations.json. sessionStart injects "
        "additional_context; beforeSubmitPrompt runs idle side effects with no "
        "model channel; preToolUse matches Write|StrReplace|Delete|EditNotebook "
        "and can inject agent_message. See docs/AUTO-COORDINATION-HOOKS.md."
    )
    return {
        Path("hooks/hooks.json"): json_text(
            {"$comment": plugin_comment, "hooks": claude_plugin_hooks}
        ),
        Path(".claude/settings.json"): json_text(
            {"$comment": project_comment, "hooks": claude_project_hooks}
        ),
        Path(".codex/hooks.json"): json_text(
            {"description": codex_comment, "hooks": codex_hooks}
        ),
        Path(".cursor/hooks.json"): json_text(
            {"$comment": cursor_comment, "version": 1, "hooks": cursor_hooks}
        ),
        CURSOR_PLUGIN_HOOKS: json_text({"version": 1, "hooks": cursor_plugin_hooks}),
        MUSE_ARTIFACT / "hooks/hooks.json": json_text({"hooks": muse_hooks}),
    }


def split_frontmatter(text: str, path: Path) -> tuple[str, str]:
    if not text.startswith("---\n"):
        raise GenerationError(f"{path}: missing YAML frontmatter")
    end = text.find("\n---\n", 4)
    if end < 0:
        raise GenerationError(f"{path}: unterminated YAML frontmatter")
    return text[4:end], text[end + 5 :]


def render_skill(
    source_root: Path, config: dict[str, Any], skill_id: str, host: str
) -> str:
    path = source_root / SKILL_ROOT / skill_id / "SKILL.md"
    _, body = split_frontmatter(path.read_text(encoding="utf-8"), path)
    metadata = dict(config["skills"][skill_id])
    overlays = metadata.pop("overlays", {})
    metadata.update(overlays.get(host, {}))
    lines = ["---"]
    for key in ("name", "description"):
        value = metadata[key]
        if "\n" in value:
            raise GenerationError(f"{path}: multiline {key} is unsupported")
        lines.append(f"{key}: {value}")
    # `user-invocable: false` keeps a sub-skill out of the host's slash menu, so
    # a plugin exposes one router rather than its internals. It lives in the
    # contract because this generator rewrites the whole frontmatter block: a
    # hand edit to SKILL.md survives exactly until the next regeneration, which
    # is how the flag added in 7a7dc04 was silently reverted here.
    if metadata.get("user-invocable") is False:
        lines.append("user-invocable: false")
    lines.extend(["---", "", body.lstrip("\n")])
    return "\n".join(lines)


def write_text(path: Path, content: str) -> None:
    # Unchanged host settings may be read-only; regeneration needs no write there.
    if path.is_file() and path.read_bytes() == content.encode("utf-8"):
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def validate_codex_package_paths(plugin_root: Path) -> None:
    """Require every manifest-declared local capability path to resolve.

    Codex resolves these paths from the marketplace plugin root, not from the
    directory containing ``plugin.json``. A package can therefore install and
    appear enabled while exposing no skills when a declared path is wrong.
    """
    root = plugin_root.resolve()
    manifest_path = plugin_root / ".codex-plugin/plugin.json"
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise GenerationError(f"cannot load {manifest_path}: {exc}") from exc

    declared_paths = {"skills": manifest.get("skills")}
    interface = manifest.get("interface") or {}
    for key in ("composerIcon", "logo"):
        if key in interface:
            declared_paths[f"interface.{key}"] = interface[key]
    for field, declared in declared_paths.items():
        if not isinstance(declared, str) or not declared:
            raise GenerationError(f"{manifest_path}: missing {field} path")
        candidate = (plugin_root / declared).resolve()
        try:
            candidate.relative_to(root)
        except ValueError as exc:
            raise GenerationError(
                f"{manifest_path}: {field} escapes plugin root: {declared}"
            ) from exc
        if not candidate.exists():
            raise GenerationError(
                f"{manifest_path}: {field} does not resolve from plugin root: "
                f"{declared}"
            )


def validate_artifact_dest(
    dest: Path,
    source_root: Path,
    dest_root: Path,
) -> Path:
    resolved = dest.resolve()
    forbidden = {
        Path("/"),
        Path.home().resolve(),
        (Path.home() / ".codex-plugin").resolve(),
        source_root.resolve(),
        dest_root.resolve(),
    }
    if dest.name != ".codex-plugin":
        raise GenerationError(
            f"refusing artifact destination without .codex-plugin basename: {dest}"
        )
    if resolved in forbidden:
        raise GenerationError(f"refusing broad artifact destination: {resolved}")
    return resolved


def copy_codex_artifact(
    source_root: Path,
    dest_root: Path,
    config: dict[str, Any],
    codex_manifest: str,
    *,
    artifact_dest: Path | None = None,
) -> Path:
    src = source_root / ".codex-plugin"
    dest = validate_artifact_dest(
        artifact_dest or dest_root / CODEX_ARTIFACT,
        source_root,
        dest_root,
    )
    if dest.exists():
        shutil.rmtree(dest)
    dest.mkdir(parents=True)
    for source in sorted(src.rglob("*")):
        relative = source.relative_to(src)
        if ".DS_Store" in relative.parts or relative == Path("plugin.json"):
            continue
        target = dest / relative
        if source.is_symlink():
            resolved = source.resolve(strict=True)
            if resolved.is_dir():
                shutil.copytree(
                    resolved,
                    target,
                    ignore=shutil.ignore_patterns(".DS_Store"),
                )
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(resolved, target)
        elif source.is_dir():
            target.mkdir(parents=True, exist_ok=True)
        else:
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, target)
    write_text(dest / "plugin.json", codex_manifest)
    for skill_id in sorted(config["skills"]):
        write_text(
            dest / "skills" / skill_id / "SKILL.md",
            render_skill(source_root, config, skill_id, "codex"),
        )
    if "rally-workflows" in config["skills"]:
        for filename in CODEX_WORKFLOW_RUNTIME_FILES:
            source = source_root / CODEX_WORKFLOW_RUNTIME_SOURCE / filename
            target = dest / CODEX_WORKFLOW_RUNTIME_DEST / filename
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, target)
        notice_target = dest / CODEX_WORKFLOW_NOTICE_DEST
        notice_target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source_root / CODEX_WORKFLOW_NOTICE_SOURCE, notice_target)
    if CODEX_REFERENCE_SKILLS.intersection(config["skills"]):
        for relative in CODEX_PACKAGED_REFERENCE_FILES:
            source = source_root / relative
            target = dest / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            if source.suffix == ".md":
                # npm docs use public URLs; the host artifact has these references
                # locally and must keep them reachable without a network request.
                text = source.read_text(encoding="utf-8")
                bundled = [*CODEX_PACKAGED_REFERENCE_FILES]
                bundled.extend(SKILL_ROOT / skill / "SKILL.md" for skill in config["skills"])
                for reference in bundled:
                    public = f"https://github.com/tyroneross/agent-rally-point/blob/main/{reference.as_posix()}"
                    local = os.path.relpath(reference, relative.parent)
                    text = text.replace(f"]({public})", f"]({local})")
                    text = text.replace(f"]({public}#", f"]({local}#")
                write_text(target, text)
            else:
                shutil.copy2(source, target)
    validate_codex_package_paths(dest.parent)
    return dest


def copy_muse_artifact(
    source_root: Path,
    dest_root: Path,
    config: dict[str, Any],
    codex_artifact: Path,
    manifest: str,
    hooks: str,
) -> Path:
    """Reuse the self-contained skill/reference payload without Codex metadata."""
    dest = dest_root / config["providers"]["muse"]["plugin_root"]
    if dest.exists():
        shutil.rmtree(dest)
    for directory in ("skills", "docs", "dynamic-workflows"):
        shutil.copytree(codex_artifact / directory, dest / directory)
    for skill in sorted(config["skills"]):
        write_text(dest / SKILL_ROOT / skill / "SKILL.md", render_skill(source_root, config, skill, "muse"))
    write_text(dest / ".claude-plugin/plugin.json", manifest)
    write_text(dest / "hooks/hooks.json", hooks)
    shutil.copy2(source_root / "hooks/rally-coordination-hook.sh", dest / "hooks/rally-coordination-hook.sh")
    return dest


def digest_inputs(
    surfaces: dict[Path, str],
    skills: dict[Path, str],
    codex_artifact: Path,
    *,
    bundles: dict[Path, Path] | None = None,
) -> str:
    digest = hashlib.sha256()
    for path, content in sorted({**surfaces, **skills}.items(), key=lambda item: str(item[0])):
        digest.update(str(path).encode())
        digest.update(b"\0")
        digest.update(content.encode())
        digest.update(b"\0")
    for prefix, artifact in {CODEX_ARTIFACT: codex_artifact, **(bundles or {})}.items():
        if not artifact.is_dir():
            continue
        for path in sorted(
            p
            for p in artifact.rglob("*")
            if p.is_file()
            and p.name not in {".DS_Store", "rally-release.json"}
        ):
            digest.update(
                str(prefix / path.relative_to(artifact)).encode()
            )
            digest.update(b"\0")
            digest.update(path.read_bytes())
            digest.update(b"\0")
    return f"sha256:{digest.hexdigest()}"


def generate(
    source_root: Path,
    dest_root: Path,
    *,
    artifact_only: bool = False,
    artifact_dest: Path | None = None,
) -> None:
    config = load_config(source_root)
    version = cargo_version(source_root)
    plugin_surfaces = render_plugin_surfaces(config, version)

    if artifact_only:
        artifact = copy_codex_artifact(
            source_root,
            dest_root,
            config,
            plugin_surfaces[Path(".codex-plugin/plugin.json")],
            artifact_dest=artifact_dest,
        )
        identity_source = source_root / "rally-release.json"
        if not identity_source.is_file():
            raise GenerationError(
                f"{identity_source}: release identity is required for artifact-only generation"
            )
        write_text(artifact / "rally-release.json", identity_source.read_text(encoding="utf-8"))
        return

    surfaces = {**plugin_surfaces, **render_hook_surfaces(config)}
    skills = {
        SKILL_ROOT / skill_id / "SKILL.md": render_skill(
            source_root, config, skill_id, "claude_code"
        )
        for skill_id in sorted(config["skills"])
    }
    for path, content in {**surfaces, **skills}.items():
        write_text(dest_root / path, content)
    artifact = copy_codex_artifact(
        source_root,
        dest_root,
        config,
        surfaces[Path(".codex-plugin/plugin.json")],
        artifact_dest=artifact_dest,
    )
    muse_artifact = copy_muse_artifact(
        source_root, dest_root, config, artifact,
        surfaces[MUSE_ARTIFACT / ".claude-plugin/plugin.json"],
        surfaces[MUSE_ARTIFACT / "hooks/hooks.json"],
    )

    identity = {
        "schema": "agent-rally.release-identity.v1",
        "version": version,
        "canonical_provider": config["providers"]["claude_code"]["canonical_id"],
        "source": config["providers"]["claude_code"]["source"],
        "generated_surface_digest": digest_inputs(
            surfaces,
            skills,
            artifact,
            bundles={MUSE_ARTIFACT: muse_artifact},
        ),
    }
    identity_text = json_text(identity)
    for path in IDENTITY_PATHS:
        write_text(dest_root / path, identity_text)


# Upper bound for one `git check-ignore` call during a surface check.
GIT_CHECK_IGNORE_TIMEOUT_SECONDS = 10


def git_ignored(directory: Path, paths: list[Path]) -> set[Path]:
    """Return the members of ``paths`` that git ignores.

    A host can write its own files into a generated directory (Codex writes
    ``migrated-command-skills/`` beside the plugin manifest). An ignored file
    cannot be committed or released, so it is not generator drift. Tracked files
    are never ignored. Without git, or outside a repository, nothing is treated
    as ignored and every extra file is still reported.
    """
    if not paths:
        return set()
    try:
        result = subprocess.run(
            ["git", "-C", str(directory), "check-ignore", "-z", "--stdin"],
            input="\0".join(str(path) for path in paths),
            capture_output=True,
            text=True,
            timeout=GIT_CHECK_IGNORE_TIMEOUT_SECONDS,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return set()
    # 0 = some ignored, 1 = none ignored; anything else is a git failure.
    if result.returncode not in (0, 1):
        return set()
    return {Path(item) for item in result.stdout.split("\0") if item}


def compare_path(actual: Path, expected: Path) -> list[str]:
    if not actual.exists() and not actual.is_symlink():
        return [f"missing: {actual}"]
    if actual.is_dir() and not actual.is_symlink():
        actual_files = {
            p.relative_to(actual)
            for p in actual.rglob("*")
            if p.is_file() or p.is_symlink()
        }
        expected_files = {
            p.relative_to(expected)
            for p in expected.rglob("*")
            if p.is_file() or p.is_symlink()
        }
        extra = [actual / path for path in sorted(actual_files - expected_files)]
        ignored = git_ignored(actual, extra)
        findings = [
            *(f"extra: {path}" for path in extra if path not in ignored),
            *(f"missing: {actual / path}" for path in sorted(expected_files - actual_files)),
        ]
        for path in sorted(actual_files & expected_files):
            if (actual / path).read_bytes() != (expected / path).read_bytes():
                findings.append(f"drift: {actual / path}")
        return findings
    if actual.read_bytes() != expected.read_bytes():
        return [f"drift: {actual}"]
    return []


def check(source_root: Path) -> list[str]:
    with tempfile.TemporaryDirectory(prefix="rally-host-surfaces-") as tmp:
        expected_root = Path(tmp)
        generate(source_root, expected_root)
        paths = [
            *JSON_SURFACES,
            *IDENTITY_PATHS,
            *GENERATED_DIRS,
            *(
                SKILL_ROOT / skill_id / "SKILL.md"
                for skill_id in sorted(load_config(source_root)["skills"])
            ),
        ]
        findings: list[str] = []
        for path in paths:
            findings.extend(compare_path(source_root / path, expected_root / path))
        return findings


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        type=Path,
        default=Path(__file__).resolve().parent.parent,
        help="Canonical agent-rally-point checkout",
    )
    parser.add_argument(
        "--dest",
        type=Path,
        help="Destination root; defaults to --root",
    )
    parser.add_argument(
        "--artifact-dest",
        type=Path,
        help="Exact Codex artifact directory (used by parity checks)",
    )
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--check", action="store_true", help="Fail on generated drift")
    mode.add_argument(
        "--artifact-only",
        action="store_true",
        help="Regenerate only plugins/codex/.codex-plugin",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    source_root = args.root.resolve()
    if args.check:
        findings = check(source_root)
        if findings:
            print("generate_host_surfaces: generated surfaces are stale", file=sys.stderr)
            for finding in findings:
                print(f"  {finding}", file=sys.stderr)
            return 1
        print("generate_host_surfaces: all generated surfaces are current")
        return 0
    dest_root = (args.dest or source_root).resolve()
    artifact_dest = args.artifact_dest.resolve() if args.artifact_dest else None
    generate(
        source_root,
        dest_root,
        artifact_only=args.artifact_only,
        artifact_dest=artifact_dest,
    )
    print(
        "generate_host_surfaces: "
        + ("Codex artifact current" if args.artifact_only else "host surfaces current")
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
