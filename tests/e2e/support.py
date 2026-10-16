"""Isolated CLI/SDK fixtures; only the Python standard library is required."""
import contextlib
import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import sys
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'sdks/python'))
from keepty.client import Session
from keepty.protocol import Frame, MsgKind, Role, socket_path

TIMEOUT = 30
AGENT_HOME = os.environ.get('CODEX_HOME', str(Path.home() / '.codex'))


def poll(predicate, description, timeout=TIMEOUT):
    deadline = time.monotonic() + timeout
    while True:
        value = predicate()
        if value:
            return value
        if time.monotonic() >= deadline:
            raise AssertionError(f'timed out: {description}')
        # A polling interval, never a readiness assumption.
        select.select([], [], [], .02)


def live(pid):
    try:
        return Path(f'/proc/{pid}/stat').read_text().split(') ')[1][0] != 'Z'
    except FileNotFoundError:
        return False


class Lab:
    def __init__(self, binary, directory):
        self.binary = str(binary)
        self.directory = Path(directory)
        self.sessions = []
        self.processes = []
        self.connections = []
        self.brokers = []

    def cli(self, *args, check=True, timeout=TIMEOUT):
        result = subprocess.run([self.binary, *map(str, args)], capture_output=True,
                                text=True, timeout=timeout)
        if check and result.returncode:
            raise AssertionError(f'CLI {args}: rc={result.returncode}\n{result.stderr}')
        return result

    def spawn(self, *args, armed=False):
        proc = subprocess.Popen([self.binary, *map(str, args)], stdout=subprocess.DEVNULL,
                                stderr=subprocess.PIPE,
                                env={**os.environ, **({'KEEPTY_DEBUG_ARMED': '1'} if armed else {})})
        self.processes.append(proc)
        if armed:
            poll(lambda: select.select([proc.stderr], [], [], 0)[0], 'wait armed pipe')
            line = proc.stderr.readline()
            assert line.strip() == b'armed', f'wait did not arm: {line!r}'
        return proc

    def start(self, command, sid=None, fd_limit=None):
        sid = sid or ('e2e' + uuid.uuid4().hex[:12])
        if sid not in self.sessions:
            self.sessions.append(sid)
        if fd_limit:
            # Direct CLI broker lets us retain its diagnostic log and prove EMFILE.
            import resource
            log = self.directory / f'{sid}.log'
            def limit():
                resource.setrlimit(resource.RLIMIT_NOFILE, (fd_limit, fd_limit))
            with log.open('wb') as out:
                proc = subprocess.Popen([self.binary, 'broker', '--session-id', sid,
                                         '--', *map(str, command)], stdout=out, stderr=out,
                                        preexec_fn=limit, start_new_session=True)
            self.processes.append(proc)
            pid = proc.pid
        else:
            self.cli('start', sid, '--detach', '--', *command)
            def find():
                for path in Path('/proc').glob('[0-9]*/cmdline'):
                    try:
                        argv = path.read_bytes().split(b'\0')
                        if b'broker' in argv and sid.encode() in argv:
                            return int(path.parent.name)
                    except (FileNotFoundError, PermissionError, ProcessLookupError):
                        pass
            pid = poll(find, f'broker pid for {sid}')
        self.brokers.append(pid)
        poll(lambda: Path(socket_path(sid)).exists(), 'socket created')
        return sid, pid

    def connect(self, sid, role=Role.MONITOR, **kwargs):
        conn = Session.connect(sid, role=role, **kwargs)
        conn._sock.settimeout(TIMEOUT)
        self.connections.append(conn)
        return conn

    def writer(self, sid):
        self.connect(sid, Role.WRITER).close()

    def screen(self, sid, text):
        def contains():
            result = self.cli('screen', sid)
            return result.stdout if text in result.stdout else None
        return poll(contains, f'screen contains {text!r}')

    def wait(self, sid, text):
        self.cli('wait', sid, '--text', text, '--timeout', str(TIMEOUT))
        self.screen(sid, text)

    def armed(self, sid, text, timeout=30):
        args = ['wait', sid, '--new', '--timeout', str(timeout)]
        args += ['--text', text]
        return self.spawn(*args, armed=True)

    def gone(self, sid, pid):
        poll(lambda: not live(pid), 'broker exit (<15 seconds)', timeout=15)
        assert not Path(socket_path(sid)).exists(), 'socket survived broker exit'
        assert sid not in self.cli('list').stdout, 'exited session still listed'
        with contextlib.suppress(ChildProcessError):
            os.waitpid(pid, os.WNOHANG)

    def stop(self, sid, pid):
        self.cli('stop', sid)
        self.gone(sid, pid)

    def cleanup(self):
        for conn in self.connections:
            conn.close()
        for sid in self.sessions:
            with contextlib.suppress(Exception):
                self.cli('stop', sid, check=False, timeout=5)
        for pid in self.brokers:
            if live(pid):
                with contextlib.suppress(ProcessLookupError):
                    os.kill(pid, signal.SIGTERM)
        for pid in self.brokers:
            with contextlib.suppress(Exception):
                poll(lambda: not live(pid), 'cleanup broker', timeout=15)
            if live(pid):
                with contextlib.suppress(ProcessLookupError):
                    os.kill(pid, signal.SIGKILL)
        for proc in self.processes:
            if proc.poll() is None:
                proc.kill()
            proc.wait(timeout=5)
            if proc.stderr:
                proc.stderr.close()
        for pid in self.brokers:
            with contextlib.suppress(ChildProcessError):
                os.waitpid(pid, os.WNOHANG)


def raw_until(conn, expected):
    data = bytearray()
    while expected not in data:
        frame = Frame.read_from(conn._sock)
        assert frame is not None, f'EOF before {expected!r}'
        if frame.kind == MsgKind.PING:
            Frame(MsgKind.PONG).write_to(conn._sock)
        elif frame.kind == MsgKind.OUTPUT:
            data.extend(frame.payload)
        elif frame.kind == MsgKind.EXIT:
            raise AssertionError('unexpected child exit')
    return bytes(data)


def sync_monitor(conn):
    """Drain through an ordered ping/pong barrier on this Monitor connection."""
    Frame(MsgKind.PING).write_to(conn._sock)
    while True:
        frame = Frame.read_from(conn._sock)
        assert frame is not None, 'EOF before Monitor sync'
        if frame.kind == MsgKind.PONG:
            return
        if frame.kind == MsgKind.PING:
            Frame(MsgKind.PONG).write_to(conn._sock)
        assert frame.kind != MsgKind.EXIT, 'child exited before Monitor sync'
