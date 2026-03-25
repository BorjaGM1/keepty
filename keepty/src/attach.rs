//! Attach to a running keepty broker session from the current terminal.

use keepty_protocol::{self as protocol, Frame, MsgKind, Role};
use nix::sys::termios::{self, SetArg, Termios};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const RESIZE_DEBOUNCE_MS: u64 = 50;
const WATCHER_OVERLAY_DELAY_MS: u64 = 300;

fn get_terminal_size() -> (u16, u16) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 {
            (ws.ws_col, ws.ws_row)
        } else {
            (80, 24)
        }
    }
}

fn enter_raw_mode() -> io::Result<Termios> {
    use nix::sys::termios::*;
    let stdin = io::stdin();
    let original = termios::tcgetattr(&stdin).map_err(io::Error::other)?;
    let mut raw = original.clone();

    raw.local_flags &= !(LocalFlags::ECHO
        | LocalFlags::ICANON
        | LocalFlags::ISIG
        | LocalFlags::IEXTEN
        | LocalFlags::ECHOE
        | LocalFlags::ECHOK
        | LocalFlags::ECHONL);
    raw.input_flags &= !(InputFlags::ICRNL
        | InputFlags::INLCR
        | InputFlags::IGNCR
        | InputFlags::IXON
        | InputFlags::ISTRIP
        | InputFlags::BRKINT);

    // Disable OPOST — prevents the outer PTY's ONLCR from double-converting
    // \r\n (from inner PTY) to \r\r\n. The extra \r shifts byte positions in
    // XNU's 100-byte ttwrite chunks, splitting escape sequences. Userspace
    // ONLCR in raw_write_stdout handles bare \n → \r\n conversion instead.
    raw.output_flags &= !nix::sys::termios::OutputFlags::OPOST;

    raw.control_chars[nix::sys::termios::SpecialCharacterIndices::VMIN as usize] = 1;
    raw.control_chars[nix::sys::termios::SpecialCharacterIndices::VTIME as usize] = 0;

    termios::tcsetattr(&stdin, SetArg::TCSANOW, &raw).map_err(io::Error::other)?;
    Ok(original)
}

fn restore_terminal(original: &Termios) {
    let stdin = io::stdin();
    let _ = termios::tcsetattr(&stdin, SetArg::TCSANOW, original);
}

/// Write bytes to stdout via raw libc::write, bypassing Rust's LineWriter.
/// Performs userspace ONLCR: converts bare \n (not preceded by \r) to \r\n.
/// Tracks cross-frame state via `last_was_cr` to avoid inserting \r when
/// the previous frame already ended with \r (preventing \r\r\n).
/// Retries on EINTR to avoid dropping bytes on SIGWINCH.
fn raw_write_stdout(data: &[u8], last_was_cr: &mut bool) {
    // Fast path: no \n in data, write directly. Still update cross-frame state.
    if !data.contains(&b'\n') {
        raw_write_bytes(data);
        if let Some(&last) = data.last() {
            *last_was_cr = last == b'\r';
        }
        return;
    }

    // Scan for bare \n and convert to \r\n
    let mut buf = Vec::with_capacity(data.len() + 32);
    for (i, &byte) in data.iter().enumerate() {
        if byte == b'\n' {
            let prev_is_cr = if i == 0 {
                *last_was_cr
            } else {
                data[i - 1] == b'\r'
            };
            if !prev_is_cr {
                buf.push(b'\r');
            }
        }
        buf.push(byte);
    }
    *last_was_cr = data.last() == Some(&b'\r');
    raw_write_bytes(&buf);
}

/// Raw write to stdout with EINTR retry.
fn raw_write_bytes(data: &[u8]) {
    let mut written = 0;
    while written < data.len() {
        let n = unsafe {
            libc::write(
                libc::STDOUT_FILENO,
                data[written..].as_ptr() as *const libc::c_void,
                data.len() - written,
            )
        };
        if n > 0 {
            written += n as usize;
        } else if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue; // EINTR — retry, don't drop bytes
            }
            break; // Real error
        } else {
            break; // write returned 0
        }
    }
}

/// Draw a one-shot overlay on the alt screen for watchers.
/// Shows detach/refresh hint on the bottom row, and size mismatch warning above it.
fn draw_watcher_overlay(session_name: &str, session_size: Option<(u16, u16)>) {
    let (term_cols, term_rows) = get_terminal_size();
    let term_rows = term_rows.max(1);
    // \x1b7 = save cursor, \x1b8 = restore cursor
    let mut overlay = String::from("\x1b7");

    // Size mismatch warning on row above
    if let Some((session_cols, session_rows)) = session_size {
        if term_rows > 1 && (session_cols != term_cols || session_rows != term_rows) {
            overlay.push_str(&format!(
                "\x1b[{};1H\x1b[2K\x1b[33;2m[Session is {}x{}, your terminal is {}x{}]\x1b[0m",
                term_rows - 1,
                session_cols,
                session_rows,
                term_cols,
                term_rows,
            ));
        }
    }

    // Status bar on bottom row
    let size_str = if let Some((c, r)) = session_size {
        format!(" ({}x{})", c, r)
    } else {
        String::new()
    };
    overlay.push_str(&format!(
        "\x1b[{};1H\x1b[2K\x1b[7m[{}{}] q detach | r refresh\x1b[0m\x1b8",
        term_rows, session_name, size_str
    ));
    raw_write_bytes(overlay.as_bytes());
}

/// Pack cols/rows into a u64 for deduplication. 0 = invalid.
fn pack_resize(cols: u16, rows: u16) -> u64 {
    ((cols as u64) << 16) | (rows as u64)
}

pub fn run(session_id: &str, watch_mode: bool) -> io::Result<()> {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 {
        return Err(io::Error::other(
            "stdin is not a terminal — must be run from an interactive terminal",
        ));
    }

    let socket_path = protocol::socket_path(session_id);
    let mut sock = UnixStream::connect(&socket_path).map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("Session '{}' is not running", session_id),
        )
    })?;

    // Send Hello
    let (cols, rows) = get_terminal_size();
    let role = if watch_mode {
        Role::Watcher
    } else {
        Role::Writer
    };
    let hello = Frame::new(MsgKind::Hello, protocol::encode_hello(role, cols, rows));
    hello.write_to(&mut sock)?;

    let mut watcher_session_size: Option<(u16, u16)> = None;

    // Wait for HelloAck
    match Frame::read_from(&mut sock)? {
        Some(frame) if frame.kind == MsgKind::HelloAck => {
            if let Some((_, broker_cols, broker_rows)) = protocol::decode_hello_ack(&frame.payload)
            {
                if watch_mode {
                    watcher_session_size = Some((broker_cols, broker_rows));
                }
            }
        }
        Some(frame) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Expected HelloAck, got {:?}", frame.kind),
            ));
        }
        None => {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Broker closed before HelloAck",
            ));
        }
    }

    if !watch_mode {
        eprintln!("[Attached. Type ,,,d to detach.]");
    }

    // Switch to alternate screen buffer. Ink/Claude Code uses cursor-addressed
    // rendering — main screen scrollback corrupts the layout. Alt screen gives
    // a clean independent canvas.
    {
        let mut stdout = io::stdout();
        let _ = stdout.write_all(b"\x1b[?1049h");
        let _ = stdout.flush();
    }

    let original_termios = enter_raw_mode()?;
    let shutdown = Arc::new(AtomicBool::new(false));

    // Watcher overlay: draw after a delay so the broker's nudge re-render completes first.
    let session_id_owned = session_id.to_string();
    if watch_mode {
        let shutdown_overlay = shutdown.clone();
        let name = session_id_owned.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(WATCHER_OVERLAY_DELAY_MS));
            if shutdown_overlay.load(Ordering::Relaxed) {
                return;
            }
            draw_watcher_overlay(&name, watcher_session_size);
        });
    }

    // Resize pipe for SIGWINCH
    let (resize_read, resize_write) = nix::unistd::pipe().map_err(io::Error::other)?;
    let resize_read_fd = resize_read.as_raw_fd();
    let resize_write_fd = resize_write.as_raw_fd();

    // Make write end non-blocking to prevent signal handler deadlock under resize storms
    unsafe {
        let flags = libc::fcntl(resize_write_fd, libc::F_GETFL);
        libc::fcntl(resize_write_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    unsafe {
        RESIZE_PIPE_FD.store(resize_write_fd, Ordering::SeqCst);
        libc::signal(
            libc::SIGWINCH,
            sigwinch_handler as *const () as libc::sighandler_t,
        );
    }

    // Reader thread: broker output -> raw relay to stdout
    let sock_reader = sock.try_clone()?;
    let shutdown_reader = shutdown.clone();
    let original_termios_clone = original_termios.clone();
    let reader_handle = std::thread::spawn(move || {
        let mut reader = sock_reader;
        let mut last_was_cr = false;

        loop {
            if shutdown_reader.load(Ordering::Relaxed) {
                break;
            }

            match Frame::read_from(&mut reader) {
                Ok(Some(frame)) => match frame.kind {
                    MsgKind::Output => {
                        raw_write_stdout(&frame.payload, &mut last_was_cr);
                        std::thread::sleep(Duration::from_micros(50));
                    }
                    MsgKind::Exit => {
                        let code = protocol::decode_exit(&frame.payload).unwrap_or(-1);
                        restore_terminal(&original_termios_clone);
                        eprintln!("\r\n[Session exited with code {}]", code);
                        shutdown_reader.store(true, Ordering::Relaxed);
                        break;
                    }
                    MsgKind::Ping => {
                        let pong = Frame::new(MsgKind::Pong, vec![]);
                        let _ = pong.write_to(&mut reader);
                    }
                    _ => {}
                },
                Ok(None) => {
                    restore_terminal(&original_termios_clone);
                    eprintln!("\r\n[Session disconnected]");
                    shutdown_reader.store(true, Ordering::Relaxed);
                    break;
                }
                Err(_) => {
                    restore_terminal(&original_termios_clone);
                    eprintln!("\r\n[Session disconnected]");
                    shutdown_reader.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
    });

    // Main thread: stdin + resize -> broker (with resize debounce)
    let mut stdin = io::stdin().lock();
    let mut buf = [0u8; 4096];
    let stdin_fd = libc::STDIN_FILENO;

    unsafe {
        let flags = libc::fcntl(stdin_fd, libc::F_GETFL);
        libc::fcntl(stdin_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    let mut debounce: Option<(Instant, u16, u16)> = None;
    let mut last_sent_dims = pack_resize(cols, rows);
    // Writer detach state machine: type ,,,d (three commas then d) to detach.
    // Why not Ctrl+key? Modern TUIs (Claude Code, Codex CLI) enable kitty keyboard
    // protocol / CSI-u, which encodes ALL Ctrl+keys as escape sequences instead of
    // raw bytes. No Ctrl+letter reliably reaches us as a single byte.
    // Why ,,,d? Printable chars always arrive as raw bytes regardless of keyboard
    // protocol. Triple comma + d is impossible to trigger accidentally in normal text.
    // Input is forwarded normally (no buffering/delay). When the pattern completes,
    // we detach — the forwarded commas don't matter since we're leaving.
    let mut comma_count: u32 = 0;

    while !shutdown.load(Ordering::Relaxed) {
        unsafe {
            let mut readfds: libc::fd_set = std::mem::zeroed();
            libc::FD_SET(stdin_fd, &mut readfds);
            libc::FD_SET(resize_read_fd, &mut readfds);
            let nfds = std::cmp::max(stdin_fd, resize_read_fd) + 1;

            let timeout_us = if let Some((started, _, _)) = &debounce {
                let remaining =
                    RESIZE_DEBOUNCE_MS.saturating_sub(started.elapsed().as_millis() as u64);
                (remaining * 1000).min(100_000) as libc::suseconds_t
            } else {
                100_000
            };

            let mut tv = libc::timeval {
                tv_sec: 0,
                tv_usec: timeout_us,
            };

            let ret = libc::select(
                nfds,
                &mut readfds,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut tv,
            );
            if ret < 0 {
                continue;
            }

            if ret > 0 && libc::FD_ISSET(resize_read_fd, &readfds) {
                let mut drain = [0u8; 64];
                let _ = libc::read(resize_read_fd, drain.as_mut_ptr() as *mut libc::c_void, 64);
                let (new_cols, new_rows) = get_terminal_size();
                let new_dims = pack_resize(new_cols, new_rows);

                // Watchers always send Resize (even if same dims) to trigger
                // re-render nudge. Writers dedup to avoid unnecessary PTY resizes.
                if new_dims != last_sent_dims || watch_mode {
                    last_sent_dims = new_dims;
                    debounce = Some((Instant::now(), new_cols, new_rows));
                }
            }

            // Check debounce expiry — send Resize to broker
            if let Some((started, d_cols, d_rows)) = debounce {
                if started.elapsed().as_millis() as u64 >= RESIZE_DEBOUNCE_MS {
                    let resize =
                        Frame::new(MsgKind::Resize, protocol::encode_resize(d_cols, d_rows));
                    if resize.write_to(&mut sock).is_err() {
                        break;
                    }
                    debounce = None;
                }
            }

            if ret > 0 && libc::FD_ISSET(stdin_fd, &readfds) {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let data = &buf[..n];

                        // Detach / redraw detection:
                        // - Watcher: q or Ctrl+C (0x03) detaches, r triggers Resize nudge
                        //   q is the primary detach key because Ctrl+C is unreliable when
                        //   the child enables kitty keyboard protocol (CSI-u encodes it
                        //   as \x1b[99;5u instead of 0x03).
                        // - Writer: ,,,d state machine — immune to CSI-u/kitty encoding
                        if watch_mode && (data.contains(&0x03) || data.contains(&b'q')) {
                            break;
                        }
                        if watch_mode && data.contains(&b'r') {
                            let (r_cols, r_rows) = get_terminal_size();
                            let resize = Frame::new(
                                MsgKind::Resize,
                                protocol::encode_resize(r_cols, r_rows),
                            );
                            if resize.write_to(&mut sock).is_err() {
                                break;
                            }
                            // Redraw overlay after the nudge re-render
                            let name = session_id_owned.clone();
                            let sd = shutdown.clone();
                            std::thread::spawn(move || {
                                std::thread::sleep(Duration::from_millis(WATCHER_OVERLAY_DELAY_MS));
                                if !sd.load(Ordering::Relaxed) {
                                    draw_watcher_overlay(&name, watcher_session_size);
                                }
                            });
                        }
                        if !watch_mode {
                            let mut should_detach = false;
                            for &byte in data.iter() {
                                if byte == b',' {
                                    comma_count += 1;
                                } else if byte == b'd' && comma_count >= 3 {
                                    should_detach = true;
                                    break;
                                } else {
                                    comma_count = 0;
                                }
                            }
                            if should_detach {
                                break;
                            }
                        }

                        let input = Frame::new(MsgKind::Input, data.to_vec());
                        if input.write_to(&mut sock).is_err() {
                            break;
                        }
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => break,
                }
            }
        }
    }

    // Cleanup
    shutdown.store(true, Ordering::Relaxed);

    unsafe {
        let flags = libc::fcntl(stdin_fd, libc::F_GETFL);
        libc::fcntl(stdin_fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
    }

    // Reset signal pipe fd before dropping to prevent signal handler writing to closed fd
    RESIZE_PIPE_FD.store(-1, Ordering::SeqCst);
    drop(resize_read);
    drop(resize_write);

    let _ = sock.shutdown(std::net::Shutdown::Both);
    let _ = reader_handle.join();

    restore_terminal(&original_termios);

    // Switch back to main screen buffer
    {
        let mut stdout = io::stdout();
        let _ = stdout.write_all(b"\x1b[?1049l");
        let _ = stdout.flush();
    }

    Ok(())
}

static RESIZE_PIPE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn sigwinch_handler(_: libc::c_int) {
    let fd = RESIZE_PIPE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        unsafe {
            let _ = libc::write(fd, b"R".as_ptr() as *const libc::c_void, 1);
        }
    }
}
