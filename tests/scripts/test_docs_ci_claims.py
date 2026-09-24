#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Docs-vs-CI lint: fail when a shipped doc's claim about CI behavior drifts
from what the workflows actually do.

This does not re-derive every fact in the docs it checks; it pins the four
claims that have drifted before (an auditor found a "no cargo audit has run"
sentence next to a workflow that installs cargo-audit, and a "degrades to a
warning" claim next to a claim path that is flatly refused) so a future edit
to either side trips a test instead of a doc review.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS_DIR = ROOT / ".github/workflows"
RALLY_MD = ROOT / "RALLY.md"
TRUST_MODEL_MD = ROOT / "docs/security/TRUST-MODEL.md"
JSON_ENVELOPE_MD = ROOT / "docs/JSON_ENVELOPE.md"
SCHEMAS_DIR = ROOT / "docs/schemas"

# Deliberately loose: matches "No `cargo audit` or\n`cargo deny` vulnerability
# pass has been run." and near variants, without pinning exact punctuation.
NO_AUDIT_CLAIM_RE = re.compile(
    r"no\s+`?cargo audit`?.{0,120}?has been run", re.IGNORECASE | re.DOTALL
)

WORKSPACE_DEGRADES_CLAIM_RE = re.compile(
    r"workspace[^.]{0,200}?degrade[sd]?\s+to\s+a\s+warning", re.IGNORECASE | re.DOTALL
)

SCHEMA_COVERAGE_RE = re.compile(
    r"Schema coverage:\s*(\d+)\s+of\s+(\d+)\s+top-level commands have a schema file",
    re.IGNORECASE,
)

SCHEMA_FILE_COUNT_RE = re.compile(
    r"the\s+(\d+)\s+`?docs/schemas/agent-rally\.command\.\*\.v1\.json`?\s+files",
    re.IGNORECASE,
)


def workflow_texts() -> dict[str, str]:
    return {
        path.name: path.read_text(encoding="utf-8")
        for path in sorted(WORKFLOWS_DIR.glob("*.yml"))
    }


def any_workflow_runs_cargo_audit_or_deny() -> bool:
    for text in workflow_texts().values():
        if "cargo-audit" in text or "cargo-deny" in text:
            return True
        if re.search(r"cargo\s+audit\b", text) or re.search(r"cargo\s+deny\b", text):
            return True
    return False


class DocsMatchCiClaims(unittest.TestCase):
    def test_trust_model_does_not_claim_no_audit_pass_when_a_workflow_runs_one(self) -> None:
        if not any_workflow_runs_cargo_audit_or_deny():
            self.skipTest("no workflow installs or runs cargo-audit/cargo-deny")
        text = TRUST_MODEL_MD.read_text(encoding="utf-8")
        match = NO_AUDIT_CLAIM_RE.search(text)
        self.assertIsNone(
            match,
            "docs/security/TRUST-MODEL.md claims no cargo audit/deny pass has ever "
            f"run, but a workflow under {WORKFLOWS_DIR} installs or runs one: "
            f"{match.group(0) if match else ''!r}",
        )
        self.assertIn(
            "cargo audit",
            text,
            "docs/security/TRUST-MODEL.md must mention `cargo audit` given that a "
            "workflow runs it (even conditionally) — silence here is as misleading "
            "as the false negative claim.",
        )

    def test_rally_md_does_not_claim_workspace_claims_degrade_to_a_warning(self) -> None:
        text = RALLY_MD.read_text(encoding="utf-8")
        match = WORKSPACE_DEGRADES_CLAIM_RE.search(text)
        self.assertIsNone(
            match,
            "RALLY.md claims a workspace:*/repo:* claim from a non-lead degrades "
            "to a warning; it is refused outright (exit 2, nothing committed) — "
            f"see crates/rally-cli/src/store.rs breadth_violation. Match: "
            f"{match.group(0) if match else ''!r}",
        )

    def test_json_envelope_documents_the_error_envelope_keys(self) -> None:
        text = JSON_ENVELOPE_MD.read_text(encoding="utf-8")
        for key in ('"error"', '"exit_code"'):
            self.assertIn(
                key,
                text,
                f"docs/JSON_ENVELOPE.md must document the error-envelope key {key} "
                "(crates/rally-cli/src/output.rs CliError::error_text)",
            )

    def test_json_envelope_schema_coverage_count_matches_the_schemas_directory(self) -> None:
        text = JSON_ENVELOPE_MD.read_text(encoding="utf-8")

        coverage_match = SCHEMA_COVERAGE_RE.search(text)
        self.assertIsNotNone(
            coverage_match,
            "docs/JSON_ENVELOPE.md must state schema coverage as "
            '"Schema coverage: N of M top-level commands have a schema file" '
            "so this lint can parse and verify it.",
        )
        assert coverage_match is not None
        documented_n, documented_m = (int(coverage_match.group(1)), int(coverage_match.group(2)))
        self.assertLessEqual(documented_n, documented_m, "documented N must not exceed documented M")

        # Ground truth: the schema-coverage count in JSON_ENVELOPE.md must equal
        # the number of docs/schemas/agent-rally.command.*.v1.json files. The
        # doc separately explains any gap between that file count and the
        # matched-command count N (e.g. a file documenting a non-command
        # transport exception); this lint pins the literal file-count claim so
        # it cannot drift silently when a schema file is added or removed.
        file_count_match = SCHEMA_FILE_COUNT_RE.search(text)
        self.assertIsNotNone(
            file_count_match,
            "docs/JSON_ENVELOPE.md must state the schema file count as "
            '"the N `docs/schemas/agent-rally.command.*.v1.json` files" '
            "so this lint can parse and verify it.",
        )
        assert file_count_match is not None
        documented_file_count = int(file_count_match.group(1))
        actual_file_count = len(sorted(SCHEMAS_DIR.glob("agent-rally.command.*.v1.json")))
        self.assertEqual(
            documented_file_count,
            actual_file_count,
            "docs/JSON_ENVELOPE.md's schema-coverage note must be re-derived "
            "when docs/schemas/agent-rally.command.*.v1.json changes: documented "
            f"{documented_file_count}, found {actual_file_count} on disk.",
        )


if __name__ == "__main__":
    unittest.main()
