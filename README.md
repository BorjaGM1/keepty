# keepty

Your Claude Code session survives terminal closes — and still renders natively.

keepty is a PTY broker. It owns the pseudo-terminal, stores raw bytes in a ring buffer, and streams them to connected clients over a typed binary protocol. Your real terminal does all the rendering. No re-emulation. TUIs look native because they are native.

```bash
# Start a persistent session (auto-attaches)
keepty start my-session -- claude

# Detach: ,,,d — session keeps running
# Close your terminal. Go home. Come back.

# Reattach — picks up exactly where you left off
keepty attach my-session

# Watch from another terminal (read-only)
keepty attach --watch my-session
```

## Install

```bash
# macOS (Apple Silicon) / Linux (x64, arm64)
curl -sSf https://raw.githubusercontent.com/BorjaGM1/keepty/main/install.sh | sh

# Or with Cargo
cargo install keepty
```

---

## The problem

Every developer who uses tmux has had this experience: you set up a beautiful terminal — GPU rendering, custom fonts, Kitty image protocol — and then tmux makes it all look slightly broken. Borders misalign. Colors shift. TUI apps redraw janky.

This isn't a bug. It's the architecture. tmux is a terminal emulator running inside your terminal. It parses every escape sequence, maintains its own screen buffer, and re-encodes everything. Mitchell Hashimoto (Ghostty) [explained why](https://mitchellh.com/writing/ghostty-and-tmux) — it's a VM for your terminal output. Every feature your terminal supports has to be independently understood and forwarded by tmux, or it breaks.

The alternative (dtach, abduco) relays bytes natively — no re-emulation. But that's all they do. Reattach gives you a blank screen. No protocol. No roles. No way to build on top.

## What keepty does differently

**Native rendering like dtach, but with infrastructure you can build on:**

- **Ring buffer replay** — reattach shows your previous screen state, not a blank terminal
- **Typed binary protocol** — build clients in any language (Python, TypeScript, Go, .NET, Rust)
- **Role-based access** — Writer (exclusive control), Watcher (read-only spectating), Monitor (agent screen reading)
- **Agent screen-reading layer** — `keepty-screen` parses the terminal into a structured grid. No screenshots. No OCR.

---

## How it works

```
┌─────────────┐                      ┌─────────────┐
│  Program    │◄────── PTY ─────────►│   keepty    │
│  (vim, htop,│     (raw bytes)      │   broker    │
│   claude)   │                      │             │
└─────────────┘                      │  ┌───────┐  │
                                     │  │ Ring  │  │
                                     │  │Buffer │  │
                                     │  │ (1MB) │  │
                                     │  └───────┘  │
                                     │             │
                                     │  Binary     │
                                     │  Protocol   │
                                     └──────┬──────┘
                                            │
                        ┌───────────────────┼───────────────────┐
                        │                   │                   │
                        ▼                   ▼                   ▼
                 ┌─────────────┐    ┌─────────────┐    ┌─────────────┐
                 │   Writer    │    │   Watcher   │    │   Monitor   │
                 │  (terminal) │    │  (viewer)   │    │  (agent)    │
                 └─────────────┘    └─────────────┘    └─────────────┘
```

1. `keepty start` spawns your command in a PTY and listens on a Unix socket.
2. All output stored in a 1MB ring buffer. The broker runs in the background — survives terminal closes.
3. Clients connect, handshake (role + terminal size), get replay + live output.
4. Your terminal renders raw bytes directly. No intermediary.

On reattach: ring buffer replay brings your terminal up to date, then a resize nudge forces the program to redraw at your current terminal size.

---

## For agents: keepty-screen

This is the part that doesn't exist anywhere else.

Agent-terminal interaction today: "capture stdout" (no interactivity), "screenshot + OCR" (slow, expensive), or "parse JSON" (only cooperative programs). None give agents structured access to what's on the terminal screen.

`keepty-screen` connects as Monitor, feeds the byte stream into a headless [vt100](https://crates.io/crates/vt100) parser, and gives you a parsed screen grid — characters, colors, cursor position, at wire speed.

```rust
use keepty_screen::{ScreenReader, Role};

let reader = ScreenReader::connect(
    &keepty_protocol::socket_path("my-session"), Role::Monitor
)?;

let screen = reader.screen();
println!("{}", screen.contents());       // Full text
let cursor = screen.cursor_position();   // (row, col)

// Or connect as Writer to drive a TUI
let mut driver = ScreenReader::connect_with_size(sock, Role::Writer, 80, 24)?;
driver.send_keys(b":wq\n")?;
```

An orchestrator watches as Monitor (can't interfere). A worker drives as Writer (can type). Both on the same session, simultaneously.

---

## The protocol

6-byte header, length-prefixed frames over Unix sockets.

```
┌──────────┬─────────┬──────┬─────────────────┐
│ u32 len  │ u8 ver  │ u8   │    payload       │
│ (BE)     │ (1)     │ kind │    (variable)    │
└──────────┴─────────┴──────┴─────────────────┘
```

| Kind | Name | Direction | Payload |
|------|------|-----------|---------|
| 1 | Hello | client -> broker | role, cols, rows |
| 2 | HelloAck | broker -> client | pty_pid, cols, rows |
| 3 | Input | client -> broker | raw keystrokes |
| 4 | Output | broker -> client | raw PTY bytes |
| 5 | Resize | client -> broker | cols, rows |
| 6 | ResizeAck | broker -> all | gen, cols, rows |
| 10 | Exit | broker -> client | exit code |
| 11 | Shutdown | client -> broker | — |
| 12/13 | Ping/Pong | keepalive | — |
| 127 | Error | broker -> client | error message |

SDKs: [Python](sdks/python) | [TypeScript](sdks/typescript) | [Go](sdks/go) | [.NET](sdks/dotnet) | [Rust](keepty-protocol)

Full spec: [PROTOCOL.md](docs/PROTOCOL.md)

---

## Comparison

| | tmux | dtach / abduco | keepty |
|---|---|---|---|
| Persistence | yes | yes | yes |
| Native rendering | no (re-emulates) | yes | yes |
| Reattach shows state | yes (re-emulated) | no (blank) | yes (ring buffer) |
| Read-only spectating | no | no | yes (Watcher) |
| Agent screen reading | no | no | yes (keepty-screen) |
| Typed protocol | no | no | yes |
| Multi-language SDKs | no | no | yes |
| Splits/tabs/scripting | yes | no | no |

keepty does less than tmux. What it does — persistence, native rendering, a protocol — it does without re-emulation.

---

## Crates

| Crate | Type | Description |
|-------|------|-------------|
| `keepty-protocol` | Library | Wire format, roles, frame encode/decode. Zero deps. |
| `keepty` | Binary | CLI + broker. Start/attach/watch/stop. |
| `keepty-screen` | Library | Agent screen reading. Connect + vt100 parse. |

## License

Dual-licensed under [MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE).
