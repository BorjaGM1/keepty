"""Event-controlled PTY child used by the regression tests."""
import json
import os
from pathlib import Path
import select
import sys
import termios
import tty


def gate(path):
    # File events are portable to both the CLI and a child running inside its PTY.
    while not Path(path).exists():
        select.select([], [], [], .01)


def write(data):
    view = memoryview(data)
    while view:
        view = view[os.write(1, view):]


def encode(text):
    return text.encode('latin1') if all(ord(ch) < 256 for ch in text) else text.encode()


tty.setraw(0, termios.TCSANOW)
config = json.loads(Path(sys.argv[1]).read_text())
base = config['base']
if config['mode'] == 'matrix':
    write(encode(config['pre']))
    Path(base + '.ready').touch()
    for i, chunk in enumerate(config['post']):
        gate(f'{base}.{i}')
        write(encode(chunk))
    Path(base + '.done').touch()
    gate(base + '.settle')
    write(b'\x1b[0m')  # SGR reset: output with no screen paint.
else:
    write(b'FIXTURE_READY')
    gate(base + '.go')
    if config.get('gate_burst'):
        write(b'BEGIN_BURST\r\n')
        gate(base + '.continue')
    if config.get('count'):
        # Same workload as seq, with an explicit release/finish barrier.
        for i in range(1, config['count'] + 1):
            write(f'{i}\r\n'.encode())
    else:
        for _ in range(2048):
            write(b'x' * 1022 + b'\r\n')
    write(b'\r\n' + config.get('marker', 'BURST_DONE').encode() + b'\r\n')
    Path(base + '.done').touch()
    gate(base + '.exit')
    sys.exit(0)
gate(base + '.exit')
