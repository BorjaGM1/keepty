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
    eprintln!("  keepty screen <id> [--cols N --rows N]");
    eprintln!(
        "  keepty send <id> <text> [--submit] [--submit-delay MS (default 1000, minimum 50)]"
    );
    eprintln!("  keepty keys <id> <name>...");
    eprintln!("  keepty wait <id> (--text STR | --quiet MS) [--timeout SECS (default 30)]");
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

    let sock = socket_path(session_id);
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
    if command == "wait" && text.is_none() && quiet.is_none() {
        return Err(invalid());
    }
    if delay < 50
        || quiet == Some(0)
        || (command == "wait" && text.as_ref().is_some_and(|t| t.is_empty()))
    {
        return Err(invalid());
    }
    let role = if command == "send" || command == "keys" {
        Role::Writer
    } else {
        Role::Monitor
    };
    if role == Role::Writer {
        let monitor = ScreenReader::connect(&socket_path(id), Role::Monitor)?;
        (rows, cols) = monitor.size();
    }
    let mut reader = ScreenReader::connect_with_size(&socket_path(id), role, cols, rows)?;
    let started = Instant::now();
    match command {
        "screen" => {
            // Drain additional replay frames, with a bounded window for live output.
            match reader.poll(Duration::from_millis(20)) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::ConnectionAborted => return Ok(3),
                Err(e) => return Err(e),
            }
            for line in reader.contents().lines() {
                println!("{}", line.trim_end());
            }
        }
        "send" => {
            let text = text.unwrap();
            reader.poll(Duration::from_millis(30))?;
            let payload = if (text.contains('\n') || text.contains('\r'))
                && reader.screen().bracketed_paste()
            {
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
        }
        "keys" => {
            reader.poll(Duration::from_millis(30))?;
            for (name, key) in args[1..].iter().zip(keys) {
                let key = if reader.screen().application_cursor() {
                    named_key(name, true).unwrap()
                } else {
                    key
                };
                reader.send_keys(&key)?;
            }
        }
        "wait" => {
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
                if matched || text.as_ref().is_some_and(|t| reader.contents().contains(t)) {
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
                match reader.poll_with(interval, |reader| {
                    matched |= text.as_ref().is_some_and(|t| reader.contents().contains(t));
                    let current = normalize(reader.contents());
                    if current != previous {
                        previous = current;
                        changed = Instant::now();
                    }
                }) {
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::ConnectionAborted => {
                        return Ok(
                            if matched
                                || text.as_ref().is_some_and(|t| reader.contents().contains(t))
                            {
                                0
                            } else {
                                3
                            },
                        );
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
