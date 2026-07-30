#!/usr/bin/env python3
"""Tests for the canonical host-surface generator."""

from __future__ import annotations

import importlib.util
import json
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

    def test_repository_surfaces_are_current(self) -> None:
        self.assertEqual(GEN.check(ROOT), [])

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
            self.assertFalse(any(path.is_symlink() for path in artifact.rglob("*")))

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
            GEN.copy_codex_artifact(source, dest, config, "{}\n")
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
