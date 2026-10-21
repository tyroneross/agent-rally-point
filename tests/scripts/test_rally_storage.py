# SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location('rally_storage', Path(__file__).resolve().parents[2] / 'scripts/rally_storage.py')
storage = importlib.util.module_from_spec(spec)
spec.loader.exec_module(storage)

class StorageTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.name = 'facts.db.corrupt.123456'
        self.payload = b'old quarantine evidence\0' * 50000

    def test_round_trip_preserves_active_and_recovery_files(self):
        (self.root / self.name).write_bytes(self.payload)
        for name in ('facts.db', 'facts.db-wal', 'recovery.bundle'):
            (self.root / name).write_bytes(b'protected')
        result = storage.compress_quarantine(self.root, self.name)
        self.assertGreater(result['saved_bytes'], 0)
        self.assertFalse((self.root / self.name).exists())
        storage.restore_archive(Path(result['archive']), self.root / self.name)
        self.assertEqual((self.root / self.name).read_bytes(), self.payload)
        for name in ('facts.db', 'facts.db-wal', 'recovery.bundle'):
            self.assertEqual((self.root / name).read_bytes(), b'protected')
        # Interrupted cleanup can safely verify the existing archive on retry.
        self.assertEqual(storage.compress_quarantine(self.root, self.name)['sha256'], result['sha256'])

    def test_reject_active_path_escape_symlink_and_archive_collision(self):
        for name in ('facts.db', '../facts.db.corrupt.123', 'log/a.jsonl', 'recovery.bundle'):
            with self.assertRaises(ValueError):
                storage.compress_quarantine(self.root, name)
        target = self.root / 'protected'
        target.write_bytes(self.payload)
        (self.root / self.name).symlink_to(target)
        with self.assertRaises(OSError):
            storage.compress_quarantine(self.root, self.name)
        (self.root / self.name).unlink()
        (self.root / self.name).write_bytes(self.payload)
        (self.root / (self.name + '.gz')).write_bytes(b'collision')
        with self.assertRaises((ValueError, OSError)):
            storage.compress_quarantine(self.root, self.name)
        self.assertEqual((self.root / self.name).read_bytes(), self.payload)

    def test_restore_never_overwrites_live_database(self):
        with self.assertRaises(ValueError):
            storage.restore_archive(self.root / (self.name + '.gz'), self.root / 'facts.db')

    def test_historical_wal_names_and_inventory_separate_builds_from_evidence(self):
        for suffix in ('', '-db-wal', '-db-shm', '-wal', '-shm'):
            name = self.name + suffix
            (self.root / name).write_bytes(self.payload)
            stamp = (self.root / name).stat().st_mtime_ns
            result = storage.compress_quarantine(self.root, name)
            self.assertEqual(result['source_mtime_ns'], stamp)
            storage.restore_archive(Path(result['archive']), self.root / name)
            self.assertEqual((self.root / name).read_bytes(), self.payload)
        for name in ('worktrees/w/target/debug/bin', 'worktrees/w/source.rs',
                     '.hook-events/a.state', 'task-results/a.txt', 'log/a.jsonl'):
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b'evidence')
        groups = storage.inventory(self.root)['groups']
        for name in ('build_cache', 'worktree_source', 'hook_cache', 'task_capture', 'ledger'):
            self.assertEqual(groups[name]['files'], 1)
        self.assertEqual(groups['quarantine']['files'], 5)

    def test_empty_snapshots_do_not_grow_during_compression(self):
        (self.root / self.name).touch()
        result = storage.compress_quarantine(self.root, self.name)
        self.assertEqual(result['state'], 'retained_not_smaller')
        self.assertTrue((self.root / self.name).exists())
        self.assertFalse((self.root / (self.name + '.gz')).exists())

    def test_marker_maintenance_preserves_long_unknown_future_boundary_and_locked(self):
        rows = {
            'expired.state': '1 1 0 0 1\nretention_secs=5\n',
            'long.state': '9300 1 0 0 1\nretention_secs=3600\n',
            'legacy.state': '1 1 0 0 1\n',
            'unknown.state': '1 1 0 0 1\nretention_secs=unknown\n',
            'future.state': '20000 1 0 0 1\nretention_secs=5\n',
            'boundary.state': '1000 1 0 0 1\nretention_secs=9000\n',
            'locked.state': '1 1 0 0 1\nretention_secs=5\n',
        }
        for name, text in rows.items(): (self.root / name).write_text(text)
        (self.root / 'locked.state.lock').mkdir()
        protected = self.root / 'protected'
        protected.write_text(rows['expired.state'])
        (self.root / 'symlink.state').symlink_to(protected)
        result = storage.reclaim_hook_events(self.root, now=10000)
        self.assertEqual(result['removed'], 1)
        self.assertEqual(result['busy'], 1)
        self.assertEqual(result['errors'], [])
        self.assertFalse((self.root / 'expired.state').exists())
        self.assertEqual(protected.read_text(), rows['expired.state'])
        for name, text in rows.items():
            if name != 'expired.state': self.assertEqual((self.root / name).read_text(), text)

    def test_marker_refreshed_before_lock_is_not_deleted(self):
        marker = self.root / 'refresh.state'
        marker.write_text('1 1 0 0 1\nretention_secs=5\n')
        fresh = '10000 1 0 0 1\nretention_secs=5\n'
        original = Path.mkdir
        def refresh(path, *args, **kwargs):
            if path.name == 'refresh.state.lock': marker.write_text(fresh)
            return original(path, *args, **kwargs)
        with mock.patch.object(Path, 'mkdir', refresh):
            result = storage.reclaim_hook_events(self.root, now=10000)
        self.assertEqual(result['removed'], 0)
        self.assertEqual(marker.read_text(), fresh)

    def test_marker_maintenance_rejects_invalid_oversized_and_nonregular_records(self):
        for index, text in enumerate(('1 1 0 0 1\nretention_secs=0\n',
                                      'x 1 0 0 1\nretention_secs=5\n',
                                      '1 1 0 0 1\nretention_secs=18446744073709551616\n',
                                      '1' * 1000)):
            p = self.root / f'{index}.state';p.write_text(text)
            self.assertFalse(storage.marker_expired(p, 10000))
        import os
        fifo = self.root / 'fifo.state'
        os.mkfifo(fifo)
        self.assertFalse(storage.marker_expired(fifo, 10000))

if __name__ == '__main__':
    unittest.main()
