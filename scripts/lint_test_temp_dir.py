#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Test scratch-folder lint: test code gets disk space from one helper.

WHY
---
Tests called `std::env::temp_dir()` at 167 places through 89 hand-written
helpers. Most never deleted what they made, and nothing survived a killed
process: one full run left about 290 entries in the system temp directory
(measured 2026-11-09). `crates/test-support/temp_root.rs` now owns scratch
space. This lint keeps a new test from walking around it.

WHAT IT FLAGS
-------------
A call to `temp_dir()` in test code. That covers `std::env::temp_dir()`,
`env::temp_dir()`, a bare `temp_dir()` after `use std::env::temp_dir`, and a
renamed import (`use std::env::temp_dir as scratch` then `scratch()`).

It also flags a path under `/tmp` that is assembled at run time
(`format!("/tmp/x{}", ..)`, `PathBuf::from("/tmp").join(..)`). Three tests built
socket-bound fixtures that way and never removed them; `TempRoot::short` is the
supported way to get a short path. A fixed literal such as `"/tmp/worktree"` is
a value, not a fixture, and is not flagged.

Test code is: any `.rs` file under a `tests/` directory, any `*_tests.rs` or
`test_*.rs` file, and everything after the first `#[cfg(test)]` in another
`.rs` file. Production code is not this lint's business.

WHAT IT ALLOWS
--------------
* The helper file itself.
* Comment lines.
* A call on a line that carries `// temp-root-read: <reason>`. Some tests need
  the temp ROOT, not a folder in it: a containment check, a command's working
  directory. The note makes that choice visible and greppable.

MODES
-----
Default: exit 1 when anything is flagged. `--warn`: print the same findings and
exit 0, for the period while call sites are being migrated.

Run: python3 scripts/lint_test_temp_dir.py [--warn] [--root DIR] [--json]
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

HELPER = Path("crates/test-support/temp_root.rs")
SCAN_DIRS = ("crates",)
SKIP_DIR_NAMES = {"target", ".git", "node_modules"}
ANNOTATION = re.compile(r"//\s*temp-root-read:\s*\S")
CFG_TEST = re.compile(r"^\s*#\[cfg\(test\)\]")
CALL = re.compile(r"\btemp_dir\s*\(\s*\)")
RENAMED_IMPORT = re.compile(r"\btemp_dir\s+as\s+([A-Za-z_][A-Za-z0-9_]*)")
# A path under /tmp that is assembled at run time: `format!("/tmp/x{}", ..)` or
# `PathBuf::from("/tmp").join(..)`. A fixed literal such as "/tmp/worktree" is a
# plain value and is left alone.
DYNAMIC_SLASH_TMP = re.compile(r'format!\(\s*"/tmp[^"]*\{|"/tmp/?"\s*\)\s*\.join\(')


def is_test_file(rel: Path) -> bool:
    name = rel.name
    return (
        "tests" in rel.parts[:-1]
        or name.endswith("_tests.rs")
        or name.startswith("test_")
    )


def strip_line_comment(line: str) -> str:
    """Code before a `//` comment. Good enough for this lint: a `//` inside a
    string literal on the same line as a real `temp_dir()` call is not a shape
    this repo's tests use, and misreading it can only hide a finding on a line
    that also carries one in plain sight."""
    cut = line.find("//")
    return line if cut < 0 else line[:cut]


def scan_file(root: Path, rel: Path) -> list[dict]:
    try:
        lines = (root / rel).read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return []
    whole_file = is_test_file(rel)
    first_cfg_test = next((i for i, line in enumerate(lines) if CFG_TEST.match(line)), None)
    if not whole_file and first_cfg_test is None:
        return []
    aliases = set()
    for line in lines:
        aliases.update(RENAMED_IMPORT.findall(strip_line_comment(line)))
    alias_calls = [re.compile(rf"\b{re.escape(alias)}\s*\(\s*\)") for alias in sorted(aliases)]

    findings = []
    for index, line in enumerate(lines):
        if not whole_file and index <= first_cfg_test:
            continue
        code = strip_line_comment(line)
        if not code.strip():
            continue
        hit = (
            CALL.search(code)
            or any(pattern.search(code) for pattern in alias_calls)
            or DYNAMIC_SLASH_TMP.search(code)
        )
        if not hit or ANNOTATION.search(line):
            continue
        findings.append({"file": rel.as_posix(), "line": index + 1, "text": line.strip()})
    return findings


def scan(root: Path) -> list[dict]:
    findings: list[dict] = []
    for scan_dir in SCAN_DIRS:
        base = root / scan_dir
        if not base.is_dir():
            continue
        for path in sorted(base.rglob("*.rs")):
            rel = path.relative_to(root)
            if SKIP_DIR_NAMES.intersection(rel.parts) or rel == HELPER:
                continue
            findings.extend(scan_file(root, rel))
    return findings


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--warn", action="store_true", help="report but exit 0")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)

    findings = scan(args.root)
    if args.json:
        print(json.dumps({"findings": findings, "count": len(findings)}, indent=2))
    else:
        for finding in findings:
            print(f"{finding['file']}:{finding['line']}: {finding['text']}")
        files = len({finding["file"] for finding in findings})
        if findings:
            print(
                f"test-temp-dir lint: {len(findings)} hand-built scratch path(s) in test code "
                f"across {files} file(s). Use TempRoot (or TempRoot::short for a socket "
                f"path) from {HELPER.as_posix()}, or add '// temp-root-read: <reason>' when "
                f"the test needs the temp root itself or only a path value.",
                file=sys.stderr,
            )
        else:
            print("test-temp-dir lint: clean", file=sys.stderr)
    return 0 if (args.warn or not findings) else 1


if __name__ == "__main__":
    sys.exit(main())
