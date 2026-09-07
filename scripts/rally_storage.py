#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Bounded storage inventory and lossless compression of quarantined DB files.

Live ledger, database/WAL, task results and recovery bundles are never candidates.
Compression is explicit and only replaces facts.db.corrupt.<timestamp> snapshots.
"""
import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import tempfile

QUARANTINE = re.compile(r"facts\.db\.corrupt\.\d+(?:-db-(?:wal|shm))?$")
BLOCK = 1024 * 1024

def fingerprint(st):
    return (st.st_dev, st.st_ino, st.st_size, st.st_mtime_ns, st.st_ctime_ns)

def digest_stream(stream):
    h = hashlib.sha256()
    size = 0
    while data := stream.read(BLOCK):
        h.update(data)
        size += len(data)
    return h.hexdigest(), size

def sync_dir(path):
    fd = os.open(path, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)

def inventory(root):
    groups = {}
    for directory, dirs, files in os.walk(root, followlinks=False):
        dirs[:] = [d for d in dirs if not Path(directory, d).is_symlink()]
        for name in files:
            path = Path(directory, name)
            st = path.lstat()
            if not stat.S_ISREG(st.st_mode):
                continue
            rel = path.relative_to(root)
            category = ('quarantine' if QUARANTINE.fullmatch(name) and len(rel.parts) == 1 else
                        'compressed_quarantine' if name.endswith('.gz') and QUARANTINE.fullmatch(name[:-3]) else
                        'ledger' if rel.parts[0] in ('log', 'archive') and name.endswith('.jsonl') else
                        'database' if name in ('facts.db', 'facts.db-wal', 'facts.db-shm') else
                        'recovery' if name.endswith('.bundle') else 'other')
            group = groups.setdefault(category, {'files': 0, 'bytes': 0})
            group['files'] += 1
            group['bytes'] += st.st_size
    return {'schema': 'agent-rally.storage.inventory.v1', 'root': str(root.resolve()), 'groups': groups,
            'note': 'Live size snapshot; only quarantined DB snapshots are compression candidates.'}

def compress_quarantine(root, name):
    if not QUARANTINE.fullmatch(name):
        raise ValueError('only a quarantined DB basename is accepted')
    root = root.resolve(strict=True)
    source = root / name
    dest = root / (name + '.gz')
    fd = os.open(source, os.O_RDONLY | os.O_NOFOLLOW)
    temp = None
    try:
        original = os.fstat(fd)
        if not stat.S_ISREG(original.st_mode) or original.st_nlink != 1:
            raise ValueError('source must be a regular unlinked quarantine snapshot')
        out_fd, temp = tempfile.mkstemp(prefix='.quarantine-', dir=root)
        with os.fdopen(out_fd, 'wb') as out, os.fdopen(os.dup(fd), 'rb') as inp:
            h = hashlib.sha256()
            with gzip.GzipFile(filename='', mode='wb', fileobj=out, mtime=0) as zipped:
                while data := inp.read(BLOCK):
                    h.update(data)
                    zipped.write(data)
            out.flush()
            os.fsync(out.fileno())
        with gzip.open(temp, 'rb') as inp:
            recovered_hash, recovered_size = digest_stream(inp)
        if (recovered_hash, recovered_size) != (h.hexdigest(), original.st_size):
            raise ValueError('compressed recovery verification failed')
        if fingerprint(os.fstat(fd)) != fingerprint(original) or fingerprint(source.lstat()) != fingerprint(original):
            raise ValueError('source changed during compression')
        saved = original.st_size - Path(temp).stat().st_size
        if saved <= 0:
            return {'source': name, 'state': 'retained_not_smaller', 'saved_bytes': 0}
        # No-clobber publication. If a crash left the durable gzip in place,
        # verify it and finish the interrupted source unlink on retry.
        try:
            os.link(temp, dest)
        except FileExistsError:
            dest_fd = os.open(dest, os.O_RDONLY | os.O_NOFOLLOW)
            with os.fdopen(dest_fd, 'rb') as raw, gzip.GzipFile(fileobj=raw) as inp:
                if digest_stream(inp) != (recovered_hash, recovered_size):
                    raise ValueError('existing archive differs; refusing overwrite')
        sync_dir(root)
        if fingerprint(source.lstat()) != fingerprint(original):
            raise ValueError('source replaced before unlink; retained')
        source.unlink()
        sync_dir(root)
        return {'source': name, 'archive': str(dest), 'state': 'compressed_verified',
                'sha256': recovered_hash, 'original_bytes': recovered_size, 'saved_bytes': saved}
    finally:
        os.close(fd)
        if temp is not None:
            Path(temp).unlink(missing_ok=True)

def restore_archive(archive, destination):
    if not archive.name.endswith('.gz') or not QUARANTINE.fullmatch(archive.name[:-3]):
        raise ValueError('not a quarantine archive')
    # Restoring is explicit, never overwrites any existing file, and must use
    # the original quarantine basename so a recovery cannot replace facts.db.
    if destination.name != archive.name[:-3]:
        raise ValueError('destination must preserve the quarantine basename')
    fd = os.open(archive, os.O_RDONLY | os.O_NOFOLLOW)
    destination.parent.mkdir(parents=True, exist_ok=True)
    temp_fd, temp = tempfile.mkstemp(prefix='.restore-', dir=destination.parent)
    try:
        with os.fdopen(fd, 'rb') as raw, gzip.GzipFile(fileobj=raw) as inp, os.fdopen(temp_fd, 'wb') as out:
            while data := inp.read(BLOCK):
                out.write(data)
            out.flush()
            os.fsync(out.fileno())
        os.link(temp, destination)
        sync_dir(destination.parent)
    finally:
        Path(temp).unlink(missing_ok=True)
    return {'restored': str(destination)}

def main():
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest='command', required=True)
    for command in ('inventory', 'compress'):
        q = sub.add_parser(command)
        q.add_argument('--rally-dir', type=Path, required=True)
        if command == 'compress':
            q.add_argument('--file', required=True, help='exact quarantine basename; no bulk deletion')
    q = sub.add_parser('restore')
    q.add_argument('archive', type=Path)
    q.add_argument('destination', type=Path)
    args = p.parse_args()
    try:
        result = (inventory(args.rally_dir) if args.command == 'inventory' else
                  compress_quarantine(args.rally_dir, args.file) if args.command == 'compress' else
                  restore_archive(args.archive, args.destination))
        print(json.dumps(result, indent=2))
    except (OSError, ValueError, EOFError) as error:
        p.exit(2, f'{error}\n')

if __name__ == '__main__':
    main()
