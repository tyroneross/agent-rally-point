"""Exercise the shipped shell dedupe function without launching Rally or a host."""
from pathlib import Path
import os
import subprocess
import tempfile
import time
import unittest
import importlib.util


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = (ROOT / "hooks/rally-coordination-hook.sh").read_text()
FUNCTION = SCRIPT.split("_duplicate_hook_event() {", 1)[1].split(
    "\n}\n", 1
)[0]
FUNCTION = "_duplicate_hook_event() {" + FUNCTION + "\n}\n"
SPEC = importlib.util.spec_from_file_location('rally_storage', ROOT / 'scripts/rally_storage.py')
STORAGE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(STORAGE)


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
                lines = state.read_text().splitlines()
                fields = lines[0].split()
                fields[0] = str(stamp)
                lines[0] = " ".join(fields)
                state.write_text("\n".join(lines) + "\n")
                os.utime(state, (stamp, stamp))

            age_marker(700)
            stale_lock = state_dir / "abandoned.state.lock"
            stale_lock.mkdir()
            stale_time = int(time.time()) - 1200
            os.utime(stale_lock, (stale_time, stale_time))
            self.assertEqual(verdict("plugin", "other", window="5"), 1)
            self.assertTrue(state.exists(), "housekeeping deleted a live marker")
            self.assertEqual(STORAGE.reclaim_hook_events(state_dir)['errors'], [])
            self.assertTrue(state.exists(), "maintenance deleted a live marker")
            self.assertFalse(stale_lock.exists(), "abandoned lock was not reclaimed")
            self.assertEqual(verdict("project", "first"), 0)
            self.assertEqual(verdict("plugin", "first"), 1,
                             "same-source repeats must still run")
            age_marker(3601)
            self.assertEqual(verdict("plugin", "cleanup", window="5"), 1)
            self.assertTrue(state.exists(), "hook performed synchronous reclamation")
            STORAGE.reclaim_hook_events(state_dir)
            self.assertFalse(state.exists(), "expired metadata-backed marker was retained")
            self.assertEqual(verdict("project", "first"), 1,
                             "retention must not extend suppression")

    def test_legacy_unknown_future_and_locked_markers_are_preserved(self):
        with tempfile.TemporaryDirectory(prefix="rally-dedupe-legacy-") as work:
            root = Path(work)
            subprocess.run(["git", "init", "-q", work], check=True)
            events = root / "events"
            events.mkdir()
            now = int(time.time())
            contents = {
                'legacy.state': f'{now - 10000} 1 0 0 1\n',
                'unknown.state': f'{now - 10000} 1 0 0 1\nretention_secs=unknown\n',
                'future.state': f'{now + 10000} 1 0 0 1\nretention_secs=5\n',
                'locked.state': f'{now - 10000} 1 0 0 1\nretention_secs=5\n',
                'boundary.state': f'{now - 3600} 1 0 0 1\nretention_secs=3600\n',
            }
            for name, value in contents.items():
                (events / name).write_text(value)
            (events / 'locked.state.lock').mkdir()
            env = dict(os.environ, RALLY_HOOK_SOURCE='plugin', RALLY_HOOK_DEDUPE_SECS='5',
                       RALLY_HOOK_DEDUPE_DIR=str(events))
            # Pin wall time so the exact retention boundary is deterministic.
            command = FUNCTION + f'\ndate() {{ echo {now}; }}\n' + (
                'session=s; phase=before-write; input=other; _duplicate_hook_event')
            result = subprocess.run(['bash', '-c', command], cwd=root, env=env,
                                    capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, 1, result.stderr)
            for name, value in contents.items():
                self.assertEqual((events / name).read_text(), value)

    def test_old_readers_accept_new_metadata_and_retention_never_shrinks(self):
        with tempfile.TemporaryDirectory(prefix="rally-dedupe-compatible-") as work:
            root = Path(work)
            subprocess.run(['git', 'init', '-q', work], check=True)
            events = root / 'events'
            def call(window, source):
                env = dict(os.environ, RALLY_HOOK_SOURCE=source, RALLY_HOOK_DEDUPE_SECS=window,
                           RALLY_HOOK_DEDUPE_DIR=str(events))
                return subprocess.run(['bash', '-c', FUNCTION +
                                       '\nsession=s; phase=before-write; input=same; _duplicate_hook_event'],
                                      cwd=root, env=env, capture_output=True, timeout=5).returncode
            self.assertEqual(call('3600', 'plugin'), 1)
            self.assertEqual(call('5', 'project'), 0)
            marker = next(events.glob('*.state'))
            text = marker.read_text()
            self.assertEqual(text.splitlines()[1], 'retention_secs=3600')
            # The old Rust reader accepted exactly five numeric whitespace tokens.
            self.assertEqual(len([v for v in text.split() if v.isdecimal()]), 5)
            # The old shell reader consumes only the five-number first line.
            read = subprocess.run(['bash', '-c', 'read -r a b c d e < "$1"; '
                                   'printf "%s %s %s %s %s" "$a" "$b" "$c" "$d" "$e"',
                                   '_', str(marker)], capture_output=True, text=True, check=True)
            self.assertEqual(read.stdout, text.splitlines()[0])


if __name__ == "__main__":
    unittest.main()
