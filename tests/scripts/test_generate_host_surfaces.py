#!/usr/bin/env python3
"""Tests for the canonical host-surface generator."""

from __future__ import annotations

import importlib.util
import json
import re
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "generate_host_surfaces", ROOT / "scripts/generate_host_surfaces.py"
)
assert SPEC and SPEC.loader
GEN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GEN)


def snapshot(root: Path) -> dict[str, bytes]:
    return {
        str(path.relative_to(root)): path.read_bytes()
        for path in root.rglob("*")
        if path.is_file()
    }


def local_markdown_links(markdown: str) -> list[str]:
    links: list[str] = []
    for match in re.finditer(r"!?\[[^\]]*\]\(([^)]+)\)", markdown):
        target = match.group(1).strip()
        if target.startswith("<") and ">" in target:
            target = target[1 : target.index(">")]
        else:
            target = target.split(maxsplit=1)[0]
        if target.startswith(("#", "http://", "https://", "mailto:")):
            continue
        links.append(target.split("#", 1)[0])
    return links


class GenerateHostSurfacesTests(unittest.TestCase):
    def test_cargo_version_ignores_brackets_inside_package_values(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cargo = root / "crates/rally-cli/Cargo.toml"
            cargo.parent.mkdir(parents=True)
            cargo.write_text(
                '[package]\nauthors = ["A [Team]"]\nversion = "9.8.7"\n'
                "\n[dependencies]\nserde = \"1\"\n",
                encoding="utf-8",
            )
            self.assertEqual(GEN.cargo_version(root), "9.8.7")

    def test_user_invocable_false_survives_regeneration(self) -> None:
        """A sub-skill hidden from the host slash menu must stay hidden.

        `render_skill` rewrites the whole frontmatter block from the contract, so
        a `user-invocable: false` added by hand to SKILL.md survives exactly
        until the next regeneration. That is how the flag added in 7a7dc04 was
        silently reverted: the generator emitted only `name` and `description`,
        and the drift check then reported the HAND edit as the stale side.

        Reds if the key is dropped from `render_skill` or from the contract.
        """
        config = GEN.load_config(ROOT)
        hidden = [
            skill_id
            for skill_id, meta in config["skills"].items()
            if meta.get("user-invocable") is False
        ]
        self.assertTrue(
            hidden,
            "the contract must still declare which sub-skills stay out of the menu",
        )
        for skill_id in hidden:
            for host in ("claude_code", "codex"):
                rendered = GEN.render_skill(ROOT, config, skill_id, host)
                self.assertIn(
                    "user-invocable: false",
                    rendered.split("---")[1],
                    f"{skill_id} ({host}) must render the hidden flag in its frontmatter",
                )
            on_disk = (ROOT / GEN.SKILL_ROOT / skill_id / "SKILL.md").read_text(
                encoding="utf-8"
            )
            self.assertIn("user-invocable: false", on_disk.split("---")[1])

    def test_repository_surfaces_are_current(self) -> None:
        self.assertEqual(GEN.check(ROOT), [])

    def test_native_effect_registry_matches_hook_classifier(self) -> None:
        config = GEN.load_config(ROOT)
        registry = GEN.native_effects(config)
        hook = (ROOT / "hooks/rally-coordination-hook.sh").read_text(encoding="utf-8")
        shell_names = {
            "pure_read": "_RALLY_NATIVE_PURE_READ_TOOLS",
            "opaque_shell": "_RALLY_NATIVE_OPAQUE_SHELL_TOOLS",
            "mutation": "_RALLY_NATIVE_MUTATION_TOOLS",
        }
        seen: set[str] = set()
        for effect, shell_name in shell_names.items():
            match = re.search(
                rf"^{re.escape(shell_name)}='([^']+)'$",
                hook,
                flags=re.MULTILINE,
            )
            self.assertIsNotNone(match, f"hook is missing {shell_name}")
            hook_tools = json.loads(match.group(1))
            self.assertEqual(hook_tools, registry[effect])
            lowered = {tool.lower() for tool in hook_tools}
            self.assertTrue(seen.isdisjoint(lowered), f"overlapping effect tools: {effect}")
            seen.update(lowered)

        # Rust owns the native classification; pin its consts to the same
        # registry so the shell arrays above and hook_runtime.rs cannot
        # silently diverge (build plan 2026-08-15, chunk C4).
        rust_names = {
            "pure_read": "PURE_READ_TOOLS",
            "opaque_shell": "OPAQUE_SHELL_TOOLS",
            "mutation": "MUTATION_TOOLS",
        }
        hook_runtime = (
            ROOT / "crates/rally-cli/src/hook_runtime.rs"
        ).read_text(encoding="utf-8")
        for effect, const_name in rust_names.items():
            match = re.search(
                r"^pub\(crate\) const "
                rf"(?:{const_name}): &\[&str\] = &\[(.*)\];$",
                hook_runtime,
                flags=re.MULTILINE,
            )
            self.assertIsNotNone(
                match, f"hook_runtime.rs is missing {const_name}"
            )
            rust_tools = json.loads(f"[{match.group(1)}]")
            self.assertEqual(
                rust_tools,
                registry[effect],
                f"hook_runtime.rs {const_name} diverges from "
                "config/host-integrations.json",
            )

        max_targets_match = re.search(
            r"^pub\(crate\) const MAX_TARGETS: usize = (\d+);$",
            hook_runtime,
            flags=re.MULTILINE,
        )
        self.assertIsNotNone(
            max_targets_match, "hook_runtime.rs is missing MAX_TARGETS"
        )
        self.assertEqual(max_targets_match.group(1), "16")

    def test_codex_before_write_uses_wrapper_until_native_matcher_is_proven(self) -> None:
        config = GEN.load_config(ROOT)
        codex = json.loads(GEN.render_hook_surfaces(config)[Path(".codex/hooks.json")])
        description = codex.get("description", "")
        self.assertIn("native matcher evidence", description)
        self.assertIn("wrapper classifies", description)
        groups = codex["hooks"]["PreToolUse"]
        before_write = [
            group
            for group in groups
            if any("before-write codex" in hook.get("command", "") for hook in group["hooks"])
        ]
        self.assertEqual(len(before_write), 1)
        self.assertNotIn("matcher", before_write[0])

    def test_codex_hook_surface_matches_01443_schema_and_timeout_units(self) -> None:
        config = GEN.load_config(ROOT)
        codex = json.loads(GEN.render_hook_surfaces(config)[Path(".codex/hooks.json")])
        self.assertEqual(set(codex), {"description", "hooks"})

        expected = {
            phase["events"]["codex"]: phase["timeout_ms"] // 1000
            for phase in config["hooks"]["phases"]
            if "codex" in phase["events"]
        }
        actual = {
            event: groups[0]["hooks"][0]["timeout"]
            for event, groups in codex["hooks"].items()
        }
        self.assertEqual(actual, expected)

    def test_claude_hook_surfaces_use_documented_second_timeouts(self) -> None:
        config = GEN.load_config(ROOT)
        surfaces = GEN.render_hook_surfaces(config)
        expected = {
            phase["events"]["claude_code"]: phase["timeout_ms"] // 1000
            for phase in config["hooks"]["phases"]
            if "claude_code" in phase["events"]
        }
        for path in (Path("hooks/hooks.json"), Path(".claude/settings.json")):
            surface = json.loads(surfaces[path])
            actual = {
                event: groups[0]["hooks"][0]["timeout"]
                for event, groups in surface["hooks"].items()
            }
            self.assertEqual(actual, expected, str(path))

    def test_generation_is_deterministic_and_artifact_is_self_contained(self) -> None:
        with tempfile.TemporaryDirectory() as first_tmp, tempfile.TemporaryDirectory() as second_tmp:
            first = Path(first_tmp)
            second = Path(second_tmp)
            GEN.generate(ROOT, first)
            GEN.generate(ROOT, second)
            self.assertEqual(snapshot(first), snapshot(second))
            artifact = first / "plugins/codex/.codex-plugin"
            self.assertTrue(
                (artifact / "skills/rally-workflows/references/decomposition.md").is_file()
            )
            runtime = artifact / GEN.CODEX_WORKFLOW_RUNTIME_DEST
            self.assertEqual(
                {path.name for path in runtime.iterdir() if path.is_file()},
                set(GEN.CODEX_WORKFLOW_RUNTIME_FILES),
            )
            for filename in GEN.CODEX_WORKFLOW_RUNTIME_FILES:
                self.assertEqual(
                    (runtime / filename).read_bytes(),
                    (
                        ROOT
                        / GEN.CODEX_WORKFLOW_RUNTIME_SOURCE
                        / filename
                    ).read_bytes(),
                )
            self.assertEqual(
                (artifact / GEN.CODEX_WORKFLOW_NOTICE_DEST).read_bytes(),
                (ROOT / GEN.CODEX_WORKFLOW_NOTICE_SOURCE).read_bytes(),
            )
            for relative in GEN.CODEX_PACKAGED_REFERENCE_FILES:
                self.assertEqual(
                    (artifact / relative).read_bytes(),
                    (ROOT / relative).read_bytes(),
                )
            skill_text = (
                artifact / "skills/rally-workflows/SKILL.md"
            ).read_text(encoding="utf-8")
            self.assertIn(
                'node "$RALLY_FLOW_CORE/workstream-lint.mjs"',
                skill_text,
            )
            self.assertIn(
                'node "$RALLY_FLOW_CORE/packet.mjs"',
                skill_text,
            )
            self.assertFalse(any(path.is_symlink() for path in artifact.rglob("*")))

            fanout_url = (runtime / "fanout.mjs").resolve().as_uri()
            limiter_url = (runtime / "limiter.mjs").resolve().as_uri()
            script = f"""
                import {{ liveAgentsFromRoom, resolveFanout }} from {json.dumps(fanout_url)};
                import {{ createLimiter }} from {json.dumps(limiter_url)};
                const live = liveAgentsFromRoom({{
                  data: {{ room: {{ squads: [], composition: {{ over_budget: true }} }} }},
                }});
                const result = resolveFanout({{
                  readyTasks: 8,
                  roomOverBudget: live.over_budget,
                }});
                const run = createLimiter(result.effective_max);
                const limited = await run(async () => "packaged-runtime-ok");
                process.stdout.write(JSON.stringify({{ result, limited }}));
            """
            imported = subprocess.run(
                ["node", "--input-type=module", "--eval", script],
                check=True,
                capture_output=True,
                text=True,
            )
            imported_result = json.loads(imported.stdout)
            result = imported_result["result"]
            self.assertEqual(result["effective_max"], 1)
            self.assertEqual(result["limiting_factors"], ["room_output_pressure"])
            self.assertEqual(imported_result["limited"], "packaged-runtime-ok")

    def test_all_reachable_packaged_markdown_links_resolve_inside_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp)
            GEN.generate(ROOT, dest)
            artifact = (dest / GEN.CODEX_ARTIFACT).resolve()
            unresolved = []
            pending = [
                skill.resolve()
                for skill in sorted((artifact / "skills").glob("*/SKILL.md"))
            ]
            visited: set[Path] = set()
            local_edges = 0
            while pending:
                document = pending.pop(0)
                if document in visited:
                    continue
                visited.add(document)
                markdown = document.read_text(encoding="utf-8")
                for target in local_markdown_links(markdown):
                    local_edges += 1
                    if not target:
                        unresolved.append(
                            f"{document.relative_to(artifact)} -> empty local target"
                        )
                        continue
                    resolved = (document.parent / target).resolve()
                    try:
                        relative = resolved.relative_to(artifact)
                    except ValueError:
                        unresolved.append(
                            f"{document.relative_to(artifact)} -> {target} escapes artifact"
                        )
                        continue
                    if not resolved.exists():
                        unresolved.append(
                            f"{document.relative_to(artifact)} -> {relative} is missing"
                        )
                        continue
                    if resolved.suffix.lower() == ".md" and resolved not in visited:
                        pending.append(resolved)
            self.assertEqual(unresolved, [])
            self.assertEqual(len(visited), 11)
            self.assertEqual(local_edges, 18)

    def test_packaged_workflow_commands_run_from_empty_consumer_repo(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "generated"
            GEN.generate(ROOT, dest)
            runtime = (
                dest
                / GEN.CODEX_ARTIFACT
                / GEN.CODEX_WORKFLOW_RUNTIME_DEST
            ).resolve()

            consumer = Path(tmp) / "consumer"
            consumer.mkdir()
            subprocess.run(
                ["git", "init", "--quiet"],
                cwd=consumer,
                check=True,
                capture_output=True,
                text=True,
            )
            descriptor = consumer / "workstream.json"
            descriptor.write_text(
                json.dumps(
                    {
                        "workstream": "packaged-smoke",
                        "description": "prove package commands ignore consumer cwd",
                        "tasks": [
                            {
                                "id": "p01",
                                "intent": "verify packaged workflow tools",
                                "owns": "read-only",
                                "validation": "Confirm the rendered packet is present.",
                                "validation_recipe": "none",
                                "output": "one rendered task packet",
                            }
                        ],
                    }
                ),
                encoding="utf-8",
            )

            linted = subprocess.run(
                ["node", str(runtime / "workstream-lint.mjs"), str(descriptor)],
                cwd=consumer,
                check=True,
                capture_output=True,
                text=True,
            )
            self.assertIn("workstream descriptor is valid", linted.stdout)

            packet = subprocess.run(
                [
                    "node",
                    str(runtime / "packet.mjs"),
                    str(descriptor),
                    "--run",
                    "packaged-smoke",
                    "--task",
                    "p01",
                ],
                cwd=consumer,
                check=True,
                capture_output=True,
                text=True,
            )
            self.assertIn("# Task packet — p01", packet.stdout)

    def test_codex_manifest_paths_resolve_from_marketplace_plugin_root(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp)
            GEN.generate(ROOT, dest)
            plugin_root = dest / "plugins/codex"
            manifest = json.loads(
                (plugin_root / ".codex-plugin/plugin.json").read_text()
            )

            declared = [
                manifest["skills"],
                manifest["interface"]["composerIcon"],
                manifest["interface"]["logo"],
            ]
            for relative in declared:
                resolved = (plugin_root / relative).resolve(strict=True)
                resolved.relative_to(plugin_root.resolve())

            self.assertTrue(
                (
                    plugin_root
                    / manifest["skills"]
                    / "agent-rally-point/SKILL.md"
                ).is_file()
            )

    def test_release_identity_uses_cargo_version_and_content_digest(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp)
            GEN.generate(ROOT, dest)
            identity = json.loads((dest / "rally-release.json").read_text())
            self.assertEqual(identity["version"], GEN.cargo_version(ROOT))
            self.assertRegex(
                identity["generated_surface_digest"], r"^sha256:[0-9a-f]{64}$"
            )
            self.assertEqual(
                (dest / "rally-release.json").read_bytes(),
                (dest / "plugins/codex/rally-release.json").read_bytes(),
            )
            self.assertEqual(
                (dest / "rally-release.json").read_bytes(),
                (
                    dest
                    / "plugins/codex/.codex-plugin/rally-release.json"
                ).read_bytes(),
            )

    def test_compare_path_reports_content_drift(self) -> None:
        with tempfile.TemporaryDirectory() as actual_tmp, tempfile.TemporaryDirectory() as expected_tmp:
            actual = Path(actual_tmp) / "surface.json"
            expected = Path(expected_tmp) / "surface.json"
            actual.write_text('{"version":"old"}\n')
            expected.write_text('{"version":"new"}\n')
            self.assertEqual(GEN.compare_path(actual, expected), [f"drift: {actual}"])

    def test_artifact_destination_rejects_broad_or_misnamed_paths(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with self.assertRaises(GEN.GenerationError):
                GEN.validate_artifact_dest(Path("/"), ROOT, root)
            with self.assertRaises(GEN.GenerationError):
                GEN.validate_artifact_dest(root / "artifact", ROOT, root)
            with self.assertRaises(GEN.GenerationError):
                GEN.validate_artifact_dest(Path.home() / ".codex-plugin", ROOT, root)

    def test_artifact_only_generation_includes_release_identity(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            artifact = Path(tmp) / ".codex-plugin"
            GEN.generate(ROOT, Path(tmp), artifact_only=True, artifact_dest=artifact)
            self.assertEqual(
                (artifact / "rally-release.json").read_bytes(),
                (ROOT / "rally-release.json").read_bytes(),
            )

    def test_artifact_copy_strips_ds_store_from_symlinked_skill_tree(self) -> None:
        with tempfile.TemporaryDirectory() as source_tmp, tempfile.TemporaryDirectory() as dest_tmp:
            source = Path(source_tmp)
            skill = source / "skills/demo"
            skill.mkdir(parents=True)
            (skill / "SKILL.md").write_text(
                "---\nname: demo\ndescription: demo\n---\n\nbody\n",
                encoding="utf-8",
            )
            (skill / ".DS_Store").write_text("cruft", encoding="utf-8")
            (skill / "reference.md").write_text("reference", encoding="utf-8")
            plugin_skills = source / ".codex-plugin/skills"
            plugin_skills.mkdir(parents=True)
            (plugin_skills / "demo").symlink_to("../../skills/demo")
            config = {
                "skills": {
                    "demo": {
                        "name": "demo",
                        "description": "demo",
                        "overlays": {"codex": {}},
                    }
                }
            }
            dest = Path(dest_tmp)
            manifest = json.dumps({"skills": "./.codex-plugin/skills"}) + "\n"
            GEN.copy_codex_artifact(source, dest, config, manifest)
            artifact = dest / GEN.CODEX_ARTIFACT
            self.assertTrue((artifact / "skills/demo/reference.md").is_file())
            self.assertFalse(any(path.name == ".DS_Store" for path in artifact.rglob("*")))

    def test_release_digest_covers_packaged_skill_references(self) -> None:
        config = GEN.load_config(ROOT)
        surfaces = {
            **GEN.render_plugin_surfaces(config, GEN.cargo_version(ROOT)),
            **GEN.render_hook_surfaces(config),
        }
        skills = {
            GEN.SKILL_ROOT / skill_id / "SKILL.md": GEN.render_skill(
                ROOT, config, skill_id, "claude_code"
            )
            for skill_id in sorted(config["skills"])
        }
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp)
            GEN.copy_codex_artifact(
                ROOT,
                dest,
                config,
                surfaces[Path(".codex-plugin/plugin.json")],
            )
            artifact = dest / GEN.CODEX_ARTIFACT
            before = GEN.digest_inputs(surfaces, skills, artifact)
            reference = (
                artifact
                / "skills/rally-workflows/references/decomposition.md"
            )
            reference.write_text(
                reference.read_text(encoding="utf-8") + "\ndigest regression\n",
                encoding="utf-8",
            )
            after = GEN.digest_inputs(surfaces, skills, artifact)
        self.assertNotEqual(before, after)

    def test_host_overlay_changes_only_packaged_codex_frontmatter(self) -> None:
        config = GEN.load_config(ROOT)
        version = GEN.cargo_version(ROOT)
        surfaces = {
            **GEN.render_plugin_surfaces(config, version),
            **GEN.render_hook_surfaces(config),
        }
        claude_skills = {
            GEN.SKILL_ROOT / skill_id / "SKILL.md": GEN.render_skill(
                ROOT, config, skill_id, "claude_code"
            )
            for skill_id in sorted(config["skills"])
        }
        with tempfile.TemporaryDirectory() as before_tmp:
            before_root = Path(before_tmp)
            GEN.copy_codex_artifact(
                ROOT,
                before_root,
                config,
                surfaces[Path(".codex-plugin/plugin.json")],
            )
            before_digest = GEN.digest_inputs(
                surfaces,
                claude_skills,
                before_root / GEN.CODEX_ARTIFACT,
            )
        config["skills"]["mini-loop"]["overlays"]["codex"] = {
            "description": "Codex-specific test description."
        }
        codex = GEN.render_skill(ROOT, config, "mini-loop", "codex")
        claude = GEN.render_skill(ROOT, config, "mini-loop", "claude_code")
        self.assertIn("Codex-specific test description.", codex)
        self.assertNotIn("Codex-specific test description.", claude)
        with tempfile.TemporaryDirectory() as after_tmp:
            after_root = Path(after_tmp)
            GEN.copy_codex_artifact(
                ROOT,
                after_root,
                config,
                surfaces[Path(".codex-plugin/plugin.json")],
            )
            after_digest = GEN.digest_inputs(
                surfaces,
                claude_skills,
                after_root / GEN.CODEX_ARTIFACT,
            )
        self.assertNotEqual(before_digest, after_digest)


if __name__ == "__main__":
    unittest.main()
