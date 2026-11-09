#!/usr/bin/env python3
"""Fixture-driven tests for installed-host synchronization."""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "sync_host_integrations", ROOT / "scripts/sync_host_integrations.py"
)
assert SPEC and SPEC.loader
SYNC = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = SYNC
SPEC.loader.exec_module(SYNC)


class FakeRunner:
    def __init__(self, responses: dict[tuple[str, ...], tuple[int, object]]) -> None:
        self.responses = responses
        self.commands: list[list[str]] = []

    def __call__(self, command: list[str]) -> SYNC.CommandResult:
        self.commands.append(list(command))
        returncode, payload = self.responses.get(tuple(command), (0, {}))
        stdout = payload if isinstance(payload, str) else json.dumps(payload)
        return SYNC.CommandResult(list(command), returncode, stdout, "")


def identity() -> dict[str, str]:
    return json.loads((ROOT / "rally-release.json").read_text())


class HostSyncTests(unittest.TestCase):
    def test_identity_lookup_never_climbs_out_of_plugin_root(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            parent = Path(tmp)
            plugin = parent / "plugin"
            plugin.mkdir()
            (parent / "rally-release.json").write_text(json.dumps(identity()))
            self.assertEqual(SYNC.identity_at(plugin), {})

    def test_current_claude_provider(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            install = Path(tmp)
            (install / "rally-release.json").write_text(json.dumps(identity()))
            runner = FakeRunner(
                {
                    ("claude", "plugin", "list", "--json"): (
                        0,
                        [
                            {
                                "id": "agent-rally-point@agent-rally-point",
                                "enabled": True,
                                "version": "test",
                                "installPath": str(install),
                            }
                        ],
                    )
                }
            )
            config = json.loads(
                (ROOT / "config/host-integrations.json").read_text()
            )["providers"]["claude_code"]
            record = SYNC.claude_status(config, identity(), runner)
            self.assertEqual(record["status"], "current")

    def test_current_codex_provider_uses_real_cache_layout(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            cache = (
                home
                / ".codex/plugins/cache/agent-rally-point/agent-rally-point/local"
                / ".codex-plugin"
            )
            cache.mkdir(parents=True)
            (cache / "rally-release.json").write_text(json.dumps(identity()))
            runner = FakeRunner(
                {
                    ("codex", "plugin", "list", "--json"): (
                        0,
                        {
                            "installed": [
                                {
                                    "pluginId": "agent-rally-point@agent-rally-point",
                                    "name": "agent-rally-point",
                                    "marketplaceName": "agent-rally-point",
                                    "enabled": True,
                                    "source": {},
                                }
                            ]
                        },
                    )
                }
            )
            config = json.loads(
                (ROOT / "config/host-integrations.json").read_text()
            )["providers"]["codex"]
            record = SYNC.codex_status(config, identity(), runner, home)
            self.assertEqual(record["status"], "current")
            self.assertEqual(record["providers"][0]["cache_path"], str(cache))

    def test_duplicate_codex_provider_is_a_hard_sync_error(self) -> None:
        runner = FakeRunner(
            {
                ("codex", "plugin", "list", "--json"): (
                    0,
                    {
                        "installed": [
                            {
                                "pluginId": "agent-rally-point@agent-rally-point",
                                "name": "agent-rally-point",
                                "marketplaceName": "agent-rally-point",
                                "source": {},
                            },
                            {
                                "pluginId": "agent-rally-point@ross-labs-local",
                                "name": "agent-rally-point",
                                "marketplaceName": "ross-labs-local",
                                "source": {},
                            },
                        ]
                    },
                )
            }
        )
        config = json.loads(
            (ROOT / "config/host-integrations.json").read_text()
        )["providers"]["codex"]
        with tempfile.TemporaryDirectory() as home:
            record = SYNC.codex_status(
                config, identity(), runner, Path(home)
            )
        self.assertEqual(record["status"], "duplicate_provider")
        self.assertEqual(len(record["providers"]), 2)

    def test_disabled_codex_provider_does_not_count_as_duplicate(self) -> None:
        runner = FakeRunner(
            {
                ("codex", "plugin", "list", "--json"): (
                    0,
                    {
                        "installed": [
                            {
                                "pluginId": "agent-rally-point@ross-labs-local",
                                "name": "agent-rally-point",
                                "marketplaceName": "ross-labs-local",
                                "enabled": False,
                                "source": {},
                            }
                        ]
                    },
                )
            }
        )
        config = json.loads(
            (ROOT / "config/host-integrations.json").read_text()
        )["providers"]["codex"]
        with tempfile.TemporaryDirectory() as home:
            record = SYNC.codex_status(config, identity(), runner, Path(home))
        self.assertEqual(record["status"], "uninstalled")

    def test_uninstalled_and_invalid_json_are_distinct(self) -> None:
        config = json.loads(
            (ROOT / "config/host-integrations.json").read_text()
        )["providers"]["claude_code"]
        uninstalled = SYNC.claude_status(
            config,
            identity(),
            FakeRunner({("claude", "plugin", "list", "--json"): (0, [])}),
        )
        unknown = SYNC.claude_status(
            config,
            identity(),
            FakeRunner(
                {("claude", "plugin", "list", "--json"): (0, "not-json")}
            ),
        )
        self.assertEqual(uninstalled["status"], "uninstalled")
        self.assertEqual(unknown["status"], "unknown")

    def test_codex_reconciliation_removes_duplicates_before_reinstall(self) -> None:
        config = json.loads(
            (ROOT / "config/host-integrations.json").read_text()
        )
        report = {
            "hosts": {
                "codex": {
                    "status": "duplicate_provider",
                    "providers": [
                        {"id": "agent-rally-point@agent-rally-point"},
                        {"id": "agent-rally-point@ross-labs-local"},
                    ],
                }
            }
        }
        commands = SYNC.apply_commands(report, config, "codex")
        self.assertEqual(
            commands[0],
            [
                "codex",
                "plugin",
                "remove",
                "agent-rally-point@ross-labs-local",
                "--json",
            ],
        )
        self.assertEqual(
            commands[-1],
            [
                "codex",
                "plugin",
                "add",
                "agent-rally-point@agent-rally-point",
                "--json",
            ],
        )
        self.assertLess(
            commands.index(config["providers"]["codex"]["upgrade_command"]),
            len(commands) - 1,
        )

    def test_dry_run_never_executes_apply_commands(self) -> None:
        claude_list = (
            "claude",
            "plugin",
            "list",
            "--json",
        )
        runner = FakeRunner({claude_list: (0, [])})
        report = SYNC.reconcile(
            ROOT,
            host="claude_code",
            apply=False,
            runner=runner,
        )
        self.assertFalse(report["apply"]["requested"])
        self.assertEqual(runner.commands, [list(claude_list)])
        self.assertTrue(report["apply"]["planned_commands"])

    def test_unknown_host_status_never_plans_mutating_commands(self) -> None:
        config = json.loads(
            (ROOT / "config/host-integrations.json").read_text()
        )
        report = {
            "hosts": {
                "claude_code": {"status": "unknown", "providers": []},
                "codex": {"status": "unknown", "providers": []},
            }
        }
        self.assertEqual(SYNC.apply_commands(report, config, "all"), [])

    def test_claude_reconciliation_removes_noncanonical_provider(self) -> None:
        config = json.loads(
            (ROOT / "config/host-integrations.json").read_text()
        )
        report = {
            "hosts": {
                "claude_code": {
                    "status": "duplicate_provider",
                    "providers": [
                        {"id": "agent-rally-point@agent-rally-point"},
                        {"id": "agent-rally-point@ross-labs-local"},
                    ],
                }
            }
        }
        commands = SYNC.apply_commands(report, config, "claude_code")
        self.assertEqual(
            commands[0],
            [
                "claude",
                "plugin",
                "uninstall",
                "agent-rally-point@ross-labs-local",
                "--yes",
            ],
        )
        self.assertEqual(
            commands[-1],
            config["providers"]["claude_code"]["update_command"],
        )

    def test_apply_stops_before_canonical_removal_when_upgrade_fails(self) -> None:
        codex_list = ("codex", "plugin", "list", "--json")
        remove_duplicate = (
            "codex",
            "plugin",
            "remove",
            "agent-rally-point@ross-labs-local",
            "--json",
        )
        upgrade = (
            "codex",
            "plugin",
            "marketplace",
            "upgrade",
            "agent-rally-point",
            "--json",
        )
        runner = FakeRunner(
            {
                codex_list: (
                    0,
                    {
                        "installed": [
                            {
                                "pluginId": "agent-rally-point@agent-rally-point",
                                "name": "agent-rally-point",
                                "marketplaceName": "agent-rally-point",
                                "source": {},
                            },
                            {
                                "pluginId": "agent-rally-point@ross-labs-local",
                                "name": "agent-rally-point",
                                "marketplaceName": "ross-labs-local",
                                "source": {},
                            },
                        ]
                    },
                ),
                remove_duplicate: (0, {}),
                upgrade: (1, "upgrade failed"),
            }
        )
        report = SYNC.reconcile(
            ROOT,
            host="codex",
            apply=True,
            runner=runner,
            home=Path("/tmp/host-sync-test-home"),
        )
        canonical_remove = [
            "codex",
            "plugin",
            "remove",
            "agent-rally-point@agent-rally-point",
            "--json",
        ]
        canonical_add = [
            "codex",
            "plugin",
            "add",
            "agent-rally-point@agent-rally-point",
            "--json",
        ]
        self.assertNotIn(canonical_remove, runner.commands)
        self.assertNotIn(canonical_add, runner.commands)
        self.assertEqual(report["apply"]["failed"], 1)

    def test_apply_does_not_require_restart_for_untouched_current_host(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            install = Path(tmp) / "claude-install"
            install.mkdir()
            (install / "rally-release.json").write_text(json.dumps(identity()))
            runner = FakeRunner(
                {
                    ("claude", "plugin", "list", "--json"): (
                        0,
                        [
                            {
                                "id": "agent-rally-point@agent-rally-point",
                                "enabled": True,
                                "installPath": str(install),
                            }
                        ],
                    ),
                    ("codex", "plugin", "list", "--json"): (0, {"installed": []}),
                }
            )
            report = SYNC.reconcile(
                ROOT,
                host="all",
                apply=True,
                runner=runner,
                home=Path(tmp),
            )
        claude = report["hosts"]["claude_code"]
        self.assertEqual(claude["status"], "current")
        self.assertFalse(claude["restart_required"])


LOCAL_CLAUDE_ID = "agent-rally-point@canonical-local"


def claude_list_with(install: Path, plugin_id: str = LOCAL_CLAUDE_ID) -> FakeRunner:
    return FakeRunner(
        {
            ("claude", "plugin", "list", "--json"): (
                0,
                [{"id": plugin_id, "enabled": True, "installPath": str(install)}],
            ),
            ("codex", "plugin", "list", "--json"): (0, {"installed": []}),
        }
    )


class LocalMarketplaceIdentityTests(unittest.TestCase):
    """Sync decides on release identity, not on the marketplace name."""

    def plan(self, installed: dict[str, str]) -> tuple[dict, list[list[str]]]:
        with tempfile.TemporaryDirectory() as tmp:
            install = Path(tmp)
            (install / "rally-release.json").write_text(json.dumps(installed))
            report = SYNC.reconcile(
                ROOT,
                host="claude_code",
                apply=False,
                runner=claude_list_with(install),
                home=install,
            )
        return report["hosts"]["claude_code"], report["apply"]["planned_commands"]

    def test_equal_identity_from_local_marketplace_is_in_sync(self) -> None:
        record, planned = self.plan(identity())
        self.assertEqual(record["status"], "current")
        self.assertIn("canonical-local", record["reason"])
        self.assertEqual(planned, [])

    def test_newer_local_release_is_never_removed(self) -> None:
        record, planned = self.plan({**identity(), "version": "99.0.0"})
        self.assertEqual(record["status"], "ahead")
        self.assertEqual(planned, [])

    def test_older_local_release_plans_update(self) -> None:
        record, planned = self.plan({**identity(), "version": "0.0.1"})
        self.assertEqual(record["status"], "stale")
        self.assertIn(
            ["claude", "plugin", "uninstall", LOCAL_CLAUDE_ID, "--yes"], planned
        )

    def test_same_version_different_digest_is_not_removed(self) -> None:
        record, planned = self.plan(
            {**identity(), "generated_surface_digest": "sha256:other"}
        )
        self.assertEqual(record["status"], "diverged")
        self.assertEqual(planned, [])

    def test_versions_compare_numerically_not_as_strings(self) -> None:
        self.assertEqual(SYNC.compare_versions("0.10.0", "0.9.0"), 1)
        self.assertEqual(SYNC.compare_versions("0.2", "0.2.0"), 0)
        self.assertEqual(SYNC.compare_versions("1.0.0-rc1", "1.0.0"), -1)
        self.assertIsNone(SYNC.compare_versions("local", "0.2.9"))

    def test_two_enabled_providers_stay_a_duplicate(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            install = Path(tmp)
            (install / "rally-release.json").write_text(json.dumps(identity()))
            runner = FakeRunner(
                {
                    ("claude", "plugin", "list", "--json"): (
                        0,
                        [
                            {"id": LOCAL_CLAUDE_ID, "enabled": True, "installPath": str(install)},
                            {
                                "id": "agent-rally-point@agent-rally-point",
                                "enabled": True,
                                "installPath": str(install),
                            },
                        ],
                    )
                }
            )
            report = SYNC.reconcile(
                ROOT, host="claude_code", apply=False, runner=runner, home=install
            )
        self.assertEqual(report["hosts"]["claude_code"]["status"], "duplicate_provider")
        self.assertTrue(report["apply"]["planned_commands"])


if __name__ == "__main__":
    unittest.main()
