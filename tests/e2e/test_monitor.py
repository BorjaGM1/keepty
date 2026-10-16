"""Output bursts, blocked/slow drains, restart races and actual EMFILE recovery."""
import contextlib
from pathlib import Path
import socket
import sys
import threading
import time
from fixture_helpers import fixture
from support import ROOT, live, poll, raw_until, socket_path


def burst(lab, count, kind):
    sid, pid, base = fixture(lab, mode='burst', count=count, marker='END_BURST',
                             gate_burst=kind == 'quiet')
    lab.writer(sid)
    lab.wait(sid, 'FIXTURE_READY')
    if kind == 'text':
        waiter = lab.spawn('wait', sid, '--text', 'END_BURST', '--timeout', '30')
    elif kind == 'new':
        waiter = lab.armed(sid, 'END_BURST')
    else:
        waiter = lab.spawn('wait', sid, '--quiet', '500', '--timeout', '30')
    # Observe output independently of the CLI's return code.
    monitors = [lab.connect(sid) for _ in range(3 if kind == 'new' else 1)]
    exits = []
    def observe_exit():
        rc = waiter.wait()
        exits.append((time.monotonic_ns(), rc))
    observer = threading.Thread(target=observe_exit, daemon=True)
    observer.start()
    try:
        base.with_suffix('.go').touch()
        if kind == 'quiet':
            raw_until(monitors[0], b'BEGIN_BURST')
            assert waiter.poll() is None, 'quiet waiter exited before burst overlap'
            base.with_suffix('.continue').touch()
        if kind == 'new':
            for _ in range(3):
                lab.cli('screen', sid)
        raw_until(monitors[0], b'END_BURST')
        end_observed = time.monotonic_ns()
        if kind == 'quiet':
            assert waiter.poll() is None, 'quiet waiter exited before END_BURST observation'
        for monitor in monitors[1:]:
            raw_until(monitor, b'END_BURST')
        lab.screen(sid, 'END_BURST')
        observer.join(timeout=40)
        assert not observer.is_alive(), 'Monitor waiter did not complete'
        exit_observed, rc = exits[0]
        assert rc == 0, f'Monitor waiter dropped or failed: rc={rc}'
        if kind == 'quiet':
            assert end_observed < exit_observed, (end_observed, exit_observed)
    finally:
        if waiter.poll() is None:
            waiter.terminate()
        observer.join(timeout=10)
        assert not observer.is_alive(), 'wait exit observer leaked'
    assert live(pid), 'broker died during output'
    lab.stop(sid, pid)


class SlowReader:
    """Small receive buffer and paced reads: a workload, never a timing assertion."""
    def __init__(self, lab, sid):
        self.conn = lab.connect(sid)
        self.conn._sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
        self.stop = threading.Event()
        self.started = threading.Event()
        self.thread = threading.Thread(target=self.read, daemon=True)
        self.thread.start()
        assert self.started.wait(10)

    def read(self):
        self.started.set()
        while not self.stop.is_set():
            try:
                if not self.conn._sock.recv(4096):
                    return
            except OSError:
                return
            self.stop.wait(.05)

    def close(self):
        self.stop.set()
        with contextlib.suppress(OSError):
            self.conn._sock.shutdown(socket.SHUT_RDWR)
        self.conn.close()
        self.thread.join(timeout=5)
        assert not self.thread.is_alive(), 'slow reader thread leaked'


def shutdown(lab, mode):
    sid, pid, base = fixture(lab, mode='burst')
    lab.writer(sid)
    lab.wait(sid, 'FIXTURE_READY')
    slow = SlowReader(lab, sid)
    try:
        base.with_suffix('.go').touch()
        lab.wait(sid, 'BURST_DONE')  # All ~2MiB processed before shutdown.
        waiter = lab.armed(sid, 'NEVER_PRINTED')
        if mode == 'selfexit':
            base.with_suffix('.exit').touch()
        else:
            lab.cli('stop', sid)
        lab.gone(sid, pid)
        assert waiter.wait(timeout=15) == 3, 'wait did not report session exit'
    finally:
        slow.close()


def restart(lab, race):
    sid, old_pid, base = fixture(lab, mode='burst', count=100000)
    lab.writer(sid)
    lab.wait(sid, 'FIXTURE_READY')
    # An unread client backpressures shutdown. The broker may close it at its
    # drain deadline; correctness does not depend on when the runner is scheduled.
    blocked = lab.connect(sid)
    blocked._sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
    base.with_suffix('.go').touch()
    lab.wait(sid, 'BURST_DONE')
    command = [sys.executable, str(ROOT / 'tests/e2e/tui_app.py')]
    if race:
        # The competing starter is already attempting this ID before shutdown.
        # It retries on the listener-release event; there is deliberately no
        # assertion that the scheduler runs it within a fixed drain window.
        attempted = threading.Event()
        outcome = []
        def competing_start():
            try:
                first = lab.cli('start', sid, '--detach', '--', *command, check=False)
                assert first.returncode == 1, 'old session should still own socket'
                attempted.set()
                def attempt():
                    result = lab.cli('start', sid, '--detach', '--', *command, check=False)
                    return result.returncode == 0
                poll(attempt, 'concurrent start succeeds')
                outcome.append(None)
            except BaseException as error:
                outcome.append(error)
                attempted.set()
        starter = threading.Thread(target=competing_start)
        starter.start()
        try:
            assert attempted.wait(30), 'starter did not attempt old session'
            lab.cli('stop', sid)
        finally:
            starter.join(timeout=60)
        assert not starter.is_alive(), 'starter thread leaked'
        assert outcome == [None], f'concurrent starter failed: {outcome}'
        def find_new():
            for path in Path('/proc').glob('[0-9]*/cmdline'):
                try:
                    argv = path.read_bytes().split(b'\0')
                    pid = int(path.parent.name)
                    if b'broker' in argv and sid.encode() in argv and pid != old_pid:
                        return pid
                except (FileNotFoundError, PermissionError, ProcessLookupError):
                    pass
        new_pid = poll(find_new, 'replacement broker pid')
        lab.brokers.append(new_pid)
    else:
        lab.cli('stop', sid)
        lab.gone(sid, old_pid)
        sid, new_pid = lab.start(command, sid=sid)
    lab.writer(sid)
    lab.wait(sid, 'count=0')
    blocked.close()
    poll(lambda: not live(old_pid), 'old broker exited', timeout=15)
    # Verify *after* old broker exit: it must not unlink the new broker's socket.
    assert Path(socket_path(sid)).exists(), 'old broker unlinked replacement socket'
    lab.cli('send', sid, 'r')
    lab.wait(sid, 'count=1')
    lab.stop(sid, new_pid)


def accept_errors(lab):
    sid, pid = lab.start([sys.executable, str(ROOT / 'tests/e2e/tui_app.py')], fd_limit=24)
    lab.writer(sid)
    lab.wait(sid, 'count=0')
    inode = Path(socket_path(sid)).stat().st_ino
    log = lab.directory / f'{sid}.log'
    clients = []
    try:
        # Incomplete Hello connections retain broker fds. Nonblocking connects
        # also fill its listen backlog without blocking the runner on EMFILE.
        for _ in range(80):
            conn = socket.socket(socket.AF_UNIX)
            lab.connections.append(conn)
            conn.setblocking(False)
            conn.connect_ex(socket_path(sid))
            clients.append(conn)
        poll(lambda: 'Too many open files' in log.read_text(), 'actual EMFILE accept error')
        assert live(pid)
        assert Path(socket_path(sid)).stat().st_ino == inode, 'accept error unlinked listener'
    finally:
        for conn in clients:
            conn.close()
    # Wait for incomplete-Hello handlers to release their descriptors.
    def recovered():
        result = lab.cli('send', sid, 'e', check=False)
        return result.returncode == 0
    poll(recovered, 'broker accepts clients again')
    lab.wait(sid, 'key=e')
    lab.stop(sid, pid)


def cases():
    result = [(f'monitor/seq{n}_{kind}', lambda lab, n=n, kind=kind: burst(lab, n, kind))
              for n in (20000, 100000) for kind in ('text', 'new', 'quiet')]
    result += [(f'shutdown/{mode}', lambda lab, mode=mode: shutdown(lab, mode))
               for mode in ('selfexit', 'stop')]
    result += [('shutdown/stop_start', lambda lab: restart(lab, False)),
               ('shutdown/start_during_shutdown', lambda lab: restart(lab, True))]
    result += [(f'accept/emfile_{i}', accept_errors) for i in range(1, 4)]
    return result
