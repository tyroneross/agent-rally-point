"""Exercise the shipped shell dedupe function without launching Rally or a host."""
from pathlib import Path
import os
import subprocess
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = (ROOT / "hooks/rally-coordination-hook.sh").read_text()
FUNCTION = SCRIPT.split("_duplicate_hook_event() {", 1)[1].split(
    "\n}\n", 1
)[0]
FUNCTION = "_duplicate_hook_event() {" + FUNCTION + "\n}\n"


class DedupeRetentionTest(unittest.TestCase):
    def test_live_window_survives_housekeeping_and_expired_event_runs(self):
        with tempfile.TemporaryDirectory(prefix="rally-dedupe-retention-") as work:
            root = Path(work)
            subprocess.run(["git", "init", "-q", work], check=True)
            state_dir = root / "events"

            def verdict(source, material, window="3600"):
                env = dict(os.environ, RALLY_HOOK_SOURCE=source,
                           RALLY_HOOK_DEDUPE_SECS=window,
                           RALLY_HOOK_DEDUPE_DIR=str(state_dir),
                           TEST_MATERIAL=material)
                return subprocess.run(
                    ["bash", "-c", FUNCTION + '\nsession=s; phase=before-write; '
                     'input="$TEST_MATERIAL"; _duplicate_hook_event'],
                    cwd=root, env=env, check=False, timeout=5,
                    capture_output=True, text=True,
                ).returncode

            self.assertEqual(verdict("plugin", "first"), 1)
            state = next(state_dir.glob("*.state"))

            def age_marker(seconds):
                stamp = int(time.time()) - seconds
                fields = state.read_text().split()
                fields[0] = str(stamp)
                state.write_text(" ".join(fields) + "\n")
                os.utime(state, (stamp, stamp))

            age_marker(700)
            stale_lock = state_dir / "abandoned.state.lock"
            stale_lock.mkdir()
            stale_time = int(time.time()) - 1200
            os.utime(stale_lock, (stale_time, stale_time))
            self.assertEqual(verdict("plugin", "other", window="5"), 1)
            self.assertTrue(state.exists(), "housekeeping deleted a live marker")
            self.assertFalse(stale_lock.exists(), "abandoned lock was not reclaimed")
            self.assertEqual(verdict("project", "first"), 0)
            self.assertEqual(verdict("plugin", "first"), 1,
                             "same-source repeats must still run")
            age_marker(3601)
            self.assertEqual(verdict("project", "first"), 1,
                             "retention must not extend suppression")


if __name__ == "__main__":
    unittest.main()
