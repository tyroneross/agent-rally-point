#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""append_run_provenance — validate commit/goal provenance for build-loop's
`.build-loop/state.json` `runs[]` append path (`append_run`).

This module implements enforce-candidate E3 ("append_run provenance check")
as a standalone, testable validator that build-loop's `append_run` (which
lives in the external build-loop plugin, not this repo) can call before it
writes a new `runs[]` entry.

Two checks:

1. COMMIT provenance — if the caller supplies a commit SHA, verify it is
   reachable from the run's push range (or, absent a push range, from HEAD).
   A `None`/empty/"pending" commit is allowed (mid-run append, before push).
   An unreachable commit is a BLOCK finding (fail closed).
2. GOAL provenance — if an intent/plan file is supplied, compare the
   caller-supplied goal text against that file's first markdown headline
   using difflib similarity. A low-similarity match is a WARN finding
   (flagged, never blocking) per the E3 spec.

Usable as a library (`validate_run_provenance`) or as a CLI:

    python3 scripts/append_run_provenance.py \\
        --run-id bl-... --commit 6616b71 --goal "..." \\
        --repo-root . --push-range cb9cba9..3cb8295 --json
"""

from __future__ import annotations

import argparse
import difflib
import json
import subprocess
import sys
from typing import Optional


def _run_git(repo_root: str, args: list) -> Optional[str]:
    """Run a git command, returning stdout text or None on any failure.

    Never raises: subprocess/OS errors, non-zero exit codes, and missing
    git binaries are all treated as "could not determine" so callers can
    fail closed.
    """
    try:
        result = subprocess.run(
            ["git", "-C", repo_root] + args,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    return result.stdout


def _extract_headline(path: str) -> Optional[str]:
    """Extract the first markdown headline (or first non-empty line) from path."""
    try:
        with open(path, "r", encoding="utf-8") as fh:
            lines = fh.readlines()
    except OSError:
        return None
    first_nonempty = None
    for line in lines:
        stripped = line.strip()
        if not stripped:
            continue
        if first_nonempty is None:
            first_nonempty = stripped
        if stripped.startswith("#"):
            return stripped.lstrip("#").strip()
    return first_nonempty


def validate_run_provenance(
    *,
    run_id: str,
    commit: Optional[str],
    goal: str,
    repo_root: str,
    push_range: Optional[str] = None,
    intent_path: Optional[str] = None,
    plan_path: Optional[str] = None,
    similarity_threshold: float = 0.5,
) -> dict:
    """Validate that `commit` and `goal` are corroborated by git history and
    the run's own intent/plan docs before an append_run write.

    Returns {"ok": bool, "findings": [...], "derived_commit": str|None}.
    """
    findings = []

    derived_commit = _run_git(repo_root, ["rev-parse", "HEAD"])
    if derived_commit is not None:
        derived_commit = derived_commit.strip()

    is_pending = commit is None or commit == "" or commit == "pending"

    if not is_pending:
        reachable_output = None
        if push_range:
            reachable_output = _run_git(repo_root, ["rev-list", push_range])
        if reachable_output is None:
            reachable_output = _run_git(repo_root, ["rev-list", "HEAD"])

        reachable = False
        if reachable_output is not None:
            revs = reachable_output.split()
            for rev in revs:
                if rev == commit or rev.startswith(commit) or commit.startswith(rev):
                    reachable = True
                    break

        if not reachable:
            findings.append(
                {
                    "code": "commit_unreachable",
                    "severity": "block",
                    "detail": (
                        f"commit {commit!r} for run {run_id!r} is not reachable "
                        f"from {'push_range ' + push_range if push_range else 'HEAD'}"
                    ),
                }
            )

    headline = None
    source_path = intent_path or plan_path
    if source_path:
        headline = _extract_headline(source_path)

    if headline:
        ratio = difflib.SequenceMatcher(None, goal.lower(), headline.lower()).ratio()
        if ratio < similarity_threshold:
            findings.append(
                {
                    "code": "goal_mismatch",
                    "severity": "warn",
                    "detail": (
                        f"goal {goal!r} has similarity {ratio:.2f} (< "
                        f"{similarity_threshold}) vs headline {headline!r} in "
                        f"{source_path!r}"
                    ),
                }
            )

    ok = not any(f["severity"] == "block" for f in findings)
    return {"ok": ok, "findings": findings, "derived_commit": derived_commit}


def _build_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Validate commit/goal provenance for an append_run write.",
    )
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--commit", default=None)
    parser.add_argument("--goal", required=True)
    parser.add_argument("--repo-root", default=".")
    parser.add_argument("--push-range", default=None)
    parser.add_argument("--intent", default=None, dest="intent_path")
    parser.add_argument("--plan", default=None, dest="plan_path")
    parser.add_argument("--threshold", type=float, default=0.5)
    parser.add_argument("--json", action="store_true")
    return parser


def main(argv: Optional[list] = None) -> int:
    parser = _build_arg_parser()
    try:
        args = parser.parse_args(argv)
    except SystemExit as exc:
        return 2 if exc.code else 0

    try:
        result = validate_run_provenance(
            run_id=args.run_id,
            commit=args.commit,
            goal=args.goal,
            repo_root=args.repo_root,
            push_range=args.push_range,
            intent_path=args.intent_path,
            plan_path=args.plan_path,
            similarity_threshold=args.threshold,
        )
    except Exception as exc:  # noqa: BLE001 - fail closed, never traceback
        print(f"append_run_provenance: usage/IO error: {exc}", file=sys.stderr)
        return 2

    block_findings = [f for f in result["findings"] if f["severity"] == "block"]
    warn_findings = [f for f in result["findings"] if f["severity"] == "warn"]

    print(f"append_run_provenance: run_id={args.run_id} ok={result['ok']}", file=sys.stderr)
    for f in block_findings:
        print(f"  [BLOCK] {f['code']}: {f['detail']}", file=sys.stderr)
    for f in warn_findings:
        print(f"  [WARN]  {f['code']}: {f['detail']}", file=sys.stderr)

    if args.json:
        print(json.dumps(result))

    return 0 if result["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
