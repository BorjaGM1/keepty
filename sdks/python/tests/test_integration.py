"""
Integration tests for the keepty Python SDK.

These tests spawn a real keepty broker and test the full flow.
Requires the keepty binary to be built: cargo build --release -p keepty
"""

import os
import subprocess
import time
import pytest

# Find the keepty binary
KEEPTY_BIN = os.path.join(
    os.path.dirname(__file__), "..", "..", "..", "target", "release", "keepty"
)
if not os.path.exists(KEEPTY_BIN):
    # Try debug build
    KEEPTY_BIN = os.path.join(
        os.path.dirname(__file__), "..", "..", "..", "target", "debug", "keepty"
    )

# Add the SDK to the path
import sys
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from keepty import Session, Role, Frame, MsgKind, socket_path
from keepty.protocol import (
    encode_hello, decode_hello_ack, encode_resize, decode_exit,
)

_counter = 0

def unique_id(label: str) -> str:
    global _counter
    _counter += 1
    return f"pytest-{os.getpid()}-{_counter}-{label}"


def start_broker(session_id: str, command: str):
    """Start a keepty broker in the background."""
    child = subprocess.Popen(
        [KEEPTY_BIN, "broker", "--session-id", session_id,
         "--cols", "80", "--rows", "24", "--", "sh", "-c", command],
        stderr=subprocess.PIPE,
    )
    # Wait for socket
    sock = socket_path(session_id)
    for _ in range(50):
        if os.path.exists(sock):
            time.sleep(0.05)
            return child
        time.sleep(0.1)
    child.kill()
    raise RuntimeError(f"Broker socket never appeared: {sock}")


def cleanup(child, session_id):
    child.kill()
    child.wait()
    try:
        os.unlink(socket_path(session_id))
    except FileNotFoundError:
        pass


@pytest.fixture
def keepty_bin():
    if not os.path.exists(KEEPTY_BIN):
        pytest.skip("keepty binary not built (run: cargo build -p keepty)")
    return KEEPTY_BIN


# --- Protocol tests (no broker needed) ---

class TestProtocol:
    def test_frame_roundtrip(self):
        frame = Frame(MsgKind.OUTPUT, b"hello world")
        encoded = frame.encode()
        import io
        import socket as sock_mod

        # Simulate reading from a buffer
        class FakeSock:
            def __init__(self, data):
                self._data = data
                self._pos = 0
            def recv(self, n):
                chunk = self._data[self._pos:self._pos + n]
                self._pos += n
                return chunk

        decoded = Frame.read_from(FakeSock(encoded))
        assert decoded is not None
        assert decoded.kind == MsgKind.OUTPUT
        assert decoded.payload == b"hello world"

    def test_hello_roundtrip(self):
        payload = encode_hello(Role.WRITER, 132, 51)
        role, cols, rows = decode_hello_ack(b"\x00\x00\x00\x00" + payload[1:])  # skip role byte for ack
        # Test encode/decode directly
        from keepty.protocol import decode_hello
        role, cols, rows = decode_hello(payload)
        assert role == Role.WRITER
        assert cols == 132
        assert rows == 51

    def test_resize_roundtrip(self):
        from keepty.protocol import decode_resize
        payload = encode_resize(80, 24)
        cols, rows = decode_resize(payload)
        assert cols == 80
        assert rows == 24

    def test_exit_roundtrip(self):
        from keepty.protocol import encode_exit
        payload = encode_exit(42)
        code = decode_exit(payload)
        assert code == 42

    def test_socket_path(self):
        path = socket_path("abc123")
        assert path.endswith("/keepty/keepty-abc123.sock"), f"unexpected path: {path}"

    def test_eof_returns_none(self):
        class EmptySock:
            def recv(self, n):
                return b""
        assert Frame.read_from(EmptySock()) is None


# --- Integration tests (need broker) ---

class TestIntegration:
    def test_connect_and_receive_output(self, keepty_bin):
        sid = unique_id("output")
        child = start_broker(sid, "echo 'hello from python'; sleep 30")
        try:
            with Session.connect(sid) as session:
                output = session.read_until("hello from python", timeout=5)
                assert "hello from python" in output
        finally:
            cleanup(child, sid)

    def test_send_input(self, keepty_bin):
        sid = unique_id("input")
        child = start_broker(sid, "cat")
        try:
            with Session.connect(sid) as session:
                time.sleep(0.2)
                session.send_keys(b"python sdk works!\n")
                output = session.read_until("python sdk works!", timeout=5)
                assert "python sdk works!" in output
        finally:
            cleanup(child, sid)

    def test_watcher_receives_output(self, keepty_bin):
        sid = unique_id("watcher")
        # Delay echo so it runs AFTER watcher connects (watchers skip ring replay)
        child = start_broker(sid, "sleep 2 && echo 'watcher test'; sleep 30")
        try:
            # Writer must connect first to trigger deferred spawn
            writer = Session.connect(sid, role=Role.WRITER)
            time.sleep(0.5)
            with Session.connect(sid, role=Role.WATCHER) as session:
                output = session.read_until("watcher test", timeout=10)
                assert "watcher test" in output
            writer.close()
        finally:
            cleanup(child, sid)

    def test_watcher_cannot_send_input(self, keepty_bin):
        sid = unique_id("watcher-input")
        child = start_broker(sid, "cat")
        try:
            # Writer must connect first to trigger deferred spawn
            writer = Session.connect(sid, role=Role.WRITER)
            with Session.connect(sid, role=Role.WATCHER) as session:
                with pytest.raises(PermissionError):
                    session.send_keys(b"test\n")
            writer.close()
        finally:
            cleanup(child, sid)

    def test_monitor_receives_output(self, keepty_bin):
        sid = unique_id("monitor")
        child = start_broker(sid, "echo 'monitor test'; sleep 30")
        try:
            # Writer must connect first to trigger deferred spawn
            writer = Session.connect(sid, role=Role.WRITER)
            time.sleep(0.5)
            with Session.connect(sid, role=Role.MONITOR) as session:
                output = session.read_until("monitor test", timeout=5)
                assert "monitor test" in output
            writer.close()
        finally:
            cleanup(child, sid)

    def test_session_repr(self, keepty_bin):
        sid = unique_id("repr")
        child = start_broker(sid, "sleep 30")
        try:
            with Session.connect(sid) as session:
                r = repr(session)
                assert sid in r
                assert "WRITER" in r
        finally:
            cleanup(child, sid)


class TestScreenReader:
    def test_screen_reading(self, keepty_bin):
        """Test the full agent screen-reading flow."""
        pytest.importorskip("pyte")
        from keepty import ScreenReader

        sid = unique_id("screen")
        child = start_broker(sid, "echo 'screen content here'; sleep 30")
        try:
            with ScreenReader.connect(sid) as reader:
                found = reader.poll_until("screen content here", timeout=5)
                assert found, f"Expected text not found. Screen:\n{reader.contents()}"
                assert "screen content here" in reader.contents()
        finally:
            cleanup(child, sid)

    def test_screen_send_keys(self, keepty_bin):
        """Test agent driving a TUI via screen reader."""
        pytest.importorskip("pyte")
        from keepty import ScreenReader

        sid = unique_id("screen-keys")
        child = start_broker(sid, "cat")
        try:
            with ScreenReader.connect(sid, role=Role.WRITER) as reader:
                time.sleep(0.2)
                reader.send_keys(b"agent typed this\n")
                found = reader.poll_until("agent typed this", timeout=5)
                assert found
        finally:
            cleanup(child, sid)
