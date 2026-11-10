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
`test_*.rs` file, any file declared as `#[cfg(test)] mod name;` (and the modules
that file declares), and the body of every `#[cfg(test)] mod name { .. }` block
in another `.rs` file. Production code is not this lint's business.

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
# The literal is matched on its own, without `format!(` in front, because the
# macro call and its string are often on different lines.
DYNAMIC_SLASH_TMP = re.compile(r'"/tmp/[^"]*\{[^"]*"|"/tmp/?"\s*\)\s*\.join\(')


MOD_DECL = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;")


def child_module_files(root: Path, rel: Path, name: str) -> list[Path]:
    """Where `mod name;` declared in `rel` can live."""
    parent = rel.parent
    if rel.name not in ("mod.rs", "main.rs", "lib.rs"):
        parent = parent / rel.stem
    candidates = [parent / f"{name}.rs", parent / name / "mod.rs"]
    return [c for c in candidates if (root / c).is_file()]


def test_only_module_files(root: Path, files: list[Path]) -> set[Path]:
    """Files that are test code because of how they are declared, not named:
    `#[cfg(test)] mod concurrency_tests;` puts `concurrency_tests.rs` in the
    test build even though nothing in that file says so. Everything such a file
    declares in turn is test code too."""
    found: set[Path] = set()
    queue: list[Path] = []
    for rel in files:
        try:
            lines = (root / rel).read_text(encoding="utf-8", errors="replace").splitlines()
        except OSError:
            continue
        for index, line in enumerate(lines):
            match = MOD_DECL.match(line)
            if not match:
                continue
            attrs = lines[max(0, index - 3) : index]
            if any(CFG_TEST.match(attr) for attr in attrs):
                queue.extend(child_module_files(root, rel, match.group(1)))
    while queue:
        rel = queue.pop()
        if rel in found:
            continue
        found.add(rel)
        try:
            lines = (root / rel).read_text(encoding="utf-8", errors="replace").splitlines()
        except OSError:
            continue
        for line in lines:
            match = MOD_DECL.match(line)
            if match:
                queue.extend(child_module_files(root, rel, match.group(1)))
    return found


INLINE_MOD = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+[A-Za-z_][A-Za-z0-9_]*\s*\{")
STRING = re.compile(r'"(?:\\.|[^"\\])*"')


def inline_test_lines(lines: list[str]) -> set[int]:
    """Indexes of lines inside a `#[cfg(test)] mod name { .. }` block.

    A file can mix production code and test modules in any order, and a lone
    `#[cfg(test)]` can sit on a single item near the top. Treating everything
    after the first `#[cfg(test)]` as test code flagged a production function
    in such a file, so the block is followed by its braces instead."""
    inside: set[int] = set()
    index = 0
    while index < len(lines):
        if not CFG_TEST.match(lines[index]):
            index += 1
            continue
        start = index + 1
        while start < len(lines) and lines[start].lstrip().startswith("#["):
            start += 1
        if start >= len(lines) or not INLINE_MOD.match(lines[start]):
            index += 1
            continue
        depth = 0
        cursor = start
        while cursor < len(lines):
            code = STRING.sub('""', strip_line_comment(lines[cursor]))
            depth += code.count("{") - code.count("}")
            inside.add(cursor)
            cursor += 1
            if depth <= 0:
                break
        index = cursor
    return inside


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


def scan_file(root: Path, rel: Path, declared_test: bool = False) -> list[dict]:
    try:
        text = (root / rel).read_text(encoding="utf-8", errors="replace")
    except OSError:
        return []
    # Most files mention neither; skip the line-by-line pass for them.
    if "temp_dir" not in text and "/tmp" not in text:
        return []
    lines = text.splitlines()
    whole_file = declared_test or is_test_file(rel)
    test_lines = set() if whole_file else inline_test_lines(lines)
    if not whole_file and not test_lines:
        return []
    aliases = set()
    for line in lines:
        aliases.update(RENAMED_IMPORT.findall(strip_line_comment(line)))
    alias_calls = [re.compile(rf"\b{re.escape(alias)}\s*\(\s*\)") for alias in sorted(aliases)]

    findings = []
    for index, line in enumerate(lines):
        if not whole_file and index not in test_lines:
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
    files: list[Path] = []
    for scan_dir in SCAN_DIRS:
        base = root / scan_dir
        if not base.is_dir():
            continue
        for path in sorted(base.rglob("*.rs")):
            rel = path.relative_to(root)
            if SKIP_DIR_NAMES.intersection(rel.parts) or rel == HELPER:
                continue
            files.append(rel)
    declared = test_only_module_files(root, files)
    findings: list[dict] = []
    for rel in files:
        findings.extend(scan_file(root, rel, rel in declared))
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
