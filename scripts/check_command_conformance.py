#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Release-gate conformance between the rally binary's command table and every
documented `rally <cmd>` string.

Two directions, one source of truth:

1. Binary → help: every command in the parser's dispatch table
   (`cli::COMMANDS` in crates/rally-cli/src/cli.rs) must appear as a
   `rally <cmd>` usage line in the BUILT binary's `--help` output. The unit
   test `help_text_names_every_registered_command` guards the same invariant
   at the source level; this check re-proves it against the shipped
   executable, so a stale installed binary or a build/source mismatch fails
   the gate rather than the user.

2. Docs → binary: every `rally <cmd>` string that appears in command position
   inside skills/, docs/, hooks/, and config/host-integrations.json must name
   a command the parser accepts. Command position means: inside a Markdown
   code fence or inline backtick span, anywhere in a shell file, or inside a
   JSON string value — prose like "the per-task rally loop" never matches.

Single-sourcing decision (recorded, not deferred): the parser's dispatch
table `cli::COMMANDS` is the one command schema. Help text and skills are
hand-written surfaces VALIDATED against that table by this gate plus the
in-crate unit test, rather than generated from schemars — regenerating the
bpaf parser, help text, and skill prose from JSON schemas would rebuild the
whole CLI surface for the same drift guarantee this check already enforces.
If the surfaces start drifting faster than this gate catches them, revisit
generation from docs/schemas/.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
CLI_SOURCE = REPO_ROOT / "crates/rally-cli/src/cli.rs"

# Surfaces whose `rally <cmd>` strings must parse. Directories are walked
# recursively; only these suffixes are scanned (binary assets are skipped).
SCANNED_SURFACES = ("skills", "docs", "hooks", "config/host-integrations.json")
SCANNED_SUFFIXES = {".md", ".sh", ".json"}

# Accepted despite not being rows in cli::COMMANDS: `rally help` and the flag
# aliases are handled before dispatch (lib.rs run() + cli::FLAG_ALIASES).
EXTRA_ACCEPTED = {"help", "--help", "-h", "--version", "-V"}

# `rally <cmd>` must start the context (line, backtick span, or JSON string)
# or follow a shell operator/quote. A bare space is deliberately NOT a
# boundary: mid-sentence prose like "the rally binary" or "per-task rally
# loop" must never match. A hyphen is likewise absent so `agent-rally-point`
# never matches. Under-matching is safe (a missed reference is not a false
# failure); drift shows up in plain command position.
COMMAND_POSITION = re.compile(
    r"(?:^\s*|[;|&(!=`\"']\s*)rally\s+(-{1,2}[A-Za-z][\w-]*|[a-z][a-z0-9-]*)"
)

# Same-line opt-out for deliberately documented FUTURE commands (e.g. backlog
# items in ANY-AGENT-ONBOARDING.md §"What Is Not Automatic Yet"). Fail-closed
# by default; the waiver must be visible on the referencing line itself.
PLANNED_WAIVER = "conformance:planned"
# Tokens that are placeholders, not commands: `rally <cmd>`, `rally $CMD`.
PLAIN_TOKEN = re.compile(r"^(-{1,2}[A-Za-z][\w-]*|[a-z][a-z0-9-]*)$")

FENCE = re.compile(r"^(```|~~~)")
INLINE_CODE = re.compile(r"`([^`]+)`")


def load_command_table(cli_source: Path) -> set[str]:
    """Parse the COMMANDS const — the parser's own dispatch table."""
    text = cli_source.read_text(encoding="utf-8")
    match = re.search(
        r"pub\(crate\)\s+const\s+COMMANDS:\s*&\[&str\]\s*=\s*&\[(.*?)\];",
        text,
        re.DOTALL,
    )
    if not match:
        raise SystemExit(f"conformance: COMMANDS table not found in {cli_source}")
    commands = set(re.findall(r'"([a-z][a-z0-9-]*)"', match.group(1)))
    if not commands:
        raise SystemExit(f"conformance: COMMANDS table in {cli_source} parsed empty")
    return commands


def help_output(binary: str) -> str:
    result = subprocess.run(
        [binary, "--help"], capture_output=True, text=True, timeout=30
    )
    output = result.stdout + result.stderr
    if result.returncode != 0:
        raise SystemExit(
            f"conformance: `{binary} --help` exited {result.returncode}:\n{output}"
        )
    return output


def commands_missing_from_help(commands: set[str], help_text: str) -> list[str]:
    lines = [line.strip() for line in help_text.splitlines()]
    missing = []
    for command in sorted(commands):
        if not any(
            line.startswith(f"rally {command} ") or line == f"rally {command}"
            for line in lines
        ):
            missing.append(command)
    return missing


def markdown_code_texts(text: str) -> list[tuple[int, str]]:
    """(line_number, code_text) for fenced blocks and inline backtick spans."""
    out: list[tuple[int, str]] = []
    in_fence = False
    for number, line in enumerate(text.splitlines(), start=1):
        if FENCE.match(line.strip()):
            in_fence = not in_fence
            continue
        if in_fence:
            out.append((number, line))
        else:
            out.extend((number, span) for span in INLINE_CODE.findall(line))
    return out


def json_string_values(value: object) -> list[str]:
    if isinstance(value, str):
        return [value]
    if isinstance(value, dict):
        return [s for v in value.values() for s in json_string_values(v)]
    if isinstance(value, list):
        return [s for v in value for s in json_string_values(v)]
    return []


def scan_file(path: Path) -> list[tuple[int, str]]:
    """(line_number, first_token) for every command-position rally reference."""
    text = path.read_text(encoding="utf-8")
    contexts: list[tuple[int, str]]
    if path.suffix == ".md":
        contexts = markdown_code_texts(text)
    elif path.suffix == ".json":
        try:
            contexts = [(0, s) for s in json_string_values(json.loads(text))]
        except json.JSONDecodeError:
            contexts = [(n, l) for n, l in enumerate(text.splitlines(), 1)]
    else:
        contexts = list(enumerate(text.splitlines(), start=1))
    lines = text.splitlines()
    found = []
    for number, context in contexts:
        if PLANNED_WAIVER in context:
            continue
        # For Markdown spans the waiver comment sits outside the span but on
        # the same source line — honor it there too.
        if 1 <= number <= len(lines) and PLANNED_WAIVER in lines[number - 1]:
            continue
        for token in COMMAND_POSITION.findall(context):
            if PLAIN_TOKEN.match(token):
                found.append((number, token))
    return found


def scanned_files(repo_root: Path) -> list[Path]:
    files: list[Path] = []
    for surface in SCANNED_SURFACES:
        root = repo_root / surface
        if root.is_file():
            files.append(root)
        elif root.is_dir():
            files.extend(
                p
                for p in sorted(root.rglob("*"))
                if p.is_file() and p.suffix in SCANNED_SUFFIXES
            )
    return files


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--binary", required=True, help="Path to the built rally executable"
    )
    parser.add_argument(
        "--repo-root", default=str(REPO_ROOT), help="Repository root to scan"
    )
    args = parser.parse_args()
    repo_root = Path(args.repo_root)

    commands = load_command_table(repo_root / "crates/rally-cli/src/cli.rs")
    accepted = commands | EXTRA_ACCEPTED

    failures: list[str] = []

    missing = commands_missing_from_help(commands, help_output(args.binary))
    for command in missing:
        failures.append(
            f"`rally --help` omits registered command `{command}` — add a usage "
            "line (help_text() in crates/rally-cli/src/lib.rs)"
        )

    files = scanned_files(repo_root)
    if not files:
        failures.append("no scannable files found — surface layout changed?")
    for path in files:
        for number, token in scan_file(path):
            if token not in accepted:
                where = f"{path.relative_to(repo_root)}:{number}"
                failures.append(
                    f"{where}: `rally {token}` does not parse — not in the "
                    "binary's command table (cli::COMMANDS)"
                )

    if failures:
        print("conformance: FAIL", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1

    print(
        f"conformance: OK — {len(commands)} commands in table, all present in "
        f"--help; {len(files)} files scanned with no unknown `rally <cmd>` strings"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
