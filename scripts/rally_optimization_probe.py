#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Real stock-tmux transport/fault probe; no model success is inferred from it."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import hashlib
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import time


def run_probe(binary):
    with tempfile.TemporaryDirectory(prefix='rally-transport-') as temporary:
        root = Path(temporary)
        (root / '.git').mkdir()
        socket = root / 'tmux.sock'
        wrapper = root / 'tmux-wrapper'
        wrapper.write_text('#!/bin/sh\nexec tmux -S ' + shlex.quote(str(socket)) + ' "$@"\n')
        wrapper.chmod(0o700)
        frames = root / 'frames.bin'
        ready = root / 'ready'
        child = root / 'receiver.py'
        child.write_text('import os,tty\nfrom pathlib import Path\n'
                         'tty.setraw(0)\n' + f'Path({str(ready)!r}).touch()\n'
                         'while True:\n data=os.read(0,65536)\n if not data: break\n'
                         f' with open({str(frames)!r},"ab",buffering=0) as out: out.write(data)\n')
        env = {**os.environ, 'RALLY_SESSION_ID': 'optimization-probe-sender',
               'RALLY_HOOKS': 'off', 'RALLY_HOOK_TIMEOUT_MS': '30000', 'RALLY_DAEMON_AUTOSTART': '0'}
        def tmux(*args):
            return subprocess.run([str(wrapper), *args], capture_output=True, timeout=15)
        def rally(*args):
            p = subprocess.run([str(binary), *args, '--json'], cwd=root, env=env, capture_output=True, timeout=40)
            if p.returncode:
                raise RuntimeError(f'rally {args[0]} failed: {p.stderr.decode()[:500]}')
            data = json.loads(p.stdout)
            if not data.get('ok'):
                raise RuntimeError(f'rally {args[0]} refused: {str(data)[:500]}')
            return data
        def wait_for(check):
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                if check(): return
                time.sleep(.02)
            raise AssertionError('receiver observation deadline exceeded')
        def received():
            return frames.read_bytes() if frames.exists() else b''
        def inject(text):
            return rally('inject', 'neutral-host-01', '--tool', 'probe:sender', '--intent', 'inform', '--text', text, '--tmux-bin', str(wrapper))
        result = {'schema':'agent-rally.optimization.transport-probe.v1', 'model_evidence':False,
                  'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(), 'checks':{}}
        try:
            launched = rally('run', 'rosslabs-agent-harness', '--name', 'neutral-host', '--tool', 'probe:receiver', '--shared',
                             '--backend', 'tmux', '--tmux-bin', str(wrapper), '--command-json', json.dumps([sys.executable, str(child)]))
            session = launched['data']['run']['session']
            binding = session.get('tmux_binding')
            assert binding, 'real tmux launch must persist exact pane binding'
            pane = binding['pane']
            wait_for(ready.exists)
            one = inject('Unicode 日本語 control\x1b[201~ breakout')
            wait_for(lambda: b'Unicode' in received())
            assert b'\x1b[201~ breakout' not in received(), 'payload must not close paste frame'
            assert one['data']['inject']['verified_received'] is False
            assert one['data']['inject']['reached_target'] is False
            result['checks']['unicode_and_control_sanitization'] = True
            result['checks']['transport_is_not_receiver_ack'] = True
            before = len(received())
            large = 'large-payload-' + 'x' * 15000
            inject(large)
            wait_for(lambda: b'x' * 15000 in received()[before:])
            result['checks']['large_payload_argv_limit_avoided'] = True
            before = len(received())
            with ThreadPoolExecutor(max_workers=4) as pool:
                list(pool.map(inject, [f'parallel-frame-{i}-' + ('x' * 80) for i in range(4)]))
            wait_for(lambda: received()[before:].count(b'\x1b[201~\r') == 4)
            data = received()[before:]
            assert data.count(b'\x15\x1b[200~') == 4
            for body in data.split(b'\x15\x1b[200~')[1:]:
                assert body.count(b'parallel-frame-') == 1, 'interleaved frames'
                assert body.endswith(b'\x1b[201~\r'), 'partial frame'
            result['checks']['concurrent_frames_do_not_interleave'] = True
            # Changing the active pane must not redirect the registered target.
            tmux('split-window', '-d', '-t', pane, 'sleep 60')
            panes = tmux('list-panes', '-t', session['target'], '-F', '#{pane_id}').stdout.decode().split()
            other = next(p for p in panes if p != pane)
            tmux('select-pane', '-t', other)
            inject('exact-pane-after-active-switch')
            wait_for(lambda: b'exact-pane-after-active-switch' in received())
            result['checks']['active_pane_switch_preserves_binding'] = True
            before = received()
            tmux('copy-mode', '-t', pane)
            refused = inject('copy-mode-must-stay-pending')
            assert refused['data']['inject']['delivery_state'] == 'failed'
            assert received() == before
            tmux('send-keys', '-t', pane, '-X', 'cancel')
            result['checks']['copy_mode_refuses_and_preserves_input'] = True
            tmux('respawn-pane', '-k', '-t', pane, 'sleep 60')
            refused = inject('replacement-process-must-not-receive')
            assert refused['data']['inject']['delivery_state'] == 'failed'
            assert received() == before
            inbox_files = list((root / '.rally' / 'inbox').glob('*'))
            assert inbox_files, 'failed transport still has durable directives'
            result['checks']['replacement_process_refused_directive_retained'] = True
            result['tmux_version'] = tmux('-V').stdout.decode().strip()
            result['pass'] = all(result['checks'].values())
            return result
        finally:
            # Dedicated socket only. Never affect an existing user tmux server.
            tmux('kill-server')


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binary', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    args = p.parse_args()
    result = run_probe(args.binary.resolve())
    args.output.write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result, indent=2))

if __name__ == '__main__':
    main()
