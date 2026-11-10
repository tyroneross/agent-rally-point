#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Tests for the test scratch-folder lint.

Run: python3 tests/scripts/test_lint_test_temp_dir.py

Each spelling a test could use to reach the system temp directory is planted
once and must be flagged. A lint proven on a single spelling would pass while
the leak came back through another.
"""

from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
LINT = REPO / "scripts/lint_test_temp_dir.py"


def load_lint():
    spec = importlib.util.spec_from_file_location("lint_test_temp_dir", LINT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class Tree:
    def __init__(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)

    def write(self, rel: str, text: str) -> None:
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")

    def close(self) -> None:
        self._tmp.cleanup()


class LintTestTempDir(unittest.TestCase):
    def setUp(self):
        self.lint = load_lint()
        self.tree = Tree()
        self.addCleanup(self.tree.close)

    def flagged(self) -> list[tuple[str, int]]:
        return [(f["file"], f["line"]) for f in self.lint.scan(self.tree.root)]

    def test_flags_the_full_path_in_an_integration_test(self):
        self.tree.write("crates/a/tests/t.rs", "fn f() {\n    let d = std::env::temp_dir();\n}\n")
        self.assertEqual(self.flagged(), [("crates/a/tests/t.rs", 2)])

    def test_flags_the_short_path(self):
        self.tree.write("crates/a/tests/t.rs", "use std::env;\nfn f() { let d = env::temp_dir(); }\n")
        self.assertEqual(self.flagged(), [("crates/a/tests/t.rs", 2)])

    def test_flags_a_bare_imported_call(self):
        self.tree.write("crates/a/tests/t.rs", "use std::env::temp_dir;\nfn f() { let d = temp_dir(); }\n")
        self.assertEqual(self.flagged(), [("crates/a/tests/t.rs", 2)])

    def test_flags_a_renamed_import(self):
        self.tree.write(
            "crates/a/tests/t.rs",
            "use std::env::temp_dir as scratch;\nfn f() { let d = scratch(); }\n",
        )
        self.assertEqual(self.flagged(), [("crates/a/tests/t.rs", 2)])

    def test_flags_a_call_inside_a_cfg_test_module_only(self):
        self.tree.write(
            "crates/a/src/lib.rs",
            "pub fn prod() { let d = std::env::temp_dir(); }\n"
            "#[cfg(test)]\n"
            "mod tests {\n"
            "    fn f() { let d = std::env::temp_dir(); }\n"
            "}\n",
        )
        self.assertEqual(self.flagged(), [("crates/a/src/lib.rs", 4)])

    def test_flags_a_tests_suffixed_source_file(self):
        self.tree.write("crates/a/src/x_tests.rs", "fn f() { let d = std::env::temp_dir(); }\n")
        self.assertEqual(self.flagged(), [("crates/a/src/x_tests.rs", 1)])

    def test_flags_a_formatted_path_under_slash_tmp(self):
        self.tree.write(
            "crates/a/tests/t.rs",
            'fn f() { let d = PathBuf::from(format!("/tmp/x{}", std::process::id())); }\n',
        )
        self.assertEqual(self.flagged(), [("crates/a/tests/t.rs", 1)])

    def test_flags_a_joined_path_under_slash_tmp(self):
        self.tree.write(
            "crates/a/tests/t.rs",
            'fn f() { let d = PathBuf::from("/tmp").join(format!("s-{n}.sock")); }\n',
        )
        self.assertEqual(self.flagged(), [("crates/a/tests/t.rs", 1)])

    def test_flags_a_formatted_slash_tmp_path_split_across_lines(self):
        self.tree.write(
            "crates/a/tests/t.rs",
            'fn f() {\n    let s = format!(\n        "/tmp/x-{}-{}.sock",\n        1, 2\n    );\n}\n',
        )
        self.assertEqual(self.flagged(), [("crates/a/tests/t.rs", 3)])

    def test_flags_a_slash_tmp_base_joined_on_the_next_line(self):
        self.tree.write(
            "crates/a/tests/t.rs",
            'fn f() {\n    let base = PathBuf::from("/tmp")\n        .join(format!("x-{}", 1));\n}\n',
        )
        self.assertEqual(self.flagged(), [("crates/a/tests/t.rs", 2)])

    def test_leaves_a_fixed_slash_tmp_value_alone(self):
        self.tree.write(
            "crates/a/tests/t.rs",
            'fn f() { let a = Path::new("/tmp/foo"); let b = PathBuf::from("/tmp"); }\n',
        )
        self.assertEqual(self.flagged(), [])

    def test_flags_a_sibling_file_declared_under_cfg_test(self):
        self.tree.write(
            "crates/a/src/store.rs",
            "pub fn prod() {}\n#[cfg(test)]\nmod concurrency;\n",
        )
        self.tree.write(
            "crates/a/src/store/concurrency.rs",
            "mod deeper;\nfn f() { let d = std::env::temp_dir(); }\n",
        )
        self.tree.write(
            "crates/a/src/store/concurrency/deeper.rs",
            "fn g() { let d = std::env::temp_dir(); }\n",
        )
        self.assertEqual(
            sorted(self.flagged()),
            [("crates/a/src/store/concurrency.rs", 2), ("crates/a/src/store/concurrency/deeper.rs", 1)],
        )

    def test_a_sibling_file_declared_without_cfg_test_is_production(self):
        self.tree.write("crates/a/src/lib.rs", "mod store;\n")
        self.tree.write("crates/a/src/store.rs", "pub fn p() { let d = std::env::temp_dir(); }\n")
        self.assertEqual(self.flagged(), [])

    def test_production_code_after_a_cfg_test_item_is_not_flagged(self):
        self.tree.write(
            "crates/a/src/lib.rs",
            "#[cfg(test)]\nuse std::sync::Mutex;\n"
            "pub fn prod() { let d = std::env::temp_dir(); }\n"
            "#[cfg(test)]\nmod tests {\n"
            "    fn f() { let s = \"{\"; let d = std::env::temp_dir(); }\n"
            "}\n"
            "pub fn later() { let d = std::env::temp_dir(); }\n",
        )
        self.assertEqual(self.flagged(), [("crates/a/src/lib.rs", 6)])

    def test_ignores_production_code(self):
        self.tree.write("crates/a/src/lib.rs", "pub fn prod() { let d = std::env::temp_dir(); }\n")
        self.assertEqual(self.flagged(), [])

    def test_allows_an_annotated_root_read(self):
        self.tree.write(
            "crates/a/tests/t.rs",
            "fn f() { let d = std::env::temp_dir(); } // temp-root-read: containment check\n",
        )
        self.assertEqual(self.flagged(), [])

    def test_an_annotation_with_no_reason_does_not_count(self):
        self.tree.write(
            "crates/a/tests/t.rs",
            "fn f() { let d = std::env::temp_dir(); } // temp-root-read:\n",
        )
        self.assertEqual(self.flagged(), [("crates/a/tests/t.rs", 1)])

    def test_ignores_comments_and_the_helper_itself(self):
        self.tree.write("crates/a/tests/t.rs", "// was: std::env::temp_dir()\n/// see temp_dir()\n")
        self.tree.write("crates/test-support/temp_root.rs", "fn b() { std::env::temp_dir(); }\n")
        self.assertEqual(self.flagged(), [])

    def test_ignores_build_output(self):
        self.tree.write("crates/a/target/tests/t.rs", "fn f() { std::env::temp_dir(); }\n")
        self.assertEqual(self.flagged(), [])

    def test_exit_codes(self):
        self.tree.write("crates/a/tests/t.rs", "fn f() { std::env::temp_dir(); }\n")
        root = ["--root", str(self.tree.root)]
        self.assertEqual(self.lint.main(root), 1)
        self.assertEqual(self.lint.main(root + ["--warn"]), 0)
        (self.tree.root / "crates/a/tests/t.rs").write_text("fn f() {}\n", encoding="utf-8")
        self.assertEqual(self.lint.main(root), 0)


if __name__ == "__main__":
    unittest.main()
