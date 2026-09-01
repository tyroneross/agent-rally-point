#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Sibling-asymmetry lint — name the peer file that omits what its peers set.

WHAT IT KEYS ON
---------------
A convention that spread by copy-paste rather than by mechanism stops at the
first file that did not copy, and that file is where the bug lands. The signal
is not prose ("flaky", "timeout") — those words appear everywhere and would
fire constantly. The signal is a STRUCTURAL ASYMMETRY inside a peer group: a
clear majority of files that do the same kind of thing set some property, and a
minority does not.

Worked example, and the reason this exists. `DEFAULT_WATCHDOG_TIMEOUT_MS` is
3000ms in `crates/rally-cli/src/lib.rs` — correct for production, wrong for a
test that starts real daemon children on a loaded machine. Fifteen test files
worked around it individually with `--timeout-ms` / `RALLY_HOOK_TIMEOUT_MS`.
`referenced_handoff_targeting.rs` did not, and it is the file that flaked.

HOW A PEER GROUP IS DEFINED
---------------------------
Two mechanical stages, both regex over file text — no history, no model:

1. `cohort` — does this file participate at all? (here: does it spawn the
   rally binary)
2. `refine` — is it the SAME SHAPE as the others? (here: does it spawn a
   background child, and therefore run concurrent work under a wall clock)

Stage 2 matters. The proposal's plain reading — "files in one test directory
that invoke the same binary" — puts 36 files in one group of which 19 set a
budget. 53% is not a majority, so the unrefined grouping emits NOTHING and
would have missed the real bug. Peer groups have to be shape-matched, and this
script reports the unrefined numbers alongside the refined ones so that
judgement stays visible rather than buried in a threshold.

PRECISION
---------
WARN-only (exit 0) until precision is measured on a real corpus and reported
with its denominator. `--strict` opts into a non-zero exit; nothing should
enable it before the measurement exists. A detector that fires on every run
gets ignored within a week, and then the next N-instance bug goes unconnected
anyway.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

# --------------------------------------------------------------------------
# Rules. Data, not code — a new asymmetry is a new entry here.
# --------------------------------------------------------------------------

RULES: list[dict] = [
    {
        "id": "test-watchdog-budget",
        "root": "crates/rally-cli/tests",
        "glob": "*.rs",
        # A peer at all: the file spawns the rally binary.
        "cohort": r"CARGO_BIN_EXE_rally|rally_cmd::|rally_command\s*\(",
        # Same shape: the file spawns a BACKGROUND child, so it runs
        # concurrent work and is wall-clock sensitive. A file that only makes
        # blocking one-shot calls is not a peer of one that runs a daemon.
        "refine": r"\.spawn\(\)",
        # The property the majority sets: an explicit watchdog budget, either
        # per call site or via the shared choke point.
        "property": r"--timeout-ms|RALLY_HOOK_TIMEOUT_MS|rally_command\s*\(",
        "threshold": 0.6,
        # Excluded from BOTH numerator and denominator: for these the watchdog
        # is the subject under test, so their budgets are the measurement, not
        # evidence that a convention spread.
        "exempt": [
            "watchdog_timeout.rs",
            "watchdog_write_durability.rs",
            "watchdog_concurrency.rs",
            "retry_budget_watchdog.rs",
        ],
        "property_name": "an explicit watchdog budget",
        "remedy": (
            "spawn the binary through `support::rally_cmd::rally_command()`, "
            "which carries TEST_WATCHDOG_TIMEOUT_MS, instead of "
            "`Command::new(env!(\"CARGO_BIN_EXE_rally\"))`"
        ),
    },
]


@dataclass
class RuleResult:
    rule_id: str
    property_name: str
    remedy: str
    threshold: float
    cohort_size: int = 0
    cohort_with_property: int = 0
    group_size: int = 0
    group_with_property: int = 0
    majority_ratio: float = 0.0
    fired: bool = False
    outliers: list[str] = field(default_factory=list)
    conformers: list[str] = field(default_factory=list)
    exempted: list[str] = field(default_factory=list)
    skipped_reason: str | None = None

    def as_dict(self) -> dict:
        return {
            "rule": self.rule_id,
            "property": self.property_name,
            "threshold": self.threshold,
            "cohort": {
                "size": self.cohort_size,
                "with_property": self.cohort_with_property,
                "ratio": round(self.cohort_ratio, 3),
                "note": (
                    "unrefined grouping — reported for transparency, not used "
                    "for the verdict"
                ),
            },
            "peer_group": {
                "size": self.group_size,
                "with_property": self.group_with_property,
                "ratio": round(self.majority_ratio, 3),
            },
            "fired": self.fired,
            "outliers": self.outliers,
            "conformers": self.conformers,
            "exempted": self.exempted,
            "remedy": self.remedy,
            "skipped_reason": self.skipped_reason,
        }

    @property
    def cohort_ratio(self) -> float:
        return self.cohort_with_property / self.cohort_size if self.cohort_size else 0.0


def evaluate_rule(repo: Path, rule: dict) -> RuleResult:
    result = RuleResult(
        rule_id=rule["id"],
        property_name=rule["property_name"],
        remedy=rule["remedy"],
        threshold=rule["threshold"],
    )
    root = repo / rule["root"]
    if not root.is_dir():
        result.skipped_reason = f"{rule['root']} is not a directory under {repo}"
        return result

    cohort_re = re.compile(rule["cohort"])
    refine_re = re.compile(rule["refine"]) if rule.get("refine") else None
    property_re = re.compile(rule["property"])
    exempt = set(rule.get("exempt", []))

    group_hits: list[str] = []
    group_misses: list[str] = []
    for path in sorted(root.glob(rule["glob"])):
        name = path.name
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        if not cohort_re.search(text):
            continue
        has_property = bool(property_re.search(text))
        result.cohort_size += 1
        result.cohort_with_property += int(has_property)
        if name in exempt:
            result.exempted.append(name)
            continue
        if refine_re is not None and not refine_re.search(text):
            continue
        (group_hits if has_property else group_misses).append(name)

    result.group_size = len(group_hits) + len(group_misses)
    result.group_with_property = len(group_hits)
    if result.group_size == 0:
        result.skipped_reason = "peer group is empty"
        return result

    result.majority_ratio = result.group_with_property / result.group_size
    if result.majority_ratio >= rule["threshold"] and group_misses:
        result.fired = True
        result.outliers = sorted(group_misses)
        result.conformers = sorted(group_hits)
    return result


def render(results: list[RuleResult]) -> str:
    lines: list[str] = []
    for r in results:
        if r.skipped_reason:
            lines.append(f"sibling-asymmetry[{r.rule_id}]: skipped — {r.skipped_reason}")
            continue
        if not r.fired:
            lines.append(
                f"sibling-asymmetry[{r.rule_id}]: no asymmetry "
                f"({r.group_with_property}/{r.group_size} peers set {r.property_name}, "
                f"threshold {r.threshold:.0%})"
            )
            continue
        lines.append(
            f"WARN sibling-asymmetry[{r.rule_id}]: "
            f"{r.group_with_property} of {r.group_size} peers set {r.property_name}; "
            f"{len(r.outliers)} do not."
        )
        for name in r.outliers:
            lines.append(f"  outlier: {name}")
        lines.append(f"  remedy: {r.remedy}")
        lines.append(
            f"  (unrefined cohort, for comparison: "
            f"{r.cohort_with_property}/{r.cohort_size} = {r.cohort_ratio:.0%})"
        )
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--repo",
        default=".",
        help="repository root to lint (default: cwd). Point at an extracted "
        "tree to run the lint against a past revision.",
    )
    parser.add_argument("--json", action="store_true", help="emit JSON")
    parser.add_argument(
        "--strict",
        action="store_true",
        help="exit 1 when an asymmetry fires. Do not enable this before "
        "precision has been measured on this repo's corpus.",
    )
    args = parser.parse_args(argv)

    repo = Path(args.repo).resolve()
    results = [evaluate_rule(repo, rule) for rule in RULES]

    if args.json:
        print(
            json.dumps(
                {
                    "repo": str(repo),
                    "severity": "warn",
                    "fired": any(r.fired for r in results),
                    "rules": [r.as_dict() for r in results],
                },
                indent=2,
            )
        )
    else:
        print(render(results))

    if args.strict and any(r.fired for r in results):
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
