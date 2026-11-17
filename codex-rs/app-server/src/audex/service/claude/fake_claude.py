#!/usr/bin/python3
"""Offline executable fixture. Configuration and receipts live beside this file."""
import ctypes
import json
import os
from pathlib import Path
import signal
import sys
import time

root = Path(__file__).parent
config = json.loads((root / 'fixture.json').read_text())
prompt = sys.stdin.buffer.read().decode()
pdeathsig = ctypes.c_int()
ctypes.CDLL(None).prctl(2, ctypes.byref(pdeathsig), 0, 0, 0)
receipt = {
    'args': sys.argv[1:], 'env': dict(os.environ), 'cwd': os.getcwd(),
    'prompt': prompt, 'pid': os.getpid(), 'pgid': os.getpgrp(),
    'pdeathsig': pdeathsig.value,
    'modes': {key: os.stat(os.environ[key]).st_mode & 0o777
              for key in ('HOME', 'TMPDIR', 'CLAUDE_CONFIG_DIR')},
    'profile_entries': sorted(os.listdir(os.environ['CLAUDE_CONFIG_DIR'])),
}
with (root / 'calls.jsonl').open('a') as calls:
    calls.write(json.dumps(receipt) + '\n')

sys.stdout.buffer.write((root / 'stdout.bin').read_bytes())
sys.stdout.buffer.flush()
sys.stderr.buffer.write((root / 'stderr.bin').read_bytes())
sys.stderr.buffer.flush()
mode = config.get('mode', 'finish')
if mode in ('cooperate', 'stubborn', 'leftover'):
    child = os.fork()
    if child == 0:
        if mode == 'stubborn':
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
        while True:
            time.sleep(0.05)

    def terminate(_signal, _frame):
        os.write(2, b'SIGTERM received\n')
        if mode == 'cooperate':
            os.waitpid(child, 0)
            sys.exit(0)

    signal.signal(signal.SIGTERM, terminate)
    (root / 'processes.json').write_text(json.dumps([os.getpid(), child]))
    (root / 'ready').write_text('ready')
    if mode != 'leftover':
        while True:
            time.sleep(0.05)
sys.exit(config.get('exit', 0))
