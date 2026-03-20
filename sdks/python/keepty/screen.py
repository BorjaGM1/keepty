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
            cols: Virtual terminal columns.
            rows: Virtual terminal rows.

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

        screen = pyte.Screen(cols, rows)
        stream = pyte.Stream(screen)

        reader = cls(session, screen, stream)

        # Process ring buffer replay
        try:
            data = session.read_output(timeout=1.0)
            if data:
                stream.feed(data.decode("utf-8", errors="replace"))
        except SessionExited:
            pass

        return reader

    def poll(self, timeout: float = 0.5) -> bool:
        """Process pending output frames and update screen state.

        Args:
            timeout: Maximum seconds to wait for new output.

        Returns:
            True if any output was processed.
        """
        data = self._session.read_output(timeout=timeout)
        if data:
            self._stream.feed(data.decode("utf-8", errors="replace"))
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
        """Get the full screen text (all rows joined with newlines)."""
        return "\n".join(
            self._screen.display[row].rstrip()
            for row in range(self._screen.lines)
        )

    def row(self, n: int) -> str:
        """Get a specific row's text (0-indexed)."""
        if 0 <= n < self._screen.lines:
            return self._screen.display[n].rstrip()
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
        """Resize the terminal. Only works as Writer."""
        self._session.resize(cols, rows)
        self._screen.resize(rows, cols)

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
