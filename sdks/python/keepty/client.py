"""
keepty client — connect to a keepty broker session.
"""

import os
import select
import socket
import shutil
import subprocess
import time
from typing import Optional

from .protocol import (
    Frame, MsgKind, Role,
    socket_path, encode_hello, decode_hello_ack,
    encode_resize, decode_exit,
)


class Session:
    """A connection to a keepty broker session.

    Usage:
        # Connect to an existing session
        session = Session.connect("my-session")
        session.send_keys(b"ls\\n")
        output = session.read_output(timeout=1.0)
        session.close()

        # Or use as context manager
        with Session.connect("my-session") as session:
            session.send_keys(b"echo hello\\n")
    """

    def __init__(self, sock: socket.socket, role: Role, pty_pid: int, session_id: str,
                 cols: int = 80, rows: int = 24):
        self._sock = sock
        self._role = role
        self._pty_pid = pty_pid
        self._session_id = session_id
        self._cols = cols
        self._rows = rows

    @classmethod
    def connect(
        cls,
        session_id: str,
        role: Role = Role.WRITER,
        cols: int = 80,
        rows: int = 24,
    ) -> "Session":
        """Connect to a running keepty broker session.

        Args:
            session_id: The session identifier.
            role: Writer (can input), Watcher (read-only), or Monitor (programmatic).
            cols: Terminal columns for the handshake.
            rows: Terminal rows for the handshake.

        Returns:
            A connected Session instance.

        Raises:
            FileNotFoundError: If the session is not running.
            ConnectionError: If the handshake fails.
        """
        path = socket_path(session_id)
        if not os.path.exists(path):
            raise FileNotFoundError(f"Session '{session_id}' is not running (no socket at {path})")

        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.connect(path)

        # Send Hello
        hello = Frame(MsgKind.HELLO, encode_hello(role, cols, rows))
        hello.write_to(sock)

        # Read HelloAck
        ack = Frame.read_from(sock)
        if ack is None:
            sock.close()
            raise ConnectionError("Broker closed before HelloAck")
        if ack.kind != MsgKind.HELLO_ACK:
            sock.close()
            raise ConnectionError(f"Expected HelloAck, got {ack.kind.name}")

        pid, ack_cols, ack_rows = decode_hello_ack(ack.payload)
        return cls(sock, role, pid, session_id, ack_cols, ack_rows)

    def _read_frames(self, timeout: float) -> list:
        """Read all available frames within timeout using select().

        Keeps the socket in blocking mode during actual frame reads
        to prevent stream desynchronization from partial reads.
        """
        deadline = time.monotonic() + max(0.0, timeout)
        frames = []
        prev_timeout = self._sock.gettimeout()
        self._sock.settimeout(None)  # blocking during reads
        try:
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    break
                readable, _, _ = select.select([self._sock], [], [], remaining)
                if not readable:
                    break
                frame = Frame.read_from(self._sock)
                if frame is None:
                    break
                frames.append(frame)
                # Drain any immediately available frames without waiting
                while True:
                    readable, _, _ = select.select([self._sock], [], [], 0)
                    if not readable:
                        break
                    frame = Frame.read_from(self._sock)
                    if frame is None:
                        return frames
                    frames.append(frame)
        finally:
            self._sock.settimeout(prev_timeout)
        return frames

    def read_output(self, timeout: float = 1.0) -> bytes:
        """Read output from the broker.

        Collects all Output frames available within the timeout period.
        Returns the raw bytes (terminal escape sequences included).

        Args:
            timeout: Maximum seconds to wait for output.

        Returns:
            Raw output bytes. May be empty if no output within timeout.
        """
        collected = bytearray()
        for frame in self._read_frames(timeout):
            if frame.kind == MsgKind.OUTPUT:
                collected.extend(frame.payload)
            elif frame.kind == MsgKind.PING:
                Frame(MsgKind.PONG).write_to(self._sock)
            elif frame.kind == MsgKind.EXIT:
                code = decode_exit(frame.payload)
                raise SessionExited(code)
        return bytes(collected)

    def read_until(self, expected: str, timeout: float = 5.0) -> str:
        """Read output until a string appears or timeout.

        Args:
            expected: The string to wait for.
            timeout: Maximum seconds to wait.

        Returns:
            All collected output as a string.

        Raises:
            TimeoutError: If expected string not found within timeout.
        """
        deadline = time.monotonic() + timeout
        collected = ""

        while time.monotonic() < deadline:
            remaining = max(0.1, deadline - time.monotonic())
            data = self.read_output(timeout=min(0.2, remaining))
            collected += data.decode("utf-8", errors="replace")
            if expected in collected:
                return collected

        raise TimeoutError(
            f"Expected '{expected}' not found within {timeout}s. Got: {collected[:200]}"
        )

    def send_keys(self, keys: bytes) -> None:
        """Send raw keystrokes to the session.

        Only works if connected as Writer.

        Args:
            keys: Raw bytes to send (e.g. b"ls\\n", b"\\x03" for Ctrl+C).

        Raises:
            PermissionError: If not connected as Writer.
        """
        if self._role != Role.WRITER:
            raise PermissionError("Only Writer role can send input")
        Frame(MsgKind.INPUT, keys).write_to(self._sock)

    def resize(self, cols: int, rows: int) -> None:
        """Resize the terminal.

        Only works if connected as Writer.
        """
        if self._role != Role.WRITER:
            raise PermissionError("Only Writer role can resize")
        Frame(MsgKind.RESIZE, encode_resize(cols, rows)).write_to(self._sock)

    def shutdown(self) -> None:
        """Request the broker to terminate the session."""
        Frame(MsgKind.SHUTDOWN).write_to(self._sock)

    def close(self) -> None:
        """Close the connection."""
        try:
            self._sock.close()
        except OSError:
            pass

    @property
    def role(self) -> Role:
        return self._role

    @property
    def pty_pid(self) -> int:
        return self._pty_pid

    @property
    def session_id(self) -> str:
        return self._session_id

    def __enter__(self) -> "Session":
        return self

    def __exit__(self, *args) -> None:
        self.close()

    def __repr__(self) -> str:
        return f"Session('{self._session_id}', role={self._role.name}, pid={self._pty_pid})"


class SessionExited(Exception):
    """Raised when the session's process exits."""

    def __init__(self, exit_code: int):
        self.exit_code = exit_code
        super().__init__(f"Session exited with code {exit_code}")


# --- Convenience functions ---

def start(
    session_id: str,
    command: Optional[str] = None,
    working_dir: Optional[str] = None,
    keepty_bin: Optional[str] = None,
) -> None:
    """Start a new keepty session.

    Args:
        session_id: Unique session identifier.
        command: Command to run (default: user's shell).
        working_dir: Working directory for the command.
        keepty_bin: Path to keepty binary (default: found in PATH).

    Raises:
        FileNotFoundError: If keepty binary not found.
        RuntimeError: If session fails to start.
    """
    bin_path = keepty_bin or shutil.which("keepty")
    if not bin_path:
        raise FileNotFoundError(
            "keepty binary not found in PATH. Install it: cargo install keepty"
        )

    args = [bin_path, "start", session_id]
    if working_dir:
        args.extend(["--working-dir", working_dir])
    if command:
        args.extend(["--", command])

    result = subprocess.run(args, capture_output=True, text=True)
    if result.returncode != 0:
        raise RuntimeError(f"Failed to start session: {result.stderr.strip()}")

    # Wait for socket
    path = socket_path(session_id)
    for _ in range(50):
        if os.path.exists(path):
            return
        time.sleep(0.1)

    raise RuntimeError("Session started but socket not available")


def stop(session_id: str, keepty_bin: Optional[str] = None) -> None:
    """Stop a running keepty session.

    Args:
        session_id: Session to stop.
        keepty_bin: Path to keepty binary.
    """
    bin_path = keepty_bin or shutil.which("keepty")
    if bin_path:
        subprocess.run([bin_path, "stop", session_id], capture_output=True)
    else:
        # Direct protocol: connect as Monitor and send Shutdown
        try:
            with Session.connect(session_id, role=Role.MONITOR) as s:
                s.shutdown()
        except (FileNotFoundError, ConnectionError):
            pass


def list_sessions() -> list[dict]:
    """List running keepty sessions.

    Returns:
        List of dicts with 'id' and 'alive' keys.
    """
    import glob
    sessions = []
    for path in glob.glob(f"{socket_path('*')}"):
        name = os.path.basename(path)
        # Extract session ID from keepty-{id}.sock
        if name.startswith("keepty-") and name.endswith(".sock"):
            sid = name[7:-5]
            try:
                sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                sock.connect(path)
                sock.close()
                alive = True
            except (ConnectionRefusedError, FileNotFoundError, OSError):
                alive = False
            sessions.append({"id": sid, "alive": alive})
    return sorted(sessions, key=lambda s: s["id"])
