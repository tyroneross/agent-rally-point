#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Offline replay of team recall retests R1 (verbatim) and R3 (mutation).

Lead writes an ordered list file; N simulated members read it back. A member
may be handed a stale or mutated copy to prove the checker catches drift.

Usage:
    python3 scripts/team_recall_retest.py --list-file <path> --members 3
    python3 scripts/team_recall_retest.py --list-file <path> --stale-copy <path>

Exit 0 when all members reproduce the lead list verbatim in order; non-zero
with a diff summary otherwise.
"""

from __future__ import annotations

import argparse
import sys


def _read_lines(path: str) -> list[str]:
    with open(path, encoding="utf-8") as fh:
        return [line.rstrip("\n") for line in fh if line.strip()]


def check_recall(lead_lines: list[str], member_lines: list[str]) -> list[str]:
    """Return human-readable mismatches; empty means verbatim recall."""
    problems: list[str] = []
    if len(member_lines) != len(lead_lines):
        problems.append(
            f"line count: lead={len(lead_lines)} member={len(member_lines)}"
        )
    for i, (want, got) in enumerate(zip(lead_lines, member_lines), start=1):
        if want != got:
            problems.append(f"line {i}: want={want!r} got={got!r}")
    return problems


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Replay team recall retests.")
    parser.add_argument("--list-file", required=True, help="Lead-authored list.")
    parser.add_argument("--members", type=int, default=2, help="Members to simulate.")
    parser.add_argument(
        "--stale-copy",
        default=None,
        help="Optional mutated copy given to the last member (must fail).",
    )
    args = parser.parse_args(argv)

    lead = _read_lines(args.list_file)
    failures: dict[int, list[str]] = {}
    for member in range(args.members):
        if args.stale_copy and member == args.members - 1:
            got = _read_lines(args.stale_copy)
        else:
            got = list(lead)
        problems = check_recall(lead, got)
        if problems:
            failures[member] = problems

    if args.stale_copy:
        if args.members - 1 not in failures:
            print("ERROR: stale copy was NOT detected (checker is blind)")
            return 1
        print(f"OK: stale copy detected for member {args.members - 1}")
        good = {k: v for k, v in failures.items() if k != args.members - 1}
        if good:
            print(f"ERROR: unexpected failures in non-stale members: {good}")
            return 1
        return 0

    if failures:
        print(f"FAIL: recall mismatch: {failures}")
        return 1
    print(f"OK: {args.members} members recalled {len(lead)} lines verbatim")
    return 0


if __name__ == "__main__":
    sys.exit(main())
