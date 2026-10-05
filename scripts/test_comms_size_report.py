import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("comms_size_report.py")
sys.path.insert(0, str(SCRIPT.parent))
import comms_size_report as c  # noqa: E402


def run_assert(metrics, budgets):
    with tempfile.TemporaryDirectory() as d:
        rep, bud = Path(d) / "r.json", Path(d) / "b.json"
        rep.write_text(json.dumps({"metrics": metrics}))
        bud.write_text(json.dumps({"budgets": budgets}))
        return subprocess.run([sys.executable, str(SCRIPT), "--from-report", str(rep), "--assert-budget", str(bud),
                               "--out", str(Path(d) / "o.json")], capture_output=True, text=True)


class BudgetAssert(unittest.TestCase):
    def test_exceeded_exits_1(self):
        p = run_assert({"frame_prefix_chars": 292}, {"frame_prefix_chars": 120})
        self.assertEqual(p.returncode, 1)
        self.assertIn("frame_prefix_chars: 292 > 120", p.stderr)

    def test_within_exits_0(self):
        self.assertEqual(run_assert({"frame_prefix_chars": 103}, {"frame_prefix_chars": 120}).returncode, 0)

    def test_unmeasured_budget_ignored(self):
        self.assertEqual(run_assert({}, {"frame_prefix_chars": 120}).returncode, 0)

    def test_measure_tokens(self):
        self.assertEqual(c.measure("a" * 36)["tokens"], 10.0)
        self.assertEqual(c.measure("é")["bytes"], 2)

    def test_frame_extraction_from_fixture(self):
        f = c.measure_frames()
        self.assertTrue(f["samples"])
        self.assertTrue(f["prefix"]["chars"] > 200)


if __name__ == "__main__":
    unittest.main()
