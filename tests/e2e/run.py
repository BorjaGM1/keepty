#!/usr/bin/env python3
"""Run isolated real-process e2e tests: python3 tests/e2e/run.py [-k PATTERN] [--repeat N]."""
import argparse
import ctypes
import fnmatch
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time
import traceback

from support import Lab, ROOT
import test_monitor
import test_tui
import test_wait


def interrupted(signum, frame):
    raise KeyboardInterrupt(f'signal {signum}')


def watchdog(signum, frame):
    raise TimeoutError('case exceeded generous 120 second hang limit')


def main():
    os.environ.update(LANG='C.UTF-8', LC_ALL='C.UTF-8', PYTHONIOENCODING='utf-8')
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('-k', default='', help='substring or shell glob matching group/case')
    parser.add_argument('--repeat', type=int, default=1)
    args = parser.parse_args()
    if args.repeat < 1:
        parser.error('--repeat must be positive')
    cases = test_wait.cases() + test_monitor.cases() + test_tui.cases()
    cases = [(name, case) for name, case in cases
             if args.k in name or fnmatch.fnmatchcase(name, args.k)]
    if not cases:
        parser.error('no cases matched -k')
    override = os.environ.get('KEEPTY_BIN')
    binary = Path(override).expanduser().resolve() if override else ROOT / 'target/release/keepty'
    if override and not binary.is_file():
        parser.error(f'KEEPTY_BIN does not exist: {binary}')
    if not binary.is_file():
        subprocess.run(['cargo', 'build', '--release', '--locked', '-p', 'keepty'],
                       cwd=ROOT, check=True)
    # Reap daemon brokers orphaned by `start`; useful on containers whose PID 1
    # does not reap. Linux is also required by the /proc lifecycle assertions.
    if ctypes.CDLL(None, use_errno=True).prctl(36, 1, 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), 'PR_SET_CHILD_SUBREAPER')
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    signal.signal(signal.SIGALRM, watchdog)
    passed = failed = skipped = 0
    timings = {}
    started = time.monotonic()
    # Short /tmp path stays below sockaddr_un's 108-byte limit. HOME is also
    # private, so applications cannot consume a user's vim/curses configuration.
    with tempfile.TemporaryDirectory(prefix='kp-e2e-') as directory:
        previous = {key: os.environ.get(key) for key in ('XDG_RUNTIME_DIR', 'HOME', 'TERM')}
        os.environ.update(XDG_RUNTIME_DIR=directory, HOME=directory, TERM='xterm-256color')
        try:
            for iteration in range(1, args.repeat + 1):
                for name, case in cases:
                    lab = Lab(binary, tempfile.mkdtemp(prefix='case-', dir=directory))
                    begin = time.monotonic()
                    status, detail = 'PASS', ''
                    try:
                        signal.alarm(120)
                        case(lab)
                    except test_tui.Skip as error:
                        status, detail = 'SKIP', str(error)
                    except Exception:
                        status, detail = 'FAIL', traceback.format_exc()
                    finally:
                        signal.alarm(0)
                        try:
                            lab.cleanup()
                        except Exception:
                            status = 'FAIL'
                            detail += '\nCleanup failure:\n' + traceback.format_exc()
                    elapsed = time.monotonic() - begin
                    group = name.split('/')[0]
                    timings[group] = timings.get(group, 0) + elapsed
                    passed += status == 'PASS'
                    failed += status == 'FAIL'
                    skipped += status == 'SKIP'
                    print(f'{status} {name} [{iteration}/{args.repeat}] ({elapsed:.2f}s)', flush=True)
                    if detail:
                        print(detail, flush=True)
        finally:
            for key, value in previous.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value
    for group, elapsed in timings.items():
        print(f'TIMING {group}: {elapsed:.2f}s', flush=True)
    print(f'SUMMARY pass={passed} fail={failed} skip={skipped} total={passed+failed+skipped} '
          f'elapsed={time.monotonic()-started:.2f}s', flush=True)
    return int(failed != 0)


if __name__ == '__main__':
    raise SystemExit(main())
