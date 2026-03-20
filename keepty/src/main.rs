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
use std::process::Command;
use std::time::Duration;

fn print_usage() {
    eprintln!("keepty — persistent terminal sessions");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  keepty start <session-id> [-- <command...>]");
    eprintln!("  keepty attach <session-id> [--watch]");
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

    // Detach: redirect stdio, spawn in background
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

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
            let alive = std::os::unix::net::UnixStream::connect(&path).is_ok();
            sessions.push((id.to_string(), alive));
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
