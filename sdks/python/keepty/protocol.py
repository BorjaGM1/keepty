"""
keepty wire protocol — pure Python implementation.

Length-prefixed binary frames over Unix sockets.
Frame: [u32 len BE][u8 version][u8 kind][payload...]
"""

import os
import struct
from enum import IntEnum
from typing import Optional


PROTOCOL_VERSION = 1
SOCKET_DIR = "/tmp"  # Deprecated: use socket_dir()
MAX_PAYLOAD_SIZE = 1_048_576  # 1MB
MAX_FRAME_BODY_SIZE = MAX_PAYLOAD_SIZE + 2  # payload + version + kind


def socket_dir() -> str:
    """Resolve the runtime socket directory.
    Priority: $XDG_RUNTIME_DIR/keepty → $TMPDIR/keepty (via tempfile)
    """
    xdg = os.environ.get("XDG_RUNTIME_DIR", "")
    if xdg:
        return os.path.join(xdg, "keepty")
    import tempfile
    return os.path.join(tempfile.gettempdir(), "keepty")


class Role(IntEnum):
    """Client role when connecting to a keepty broker."""
    WRITER = 1   # Exclusive: can send input + resize
    WATCHER = 2  # Read-only: receives output broadcast
    MONITOR = 3  # Read-only: for programmatic screen analysis


class MsgKind(IntEnum):
    """Message types in the keepty protocol."""
    HELLO = 1
    HELLO_ACK = 2
    INPUT = 3
    OUTPUT = 4
    RESIZE = 5
    RESIZE_ACK = 6
    EXIT = 10
    SHUTDOWN = 11
    PING = 12
    PONG = 13
    ERROR = 127


class Frame:
    """A keepty protocol frame."""

    __slots__ = ("kind", "payload")

    def __init__(self, kind: MsgKind, payload: bytes = b""):
        self.kind = kind
        self.payload = payload

    def encode(self) -> bytes:
        """Encode to wire format: [u32 len BE][u8 version][u8 kind][payload]"""
        frame_len = 2 + len(self.payload)  # version + kind + payload
        return struct.pack(">IB", frame_len, PROTOCOL_VERSION) + bytes([self.kind]) + self.payload

    @staticmethod
    def read_from(sock) -> Optional["Frame"]:
        """Read one frame from a socket. Returns None on EOF."""
        len_buf = _recv_exact(sock, 4)
        if len_buf is None:
            return None
        frame_len = struct.unpack(">I", len_buf)[0]
        if frame_len < 2:
            raise ValueError(f"Frame too short: {frame_len}")
        if frame_len > MAX_FRAME_BODY_SIZE:
            raise ValueError(f"Frame too large: {frame_len} bytes (max {MAX_FRAME_BODY_SIZE})")
        data = _recv_exact(sock, frame_len)
        if data is None:
            return None
        version = data[0]
        if version != PROTOCOL_VERSION:
            raise ValueError(f"Unsupported protocol version: {version} (expected {PROTOCOL_VERSION})")
        kind = MsgKind(data[1])
        payload = data[2:]
        return Frame(kind, payload)

    def write_to(self, sock) -> None:
        """Write this frame to a socket."""
        sock.sendall(self.encode())

    def __repr__(self) -> str:
        return f"Frame({self.kind.name}, {len(self.payload)} bytes)"


def socket_path(session_id: str) -> str:
    """Construct the broker socket path for a session."""
    return os.path.join(socket_dir(), f"keepty-{session_id}.sock")


# --- Message helpers ---

def encode_hello(role: Role, cols: int, rows: int) -> bytes:
    return struct.pack(">BHH", role, cols, rows)


def decode_hello(payload: bytes) -> tuple[Role, int, int]:
    role_val, cols, rows = struct.unpack(">BHH", payload[:5])
    return Role(role_val), cols, rows


def encode_hello_ack(pty_pid: int, cols: int, rows: int) -> bytes:
    return struct.pack(">IHH", pty_pid, cols, rows)


def decode_hello_ack(payload: bytes) -> tuple[int, int, int]:
    pid, cols, rows = struct.unpack(">IHH", payload[:8])
    return pid, cols, rows


def encode_resize(cols: int, rows: int) -> bytes:
    return struct.pack(">HH", cols, rows)


def decode_resize(payload: bytes) -> tuple[int, int]:
    cols, rows = struct.unpack(">HH", payload[:4])
    return cols, rows


def encode_exit(code: int) -> bytes:
    return struct.pack(">i", code)


def decode_exit(payload: bytes) -> int:
    return struct.unpack(">i", payload[:4])[0]


def _recv_exact(sock, n: int) -> Optional[bytes]:
    """Read exactly n bytes from a socket. Returns None on EOF."""
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            if not buf:
                return None
            raise ConnectionError(f"Connection closed mid-read (got {len(buf)}/{n} bytes)")
        buf.extend(chunk)
    return bytes(buf)
