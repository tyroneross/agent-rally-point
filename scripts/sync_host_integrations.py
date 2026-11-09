#!/usr/bin/env python3
"""Diagnose or reconcile installed Claude Code and Codex Rally plugins."""

from __future__ import annotations

import argparse
import json
import shlex
import subprocess
import sys
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Sequence


PLUGIN_NAME = "agent-rally-point"
IDENTITY_FILE = "rally-release.json"
# Severity order; the overall status is the worst host status.
STATUS_ORDER = {
    "current": 0,
    "ahead": 1,
    "restart_required": 2,
    "uninstalled": 3,
    "stale": 4,
    "diverged": 5,
    "duplicate_provider": 6,
    "unknown": 7,
}
# Statuses that need no host change: apply plans no commands for them.
NO_ACTION_STATUSES = frozenset({"current", "ahead", "unknown"})
# Statuses that exit 0 from the CLI (nothing to reconcile).
EXIT_OK_STATUSES = frozenset({"current", "ahead", "restart_required"})
VERSION_NUMBER = re.compile(r"^v?(\d+(?:\.\d+)*)(.*)$")


@dataclass(frozen=True)
class CommandResult:
    command: list[str]
    returncode: int
    stdout: str
    stderr: str

    def as_json(self) -> dict[str, Any]:
        return {
            "command": self.command,
            "returncode": self.returncode,
            "stdout": self.stdout[-4000:],
            "stderr": self.stderr[-4000:],
        }


Runner = Callable[[Sequence[str]], CommandResult]


def default_runner(command: Sequence[str]) -> CommandResult:
    try:
        completed = subprocess.run(
            list(command),
            check=False,
            text=True,
            capture_output=True,
            timeout=120,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return CommandResult(list(command), 127, "", str(exc))
    return CommandResult(
        list(command),
        completed.returncode,
        completed.stdout,
        completed.stderr,
    )


def load_json_file(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    return value if isinstance(value, dict) else {}


def expected_identity(root: Path) -> dict[str, Any]:
    identity = load_json_file(root / IDENTITY_FILE)
    required = {
        "version",
        "canonical_provider",
        "source",
        "generated_surface_digest",
    }
    missing = sorted(required - identity.keys())
    if missing:
        raise RuntimeError(
            f"{root / IDENTITY_FILE}: missing generated identity fields: {', '.join(missing)}"
        )
    return identity


def command_json(command: Sequence[str], runner: Runner) -> tuple[Any, CommandResult]:
    result = runner(command)
    if result.returncode != 0:
        return None, result
    try:
        return json.loads(result.stdout), result
    except json.JSONDecodeError:
        return None, CommandResult(
            result.command,
            65,
            result.stdout,
            "command returned invalid JSON",
        )


def identity_at(path: Path | None) -> dict[str, Any]:
    if path is None:
        return {}
    return load_json_file(path / IDENTITY_FILE)


def identity_matches(actual: dict[str, Any], expected: dict[str, Any]) -> bool:
    return all(
        actual.get(key) == expected.get(key)
        for key in ("version", "canonical_provider", "source", "generated_surface_digest")
    )


def parse_version(value: Any) -> tuple[tuple[int, ...], bool] | None:
    """Return (numeric components, is_release) or None when unparseable.

    A pre-release suffix (``-rc1``) sorts below the same numeric release.
    Trailing zero components are ignored so 0.2 equals 0.2.0.
    """
    match = VERSION_NUMBER.match(str(value or "").strip())
    if not match:
        return None
    numbers = [int(part) for part in match.group(1).split(".")]
    while len(numbers) > 1 and numbers[-1] == 0:
        numbers.pop()
    suffix = match.group(2).strip()
    return tuple(numbers), not suffix.startswith("-")


def compare_versions(installed: Any, canonical: Any) -> int | None:
    """-1 / 0 / 1 when installed is older / equal / newer; None if unparseable."""
    left, right = parse_version(installed), parse_version(canonical)
    if left is None or right is None:
        return None
    return (left > right) - (left < right)


def marketplace_of(provider: dict[str, Any]) -> str:
    return str(provider.get("marketplace") or str(provider["id"]).partition("@")[2])


def host_record(
    *,
    host: str,
    providers: list[dict[str, Any]],
    canonical_id: str,
    expected: dict[str, Any],
    command_error: CommandResult | None = None,
) -> dict[str, Any]:
    if command_error is not None:
        return {
            "host": host,
            "status": "unknown",
            "providers": providers,
            "restart_required": False,
            "reason": command_error.stderr or f"{command_error.command[0]} failed",
        }
    if not providers:
        return {
            "host": host,
            "status": "uninstalled",
            "providers": [],
            "restart_required": False,
            "reason": "agent-rally-point is not installed",
        }
    if len(providers) != 1:
        return {
            "host": host,
            "status": "duplicate_provider",
            "providers": providers,
            "restart_required": True,
            "reason": f"expected exactly one enabled provider; found {len(providers)}",
        }
    provider = providers[0]
    marketplace = marketplace_of(provider)
    installed_identity = provider.get("identity") or {}
    source_identity = provider.get("source_identity") or {}
    if identity_matches(installed_identity, expected):
        status = "current"
        reason = (
            "installed plugin identity matches canonical generated surfaces "
            f"(marketplace {marketplace})"
        )
    elif identity_matches(source_identity, expected):
        status = "restart_required"
        reason = "marketplace source is current but installed cache is stale"
    else:
        status, reason = classify_mismatch(installed_identity, expected, marketplace)
    return {
        "host": host,
        "status": status,
        "providers": providers,
        "restart_required": status == "restart_required",
        "reason": reason,
    }


def classify_mismatch(
    installed: dict[str, Any], expected: dict[str, Any], marketplace: str
) -> tuple[str, str]:
    """Judge a single provider whose identity differs from the canonical one."""
    order = compare_versions(installed.get("version"), expected.get("version"))
    where = f"marketplace {marketplace}"
    if order is None or order < 0:
        return "stale", (
            f"installed release {installed.get('version') or 'unknown'} is older "
            f"than canonical {expected.get('version')}; refresh it from its own "
            f"marketplace ({where})"
        )
    if order > 0:
        return "ahead", (
            f"installed release {installed.get('version')} is newer than canonical "
            f"{expected.get('version')}; no change planned ({where})"
        )
    return "diverged", (
        f"installed release {installed.get('version')} equals canonical but its "
        f"generated surfaces differ; refresh it from its own marketplace ({where})"
    )


def claude_status(
    config: dict[str, Any],
    expected: dict[str, Any],
    runner: Runner,
) -> dict[str, Any]:
    data, result = command_json(["claude", "plugin", "list", "--json"], runner)
    if not isinstance(data, list):
        return host_record(
            host="claude_code",
            providers=[],
            canonical_id=config["canonical_id"],
            expected=expected,
            command_error=result,
        )
    providers: list[dict[str, Any]] = []
    for item in data:
        if not isinstance(item, dict):
            continue
        plugin_id = str(item.get("id", ""))
        if not plugin_id.startswith(f"{PLUGIN_NAME}@") or not item.get("enabled", True):
            continue
        install_path = Path(item["installPath"]) if item.get("installPath") else None
        providers.append(
            {
                "id": plugin_id,
                "version": item.get("version"),
                "install_path": str(install_path) if install_path else None,
                "identity": identity_at(install_path),
                "source_identity": {},
            }
        )
    return host_record(
        host="claude_code",
        providers=providers,
        canonical_id=config["canonical_id"],
        expected=expected,
    )


def codex_status(
    config: dict[str, Any],
    expected: dict[str, Any],
    runner: Runner,
    home: Path,
) -> dict[str, Any]:
    data, result = command_json(["codex", "plugin", "list", "--json"], runner)
    installed = data.get("installed") if isinstance(data, dict) else None
    if not isinstance(installed, list):
        return host_record(
            host="codex",
            providers=[],
            canonical_id=config["canonical_id"],
            expected=expected,
            command_error=result,
        )
    providers: list[dict[str, Any]] = []
    for item in installed:
        if (
            not isinstance(item, dict)
            or item.get("name") != PLUGIN_NAME
            or not item.get("enabled", True)
        ):
            continue
        plugin_id = str(item.get("pluginId", ""))
        marketplace = str(item.get("marketplaceName", ""))
        source = item.get("source") if isinstance(item.get("source"), dict) else {}
        source_path = Path(source["path"]) if source.get("path") else None
        cache_path = (
            home
            / ".codex/plugins/cache"
            / marketplace
            / PLUGIN_NAME
            / "local"
            / ".codex-plugin"
        )
        providers.append(
            {
                "id": plugin_id,
                "marketplace": marketplace,
                "source_path": str(source_path) if source_path else None,
                "cache_path": str(cache_path),
                "identity": identity_at(cache_path),
                "source_identity": identity_at(source_path),
            }
        )
    return host_record(
        host="codex",
        providers=providers,
        canonical_id=config["canonical_id"],
        expected=expected,
    )


def diagnose(
    root: Path,
    *,
    host: str = "all",
    runner: Runner = default_runner,
    home: Path | None = None,
) -> dict[str, Any]:
    config = json.loads(
        (root / "config/host-integrations.json").read_text(encoding="utf-8")
    )
    identity = expected_identity(root)
    hosts: dict[str, Any] = {}
    if host in {"all", "claude_code"}:
        hosts["claude_code"] = claude_status(
            config["providers"]["claude_code"], identity, runner
        )
    if host in {"all", "codex"}:
        hosts["codex"] = codex_status(
            config["providers"]["codex"],
            identity,
            runner,
            home or Path.home(),
        )
    overall = max(
        (record["status"] for record in hosts.values()),
        key=lambda status: STATUS_ORDER[status],
        default="unknown",
    )
    return {
        "schema": "agent-rally.host-sync-report.v1",
        "canonical": identity,
        "overall": overall,
        "hosts": hosts,
    }


def sole_other_marketplace_provider(
    record: dict[str, Any], canonical_id: str
) -> dict[str, Any] | None:
    """The single installed provider when it came from another marketplace.

    Such a provider is refreshed from the marketplace it was installed from. It
    is never uninstalled in favor of the canonical marketplace: a local
    directory marketplace is a legitimate source and is often the newer one.
    """
    providers = record.get("providers", [])
    if len(providers) == 1 and providers[0].get("id") != canonical_id:
        return providers[0]
    return None


def apply_commands(
    report: dict[str, Any], config: dict[str, Any], host: str
) -> list[list[str]]:
    commands: list[list[str]] = []
    if host in {"all", "claude_code"}:
        record = report["hosts"].get("claude_code", {})
        canonical_id = config["providers"]["claude_code"]["canonical_id"]
        in_place = sole_other_marketplace_provider(record, canonical_id)
        if record.get("status") in NO_ACTION_STATUSES:
            pass
        elif in_place is not None:
            commands.append(["claude", "plugin", "update", str(in_place["id"])])
        else:
            for provider in record.get("providers", []):
                if provider.get("id") != canonical_id:
                    commands.append(
                        [
                            "claude",
                            "plugin",
                            "uninstall",
                            str(provider["id"]),
                            "--yes",
                        ]
                    )
            commands.append(list(config["providers"]["claude_code"]["update_command"]))
    if host in {"all", "codex"}:
        record = report["hosts"].get("codex", {})
        if record.get("status") in NO_ACTION_STATUSES:
            return commands
        canonical_id = config["providers"]["codex"]["canonical_id"]
        in_place = sole_other_marketplace_provider(record, canonical_id)
        if in_place is not None:
            provider_id = str(in_place["id"])
            # Same order as the canonical path: refresh the marketplace
            # snapshot first, then replace the installed copy from it.
            commands.extend(
                [
                    ["codex", "plugin", "marketplace", "upgrade", marketplace_of(in_place), "--json"],
                    ["codex", "plugin", "remove", provider_id, "--json"],
                    ["codex", "plugin", "add", provider_id, "--json"],
                ]
            )
            return commands
        for provider in record.get("providers", []):
            if provider.get("id") != canonical_id:
                commands.append(
                    ["codex", "plugin", "remove", str(provider["id"]), "--json"]
                )
        commands.append(list(config["providers"]["codex"]["upgrade_command"]))
        if any(
            provider.get("id") == canonical_id
            for provider in record.get("providers", [])
        ):
            commands.append(
                ["codex", "plugin", "remove", canonical_id, "--json"]
            )
        commands.append(list(config["providers"]["codex"]["install_command"]))
    return commands


def reconcile(
    root: Path,
    *,
    host: str,
    apply: bool,
    runner: Runner = default_runner,
    home: Path | None = None,
) -> dict[str, Any]:
    config = json.loads(
        (root / "config/host-integrations.json").read_text(encoding="utf-8")
    )
    before = diagnose(root, host=host, runner=runner, home=home)
    commands = apply_commands(before, config, host)
    mutated_hosts = {
        name
        for name, record in before["hosts"].items()
        if record.get("status") not in NO_ACTION_STATUSES
    }
    if not apply:
        return {
            **before,
            "apply": {
                "requested": False,
                "planned_commands": commands,
                "actions": [],
            },
        }
    actions: list[CommandResult] = []
    for command in commands:
        action = runner(command)
        actions.append(action)
        if action.returncode != 0:
            break
    failures = [action for action in actions if action.returncode != 0]
    after = diagnose(root, host=host, runner=runner, home=home)
    if not failures:
        for name, record in after["hosts"].items():
            mutated = name in mutated_hosts
            record["restart_required"] = mutated
            if mutated and record["status"] == "current":
                record["status"] = "restart_required"
                record["reason"] = "plugin content is current; restart the host to activate it"
        after["overall"] = max(
            (record["status"] for record in after["hosts"].values()),
            key=lambda status: STATUS_ORDER[status],
            default="unknown",
        )
    return {
        **after,
        "apply": {
            "requested": True,
            "planned_commands": commands,
            "actions": [action.as_json() for action in actions],
            "failed": len(failures),
        },
    }


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        type=Path,
        default=Path(__file__).resolve().parent.parent,
        help="Canonical agent-rally-point checkout",
    )
    parser.add_argument(
        "--host",
        choices=("all", "claude_code", "codex"),
        default="all",
    )
    parser.add_argument(
        "--apply",
        action="store_true",
        help="Execute host plugin-manager commands; default is read-only",
    )
    parser.add_argument("--json", action="store_true", help="Emit JSON")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    root = args.root.resolve()
    report = reconcile(root, host=args.host, apply=args.apply)
    if args.json:
        print(json.dumps(report, indent=2, ensure_ascii=False))
    else:
        print(f"host-sync: overall={report['overall']}")
        for name, record in report["hosts"].items():
            print(f"  {name}: {record['status']} — {record['reason']}")
        if not args.apply:
            planned = report["apply"]["planned_commands"]
            if planned:
                print("  planned reconciliation:")
                for command in planned:
                    print(f"    {shlex.join(command)}")
                print("  dry-run: pass --apply to execute these commands")
            else:
                print("  dry-run: no host changes planned")
    failed = report.get("apply", {}).get("failed", 0)
    if failed:
        return 1
    return 0 if report["overall"] in EXIT_OK_STATUSES else 2


if __name__ == "__main__":
    raise SystemExit(main())
