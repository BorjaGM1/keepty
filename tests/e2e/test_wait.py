"""Paint provenance, including UTF-8 fragments, cursor edits and scrollback."""
from matrix import CASES
from support import poll, raw_until, sync_monitor
from fixture_helpers import fixture, encode


def matrix_case(lab, case):
    name, _, needle, pre, post, expected = case
    sid, pid, base = fixture(lab, mode='matrix', pre=pre, post=post)
    monitor = lab.connect(sid)
    lab.writer(sid)  # A Monitor alone deliberately does not spawn the child.
    poll(lambda: base.with_suffix('.ready').exists(), 'matrix child ready')
    if pre:
        raw_until(monitor, encode(pre))
    waiter = lab.armed(sid, needle, timeout=60)
    for i, chunk in enumerate(post):
        base.with_suffix(f'.{i}').touch()
        raw_until(monitor, encode(chunk))
    poll(lambda: base.with_suffix('.done').exists(), 'all matrix chunks emitted')
    sync_monitor(monitor)
    if expected == 0:
        rc = waiter.wait(timeout=65)
        assert rc == 0, f'{name}: rc={rc}, expected=0'
    else:
        # A separate no-op output event gives the waiter another processing turn
        # after every post chunk, without changing screen paint provenance.
        base.with_suffix('.settle').touch()
        raw_until(monitor, b'\x1b[0m')
        sync_monitor(monitor)
        assert waiter.poll() is None, f'{name}: no-match waiter exited rc={waiter.returncode}'
        waiter.terminate()
        waiter.wait(timeout=10)
    lab.stop(sid, pid)


def cases():
    return [('wait/' + c[0], lambda lab, c=c: matrix_case(lab, c)) for c in CASES]
