# SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
import importlib.util
from pathlib import Path
import tempfile
import unittest

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

if __name__ == '__main__':
    unittest.main()
