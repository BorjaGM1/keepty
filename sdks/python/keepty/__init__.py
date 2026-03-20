"""
keepty — Python SDK for persistent terminal sessions.

Connect to keepty broker sessions, send input, read output,
and parse screen state for agents.

Usage:
    import keepty

    # Start and connect
    keepty.start("my-session", command="bash")
    with keepty.Session.connect("my-session") as session:
        session.send_keys(b"echo hello\\n")
        output = session.read_until("hello")

    # Screen reading for agents
    from keepty import ScreenReader, Role
    with ScreenReader.connect("my-session", role=Role.MONITOR) as reader:
        reader.poll()
        print(reader.contents())
"""

from .protocol import Role, MsgKind, Frame, socket_path
from .client import Session, SessionExited, start, stop, list_sessions
from .screen import ScreenReader

__all__ = [
    "Role",
    "MsgKind",
    "Frame",
    "socket_path",
    "Session",
    "SessionExited",
    "ScreenReader",
    "start",
    "stop",
    "list_sessions",
]
