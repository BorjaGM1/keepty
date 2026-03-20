# Architecture

## Byte Path

```
Child app (Claude Code, vim, bash, etc.)
  ↓ writes to inner PTY slave
Inner PTY (OPOST+ONLCR ON)
  ↓ \n → \r\n conversion
  ↓ broker reads from master fd
Broker (raw byte relay — no interpretation)
  ↓ Frame(MsgKind::Output) over Unix socket
  ↓ 1MB ring buffer for reconnect replay
Attach client reader thread
  ↓ raw_write_stdout() with userspace ONLCR
  ↓ bare \n → \r\n (most already \r\n from inner PTY)
Outer PTY (OPOST OFF — passthrough)
  ↓ bytes unchanged
Terminal emulator (Ghostty/iTerm2/Terminal.app)
```

## Why OPOST OFF

The outer PTY's ONLCR would double-convert `\r\n` → `\r\r\n`. The extra `\r` shifts byte positions in XNU's 100-byte `ttwrite()` chunks, fragmenting escape sequences. Disabling OPOST makes the outer PTY transparent. Userspace ONLCR in `raw_write_stdout()` handles bare `\n` → `\r\n` instead, tracking `last_was_cr` across frames to avoid `\r\r\n` at frame boundaries.

## Resize Path

```
Terminal resizes → SIGWINCH to attach process
  → signal handler writes to pipe (async-signal-safe)
  → main thread reads pipe, queries TIOCGWINSZ
  → 50ms debounce (coalesces drag-resize bursts)
  → sends Resize frame to broker
  → broker holds clients lock across:
      1. TIOCSWINSZ on inner PTY (child gets SIGWINCH)
      2. ResizeAck broadcast to all clients
  → if widen: broker spawns 40ms timer, sends second SIGWINCH (nudge)
  → child redraws at new dimensions
```

## Key Design Decisions

- **Alt screen buffer**: Attach client enters alt screen (`\x1b[?1049h`). Required for Ink's cursor-addressed rendering — main screen scrollback corrupts layout.
- **50µs write sleep**: Keeps the outer PTY pipeline shallow, reducing stale output during resize transitions. Empirically necessary.
- **No virtual screen in the relay path**: The broker maintains a shadow vt100 parser (for future watcher replay), but the relay path never depends on parsing. Raw bytes flow from PTY to clients unmodified. This means perfect passthrough of any terminal protocol, but resize-wider is imperfect for TUIs that don't fully redraw (formal impossibility — see research docs).
- **Deferred child spawn**: Child isn't spawned until first Writer connects, so terminal capability probes (DA1, XTVERSION) get answered through the real terminal.
- **Detach key is `,,,d` (three commas + d)**: Modern TUIs enable kitty keyboard protocol / CSI-u, which encodes ALL Ctrl+keys as escape sequences instead of raw bytes — no Ctrl+letter reliably reaches the attach client. Printable characters always arrive as raw bytes regardless of keyboard protocol. Triple comma + d is impossible to trigger accidentally. Watchers use Ctrl+C (their input isn't forwarded).
- **Writer is geometry authority**: Only the writer can resize the PTY. Watchers cannot change dimensions — this prevents layout churn. Watcher resize triggers a re-render nudge (double-resize trick) instead.
- **Watcher connect uses re-render nudge**: Instead of replaying raw ring buffer (garbled for cursor-addressed TUIs), watchers trigger a double-resize trick that forces the child to re-render. The fresh render flows to all clients.
- **Single-PTY dimension limitation**: All clients share one PTY with one set of dimensions (the writer's). Watchers with smaller terminals see clipped content; watchers with larger terminals see empty space. Cursor-addressed content (Ink, vim, ncurses) cannot be reflowed — it's "put char at row X, col Y", not wrappable text. This is inherent to raw byte relay with a single PTY. Watchers should match the writer's terminal size. tmux solves this with per-client virtual screens; keepty intentionally avoids virtual screens. A viewport/pan mode (crop the PTY surface for smaller watchers) is feasible with the vt100 shadow parser but not yet implemented.

## Crate Map

| Crate | Type | Entry point |
|-------|------|-------------|
| `keepty-protocol` | lib | `src/lib.rs` — Frame, MsgKind, Role, encode/decode. Zero deps. |
| `keepty` | bin | `src/main.rs` → `broker.rs` (daemon) + `attach.rs` (client). Unified CLI: start/attach/watch/list/stop |
| `keepty-screen` | lib | `src/lib.rs` — vt100 parsing layer for agents |

## Security Model

- **Socket directory**: `$XDG_RUNTIME_DIR/keepty` (Linux) or `$TMPDIR/keepty` (macOS), created with `0700` permissions
- **Socket file**: `0600` permissions — only the owning user can connect
- **Hello timeout**: 5 seconds — prevents thread exhaustion from silent connections
- **Connection cap**: 32 simultaneous connections (RAII guard, checked before thread::spawn)
- **No protocol-level auth**: trusts Unix socket permissions for access control
- **Frame size limit**: 1MB max payload, validated on read to prevent memory exhaustion

## Runtime Notes

- **TERM**: Child process gets `TERM=xterm-256color` hardcoded. This means terminal-specific features (Kitty image protocol, Sixel) require the child to probe capabilities at runtime, not rely on `$TERM`
- **Detach keys**: Writer: `,,,d` (three commas + d). Watcher: Ctrl+C. Printable chars bypass kitty keyboard protocol / CSI-u encoding
- **Watcher replay**: Watchers skip raw ring buffer replay (garbled for cursor-addressed TUIs). Instead, watcher connect triggers a double-resize nudge that forces the child to re-render
