"""
keepty screen — agent screen-reading layer.

Connects to a keepty broker as Monitor, feeds the raw byte stream
into a terminal parser (pyte), and exposes parsed screen state.

Requires: pip install pyte
"""

import time
from typing import Optional

from .client import Session, SessionExited
from .protocol import Role


class ScreenReader:
    """Read and parse terminal screen state from a keepty session.

    Uses pyte (a Python terminal emulator) to parse raw PTY output
    into a structured screen grid.

    Usage:
        reader = ScreenReader.connect("my-session")
        reader.poll()  # Process pending output
        print(reader.contents())  # Full screen text
        print(reader.cursor)  # (row, col)

        # Or for agents that need to interact:
        reader = ScreenReader.connect("my-session", role=Role.WRITER)
        reader.poll()
        if "login:" in reader.contents():
            reader.send_keys(b"admin\\n")
    """

    def __init__(self, session: Session, screen, stream):
        self._session = session
        self._screen = screen
        self._stream = stream

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
        try:
            import pyte
        except ImportError:
            raise ImportError(
                "pyte is required for screen reading. Install it: pip install pyte"
            )

        session = Session.connect(session_id, role=role, cols=cols, rows=rows)

        # Use HelloAck dimensions (actual PTY size), not requested size
        ack_cols = session._cols
        ack_rows = session._rows

        screen = pyte.Screen(ack_cols, ack_rows)
        stream = pyte.ByteStream(screen)

        reader = cls(session, screen, stream)

        # Prime the screen: drain replay + nudge re-render.
        # Writers/Watchers get a nudge that takes ~300ms to produce output.
        # We drain until output settles to ensure the full re-render is captured.
        reader._prime(role)

        return reader

    def _feed(self, data: bytes) -> None:
        """Feed raw bytes into the terminal parser."""
        if data:
            self._stream.feed(data)

    def _prime(self, role: Role) -> None:
        """Drain initial output (replay + nudge re-render)."""
        # First drain: ring buffer replay
        try:
            data = self._session.read_output(timeout=0.3)
            self._feed(data)
        except SessionExited:
            return

        # For Writer/Watcher: wait for nudge re-render to arrive
        if role in (Role.WRITER, Role.WATCHER):
            deadline = time.monotonic() + 1.5
            saw_output = False
            quiet_since = None
            while time.monotonic() < deadline:
                remaining = min(0.15, deadline - time.monotonic())
                if remaining <= 0:
                    break
                try:
                    data = self._session.read_output(timeout=remaining)
                except SessionExited:
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
            True if any output was processed.
        """
        data = self._session.read_output(timeout=timeout)
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
            True if found, False if timeout.
        """
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.poll(timeout=min(0.2, deadline - time.monotonic()))
            if expected in self.contents():
                return True
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

    def send_keys(self, keys: bytes) -> None:
        """Send keystrokes to the session.

        Only works if connected as Writer.
        """
        self._session.send_keys(keys)

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
