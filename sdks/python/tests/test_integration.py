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
    @pytest.mark.parametrize("role", [Role.MONITOR, Role.WATCHER])
    def test_alternate_screen_replay_without_resize(self, keepty_bin, role):
        pytest.importorskip("pyte")
        import shlex
        from keepty import ScreenReader

        # Draw once, then overflow raw history with invisible bytes and remain idle.
        # Record any resize signal in the output.
        program = (
            "import signal, time; "
            "signal.signal(signal.SIGWINCH, lambda *_: print('UNEXPECTED_RESIZE', flush=True)); "
            "print('\\x1b[?1049h\\x1b[2J\\x1b[5;10Halternate screen', end='', flush=True); "
            "print('\\x00' * (1024 * 1024 + 100) + '\\x1b]0;REPLAY_READY\\x07', end='', flush=True); "
            "time.sleep(30)"
        )
        sid = unique_id("alternate-screen")
        child = start_broker(sid, "exec python3 -u -c " + shlex.quote(program))
        try:
            with Session.connect(sid, role=Role.WRITER) as writer:
                assert "REPLAY_READY" in writer.read_until("REPLAY_READY", timeout=5)
                with ScreenReader.connect(sid, role=role, cols=120, rows=35) as reader:
                    reader.poll(0.1)
                    assert "alternate screen" in reader.contents()
                    assert "UNEXPECTED_RESIZE" not in reader.contents()
                    assert reader._screen.display[4][9:25] == "alternate screen"
                    assert (reader._screen.columns, reader._screen.lines) == (80, 24)
        finally:
            cleanup(child, sid)

    def test_screen_reading(self, keepty_bin):
        """Test the full agent screen-reading flow."""
        pytest.importorskip("pyte")
        from keepty import ScreenReader

        sid = unique_id("screen")
        child = start_broker(sid, "echo 'screen content here'; sleep 30")
        try:
            # The broker defers spawning the child until the first Writer.
            with Session.connect(sid, role=Role.WRITER) as writer:
                writer.read_until("screen content here", timeout=5)
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


@pytest.mark.parametrize("role", [Role.MONITOR, Role.WATCHER])
def test_readonly_prime_does_not_wait_for_nudge(role):
    from unittest.mock import Mock
    from keepty.screen import ScreenReader

    session = Mock()
    session.read_output.return_value = b"replay"
    stream = Mock()
    reader = ScreenReader(session, Mock(), stream)
    reader._prime(role)
    session.read_output.assert_called_once_with(timeout=0.3)
    stream.feed.assert_called_once_with(b"replay")


def test_watch_detach_resets_terminal_modes(keepty_bin):
    import pty
    import select

    sid = unique_id("watch-detach")
    child = start_broker(sid, "printf '\\033[?1049h\\033[?1000h\\033[?1006h\\033[?1h\\033=\\033[?25lwatch-ready'; sleep 30")
    master, slave = pty.openpty()
    attach = None
    try:
        with Session.connect(sid, role=Role.WRITER) as writer:
            assert "watch-ready" in writer.read_until("watch-ready", timeout=5)
            attach = subprocess.Popen(
                [KEEPTY_BIN, "attach", sid, "--watch"],
                stdin=slave, stdout=slave, stderr=slave,
            )
            received = bytearray()
            deadline = time.monotonic() + 5
            while b"watch-ready" not in received and time.monotonic() < deadline:
                if select.select([master], [], [], 0.1)[0]:
                    received.extend(os.read(master, 65536))
            assert b"watch-ready" in received
            os.write(master, b"\x03")
            attach.wait(timeout=5)
            while select.select([master], [], [], 0.1)[0]:
                received.extend(os.read(master, 65536))
            for reset in (b"\x1b[?1049l", b"\x1b[?1000l", b"\x1b[?1002l",
                          b"\x1b[?1003l", b"\x1b[?1006l", b"\x1b[?1l",
                          b"\x1b>", b"\x1b[?25h"):
                assert reset in received
    finally:
        if attach is not None and attach.poll() is None:
            attach.kill()
            attach.wait()
        os.close(master)
        os.close(slave)
        cleanup(child, sid)


# --- Screen parser, exit, and named keys ---

class ScriptedSession:
    """Session stand-in that returns scripted read_output results."""

    def __init__(self, events):
        self.events = list(events)

    def read_output(self, timeout=0.0):
        if not self.events:
            return b""
        item = self.events.pop(0)
        if isinstance(item, Exception):
            raise item
        return item


def _offline_reader(session=None):
    pytest.importorskip("pyte")
    from unittest.mock import Mock
    from keepty.screen import ScreenReader, _parser_classes

    if session is None:
        session = Mock()
    screen_cls, stream_cls = _parser_classes()
    screen = screen_cls(40, 8)
    stream = stream_cls(screen)
    return ScreenReader(session, screen, stream)


class TestScreenParser:
    @pytest.fixture(autouse=True)
    def _need_pyte(self):
        pytest.importorskip("pyte")

    def test_private_sgr_and_modify_other_keys_do_not_crash_or_style(self):
        reader = _offline_reader()
        # The vim crash is the private SGR at the end of this chunk.
        reader._feed(b"version info\x1b[1;1H\x1b[?25h\x1b[?4mstill here")
        assert reader.row(0).startswith("still here")
        reader = _offline_reader()
        reader._feed(b"version info\x1b[?4m\x1b[>4;2mHi")
        assert "version info" in reader.contents()
        assert "Hi" in reader.contents()
        hi = reader.row(0).index("Hi")
        cell = reader._screen.buffer[0][hi]
        assert cell.data == "H"
        assert cell.underscore is False
        # Normal SGR still applies after a private one was ignored.
        reader._feed(b"\x1b[4m")
        assert reader._screen.cursor.attrs.underscore is True

    def test_parse_error_does_not_drop_the_rest_of_the_chunk(self):
        reader = _offline_reader()
        reader._feed(b"keep\x1b[?5A-tail")
        assert reader.row(0).startswith("keep-tail")
        reader._feed(b"A\x1b[?5AB\x1b[?6CC")
        assert "ABC" in reader.contents()

    def test_gt_csi_split_across_feeds_is_dropped(self):
        reader = _offline_reader()
        reader._feed(b"Q\x1b[>4;")
        assert reader.row(0).strip() == "Q"
        reader._feed(b"2mZ")
        assert reader.row(0).startswith("QZ")
        assert reader._screen.buffer[0][1].data == "Z"
        assert reader._screen.buffer[0][1].underscore is False

    def test_normal_sgr_split_across_feeds_still_applies(self):
        reader = _offline_reader()
        reader._feed(b"\x1b[1;3")
        reader._feed(b"1mZ")
        cell = reader._screen.buffer[0][0]
        assert cell.data == "Z"
        assert cell.bold is True
        assert cell.fg == "red"

    def test_overlong_csi_does_not_swallow_following_text(self):
        # 200 parameter digits exceed the filter's hold limit. The final
        # "m" ends the sequence; "Y" must still be drawn, not buffered forever.
        reader = _offline_reader()
        reader._feed(b"X\x1b[" + b"1" * 200 + b"mY")
        text = reader.contents()
        assert "X" in text
        assert "Y" in text


class TestScreenExit:
    @pytest.fixture(autouse=True)
    def _need_pyte(self):
        pytest.importorskip("pyte")

    def test_session_exited_keeps_output_bytes(self):
        from keepty import SessionExited

        raw = bytearray(b"abc")
        exc = SessionExited(9, raw)
        raw[:] = b"zzz"
        assert exc.exit_code == 9
        assert exc.output == b"abc"
        assert isinstance(exc.output, bytes)
        assert str(SessionExited(9)) == "Session exited with code 9"
        assert SessionExited(9).output == b""

    def test_poll_records_exit_and_applies_final_output(self):
        from keepty import SessionExited

        session = ScriptedSession([SessionExited(7, b"hello\r\n")])
        reader = _offline_reader(session)
        assert reader.poll(5) is True
        assert "hello" in reader.contents()
        assert reader.exited is True
        assert reader.exit_code == 7

        def _fail_if_called(timeout=0.0):
            raise AssertionError("read after exit")

        # Later polls must not raise and must not read again.
        session.read_output = _fail_if_called
        assert reader.poll(5) is False

    def test_poll_exit_without_output_returns_false(self):
        from keepty import SessionExited

        reader = _offline_reader(ScriptedSession([SessionExited(2, b"")]))
        assert reader.poll(0.1) is False
        assert reader.exited is True
        assert reader.exit_code == 2

    def test_exit_code_zero_is_an_exit(self):
        from keepty import SessionExited

        reader = _offline_reader(ScriptedSession([b"hello\r\n", SessionExited(0, b"")]))
        assert reader.poll_until("hello", timeout=2) is True
        assert reader.exited is False
        assert reader.exit_code is None
        assert reader.poll(0.1) is False
        assert reader.exited is True
        assert reader.exit_code == 0

    def test_poll_until_false_when_process_exits_first(self):
        from keepty import SessionExited

        reader = _offline_reader(ScriptedSession([SessionExited(4, b"nope\r\n")]))
        started = time.monotonic()
        assert reader.poll_until("zzz", timeout=5) is False
        assert time.monotonic() - started < 1
        assert reader.exit_code == 4
        assert "nope" in reader.contents()

    def test_poll_until_true_when_text_arrives_with_exit(self):
        from keepty import SessionExited

        reader = _offline_reader(ScriptedSession([SessionExited(7, b"hello\r\n")]))
        assert reader.poll_until("hello", timeout=2) is True
        assert reader.exited is True
        assert reader.exit_code == 7

    def test_prime_records_exit(self):
        from keepty import Role, SessionExited

        reader = _offline_reader(ScriptedSession([SessionExited(6, b"primed\r\n")]))
        reader._prime(Role.MONITOR)
        assert reader.exit_code == 6
        assert "primed" in reader.contents()

        reader = _offline_reader(
            ScriptedSession([b"partial\r\n", SessionExited(8, b"rest\r\n")])
        )
        reader._prime(Role.WRITER)
        assert "partial" in reader.contents()
        assert "rest" in reader.contents()
        assert reader.exit_code == 8


class TestNamedKeys:
    @pytest.fixture(autouse=True)
    def _need_pyte(self):
        pytest.importorskip("pyte")

    def test_named_key_bytes(self):
        from unittest.mock import Mock

        reader = _offline_reader(Mock())
        expected = {
            "enter": b"\r",
            "tab": b"\t",
            "esc": b"\x1b",
            "backspace": b"\x7f",
            "up": b"\x1b[A",
            "down": b"\x1b[B",
            "left": b"\x1b[D",
            "right": b"\x1b[C",
            "home": b"\x1b[H",
            "end": b"\x1b[F",
            "pageup": b"\x1b[5~",
            "pagedown": b"\x1b[6~",
            "delete": b"\x1b[3~",
            "f1": b"\x1bOP",
            "f2": b"\x1bOQ",
            "f3": b"\x1bOR",
            "f4": b"\x1bOS",
            "f5": b"\x1b[15~",
            "f6": b"\x1b[17~",
            "f7": b"\x1b[18~",
            "f8": b"\x1b[19~",
            "f9": b"\x1b[20~",
            "f10": b"\x1b[21~",
            "f11": b"\x1b[23~",
            "f12": b"\x1b[24~",
        }
        for name, payload in expected.items():
            reader.send_key(name)
            assert reader.session.send_keys.call_args.args[0] == payload
        for n, letter in enumerate("abcdefghijklmnopqrstuvwxyz", 1):
            reader.send_key(f"ctrl-{letter}")
            assert reader.session.send_keys.call_args.args[0] == bytes([n])

    def test_key_aliases_and_application_cursor_mode(self):
        from unittest.mock import Mock, call

        session = Mock()
        reader = _offline_reader(session)
        reader.send_key("Return")
        reader.send_key("ESCAPE")
        reader.send_key("Page_Up")
        reader.send_key("Page-Down")
        reader.send_key("Ctrl+C")
        reader._feed(b"\x1b[?1h")
        reader.send_key("up")
        reader.send_key("down")
        reader.send_key("left")
        reader.send_key("right")
        reader.send_key("home")
        reader.send_key("end")
        reader._feed(b"\x1b[?1l")
        reader.send_key("up")
        reader.send_key("end")
        assert session.send_keys.call_args_list == [
            call(b"\r"),
            call(b"\x1b"),
            call(b"\x1b[5~"),
            call(b"\x1b[6~"),
            call(b"\x03"),
            call(b"\x1bOA"),
            call(b"\x1bOB"),
            call(b"\x1bOD"),
            call(b"\x1bOC"),
            call(b"\x1bOH"),
            call(b"\x1bOF"),
            call(b"\x1b[A"),
            call(b"\x1b[F"),
        ]

    def test_unknown_key_is_rejected(self):
        from unittest.mock import Mock

        reader = _offline_reader(Mock())
        with pytest.raises(ValueError):
            reader.send_key("shift-enter")
        with pytest.raises(TypeError):
            reader.send_key(None)
        reader.session.send_keys.assert_not_called()

    def test_send_text_paste_and_submit_gap(self):
        from unittest.mock import Mock, patch

        session = Mock()
        reader = _offline_reader(session)
        order = []
        session.send_keys.side_effect = lambda payload: order.append(("send", payload))

        reader.send_text("hi")
        reader.send_text("ab", paste=True)
        reader.send_text("a\nb", paste=False)
        with patch("keepty.screen.time.sleep", side_effect=lambda delay: order.append(("sleep", delay))):
            reader.send_text("a\nb", submit=True, submit_delay=0.2)
        assert order == [
            ("send", b"hi"),
            ("send", b"\x1b[200~ab\x1b[201~"),
            ("send", b"a\nb"),
            ("send", b"\x1b[200~a\nb\x1b[201~"),
            ("sleep", 0.2),
            ("send", b"\r"),
        ]

        order.clear()
        reader.submit_delay = 0.07
        with patch("keepty.screen.time.sleep", side_effect=lambda delay: order.append(("sleep", delay))):
            reader.send_text("z", submit=True)
            reader.send_text("z", submit=True, submit_delay=0)
        assert order == [
            ("send", b"z"),
            ("sleep", 0.07),
            ("send", b"\r"),
            ("send", b"z"),
            ("send", b"\r"),
        ]
        with pytest.raises(TypeError):
            reader.send_text(b"nope")

    def test_send_text_default_delay_and_validation_before_input(self):
        from unittest.mock import Mock, patch, call

        reader = _offline_reader(Mock())
        with patch("keepty.screen.time.sleep") as sleep:
            reader.send_text("say hi", submit=True)
            sleep.assert_called_once_with(1.0)
        assert reader.session.send_keys.call_args_list == [call(b"say hi"), call(b"\r")]
        reader.session.send_keys.reset_mock()
        for invalid in [-1, float("nan"), float("inf"), -float("inf")]:
            for submit in [False, True]:
                with pytest.raises(ValueError):
                    reader.send_text("must not send", submit=submit, submit_delay=invalid)
        for invalid in ["1", object(), True]:
            with pytest.raises(TypeError):
                reader.send_text("must not send", submit=True, submit_delay=invalid)
        reader.submit_delay = -1
        with pytest.raises(ValueError):
            reader.send_text("must not send", submit=True)
        with pytest.raises(TypeError):
            reader.send_text("must not send", True)
        reader.session.send_keys.assert_not_called()


def _got_lines(reader):
    lines = []
    for line in reader.contents().splitlines():
        stripped = line.strip()
        if stripped.startswith("GOT "):
            parts = stripped.split()
            if len(parts) >= 2:
                lines.append(parts[1])
    return lines


def _expect_got(reader, hex_payload, timeout=3):
    before = len(_got_lines(reader))
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        reader.poll(0.1)
        lines = _got_lines(reader)
        received = "".join(lines[before:])
        assert hex_payload.startswith(received), reader.contents()
        if received == hex_payload:
            return
    raise AssertionError(
        f"timed out waiting for {hex_payload}. Screen:\n{reader.contents()}"
    )


class TestScreenIntegration:
    def test_broker_private_sequences_keep_following_text(self, keepty_bin):
        pytest.importorskip("pyte")
        import shlex
        from keepty import ScreenReader

        program = (
            "import sys, time\n"
            "sys.stdout.buffer.write(b'BEFORE\\x1b[?4m\\x1b[>4;2m\\x1b[?5AAFTER\\n')\n"
            "sys.stdout.buffer.flush()\n"
            "time.sleep(30)\n"
        )
        sid = unique_id("private")
        child = start_broker(sid, "exec python3 -u -c " + shlex.quote(program))
        try:
            with ScreenReader.connect(sid, role=Role.WRITER) as reader:
                assert reader.poll_until("BEFORE", timeout=5), reader.contents()
                assert "AFTER" in reader.contents()
        finally:
            cleanup(child, sid)

    def test_poll_until_stops_when_child_exits(self, keepty_bin):
        pytest.importorskip("pyte")
        from keepty import ScreenReader, SessionExited

        sid = unique_id("exit-miss")
        child = start_broker(sid, "echo nope; exit 4")
        try:
            with ScreenReader.connect(sid, role=Role.WRITER) as reader:
                started = time.monotonic()
                found = reader.poll_until("not-on-screen", timeout=5)
                elapsed = time.monotonic() - started
                assert found is False
                assert reader.exited is True
                assert reader.exit_code == 4
                assert "nope" in reader.contents()
                assert elapsed < 2.5
                assert reader.poll(0.2) is False
        finally:
            cleanup(child, sid)

        sid = unique_id("exit-hit")
        child = start_broker(sid, "echo final-words; exit 9")
        try:
            with ScreenReader.connect(sid, role=Role.WRITER) as reader:
                assert reader.poll_until("final-words", timeout=5), reader.contents()
                deadline = time.monotonic() + 5
                while not reader.exited and time.monotonic() < deadline:
                    reader.poll(0.2)
                assert reader.exited is True
                assert reader.exit_code == 9
        finally:
            cleanup(child, sid)

        sid = unique_id("exit-bytes")
        child = start_broker(sid, "echo final-words; exit 9")
        try:
            with Session.connect(sid) as session:
                chunks = bytearray()
                code = None
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    try:
                        chunks += session.read_output(timeout=0.3)
                    except SessionExited as exc:
                        chunks += exc.output
                        code = exc.exit_code
                        break
                assert code == 9
                assert b"final-words" in chunks
        finally:
            cleanup(child, sid)

    def test_vim_screen_and_quit(self, keepty_bin):
        import shutil
        if shutil.which("vim") is None:
            pytest.skip("vim is not installed")
        pytest.importorskip("pyte")
        from keepty import ScreenReader

        sid = unique_id("vim")
        child = start_broker(sid, "exec vim")
        try:
            with ScreenReader.connect(sid, role=Role.WRITER) as reader:
                assert reader.poll_until("VIM", timeout=8), reader.contents()
                reader.send_text(":q!", submit=True)
                deadline = time.monotonic() + 5
                while not reader.exited and time.monotonic() < deadline:
                    reader.poll(0.2)
                assert reader.exited, reader.contents()
                assert reader.exit_code == 0
        finally:
            cleanup(child, sid)

    def test_send_key_and_send_text_roundtrip(self, keepty_bin):
        pytest.importorskip("pyte")
        import shlex
        from keepty import ScreenReader

        program = (
            "import os, select, sys, termios, tty\n"
            "fd = sys.stdin.fileno()\n"
            "old = termios.tcgetattr(fd)\n"
            "tty.setraw(fd)\n"
            "def emit(msg):\n"
            "    os.write(1, (msg + '\\r\\n').encode())\n"
            "try:\n"
            "    emit('READY')\n"
            "    while True:\n"
            "        ready, _, _ = select.select([fd], [], [], 2.0)\n"
            "        if not ready:\n"
            "            emit('IDLE')\n"
            "            break\n"
            "        data = os.read(fd, 1)\n"
            "        if not data:\n"
            "            emit('EOF')\n"
            "            break\n"
            "        emit('GOT ' + data.hex())\n"
            "finally:\n"
            "    termios.tcsetattr(fd, termios.TCSANOW, old)\n"
        )
        sid = unique_id("keys")
        child = start_broker(sid, "exec python3 -u -c " + shlex.quote(program))
        try:
            with ScreenReader.connect(sid, role=Role.WRITER, rows=80) as reader:
                assert reader.poll_until("READY", timeout=5), reader.contents()
                reader.send_key("up")
                _expect_got(reader, "1b5b41")
                reader.send_key("enter")
                _expect_got(reader, "0d")
                reader.send_key("backspace")
                _expect_got(reader, "7f")
                reader.send_key("ctrl-c")
                _expect_got(reader, "03")
                reader.send_key("f5")
                _expect_got(reader, "1b5b31357e")
                reader.send_key("delete")
                _expect_got(reader, "1b5b337e")
                reader.send_text("hi")
                _expect_got(reader, "6869")
                reader.send_text("a\nb")
                _expect_got(reader, "1b5b3230307e610a621b5b3230317e")
                reader.send_text("a\nb", paste=False)
                _expect_got(reader, "610a62")
                reader.send_text("ab", paste=True)
                _expect_got(reader, "1b5b3230307e61621b5b3230317e")
                reader.send_text("z", submit=True, submit_delay=0.3)
                _expect_got(reader, "7a0d")
        finally:
            cleanup(child, sid)


@pytest.mark.parametrize("command", [
    "sh -c 'echo a b'",
    ["sh", "-c", "echo a b"],
    ("sh", "-c", "echo a b"),
])
def test_start_preserves_quoted_arguments(command, monkeypatch):
    from types import SimpleNamespace
    from unittest.mock import Mock
    from keepty import start
    from keepty import client

    run = Mock(return_value=SimpleNamespace(returncode=0))
    monkeypatch.setattr(client.subprocess, "run", run)
    monkeypatch.setattr(client.os.path, "exists", lambda path: True)
    start("quoted", command=command, keepty_bin="/test/keepty")
    run.assert_called_once_with(
        ["/test/keepty", "start", "quoted", "--", "sh", "-c", "echo a b"],
        capture_output=True, text=True,
    )


def test_start_argument_list_is_not_split_or_mutated(monkeypatch):
    from types import SimpleNamespace
    from unittest.mock import Mock
    from keepty import start
    from keepty import client

    command = ["program with spaces", "", "literal 'quotes'", "a b"]
    original = command.copy()
    run = Mock(return_value=SimpleNamespace(returncode=0))
    monkeypatch.setattr(client.subprocess, "run", run)
    monkeypatch.setattr(client.os.path, "exists", lambda path: True)
    start("args", command=command, keepty_bin="/test/keepty")
    assert command == original
    assert run.call_args.args[0] == ["/test/keepty", "start", "args", "--", *original]
