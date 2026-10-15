"""
keepty screen — agent screen-reading layer.

Connects to a keepty broker as Monitor, feeds the raw byte stream
into a terminal parser (pyte), and exposes parsed screen state.

Requires: pip install pyte
"""

import math
import time
from numbers import Real
from typing import Optional

from .client import Session, SessionExited
from .protocol import Role


# Gap before Enter on send_text(..., submit=True). TUIs such as Codex
# treat an Enter that shares a burst with a paste as another newline
# inside the paste, so the key has to be a later, separate write.
DEFAULT_SUBMIT_DELAY = 1.0

# xterm bracketed paste (CSI 200 ~ / CSI 201 ~).
_PASTE_START = b"\x1b[200~"
_PASTE_END = b"\x1b[201~"

# Private mode 1, stored shifted by pyte (see pyte.modes). Set by CSI ? 1 h.
_DECCKM = 1 << 5

# Cursor keys, home, and end. Application mode is what vim turns on.
_CURSOR_NORMAL = {
    "up": b"\x1b[A",
    "down": b"\x1b[B",
    "right": b"\x1b[C",
    "left": b"\x1b[D",
    "home": b"\x1b[H",
    "end": b"\x1b[F",
}
_CURSOR_APPLICATION = {
    "up": b"\x1bOA",
    "down": b"\x1bOB",
    "right": b"\x1bOC",
    "left": b"\x1bOD",
    "home": b"\x1bOH",
    "end": b"\x1bOF",
}

# xterm-256color terminfo (kf1..kf12, kdch1, kpp, knp). F1..F4 are SS3.
_NAMED_KEYS = {
    "enter": b"\r",
    "tab": b"\t",
    "esc": b"\x1b",
    "backspace": b"\x7f",
    "delete": b"\x1b[3~",
    "pageup": b"\x1b[5~",
    "pagedown": b"\x1b[6~",
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

_KEY_ALIASES = {
    "escape": "esc",
    "return": "enter",
    "page-up": "pageup",
    "page-down": "pagedown",
}


class _GtCsiFilter:
    """Drop CSI sequences that use the ``>`` private parameter byte.

    pyte skips ``>`` and then dispatches the rest as a normal sequence,
    so ``ESC[>4;2m`` (xterm modifyOtherKeys) becomes SGR underline.
    Those extensions do not change the screen. Bytes are held across
    ``push`` calls until a sequence is complete, so a split CSI is not
    fed to pyte in pieces.
    """

    def __init__(self) -> None:
        self._pending = bytearray()

    def push(self, data: bytes) -> bytes:
        if not data:
            return b""
        if not self._pending and 0x1B not in data:
            return data
        self._pending.extend(data)
        return self._drain()

    def _drain(self) -> bytes:
        buf = self._pending
        out = bytearray()
        i = 0
        n = len(buf)
        while i < n:
            esc = buf.find(0x1B, i)
            if esc < 0:
                out.extend(buf[i:])
                i = n
                break
            if esc > i:
                out.extend(buf[i:esc])
            if esc + 1 >= n:
                i = esc
                break
            if buf[esc + 1] != 0x5B:
                out.append(0x1B)
                i = esc + 1
                continue
            decision = self._scan_csi(buf, esc + 2)
            if decision is None:
                i = esc
                break
            kind, end = decision
            if kind == "invalid":
                out.append(0x1B)
                i = esc + 1
                continue
            if kind == "keep":
                out.extend(buf[esc:end])
            i = end
        if i:
            del buf[:i]
        return bytes(out)

    @staticmethod
    def _scan_csi(buf: bytearray, start: int):
        """Classify a CSI body starting at the first byte after ``ESC [``.

        Returns ``(kind, end)`` or None if the sequence is incomplete.
        ``kind`` is ``keep``, ``drop`` (body contains ``>``), or ``invalid``.
        ``end`` is the index just past the final byte (unused for invalid).
        """
        n = len(buf)
        limit = min(n, start + 128)
        j = start
        has_gt = False
        while j < limit:
            c = buf[j]
            if c == 0x3E:
                has_gt = True
            if 0x20 <= c <= 0x3F:
                j += 1
                continue
            if 0x40 <= c <= 0x7E:
                return ("drop" if has_gt else "keep", j + 1)
            return ("invalid", j)
        if start + 128 <= n:
            return ("invalid", start)
        return None


def _load_pyte():
    try:
        import pyte
    except ImportError as exc:
        raise ImportError(
            "pyte is required for screen reading. Install it: pip install pyte"
        ) from exc
    return pyte


_PARSER_CLASSES = None


def _parser_classes():
    """Return ``(screen_cls, stream_cls)``, importing pyte on first use."""
    global _PARSER_CLASSES
    if _PARSER_CLASSES is None:
        _PARSER_CLASSES = _build_parser_classes()
    return _PARSER_CLASSES


def _build_parser_classes():
    pyte = _load_pyte()

    class TolerantScreen(pyte.Screen):
        """pyte screen that ignores private SGR instead of raising.

        pyte's stream calls ``select_graphic_rendition(*params, private=True)``
        for CSI sequences that contain ``?`` (vim sends ``ESC[?4m``).
        The stock method does not accept ``private`` and raises TypeError.
        Private renditions do not apply character attributes.
        """

        def select_graphic_rendition(self, *attrs, private: bool = False, **kwargs):
            if private or kwargs.get("private"):
                return
            super().select_graphic_rendition(*attrs)

    class TolerantByteStream(pyte.ByteStream):
        """Byte stream that keeps parsing after an unknown sequence.

        A handler TypeError aborts pyte's ``feed`` and drops the rest of
        the chunk. Swallowing the error inside the per-byte hook lets the
        following bytes update the screen. The parent already resets its
        parser before re-raising.
        """

        def __init__(self, screen):
            super().__init__(screen)
            self._gt_csi = _GtCsiFilter()

        def _send_to_parser(self, data: str):
            try:
                return super()._send_to_parser(data)
            except Exception:
                return False

        def feed(self, data: bytes) -> None:
            if not data:
                return
            data = self._gt_csi.push(data)
            if not data:
                return
            try:
                super().feed(data)
            except Exception:
                try:
                    self._initialize_parser()
                except Exception:
                    pass

    return TolerantScreen, TolerantByteStream


class ScreenReader:
    """Read and parse terminal screen state from a keepty session.

    Uses pyte (a Python terminal emulator) to parse raw PTY output
    into a structured screen grid. Unsupported control-sequence extensions
    are tolerated. A parse error does not close the reader.

    When the child process exits, ``exited`` becomes True and
    ``exit_code`` is the status (0 is a successful exit, not "still
    running"). ``poll`` and ``poll_until`` do not raise
    ``SessionExited``. See ``poll`` for why.

    Usage:
        reader = ScreenReader.connect("my-session")
        reader.poll()  # Process pending output
        print(reader.contents())  # Full screen text
        print(reader.cursor)  # (row, col)

        # Or for agents that need to interact:
        reader = ScreenReader.connect("my-session", role=Role.WRITER)
        reader.poll()
        if "login:" in reader.contents():
            reader.send_text("admin", submit=True)
    """

    def __init__(self, session: Session, screen, stream):
        self._session = session
        self._screen = screen
        self._stream = stream
        self._exited = False
        self._exit_code: Optional[int] = None
        self.submit_delay = DEFAULT_SUBMIT_DELAY

    @classmethod
    def connect(
        cls,
        session_id: str,
        role: Role = Role.MONITOR,
        cols: int = 80,
        rows: int = 24,
    ) -> "ScreenReader":
        """Connect to a session with screen parsing.

        Args:
            session_id: The session to connect to.
            role: Monitor (read-only, default) or Writer (can interact).
            cols: Requested terminal columns (broker may use different size).
            rows: Requested terminal rows (broker may use different size).

        Returns:
            A connected ScreenReader.

        Raises:
            ImportError: If pyte is not installed.
        """
        screen_cls, stream_cls = _parser_classes()

        session = Session.connect(session_id, role=role, cols=cols, rows=rows)

        # Use HelloAck dimensions (actual PTY size), not requested size
        ack_cols = session._cols
        ack_rows = session._rows

        screen = screen_cls(ack_cols, ack_rows)
        stream = stream_cls(screen)

        reader = cls(session, screen, stream)

        # Prime the screen: drain replay + nudge re-render.
        # Writers get a nudge that takes ~300ms to produce output.
        # We drain until output settles to ensure the full re-render is captured.
        reader._prime(role)

        return reader

    def _feed(self, data: bytes) -> None:
        """Feed raw bytes into the terminal parser.

        Parser bugs and unknown sequences are dropped. They must not
        escape to ``poll`` or ``connect``.
        """
        if not data:
            return
        try:
            self._stream.feed(data)
        except Exception:
            return

    def _note_exit(self, exc: SessionExited) -> bytes:
        """Record an exit and apply any output that arrived with it."""
        output = bytes(getattr(exc, "output", b"") or b"")
        if not self._exited:
            self._exited = True
            self._exit_code = int(exc.exit_code)
        if output:
            self._feed(output)
        return output

    def _prime(self, role: Role) -> None:
        """Drain initial output (replay + nudge re-render)."""
        # First drain: ring or alternate-screen replay
        try:
            data = self._session.read_output(timeout=0.3)
        except SessionExited as exc:
            self._note_exit(exc)
            return
        self._feed(data)

        # Writers wait for nudge re-render to arrive
        if role == Role.WRITER:
            deadline = time.monotonic() + 1.5
            saw_output = False
            quiet_since = None
            while time.monotonic() < deadline:
                remaining = min(0.15, deadline - time.monotonic())
                if remaining <= 0:
                    break
                try:
                    data = self._session.read_output(timeout=remaining)
                except SessionExited as exc:
                    self._note_exit(exc)
                    return
                if data:
                    self._feed(data)
                    saw_output = True
                    quiet_since = None
                else:
                    if saw_output and quiet_since is None:
                        quiet_since = time.monotonic()
                    # If output settled for 150ms after seeing something, we're done
                    if quiet_since and time.monotonic() - quiet_since >= 0.15:
                        break

    def poll(self, timeout: float = 0.5) -> bool:
        """Process pending output frames and update screen state.

        Args:
            timeout: Maximum seconds to wait for new output.

        Returns:
            True if any PTY output was applied to the screen.

        Exit handling: this does not raise ``SessionExited``. ``poll``
        reports whether the screen changed, in the same way
        ``subprocess.Popen.poll`` reports a process status instead of
        throwing. An exit is terminal state the caller inspects via
        ``exited`` and ``exit_code``.

        Output that arrives in the same read as the exit frame is still
        applied, and this call returns True when that output is
        non-empty. Once ``exited`` is set, later calls return False
        without reading. ``Session.read_output`` still raises
        ``SessionExited`` for callers that want the exception.

        ``poll_until`` uses the same rule: it returns False when the
        child exits before the expected text is on screen, rather than
        waiting out the timeout or raising.
        """
        if self._exited:
            return False
        try:
            data = self._session.read_output(timeout=timeout)
        except SessionExited as exc:
            return bool(self._note_exit(exc))
        if data:
            self._feed(data)
            return True
        return False

    def poll_until(self, expected: str, timeout: float = 5.0) -> bool:
        """Poll until screen contains expected text.

        Args:
            expected: Text to wait for on screen.
            timeout: Maximum seconds to wait.

        Returns:
            True if the text appears. False on timeout, or if the child
            exits before the text appears (``exited`` is then True).
            Output that arrives with the exit is applied first, so text
            in that final chunk still counts as found.

        Does not raise ``SessionExited``. See ``poll``.
        """
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.poll(timeout=min(0.2, max(0.0, deadline - time.monotonic())))
            if expected in self.contents():
                return True
            if self.exited:
                return False
        return False

    def contents(self) -> str:
        """Get the full screen text (all rows joined with newlines).

        Note: rows with only styled spaces (e.g., colored backgrounds in curses)
        will appear as spaces, not empty strings. Use visible_contents() for
        a representation that marks styled cells.
        """
        return "\n".join(self._screen.display)

    def visible_contents(self) -> str:
        """Get screen text with styled spaces marked.

        Curses apps often draw rows of styled spaces (colored background).
        Regular contents() preserves these as spaces, but rstrip() would
        collapse them. This method marks styled cells so they're visible.
        """
        rows = []
        for y in range(self._screen.lines):
            chars = []
            for x in range(self._screen.columns):
                cell = self._screen.buffer[y][x]
                ch = cell.data
                if ch == " " and (cell.bg != "default" or cell.fg != "default" or cell.reverse):
                    ch = "\u2588"  # Full block for styled spaces
                chars.append(ch)
            rows.append("".join(chars))
        return "\n".join(rows)

    def row(self, n: int) -> str:
        """Get a specific row's text (0-indexed)."""
        if 0 <= n < self._screen.lines:
            return self._screen.display[n]
        return ""

    @property
    def cursor(self) -> tuple[int, int]:
        """Cursor position as (row, col), both 0-indexed."""
        return (self._screen.cursor.y, self._screen.cursor.x)

    @property
    def size(self) -> tuple[int, int]:
        """Screen size as (rows, cols)."""
        return (self._screen.lines, self._screen.columns)

    @property
    def exited(self) -> bool:
        """True after the child process has exited."""
        return self._exited

    @property
    def exit_code(self) -> Optional[int]:
        """Child exit status, or None while the child is still running.

        0 means the process exited successfully. Check ``exited`` (or
        compare this with None) rather than treating 0 as "still running".
        """
        return self._exit_code

    def send_keys(self, keys: bytes) -> None:
        """Send keystrokes to the session.

        Only works if connected as Writer.
        """
        self._session.send_keys(keys)

    def send_key(self, name: str) -> None:
        """Send one named key.

        Names are case-insensitive. ``_`` and ``+`` are treated as ``-``.

        Supported names:
            enter (alias return), tab, esc (alias escape), backspace,
            up, down, left, right, home, end, pageup (alias page-up),
            pagedown (alias page-down), delete, ctrl-a .. ctrl-z,
            f1 .. f12.

        enter is CR (``\\r``). backspace is DEL (``0x7f``). ctrl-a .. ctrl-z
        are bytes 0x01 .. 0x1a. delete, pageup, and pagedown are the
        xterm sequences ``ESC[3~``, ``ESC[5~``, and ``ESC[6~``.
        f1 .. f4 are SS3 (``ESC O P`` .. ``ESC O S``); f5 .. f12 are the
        xterm-256color terminfo sequences.

        up, down, left, right, home, and end follow DECCKM, the
        cursor-key mode full-screen programs such as vim enable.
        When it is set they are SS3 (``ESC O A`` for up). Otherwise
        they are CSI (``ESC [ A`` for up).

        Raises:
            TypeError: ``name`` is not a str.
            ValueError: ``name`` is not a known key.
            PermissionError: Not connected as Writer.
        """
        if not isinstance(name, str):
            raise TypeError(f"key name must be str, got {type(name).__name__}")
        self.send_keys(self._encode_key(name))

    def _encode_key(self, name: str) -> bytes:
        key = name.strip().lower().replace("_", "-").replace("+", "-")
        key = _KEY_ALIASES.get(key, key)
        if key in _CURSOR_NORMAL:
            table = _CURSOR_APPLICATION if self._application_cursor_keys() else _CURSOR_NORMAL
            return table[key]
        if key in _NAMED_KEYS:
            return _NAMED_KEYS[key]
        if len(key) == 6 and key.startswith("ctrl-") and "a" <= key[5] <= "z":
            return bytes([ord(key[5]) - 96])
        raise ValueError(
            f"Unknown key {name!r}. Expected enter, tab, esc, backspace, "
            "up, down, left, right, home, end, pageup, pagedown, delete, "
            "ctrl-a..ctrl-z, or f1..f12."
        )

    def _application_cursor_keys(self) -> bool:
        mode = getattr(self._screen, "mode", None)
        if isinstance(mode, (set, frozenset)):
            return _DECCKM in mode
        return False

    def send_text(
        self,
        text: str,
        *,
        submit: bool = False,
        paste: Optional[bool] = None,
        submit_delay: Optional[float] = None,
    ) -> None:
        """Send text, optionally as a bracketed paste, optionally followed by Enter.

        Args:
            text: Characters to write. Encoded as UTF-8.
            submit: After the text, wait ``submit_delay`` seconds and send
                Enter in a separate write. Submission is best-effort: the gap
                reduces paste-burst confusion but does not confirm submission.
            paste: Wrap the text in xterm bracketed-paste markers
                (``ESC[200~`` .. ``ESC[201~``). ``None`` (the default)
                enables that only when ``text`` contains ``\\n`` or ``\\r``.
                ``True`` always wraps. ``False`` never wraps.
            submit_delay: Seconds to wait before Enter when ``submit`` is
                set. ``None`` uses ``self.submit_delay`` (default
                ``DEFAULT_SUBMIT_DELAY``, 1.0s). Must be a finite non-negative
                number; zero sends Enter immediately.

        Enter is sent after the paste end marker, not inside the paste.
        There are no automatic Enter retries. This is not a sanitizing
        primitive: embedded control characters and paste-end markers remain
        active input. Do not treat arbitrary untrusted text as safe keystrokes.

        Raises:
            TypeError: ``text`` is not a str or the delay is not a number.
            ValueError: The delay is negative or non-finite.
            PermissionError: Not connected as Writer.
        """
        if not isinstance(text, str):
            raise TypeError(f"text must be str, got {type(text).__name__}")
        delay = self.submit_delay if submit_delay is None else submit_delay
        if isinstance(delay, bool) or not isinstance(delay, Real):
            raise TypeError("submit_delay must be a finite non-negative number")
        try:
            delay = float(delay)
        except OverflowError:
            raise ValueError("submit_delay must be finite") from None
        if not math.isfinite(delay) or delay < 0:
            raise ValueError("submit_delay must be finite and non-negative")
        raw = text.encode("utf-8")
        use_paste = paste if paste is not None else ("\n" in text or "\r" in text)
        if use_paste:
            self.send_keys(_PASTE_START + raw + _PASTE_END)
        elif raw:
            self.send_keys(raw)
        if submit:
            if delay > 0:
                time.sleep(delay)
            self.send_key("enter")

    def resize(self, cols: int, rows: int) -> None:
        """Resize the terminal. Only works as Writer.

        Note: local screen resize happens when poll() receives ResizeAck,
        not immediately.
        """
        self._session.resize(cols, rows)

    def close(self) -> None:
        """Close the connection."""
        self._session.close()

    @property
    def session(self) -> Session:
        """Access the underlying Session for lower-level operations."""
        return self._session

    def __enter__(self) -> "ScreenReader":
        return self

    def __exit__(self, *args) -> None:
        self.close()

    def __repr__(self) -> str:
        return f"ScreenReader({self._session!r})"
