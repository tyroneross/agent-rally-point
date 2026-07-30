#!/usr/bin/env python3
"""Diagnose or reconcile installed Claude Code and Codex Rally plugins."""

from __future__ import annotations

import argparse
import json
import shlex
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Sequence


PLUGIN_NAME = "agent-rally-point"
IDENTITY_FILE = "rally-release.json"
STATUS_ORDER = {
    "current": 0,
    "restart_required": 1,
    "uninstalled": 2,
    "stale": 3,
    "duplicate_provider": 4,
    "unknown": 5,
}


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
    canonical = [provider for provider in providers if provider["id"] == canonical_id]
    if len(providers) != 1 or len(canonical) != 1:
        return {
            "host": host,
            "status": "duplicate_provider",
            "providers": providers,
            "restart_required": True,
            "reason": f"expected only {canonical_id}; found {len(providers)} enabled provider(s)",
        }
    provider = canonical[0]
    installed_identity = provider.get("identity") or {}
    source_identity = provider.get("source_identity") or {}
    if identity_matches(installed_identity, expected):
        status = "current"
        reason = "installed plugin identity matches canonical generated surfaces"
    elif identity_matches(source_identity, expected):
        status = "restart_required"
        reason = "marketplace source is current but installed cache is stale"
    else:
        status = "stale"
        reason = "installed plugin identity does not match canonical generated surfaces"
    return {
        "host": host,
        "status": status,
        "providers": providers,
        "restart_required": status == "restart_required",
        "reason": reason,
    }


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


def apply_commands(
    report: dict[str, Any], config: dict[str, Any], host: str
) -> list[list[str]]:
    commands: list[list[str]] = []
    if host in {"all", "claude_code"}:
        record = report["hosts"].get("claude_code", {})
        if record.get("status") not in {"current", "unknown"}:
            canonical_id = config["providers"]["claude_code"]["canonical_id"]
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
        if record.get("status") in {"current", "unknown"}:
            return commands
        canonical_id = config["providers"]["codex"]["canonical_id"]
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
        if record.get("status") not in {"current", "unknown"}
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
    return 0 if report["overall"] in {"current", "restart_required"} else 2


if __name__ == "__main__":
    raise SystemExit(main())
