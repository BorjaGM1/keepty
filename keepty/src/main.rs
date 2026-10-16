//! keepty — persistent terminal sessions without the terminal-inside-a-terminal.
//!
//! Usage:
//!   keepty start <session-id> [options] -- <command...>
//!   keepty attach <session-id> [--watch]
//!   keepty list
//!   keepty stop <session-id>

mod attach;
mod broker;

use keepty_protocol::socket_path;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::Duration;

fn print_usage() {
    eprintln!("keepty — persistent terminal sessions");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  keepty start <session-id> [-- <command...>]");
    eprintln!("  keepty attach <session-id> [--watch]");
    eprintln!("  keepty screen <id> [--cols N --rows N] [--scrollback [N]]");
    eprintln!(
        "  keepty send <id> <text> [--submit] [--submit-delay MS (default 1000, minimum 50)]"
    );
    eprintln!("  keepty keys <id> <name>...");
    eprintln!("  keepty wait <id> (--text STR [--new] | --quiet MS) [--timeout SECS (default 30)]");
    eprintln!("  keepty list");
    eprintln!("  keepty stop <session-id>");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  keepty start my-session                    # starts your shell");
    eprintln!("  keepty start my-session -- vim project.rs  # starts a specific command");
    eprintln!("  keepty attach my-session");
    eprintln!("  keepty attach --watch my-session");
    eprintln!("  keepty list");
    eprintln!("  keepty stop my-session");
}

fn cmd_start(args: &[String]) -> i32 {
    let mut session_id = None;
    let mut working_dir = None;
    let mut cols: u16 = 80;
    let mut rows: u16 = 24;
    let mut command_parts: Vec<String> = Vec::new();
    let mut after_separator = false;
    let mut detach = false;

    let mut i = 0;
    while i < args.len() {
        if after_separator {
            command_parts.push(args[i].clone());
            i += 1;
            continue;
        }
        match args[i].as_str() {
            "--" => after_separator = true,
            "--detach" | "-d" => detach = true,
            "--working-dir" => {
                i += 1;
                working_dir = args.get(i).cloned();
            }
            "--cols" => {
                i += 1;
                cols = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(80);
            }
            "--rows" => {
                i += 1;
                rows = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(24);
            }
            "--help" | "-h" => {
                eprintln!("Usage: keepty start <session-id> [options] [-- <command...>]");
                eprintln!();
                eprintln!("Starts a persistent session and attaches as writer.");
                eprintln!("If no command is given, starts your shell.");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --detach, -d         Start without attaching (background only)");
                eprintln!("  --working-dir <DIR>  Working directory for the command");
                eprintln!("  --cols <N>           Terminal columns (default: current terminal)");
                eprintln!("  --rows <N>           Terminal rows (default: current terminal)");
                return 0;
            }
            arg if !arg.starts_with('-') && session_id.is_none() => {
                session_id = Some(arg.to_string());
            }
            other => {
                eprintln!("Unknown option: {}", other);
                return 1;
            }
        }
        i += 1;
    }

    let session_id = match session_id {
        Some(id) => id,
        None => {
            eprintln!("Error: session ID required");
            eprintln!("Usage: keepty start <session-id> -- <command...>");
            return 1;
        }
    };

    // Default to user's shell if no command given
    if command_parts.is_empty() {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        command_parts.push(shell);
    }

    // Check if session already exists
    let sock = socket_path(&session_id);
    if std::path::Path::new(&sock).exists()
        && std::os::unix::net::UnixStream::connect(&sock).is_ok()
    {
        eprintln!("Session '{}' is already running", session_id);
        return 1;
    }

    // Get terminal size from current terminal if available
    let (term_cols, term_rows) = get_terminal_size();
    if cols == 80 && term_cols != 80 {
        cols = term_cols;
    }
    if rows == 24 && term_rows != 24 {
        rows = term_rows;
    }

    // Spawn broker as background process, passing argv directly (no sh -c)
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: cannot determine executable path: {}", e);
            return 1;
        }
    };

    let mut cmd = Command::new(exe);
    cmd.args(["broker", "--session-id", &session_id]);
    cmd.args(["--cols", &cols.to_string(), "--rows", &rows.to_string()]);

    // Always pass working directory — default to user's current directory.
    // This ensures programs like Claude Code detect project context (CLAUDE.md, etc.)
    let effective_dir = working_dir.unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string())
    });
    cmd.args(["--working-dir", &effective_dir]);
    cmd.arg("--");
    cmd.args(&command_parts);

    // Broker is a long-lived daemon. Put it in its own session so
    // shell/job-control hangups (SIGHUP) do not kill it.
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    match cmd.spawn() {
        Ok(_child) => {
            // Wait for socket to appear
            let mut socket_ready = false;
            for i in 0..50 {
                if std::path::Path::new(&sock).exists() {
                    socket_ready = true;
                    break;
                }
                if i == 10 {
                    eprint!("Waiting for broker...");
                }
                if i > 10 {
                    eprint!(".");
                }
                std::thread::sleep(Duration::from_millis(100));
            }

            if !socket_ready {
                eprintln!();
                eprintln!("Warning: broker started but socket not yet available");
                eprintln!("Socket path: {}", sock);
                return 0;
            }

            // Auto-attach as writer if stdin is a TTY and --detach not set
            let is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) } != 0;
            if !detach && is_tty {
                eprintln!("Session '{}' started", session_id);
                if let Err(e) = attach::run(&session_id, false) {
                    eprintln!("Attach error: {}", e);
                    eprintln!(
                        "Session is still running. Re-attach: keepty attach {}",
                        session_id
                    );
                    return 1;
                }
            } else {
                eprintln!("Session '{}' started (detached)", session_id);
                eprintln!("  Attach: keepty attach {}", session_id);
                eprintln!("  Watch:  keepty attach --watch {}", session_id);
                eprintln!("  Stop:   keepty stop {}", session_id);
            }
            0
        }
        Err(e) => {
            eprintln!("Error: failed to start broker: {}", e);
            1
        }
    }
}

fn cmd_attach(args: &[String]) -> i32 {
    let mut session_id = None;
    let mut watch_mode = false;

    for arg in args {
        match arg.as_str() {
            "--watch" | "-w" => watch_mode = true,
            "--help" | "-h" => {
                eprintln!("Usage: keepty attach <session-id> [--watch]");
                return 0;
            }
            a if !a.starts_with('-') => session_id = Some(a.to_string()),
            other => {
                eprintln!("Unknown option: {}", other);
                return 1;
            }
        }
    }

    let session_id = match session_id {
        Some(id) => id,
        None => {
            eprintln!("Usage: keepty attach <session-id> [--watch]");
            return 1;
        }
    };

    if let Err(e) = attach::run(&session_id, watch_mode) {
        eprintln!("Error: {}", e);
        return 1;
    }
    0
}

fn cmd_list() -> i32 {
    let socket_dir = keepty_protocol::socket_dir();
    let prefix = "keepty-";
    let suffix = ".sock";

    let entries = match std::fs::read_dir(&socket_dir) {
        Ok(e) => e,
        Err(_) => {
            // Directory doesn't exist = no sessions
            eprintln!("No running sessions");
            return 0;
        }
    };

    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(prefix) && name.ends_with(suffix) {
            let id = &name[prefix.len()..name.len() - suffix.len()];
            let path = entry.path();
            // Only a refused connection proves the listener is gone. Preserve
            // entries on permission errors or transient resource exhaustion.
            match std::os::unix::net::UnixStream::connect(&path) {
                Ok(_) => sessions.push((id.to_string(), true)),
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    use std::os::unix::fs::FileTypeExt;
                    if entry.file_type().is_ok_and(|t| t.is_socket()) {
                        let _ = std::fs::remove_file(&path);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => sessions.push((id.to_string(), false)),
            }
        }
    }

    if sessions.is_empty() {
        eprintln!("No running sessions");
        return 0;
    }

    sessions.sort();
    for (id, alive) in &sessions {
        let status = if *alive { "running" } else { "stale" };
        println!("  {}  ({})", id, status);
    }
    0
}

fn cmd_stop(args: &[String]) -> i32 {
    let session_id = match args.first() {
        Some(id) if !id.starts_with('-') => id,
        _ => {
            eprintln!("Usage: keepty stop <session-id>");
            return 1;
        }
    };

    use std::os::unix::fs::MetadataExt;
    let sock = socket_path(session_id);
    // Snapshot before connect so a replacement during connect/handshake is also
    // recognized as a different endpoint, rather than waited on until timeout.
    let socket_identity =
        std::fs::symlink_metadata(&sock).map(|metadata| (metadata.dev(), metadata.ino()));
    let mut stream = match std::os::unix::net::UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("Session '{}' is not running", session_id);
            return 1;
        }
    };

    // Quick handshake as Monitor (minimal footprint)
    use keepty_protocol::{encode_hello, Frame, MsgKind, Role};
    let hello = Frame::new(MsgKind::Hello, encode_hello(Role::Monitor, 1, 1));
    if let Err(e) = hello.write_to(&mut stream) {
        eprintln!("Error sending handshake: {}", e);
        return 1;
    }

    // Read HelloAck
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    match Frame::read_from(&mut stream) {
        Ok(Some(f)) if f.kind == MsgKind::HelloAck => {}
        _ => {
            eprintln!("Error: broker did not respond");
            return 1;
        }
    }

    // Send Shutdown
    let shutdown = Frame::new(MsgKind::Shutdown, vec![]);
    if let Err(e) = shutdown.write_to(&mut stream) {
        eprintln!("Error sending shutdown: {}", e);
        return 1;
    }

    eprintln!("Session '{}' stopping", session_id);

    // Monitor handshakes replay scrollback. Consume it while waiting so this
    // command does not itself keep a drain blocked until the broker's deadline.
    // EOF alone can precede socket cleanup. Wait for the original endpoint to
    // disappear or be replaced; a new broker using this ID must not delay stop.
    use std::io::{ErrorKind, Read};
    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    let mut buffer = [0; 64 * 1024];
    let mut disconnected = false;
    loop {
        match std::fs::symlink_metadata(&sock) {
            Err(e) if e.kind() == ErrorKind::NotFound => break,
            Ok(metadata) => {
                if let Ok(identity) = &socket_identity {
                    if (metadata.dev(), metadata.ino()) != *identity {
                        break;
                    }
                }
            }
            Err(_) => {}
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            eprintln!(
                "Warning: session '{}' did not stop within 6 seconds",
                session_id
            );
            break;
        }
        let interval = remaining.min(Duration::from_millis(50));
        if disconnected {
            std::thread::sleep(interval);
        } else {
            let _ = stream.set_read_timeout(Some(interval));
            match stream.read(&mut buffer) {
                Ok(0) => disconnected = true,
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) => {}
                Err(_) => disconnected = true,
            }
        }
    }
    0
}

// Agent commands use ScreenReader so terminal parsing and role negotiation stay shared.
fn cmd_agent(command: &str, args: &[String]) -> i32 {
    match run_agent(command, args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e}");
            if e.kind() == std::io::ErrorKind::InvalidInput {
                2
            } else {
                1
            }
        }
    }
}

fn run_agent(command: &str, args: &[String]) -> std::io::Result<i32> {
    use keepty_screen::{Role, ScreenReader};
    use std::io::{Error, ErrorKind};
    use std::time::Instant;
    let invalid = || {
        Error::new(
            ErrorKind::InvalidInput,
            "invalid arguments (see keepty --help)",
        )
    };
    let id = args
        .first()
        .filter(|s| !s.starts_with('-'))
        .ok_or_else(invalid)?;
    let mut cols = 80u16;
    let mut rows = 24u16;
    let mut submit = false;
    let mut delay = 1000u64;
    let mut timeout = 30u64;
    let mut text = None;
    let mut quiet = None;
    let mut history = None;
    let mut new_text = false;
    let mut keys = Vec::new();
    if command == "keys" {
        if args.len() < 2 {
            return Err(invalid());
        }
        // Validate every name before connecting or sending any input.
        for name in &args[1..] {
            keys.push(named_key(name, false).ok_or_else(|| {
                Error::new(ErrorKind::InvalidInput, format!("unknown key: {name}"))
            })?);
        }
    } else {
        let mut i = if command == "send" {
            text = Some(args.get(1).ok_or_else(invalid)?.clone());
            2
        } else {
            1
        };
        while i < args.len() {
            let option = args[i].as_str();
            if option == "--submit" && command == "send" {
                submit = true;
            } else if option == "--scrollback" && command == "screen" {
                history = Some(1000);
                if let Some(value) = args.get(i + 1).filter(|s| !s.starts_with("--")) {
                    history = Some(value.parse::<usize>().map_err(|_| invalid())?);
                    i += 1;
                }
            } else if option == "--new" && command == "wait" {
                new_text = true;
            } else {
                i += 1;
                let value = args.get(i).ok_or_else(invalid)?;
                match (command, option) {
                    ("screen", "--cols") => {
                        cols = value.parse().ok().filter(|n| *n > 0).ok_or_else(invalid)?
                    }
                    ("screen", "--rows") => {
                        rows = value.parse().ok().filter(|n| *n > 0).ok_or_else(invalid)?
                    }
                    ("send", "--submit-delay") => delay = value.parse().map_err(|_| invalid())?,
                    ("wait", "--timeout") => timeout = value.parse().map_err(|_| invalid())?,
                    ("wait", "--text") if text.is_none() && quiet.is_none() => {
                        text = Some(value.clone())
                    }
                    ("wait", "--quiet") if text.is_none() && quiet.is_none() => {
                        quiet = Some(value.parse::<u64>().map_err(|_| invalid())?)
                    }
                    _ => return Err(invalid()),
                }
            }
            i += 1;
        }
    }
    if command == "wait" && ((text.is_none() && quiet.is_none()) || (new_text && text.is_none())) {
        return Err(invalid());
    }
    if delay < 50
        || quiet == Some(0)
        || (command == "wait" && text.as_ref().is_some_and(|t| t.is_empty()))
    {
        return Err(invalid());
    }
    let started = Instant::now();
    let role = if command == "send" || command == "keys" {
        Role::Writer
    } else {
        Role::Monitor
    };
    if role == Role::Writer {
        let monitor = ScreenReader::connect(&socket_path(id), Role::Monitor)?;
        (rows, cols) = monitor.size();
    }
    let connection_timeout = if command == "wait" {
        Duration::from_secs(timeout).saturating_sub(started.elapsed())
    } else {
        Duration::from_secs(5)
    };
    let mut reader = match ScreenReader::connect_with_size_history_timeout(
        &socket_path(id),
        role,
        cols,
        rows,
        history.unwrap_or(0),
        connection_timeout,
    ) {
        Ok(reader) => reader,
        Err(e) if command == "wait" && e.kind() == ErrorKind::TimedOut => return Ok(1),
        Err(e) => return Err(e),
    };
    match command {
        "screen" => {
            // Synchronize history replay; preserve the default screen polling behavior.
            let replay = if history.is_some() {
                reader.sync(Duration::from_secs(5))
            } else {
                reader.poll(Duration::from_millis(20)).map(|_| ())
            };
            match replay {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::ConnectionAborted => return Ok(3),
                Err(e) => return Err(e),
            }
            if history.is_some() {
                println!("{}", reader.history_contents());
            } else {
                for line in reader.contents().lines() {
                    println!("{}", line.trim_end());
                }
            }
        }
        "send" => {
            let text = text.unwrap();
            reader.sync(Duration::from_secs(5))?;
            let payload = if reader.screen().bracketed_paste() {
                format!("\x1b[200~{text}\x1b[201~")
            } else {
                text
            };
            reader.send_keys(payload.as_bytes())?;
            if submit {
                let sent = Instant::now();
                while sent.elapsed() < Duration::from_millis(delay) {
                    reader.poll(
                        Duration::from_millis(delay)
                            .saturating_sub(sent.elapsed())
                            .min(Duration::from_millis(20)),
                    )?;
                }
                reader.send_keys(b"\r")?;
            }
            // Input frames have been written; the child may exit before Pong.
            return match reader.sync(Duration::from_secs(5)) {
                Ok(()) => Ok(0),
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::ConnectionReset
                            | ErrorKind::ConnectionAborted
                            | ErrorKind::BrokenPipe
                    ) =>
                {
                    Ok(3)
                }
                Err(e) => Err(e),
            };
        }
        "keys" => {
            reader.sync(Duration::from_secs(5))?;
            for (name, key) in args[1..].iter().zip(keys) {
                let key = if reader.screen().application_cursor() {
                    named_key(name, true).unwrap()
                } else {
                    key
                };
                reader.send_keys(&key)?;
            }
            // Input frames have been written; the child may exit before Pong.
            return match reader.sync(Duration::from_secs(5)) {
                Ok(()) => Ok(0),
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::ConnectionReset
                            | ErrorKind::ConnectionAborted
                            | ErrorKind::BrokenPipe
                    ) =>
                {
                    Ok(3)
                }
                Err(e) => Err(e),
            };
        }
        "wait" => {
            if new_text {
                let remaining = Duration::from_secs(timeout).saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Ok(1);
                }
                match reader.sync(remaining) {
                    Ok(()) => {}
                    Err(e) if e.kind() == ErrorKind::TimedOut => return Ok(1),
                    Err(e) if e.kind() == ErrorKind::ConnectionAborted => return Ok(3),
                    Err(e) => return Err(e),
                }
                if std::env::var_os("KEEPTY_DEBUG_ARMED").is_some() {
                    eprintln!("armed");
                }
            }
            let mut raw = RawText::default();
            let mut painted = new_text.then(|| PaintedCells::new(reader.screen()));
            let text_matches = |reader: &ScreenReader| {
                !new_text
                    && text
                        .as_ref()
                        .is_some_and(|needle| reader.contents().contains(needle))
            };
            let normalize = |s: String| {
                s.chars()
                    .map(|c| {
                        if ('\u{2800}'..='\u{28ff}').contains(&c) {
                            ' '
                        } else {
                            c
                        }
                    })
                    .collect::<String>()
            };
            let mut previous = normalize(reader.contents());
            let mut changed = Instant::now();
            let mut matched = false;
            loop {
                if matched || text_matches(&reader) {
                    return Ok(0);
                }
                if quiet.is_some_and(|ms| changed.elapsed() >= Duration::from_millis(ms)) {
                    return Ok(0);
                }
                let remaining = Duration::from_secs(timeout).saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Ok(1);
                }
                let mut interval = remaining.min(Duration::from_millis(20));
                if let Some(ms) = quiet {
                    interval =
                        interval.min(Duration::from_millis(ms).saturating_sub(changed.elapsed()));
                }
                match reader.poll_with_raw(interval, |reader, output| {
                    if new_text {
                        matched |= raw.feed(output, text.as_ref().unwrap().as_bytes());
                        matched |= painted.as_mut().unwrap().feed(
                            output,
                            reader.screen(),
                            text.as_ref().unwrap(),
                        );
                    }
                    matched |= text_matches(reader);
                    let current = normalize(reader.contents());
                    if current != previous {
                        previous = current;
                        changed = Instant::now();
                    }
                }) {
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::ConnectionAborted => {
                        return Ok(if matched || text_matches(&reader) {
                            0
                        } else {
                            3
                        });
                    }
                    Err(e) => return Err(e),
                }
                let current = normalize(reader.contents());
                if current != previous {
                    previous = current;
                    changed = Instant::now();
                }
            }
        }
        _ => unreachable!(),
    }
    Ok(0)
}

// The shadow starts with no pre-arm text, but retains the terminal's state.
// Clone through screen_mut rather than serializing away terminal modes.
struct PaintedCells {
    shadow: vt100_screen::Parser,
    escape: bool,
}

impl PaintedCells {
    const SENTINEL: char = '\u{e000}';

    fn new(screen: &vt100_screen::Screen) -> Self {
        let (rows, cols) = screen.size();
        let mut shadow = vt100_screen::Parser::new(rows, cols, 0);
        *shadow.screen_mut() = screen.clone();
        // Save/restore also preserves origin mode and the pending-wrap cursor.
        shadow.process(b"\x1b7\x1b[?6l");
        for row in 0..rows {
            let fill = format!(
                "\x1b[{};1H{}",
                row + 1,
                Self::SENTINEL.to_string().repeat(cols.into())
            );
            shadow.process(fill.as_bytes());
        }
        shadow.process(b"\x1b8");
        Self {
            shadow,
            escape: false,
        }
    }

    fn feed(&mut self, bytes: &[u8], live: &vt100_screen::Screen, needle: &str) -> bool {
        // Resizing cannot expose old text as newly painted.
        if self.shadow.screen().size() != live.size() {
            *self = Self::new(live);
            return false;
        }
        // Filling uses DEC save/restore. Its saved-cursor slot is private in
        // vt100, so conservatively re-arm after any subsequent DEC restore.
        // Keep escape detection across Output frames as well.
        let mut restored = false;
        for &byte in bytes {
            restored |= self.escape && byte == b'8';
            self.escape = byte == 0x1b;
        }
        let switched = self.shadow.screen().alternate_screen() != live.alternate_screen();
        self.shadow.process(bytes);
        if restored || switched {
            *self = Self::new(live);
            return false;
        }
        let (rows, cols) = live.size();
        for row in 0..rows {
            let mut run = String::new();
            let mut gap = 0;
            for col in 0..cols {
                let cell = self.shadow.screen().cell(row, col).unwrap();
                let content = cell.contents();
                if content.contains(Self::SENTINEL) {
                    let live_cell = live.cell(row, col).unwrap();
                    if !run.is_empty()
                        && !live_cell.is_wide_continuation()
                        && (live_cell.contents().is_empty() || live_cell.contents() == " ")
                    {
                        gap += 1;
                    } else {
                        run.clear();
                        gap = 0;
                    }
                } else {
                    // If a terminal transition diverged, prefer a missed match
                    // to reporting text that is not on the live screen.
                    let live_cell = live.cell(row, col).unwrap();
                    if content != live_cell.contents()
                        || cell.is_wide_continuation() != live_cell.is_wide_continuation()
                    {
                        run.clear();
                        gap = 0;
                        continue;
                    }
                    // Only internal blank gaps are eligible; a trailing gap is
                    // never searched before another painted cell arrives.
                    run.extend(std::iter::repeat_n(' ', gap));
                    gap = 0;
                    if content.is_empty() && !cell.is_wide_continuation() {
                        run.push(' ');
                    } else {
                        run.push_str(content);
                    }
                    if run.contains(needle) {
                        return true;
                    }
                }
            }
        }
        false
    }
}

// Stateful across Output frames, including escape sequences and UTF-8 bytes.
#[derive(Default)]
struct RawText {
    state: u8,
    tail: Vec<u8>,
}

impl RawText {
    fn feed(&mut self, bytes: &[u8], needle: &[u8]) -> bool {
        let mut matched = false;
        for &b in bytes {
            match self.state {
                0 => match b {
                    0x1b => self.state = 1,
                    b'\r' | b'\n' | 0x08 | 0x09 => self.tail.push(b'\n'),
                    0..=0x1f | 0x7f => {}
                    _ => self.tail.push(b),
                },
                1 => match b {
                    b'[' => self.state = 2,
                    b']' => self.state = 3,
                    b'P' | b'X' | b'^' | b'_' => self.state = 4,
                    0x20..=0x2f => self.state = 6, // charset / intermediate ESC
                    _ => {
                        // Save/restore cursor, index, next line, reverse index, reset.
                        if matches!(b, b'7' | b'8' | b'D' | b'E' | b'M' | b'c') {
                            self.tail.push(b'\n');
                        }
                        self.state = 0;
                    }
                },
                2 => {
                    if (0x40..=0x7e).contains(&b) {
                        // SGR and mode switches do not interrupt adjacent text.
                        if !matches!(b, b'm' | b'h' | b'l') {
                            self.tail.push(b'\n');
                        }
                        self.state = 0;
                    }
                }
                3 | 4 => {
                    if b == 0x1b {
                        self.state += 4;
                    } else if b == 7 && self.state == 3 {
                        self.state = 0;
                    }
                }
                6 => {
                    if (0x30..=0x7e).contains(&b) {
                        self.state = 0;
                    }
                }
                7 | 8 => {
                    if b == b'\\' {
                        self.state = 0;
                    } else if b != 0x1b {
                        self.state -= 4;
                    }
                }
                _ => unreachable!(),
            }
            matched |= self.tail.ends_with(needle);
            let limit = needle.len().saturating_add(256);
            if self.tail.len() > limit {
                self.tail.drain(..self.tail.len() - limit);
            }
        }
        matched
    }
}

fn named_key(name: &str, application: bool) -> Option<Vec<u8>> {
    let normalized = name.to_ascii_lowercase();
    let name = match normalized.as_str() {
        "return" => "enter",
        "escape" => "esc",
        "page-up" => "pageup",
        "page-down" => "pagedown",
        name => name,
    };
    let bytes: &[u8] = match name {
        "enter" => b"\r",
        "tab" => b"\t",
        "esc" => b"\x1b",
        "backspace" => b"\x7f",
        "space" => b" ",
        "delete" => b"\x1b[3~",
        "pageup" => b"\x1b[5~",
        "pagedown" => b"\x1b[6~",
        "up" => {
            if application {
                b"\x1bOA"
            } else {
                b"\x1b[A"
            }
        }
        "down" => {
            if application {
                b"\x1bOB"
            } else {
                b"\x1b[B"
            }
        }
        "right" => {
            if application {
                b"\x1bOC"
            } else {
                b"\x1b[C"
            }
        }
        "left" => {
            if application {
                b"\x1bOD"
            } else {
                b"\x1b[D"
            }
        }
        "home" => {
            if application {
                b"\x1bOH"
            } else {
                b"\x1b[H"
            }
        }
        "end" => {
            if application {
                b"\x1bOF"
            } else {
                b"\x1b[F"
            }
        }
        "f1" => b"\x1bOP",
        "f2" => b"\x1bOQ",
        "f3" => b"\x1bOR",
        "f4" => b"\x1bOS",
        "f5" => b"\x1b[15~",
        "f6" => b"\x1b[17~",
        "f7" => b"\x1b[18~",
        "f8" => b"\x1b[19~",
        "f9" => b"\x1b[20~",
        "f10" => b"\x1b[21~",
        "f11" => b"\x1b[23~",
        "f12" => b"\x1b[24~",
        _ => {
            let letter = name.strip_prefix("ctrl-")?;
            if letter.len() == 1 && letter.as_bytes()[0].is_ascii_alphabetic() {
                return Some(vec![letter.as_bytes()[0].to_ascii_lowercase() - b'a' + 1]);
            }
            return None;
        }
    };
    Some(bytes.to_vec())
}

fn get_terminal_size() -> (u16, u16) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            (ws.ws_col, ws.ws_row)
        } else {
            (80, 24)
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        print_usage();
        std::process::exit(1);
    }

    let code = match args[1].as_str() {
        "start" => cmd_start(&args[2..]),
        "attach" => cmd_attach(&args[2..]),
        "list" | "ls" => cmd_list(),
        "stop" => cmd_stop(&args[2..]),
        "screen" | "send" | "keys" | "wait" => cmd_agent(&args[1], &args[2..]),
        // Internal: run the broker daemon directly (used by `start` and integration tests)
        "broker" => {
            if let Err(e) = broker::run() {
                eprintln!("Broker error: {}", e);
                std::process::exit(1);
            }
            0
        }
        "--help" | "-h" | "help" => {
            print_usage();
            0
        }
        other => {
            eprintln!("Unknown command: {}", other);
            eprintln!();
            print_usage();
            1
        }
    };

    std::process::exit(code);
}

#[cfg(test)]
mod raw_text_tests {
    use super::RawText;

    #[test]
    fn escapes_and_frames() {
        for decoration in [
            b"\x1b[1m".as_slice(),
            b"\x1b]0;title\x07",
            b"\x1bPignored\x1b\\",
            b"\x1b(B",
            b"\x1b=",
            b"\x00",
        ] {
            let bytes = [b"ans".as_slice(), decoration, b"wer"].concat();
            let mut raw = RawText::default();
            let mut matched = false;
            for b in bytes {
                matched |= raw.feed(&[b], b"answer");
            }
            assert!(matched, "{decoration:?}");
        }
        for boundary in [b"\r".as_slice(), b"\n", b"\x1b[1A", b"\x1bD"] {
            let mut raw = RawText::default();
            assert!(!raw.feed(&[b"ans".as_slice(), boundary, b"wer"].concat(), b"answer"));
        }
        let mut raw = RawText::default();
        raw.feed(&vec![b'x'; 100_000], b"answer");
        assert!(raw.tail.len() <= 262);
    }
}

#[cfg(test)]
mod painted_cell_tests {
    use super::PaintedCells;

    fn check(initial: &[u8], chunks: &[&[u8]], needle: &str) -> bool {
        let mut live = vt100_screen::Parser::new(4, 24, 0);
        live.process(initial);
        let mut painted = PaintedCells::new(live.screen());
        let mut matched = false;
        for bytes in chunks {
            live.process(bytes);
            matched |= painted.feed(bytes, live.screen(), needle);
        }
        matched
    }

    #[test]
    fn moved_text_ignored() {
        for movement in [
            b"\x1b[1;1H\x1b[L".as_slice(),
            b"\x1b[1;1H\x1b[M",
            b"\x1b[4;1H\n",
        ] {
            assert!(!check(b"old\r\nold\r\nold", &[movement], "old"));
        }
    }

    #[test]
    fn cup_out_of_order() {
        assert!(check(
            b"",
            &[b"\x1b[1;7Hworld", b"\x1b[1;1Hhello"],
            "hello world"
        ));
        assert!(check(b"", &[b"hello\x1b[1Cworld"], "hello world"));
    }

    #[test]
    fn overpaint() {
        assert!(check(b"", &[b"hxllo", b"\x1b[1;2He"], "hello"));
        assert!(!check(b"", &[b"hello\x1b[1;2Hx"], "hello"));
    }

    #[test]
    fn nonblank_gap_blocks_joining() {
        assert!(!check(
            b"\x1b[1;6HX",
            &[b"\x1b[1;1Hhello\x1b[1Cworld"],
            "hello world"
        ));
        assert!(!check(b"", &[b"hello\x1b[1C"], "hello "));
    }

    #[test]
    fn rows_not_joined() {
        assert!(!check(b"", &[b"hello\r\nworld"], "helloworld"));
        assert!(!check(b"", &[b"hello\r\nworld"], "hello world"));
    }

    #[test]
    fn spinner() {
        assert!(!check(b"ready", &["\x1b[2;1H⠋\r⠙\r⠹".as_bytes()], "ready"));
        assert!(check(b"", &["⠋\r⠙\r⠹".as_bytes(), b"\rready"], "ready"));
    }

    #[test]
    fn unicode_and_split_output() {
        let bytes = "\x1b[1;4H界\x1b[1;1Hé".as_bytes();
        let chunks: Vec<&[u8]> = bytes.chunks(1).collect();
        assert!(check(b"", &chunks, "é  界"));
    }

    #[test]
    fn scroll_region_origin_and_pending_wrap() {
        assert!(check(
            b"\x1b[2;3r\x1b[?6h",
            &[b"\x1b[1;7Hworld", b"\x1b[1;1Hhello"],
            "hello world"
        ));
        assert!(!check(
            b"abcdefghijklmnopqrstuvwx",
            &[b"hello", b"\x1b[2;7Hworld"],
            "xhello"
        ));
        assert!(check(
            b"abcdefghijklmnopqrstuvwx",
            &[b"hello", b"\x1b[2;7Hworld"],
            "hello world"
        ));
    }

    #[test]
    fn saved_cursor_and_alternate_screen_are_conservative() {
        assert!(!check(b"old\x1b7\x1b[3;1H", &[b"\x1b8"], "old"));
        assert!(!check(
            b"old\x1b[?47hother\x1b[?47l",
            &[b"\x1b[?47h"],
            "other"
        ));
    }
}
