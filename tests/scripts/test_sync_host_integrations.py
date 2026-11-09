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
        default = (0, []) if command == ["claude", "plugin", "marketplace", "list", "--json"] else (0, {"marketplaces": []}) if command == ["codex", "plugin", "marketplace", "list", "--json"] else (0, {})
        returncode, payload = self.responses.get(tuple(command), default)
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
        self.assertEqual(runner.commands, [list(claude_list), ["claude", "plugin", "marketplace", "list", "--json"]])
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

    # A provider from another marketplace is refreshed where it came from. It is
    # never uninstalled in favor of the canonical marketplace.
    IN_PLACE_CLAUDE = [["claude", "plugin", "update", LOCAL_CLAUDE_ID]]

    def test_older_local_release_is_refreshed_in_place(self) -> None:
        record, planned = self.plan({**identity(), "version": "0.0.1"})
        self.assertEqual(record["status"], "stale")
        self.assertEqual(planned, self.IN_PLACE_CLAUDE)

    def test_same_version_different_digest_is_refreshed_in_place(self) -> None:
        record, planned = self.plan(
            {**identity(), "generated_surface_digest": "sha256:other"}
        )
        self.assertEqual(record["status"], "diverged")
        self.assertEqual(planned, self.IN_PLACE_CLAUDE)

    def test_codex_local_provider_is_refreshed_from_its_own_marketplace(self) -> None:
        local = {"id": LOCAL_CLAUDE_ID, "marketplace": "canonical-local"}
        report = {"hosts": {"codex": {"status": "diverged", "providers": [local]}}}
        config = json.loads((ROOT / "config/host-integrations.json").read_text())
        self.assertEqual(
            SYNC.apply_commands(report, config, "codex"),
            [
                ["codex", "plugin", "marketplace", "upgrade", "canonical-local", "--json"],
                ["codex", "plugin", "remove", LOCAL_CLAUDE_ID, "--json"],
                ["codex", "plugin", "add", LOCAL_CLAUDE_ID, "--json"],
            ],
        )

    def test_canonical_provider_keeps_the_canonical_update_path(self) -> None:
        config = json.loads((ROOT / "config/host-integrations.json").read_text())
        canonical_id = config["providers"]["claude_code"]["canonical_id"]
        report = {
            "hosts": {
                "claude_code": {"status": "stale", "providers": [{"id": canonical_id}]}
            }
        }
        self.assertEqual(
            SYNC.apply_commands(report, config, "claude_code"),
            [list(config["providers"]["claude_code"]["update_command"])],
        )

    def test_versions_compare_numerically_not_as_strings(self) -> None:
        self.assertEqual(SYNC.compare_versions("0.10.0", "0.9.0"), 1)
        self.assertEqual(SYNC.compare_versions("0.2", "0.2.0"), 0)
        self.assertEqual(SYNC.compare_versions("1.0.0-rc1", "1.0.0"), -1)
        self.assertIsNone(SYNC.compare_versions("local", "0.2.9"))


CANONICAL_CLAUDE_ID = "agent-rally-point@agent-rally-point"
OTHER_CLAUDE_ID = "agent-rally-point@ross-labs-local"


def uninstall(plugin_id: str) -> list[str]:
    return ["claude", "plugin", "uninstall", plugin_id, "--yes"]


class DuplicateProviderTests(unittest.TestCase):
    """Two or more providers: keep exactly one by version, remove the rest."""

    def plan(self, providers: list[tuple[str, dict[str, str]]]) -> tuple[dict, list]:
        with tempfile.TemporaryDirectory() as tmp:
            entries = []
            for index, (plugin_id, installed) in enumerate(providers):
                install = Path(tmp) / str(index)
                install.mkdir()
                (install / "rally-release.json").write_text(json.dumps(installed))
                entries.append(
                    {"id": plugin_id, "enabled": True, "installPath": str(install)}
                )
            runner = FakeRunner(
                {("claude", "plugin", "list", "--json"): (0, entries)}
            )
            report = SYNC.reconcile(
                ROOT, host="claude_code", apply=False, runner=runner, home=Path(tmp)
            )
        return report["hosts"]["claude_code"], report["apply"]["planned_commands"]

    def test_newer_local_beats_older_canonical(self) -> None:
        record, planned = self.plan(
            [
                (CANONICAL_CLAUDE_ID, {**identity(), "version": "0.0.1"}),
                (LOCAL_CLAUDE_ID, {**identity(), "version": "99.0.0"}),
            ]
        )
        self.assertEqual(record["status"], "duplicate_provider")
        self.assertEqual(record["kept_provider"], LOCAL_CLAUDE_ID)
        self.assertEqual(planned, [uninstall(CANONICAL_CLAUDE_ID)])

    def test_newer_canonical_beats_older_local(self) -> None:
        record, planned = self.plan(
            [
                (LOCAL_CLAUDE_ID, {**identity(), "version": "0.0.1"}),
                (CANONICAL_CLAUDE_ID, {**identity(), "version": "99.0.0"}),
            ]
        )
        self.assertEqual(record["kept_provider"], CANONICAL_CLAUDE_ID)
        self.assertEqual(planned, [uninstall(LOCAL_CLAUDE_ID)])

    def test_two_noncanonical_providers_keep_the_newer_and_refresh_it(self) -> None:
        record, planned = self.plan(
            [
                (OTHER_CLAUDE_ID, {**identity(), "version": "0.0.1"}),
                (LOCAL_CLAUDE_ID, {**identity(), "version": "0.0.5"}),
            ]
        )
        self.assertEqual(record["kept_provider"], LOCAL_CLAUDE_ID)
        self.assertEqual(
            planned,
            [uninstall(OTHER_CLAUDE_ID), ["claude", "plugin", "update", LOCAL_CLAUDE_ID]],
        )

    def test_two_noncanonical_providers_never_leave_zero(self) -> None:
        _, planned = self.plan(
            [(OTHER_CLAUDE_ID, identity()), (LOCAL_CLAUDE_ID, identity())]
        )
        removed = [c[3] for c in planned if c[:3] == ["claude", "plugin", "uninstall"]]
        self.assertEqual(len(removed), 1)

    def test_version_tie_keeps_the_identity_matching_provider(self) -> None:
        record, planned = self.plan(
            [
                (
                    CANONICAL_CLAUDE_ID,
                    {**identity(), "generated_surface_digest": "sha256:other"},
                ),
                (LOCAL_CLAUDE_ID, identity()),
            ]
        )
        self.assertEqual(record["kept_provider"], LOCAL_CLAUDE_ID)
        self.assertEqual(planned, [uninstall(CANONICAL_CLAUDE_ID)])

    def test_full_tie_keeps_the_canonical_id(self) -> None:
        record, planned = self.plan(
            [(LOCAL_CLAUDE_ID, identity()), (CANONICAL_CLAUDE_ID, identity())]
        )
        self.assertEqual(record["status"], "duplicate_provider")
        self.assertEqual(planned, [uninstall(LOCAL_CLAUDE_ID)])

    def test_unparseable_version_ranks_lowest(self) -> None:
        _, planned = self.plan(
            [
                (CANONICAL_CLAUDE_ID, {**identity(), "version": "local"}),
                (LOCAL_CLAUDE_ID, {**identity(), "version": "0.0.1"}),
            ]
        )
        self.assertEqual(
            planned,
            [
                uninstall(CANONICAL_CLAUDE_ID),
                ["claude", "plugin", "update", LOCAL_CLAUDE_ID],
            ],
        )


class VersionPrecedenceTests(unittest.TestCase):
    def test_prerelease_precedence(self) -> None:
        cmp = SYNC.compare_versions
        self.assertEqual(cmp("0.3.0-rc.2", "0.3.0-rc.1"), 1)
        self.assertEqual(cmp("1.0.0-rc.1", "1.0.0"), -1)
        self.assertEqual(cmp("1.0.0-alpha", "1.0.0-alpha.1"), -1)
        self.assertEqual(cmp("1.0.0-alpha.1", "1.0.0-beta"), -1)
        self.assertEqual(cmp("1.0.0-rc.10", "1.0.0-rc.9"), 1)
        self.assertEqual(cmp("1.0.0-1", "1.0.0-alpha"), -1)
        self.assertEqual(cmp("1.0.0+build.5", "1.0.0+build.9"), 0)
        self.assertEqual(cmp("1.0.0-rc.1+x", "1.0.0-rc.1"), 0)

    def test_non_semver_suffixes_are_unparseable(self) -> None:
        for text in ("0.3.0rc1", "0.3.0.dev1", "local", "", None, "1.0.0-"):
            self.assertIsNone(SYNC.parse_version(text), text)
        self.assertIsNone(SYNC.compare_versions("0.3.0rc1", "0.3.0"))


class UnreadableIdentityTests(unittest.TestCase):
    """A missing or unparseable identity never becomes a swap to another marketplace."""

    def plan(self, plugin_id: str, installed: dict | None) -> list[list[str]]:
        with tempfile.TemporaryDirectory() as tmp:
            install = Path(tmp)
            if installed is not None:
                (install / "rally-release.json").write_text(json.dumps(installed))
            report = SYNC.reconcile(
                ROOT,
                host="claude_code",
                apply=False,
                runner=claude_list_with(install, plugin_id),
                home=install,
            )
        return report["apply"]["planned_commands"]

    def test_local_provider_without_identity_is_refreshed_in_place(self) -> None:
        self.assertEqual(
            self.plan(LOCAL_CLAUDE_ID, None),
            [["claude", "plugin", "update", LOCAL_CLAUDE_ID]],
        )

    def test_local_provider_with_unparseable_version_is_refreshed_in_place(self) -> None:
        self.assertEqual(
            self.plan(LOCAL_CLAUDE_ID, {**identity(), "version": "local"}),
            [["claude", "plugin", "update", LOCAL_CLAUDE_ID]],
        )

    def test_canonical_provider_without_identity_takes_the_canonical_update(self) -> None:
        config = json.loads((ROOT / "config/host-integrations.json").read_text())
        self.assertEqual(
            self.plan(CANONICAL_CLAUDE_ID, None),
            [list(config["providers"]["claude_code"]["update_command"])],
        )


class StatefulClaudeRunner(FakeRunner):
    """Claude host whose installed version changes when `plugin update` runs."""

    def __init__(self, install: Path, before: dict, after: dict) -> None:
        super().__init__(
            {
                ("codex", "plugin", "list", "--json"): (0, {"installed": []}),
            }
        )
        self.install, self.after = install, after
        (install / "rally-release.json").write_text(json.dumps(before))

    def __call__(self, command: list[str]) -> SYNC.CommandResult:
        if command[:3] == ["claude", "plugin", "list"]:
            self.commands.append(list(command))
            entry = [{"id": LOCAL_CLAUDE_ID, "enabled": True, "installPath": str(self.install)}]
            return SYNC.CommandResult(list(command), 0, json.dumps(entry), "")
        if command[:3] == ["claude", "plugin", "update"]:
            (self.install / "rally-release.json").write_text(json.dumps(self.after))
        return super().__call__(command)


class InPlaceApplyTests(unittest.TestCase):
    def test_apply_refreshes_in_place_then_requires_restart(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            runner = StatefulClaudeRunner(
                Path(tmp), {**identity(), "version": "0.0.1"}, identity()
            )
            report = SYNC.reconcile(
                ROOT, host="claude_code", apply=True, runner=runner, home=Path(tmp)
            )
        self.assertEqual(
            runner.commands,
            [
                ["claude", "plugin", "list", "--json"],
                ["claude", "plugin", "marketplace", "list", "--json"],
                ["claude", "plugin", "update", LOCAL_CLAUDE_ID],
                ["claude", "plugin", "list", "--json"],
                ["claude", "plugin", "marketplace", "list", "--json"],
            ],
        )
        record = report["hosts"]["claude_code"]
        self.assertEqual(record["status"], "restart_required")
        self.assertTrue(record["restart_required"])
        self.assertEqual(report["apply"]["failed"], 0)

    def test_main_exit_codes_for_ahead_and_diverged(self) -> None:
        import contextlib
        import io
        import subprocess
        from unittest import mock

        def exit_code(installed: dict) -> int:
            with tempfile.TemporaryDirectory() as tmp:
                install = Path(tmp)
                (install / "rally-release.json").write_text(json.dumps(installed))
                runner = claude_list_with(install)

                def fake_run(command, **_kwargs):
                    result = runner(command)
                    return subprocess.CompletedProcess(
                        command, result.returncode, result.stdout, result.stderr
                    )

                with mock.patch.object(SYNC.subprocess, "run", fake_run), \
                        contextlib.redirect_stdout(io.StringIO()):
                    return SYNC.main(["--root", str(ROOT), "--host", "claude_code"])

        self.assertEqual(exit_code({**identity(), "version": "99.0.0"}), 0)
        self.assertEqual(
            exit_code({**identity(), "generated_surface_digest": "sha256:other"}), 2
        )


class RegressionTests(unittest.TestCase):
    def retry_report(self, failures: int, provider: str) -> tuple[dict, FakeRunner]:
        class RetryRunner(FakeRunner):
            def __init__(self, home):
                super().__init__({("codex", "plugin", "list", "--json"): (0, {"installed": [{"name": "agent-rally-point", "pluginId": provider, "marketplaceName": provider.split("@")[1]}]})})
                self.remaining = failures
                self.cache = home / ".codex/plugins/cache" / provider.split("@")[1] / "agent-rally-point/local/.codex-plugin"

            def __call__(self, command):
                if command[:3] == ["codex", "plugin", "add"] and self.remaining:
                    self.remaining -= 1
                    self.commands.append(list(command))
                    return SYNC.CommandResult(list(command), 1, "", "add failed")
                if command[:3] == ["codex", "plugin", "add"]:
                    self.cache.mkdir(parents=True, exist_ok=True)
                    (self.cache / "rally-release.json").write_text(json.dumps(identity()))
                return super().__call__(command)

        with tempfile.TemporaryDirectory() as tmp:
            runner = RetryRunner(Path(tmp))
            report = SYNC.reconcile(ROOT, host="codex", apply=True, runner=runner, home=Path(tmp))
        return report, runner

    def test_add_retry_succeeds_for_both_provider_paths(self):
        for provider in (CANONICAL_CLAUDE_ID, LOCAL_CLAUDE_ID):
            with self.subTest(provider=provider):
                report, runner = self.retry_report(1, provider)
                self.assertEqual(runner.commands.count(["codex", "plugin", "add", provider, "--json"]), 2)
                self.assertEqual(report["apply"]["failed"], 0)
                self.assertEqual(report["hosts"]["codex"]["status"], "restart_required")

    def test_add_retry_failure_reports_recovery_and_failure_count(self):
        report, runner = self.retry_report(2, LOCAL_CLAUDE_ID)
        self.assertEqual(report["apply"]["failed"], 2)
        self.assertEqual(report["hosts"]["codex"]["status"], "uninstalled")
        self.assertIn("uninstalled", report["hosts"]["codex"]["reason"])
        self.assertIn("codex plugin add agent-rally-point@canonical-local --json", report["hosts"]["codex"]["reason"])
        from unittest import mock
        import contextlib
        import io
        with mock.patch.object(SYNC, "reconcile", return_value=report), contextlib.redirect_stdout(io.StringIO()):
            self.assertNotEqual(SYNC.main(["--apply", "--json"]), 0)

    def test_failed_marketplace_list_is_unknown_and_other_host_is_planned(self):
        runner = FakeRunner({("claude", "plugin", "list", "--json"): (0, []), ("claude", "plugin", "marketplace", "list", "--json"): (1, "marketplace corrupted"), ("codex", "plugin", "list", "--json"): (0, {"installed": []})})
        report = SYNC.reconcile(ROOT, host="all", apply=False, runner=runner)
        self.assertEqual(report["hosts"]["claude_code"]["status"], "unknown")
        self.assertIn("marketplace corrupted", report["hosts"]["claude_code"]["reason"])
        self.assertEqual(report["overall"], "unknown")
        self.assertTrue(report["apply"]["planned_commands"])
        self.assertTrue(all(c[0] == "codex" for c in report["apply"]["planned_commands"]))
        from unittest import mock
        import contextlib
        import io
        with mock.patch.object(SYNC, "default_runner", runner), contextlib.redirect_stdout(io.StringIO()):
            with mock.patch.object(SYNC, "reconcile", return_value=report):
                self.assertNotEqual(SYNC.main(["--json"]), 0)

    def test_current_host_stays_current_when_its_marketplace_list_fails(self):
        """What the plugin list proved is kept; the marketplace error rides along."""
        with tempfile.TemporaryDirectory() as tmp:
            install = Path(tmp)
            (install / "rally-release.json").write_text(json.dumps(identity()))
            runner = claude_list_with(install)
            runner.responses[("claude", "plugin", "marketplace", "list", "--json")] = (1, "marketplace corrupted")
            report = SYNC.reconcile(ROOT, host="claude_code", apply=False, runner=runner, home=install)
        record = report["hosts"]["claude_code"]
        self.assertEqual(record["status"], "current")
        self.assertEqual(record["marketplace_error"], "marketplace corrupted")
        self.assertEqual(report["apply"]["planned_commands"], [])
        self.assertEqual(report["overall"], "current")

    def test_host_needing_a_refresh_is_unknown_when_its_marketplace_list_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            install = Path(tmp)
            (install / "rally-release.json").write_text(json.dumps({**identity(), "version": "0.0.1"}))
            runner = claude_list_with(install)
            runner.responses[("claude", "plugin", "marketplace", "list", "--json")] = (1, "marketplace corrupted")
            report = SYNC.reconcile(ROOT, host="claude_code", apply=False, runner=runner, home=install)
        record = report["hosts"]["claude_code"]
        self.assertEqual(record["status"], "unknown")
        self.assertEqual(record["needed_status"], "stale")
        self.assertIn("marketplace corrupted", record["reason"])
        self.assertEqual(report["apply"]["planned_commands"], [])

    def test_unparseable_marketplace_list_is_unknown(self):
        runner = FakeRunner({("claude", "plugin", "list", "--json"): (0, []), ("claude", "plugin", "marketplace", "list", "--json"): (0, "not json" )})
        report = SYNC.reconcile(ROOT, host="claude_code", apply=False, runner=runner)
        self.assertEqual(report["overall"], "unknown")
        self.assertEqual(report["apply"]["planned_commands"], [])
        self.assertIn("not json", report["hosts"]["claude_code"]["reason"])


if __name__ == "__main__":
    unittest.main()
