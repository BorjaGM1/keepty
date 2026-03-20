# keepty Wire Protocol Specification

Version 1

## Overview

keepty uses a length-prefixed binary protocol over Unix domain sockets. The protocol is intentionally minimal — it exists to frame raw bytes, negotiate roles, and handle session lifecycle. It does **not** interpret terminal escape sequences.

## Frame Format

Every message is a frame with this structure:

```
Offset  Size    Field       Description
0       4       length      Frame body length in bytes (big-endian u32)
4       1       version     Protocol version (currently 1)
5       1       kind        Message type (see below)
6       N       payload     Message-specific data (length - 2 bytes)
```

The `length` field includes `version` + `kind` + `payload`. Minimum length is 2 (version + kind, no payload).

All multi-byte integers are big-endian.

## Connection Flow

```
Client                              Broker
   │                                   │
   ├──── Hello ───────────────────────►│  Client declares role + terminal size
   │                                   │
   │◄──── HelloAck ───────────────────┤  Broker confirms with PID + current size
   │                                   │
   │◄──── Output (ring buffer) ───────┤  Replay of recent output (up to 1MB)
   │                                   │
   │◄──── Output (live) ─────────────┤  Ongoing PTY output broadcast
   │                                   │
   ├──── Input ───────────────────────►│  Keystrokes (Writer only)
   │                                   │
   ├──── Resize ──────────────────────►│  Terminal size change (Writer only)
   │                                   │
   │◄──── Exit ───────────────────────┤  Process exited
   │                                   │
```

## Message Types

### Hello (kind = 1)

Direction: client -> broker

Sent immediately after connecting. Declares the client's role and terminal dimensions.

```
Offset  Size    Field    Description
0       1       role     Client role (1=Writer, 2=Watcher, 3=Monitor)
1       2       cols     Terminal columns (big-endian u16)
3       2       rows     Terminal rows (big-endian u16)
```

### HelloAck (kind = 2)

Direction: broker -> client

Sent in response to Hello. Confirms the connection and provides session info.

```
Offset  Size    Field     Description
0       4       pty_pid   PID of the process running in the PTY (big-endian u32)
4       2       cols      Current terminal columns (big-endian u16)
6       2       rows      Current terminal rows (big-endian u16)
```

**Note:** For the first Writer, `pty_pid` is 0. The child process is spawned *after* HelloAck is sent (deferred spawn), so the PID isn't known yet. This is intentional — it ensures terminal capability probes (DA1, XTVERSION) from TUI apps get answered through the real terminal from the first byte. Subsequent clients receive the real PID.

After HelloAck, the broker immediately sends the ring buffer contents as a single Output frame (if non-empty). This is the replay that allows the client to reconstruct screen state.

### Input (kind = 3)

Direction: client -> broker

Raw keystroke data to be written to the PTY's stdin. Only accepted from the current Writer.

```
Offset  Size    Field     Description
0       N       data      Raw bytes to write to PTY stdin
```

### Output (kind = 4)

Direction: broker -> client

Raw PTY output bytes, broadcast to all connected clients. This is the unmodified byte stream from the program running in the PTY — escape sequences, control characters, everything.

```
Offset  Size    Field     Description
0       N       data      Raw bytes from PTY stdout
```

### Resize (kind = 5)

Direction: client -> broker

Terminal size change. Only accepted from the current Writer. The broker resizes the PTY and sends SIGWINCH to the running program.

```
Offset  Size    Field    Description
0       2       cols     New terminal columns (big-endian u16)
2       2       rows     New terminal rows (big-endian u16)
```

### ResizeAck (kind = 6)

Direction: broker -> all clients

Sent after the broker applies a Resize. Broadcast to all clients while holding the clients lock, guaranteeing it arrives before any post-resize Output frames.

```
Offset  Size    Field    Description
0       4       gen      Resize generation counter (big-endian u32, monotonically increasing)
4       2       cols     New terminal columns (big-endian u16)
6       2       rows     New terminal rows (big-endian u16)
```

### Exit (kind = 10)

Direction: broker -> client

The process running in the PTY has exited. Sent to all connected clients.

```
Offset  Size    Field       Description
0       4       exit_code   Process exit code (big-endian i32)
```

### Shutdown (kind = 11)

Direction: client -> broker

Request the broker to terminate the session. The broker sends SIGTERM to the process group, waits for a grace period, then SIGKILL if needed.

No payload.

### Ping (kind = 12)

Direction: client -> broker

Connection keepalive. The broker responds with Pong.

No payload.

### Pong (kind = 13)

Direction: broker -> client

Response to Ping.

No payload.

### Error (kind = 127)

Direction: broker -> client

Error message.

```
Offset  Size    Field     Description
0       N       message   UTF-8 encoded error string
```

## Roles

Roles are declared in the Hello message and enforced by the broker.

### Writer (role = 1)

- Can send Input frames (keystrokes written to PTY stdin)
- Can send Resize frames (PTY resized, SIGWINCH sent to program)
- Can send Shutdown frames
- Receives Output, ResizeAck, Exit frames (broadcast)
- **Exclusive**: only one Writer at a time. New Writer connections replace the previous one.

### Watcher (role = 2)

- Cannot send Input
- Can send Resize frames — treated as a redraw request (triggers a double-resize nudge), NOT an actual PTY resize. Only when watcher size >= current PTY width.
- Receives Output, ResizeAck, Exit frames (broadcast)
- **Does NOT receive raw ring buffer replay** on connect. Instead, watcher connect triggers a double-resize nudge that forces the child to re-render, providing a clean initial view.
- Multiple Watchers can connect simultaneously
- Use case: read-only spectating, pair programming

### Monitor (role = 3)

- Cannot send Input or Resize
- Can send Shutdown frames (used by `keepty stop`)
- Receives Output, ResizeAck, Exit frames (broadcast)
- Receives raw ring buffer replay on connect
- Intended for programmatic clients that analyze the byte stream
- Use case: screen parsing via vt100, health monitoring, agent observation

## Ring Buffer Replay

When a Writer or Monitor connects, the broker sends the contents of its ring buffer (up to 1MB) as a single Output frame immediately after HelloAck. This allows the client's terminal to reconstruct the current screen state without the broker needing to understand terminal escape sequences.

**Watchers do NOT receive raw ring buffer replay.** Raw replay is garbled for cursor-addressed TUIs (Ink, vim, ncurses) because it doesn't include the resize history. Instead, watcher connect triggers a double-resize nudge: the broker briefly resizes the PTY to (cols-1, rows) then back to (cols, rows), forcing the child to re-render. The fresh render flows to all clients including the newly-connected watcher.

The replay may contain partial escape sequences or content rendered for a different terminal size. Well-implemented terminal emulators (xterm.js, vt100 crate) handle this gracefully and converge to the correct state. Live output from the program then continues normally.

## Frame Size Limits

The reference implementation enforces a maximum payload size of 1,048,576 bytes (1MB, matching the ring buffer capacity). The maximum frame body size is payload + 2 (version + kind bytes). Frames exceeding this limit are rejected without allocating memory. Client implementations SHOULD enforce similar limits to prevent memory exhaustion.

## Socket Path

The socket path is resolved at runtime:

1. `$XDG_RUNTIME_DIR/keepty/keepty-{session_id}.sock` (if `$XDG_RUNTIME_DIR` is set, typical on Linux: `/run/user/$UID/keepty/`)
2. `$TMPDIR/keepty/keepty-{session_id}.sock` (fallback, typical on macOS: `/var/folders/.../T/keepty/`)

The socket directory is created with `0700` permissions and the socket file with `0600` permissions, ensuring only the owning user can access sessions.

All SDKs provide a `socket_dir()` / `socketDir()` / `SocketDirResolved()` function that implements this resolution.

## Implementing a Client

A minimal client implementation needs to:

1. Connect to the Unix socket
2. Send a Hello frame with desired role and terminal size
3. Read and validate the HelloAck frame
4. Process the ring buffer replay (first Output frame)
5. Enter a read loop processing Output frames

For a Writer client, additionally:
- Forward stdin to the broker as Input frames
- Handle SIGWINCH by sending Resize frames

The protocol is simple enough to implement in any language with Unix socket support. The `keepty-protocol` Rust crate provides reference encode/decode implementations.
