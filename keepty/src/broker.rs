#![allow(clippy::type_complexity)]
//! keepty: persistent PTY broker daemon
//!
//! Spawns a command in a PTY it owns permanently. Clients connect via Unix socket
//! for raw bidirectional I/O using the keepty protocol. The broker survives client
//! disconnects — the process keeps running until it exits or is explicitly shut down.
//!
//! The child process is NOT spawned until the first Writer connects. This ensures
//! that terminal capability probes (DA1, XTVERSION) from TUI apps like Claude Code
//! get answered through the connected client's terminal from the very start.
//!
//! Usage: keepty --session-id <ID> --command <CMD> [--working-dir <DIR>] [--cols N] [--rows N]

use anyhow::{Context, Result};
use keepty_protocol::{
    decode_hello, decode_resize, encode_exit, encode_hello_ack, encode_resize_ack, socket_path,
    Frame, MsgKind, Role,
};
use nix::libc;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, IntoRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;

fn log(msg: &str) {
    eprintln!("[keepty] {}", msg);
}

/// Ring buffer for output history (allows replay on reconnect)
struct RingBuffer {
    buf: Vec<u8>,
    write_pos: usize,
    total_written: u64,
    capacity: usize,
}

impl RingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            buf: vec![0u8; capacity],
            write_pos: 0,
            total_written: 0,
            capacity,
        }
    }

    fn push(&mut self, data: &[u8]) {
        for &byte in data {
            self.buf[self.write_pos] = byte;
            self.write_pos = (self.write_pos + 1) % self.capacity;
        }
        self.total_written += data.len() as u64;
    }

    /// Get recent output (up to capacity bytes)
    fn recent(&self) -> Vec<u8> {
        let available = self.total_written.min(self.capacity as u64) as usize;
        if available == 0 {
            return Vec::new();
        }
        let start = if self.total_written <= self.capacity as u64 {
            0
        } else {
            self.write_pos
        };
        let mut result = Vec::with_capacity(available);
        for i in 0..available {
            result.push(self.buf[(start + i) % self.capacity]);
        }
        result
    }
}

/// Per-client outbound channel sender.
struct ClientSender {
    tx: std::sync::mpsc::SyncSender<Arc<[u8]>>,
    role: Role,
}

/// Combined output state: ring buffer + client senders.
/// One mutex protects both to prevent replay/live duplication.
struct OutputState {
    clients: HashMap<u32, ClientSender>,
    ring: RingBuffer,
}

const MAX_CONNECTIONS: u32 = 32;
const HELLO_TIMEOUT_SECS: u64 = 5;
const CLIENT_QUEUE_BOUND: usize = 64; // ~1MB at 16KB/frame

/// Shared broker state
struct BrokerState {
    output: Mutex<OutputState>,
    writer_id: AtomicU32, // 0 = no writer
    next_client_id: AtomicU32,
    /// Virtual screen for watcher replay. Fed the same bytes as the ring buffer.
    screen: Mutex<vt100::Parser>,
    running: AtomicBool,
    pty_pid: AtomicU32,
    cols: RwLock<u16>,
    rows: RwLock<u16>,
    resize_gen: AtomicU32,
    active_connections: AtomicU32,
}

/// Drain thread: reads from channel, writes to client socket.
/// Exits when channel disconnects or write fails.
fn spawn_client_drain(
    client_id: u32,
    rx: std::sync::mpsc::Receiver<Arc<[u8]>>,
    mut stream: UnixStream,
    state: Arc<BrokerState>,
) -> thread::JoinHandle<()> {
    // Write timeout prevents drain thread from blocking forever on frozen clients
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();
    thread::spawn(move || {
        for data in rx.iter() {
            if stream.write_all(&data).is_err() {
                break;
            }
        }
        // Clean up on disconnect
        let mut out = state.output.lock().unwrap();
        if out.clients.remove(&client_id).is_some() {
            log(&format!("Client {} disconnected (drain)", client_id));
        }
        drop(out);
        let _ = state
            .writer_id
            .compare_exchange(client_id, 0, Ordering::SeqCst, Ordering::SeqCst);
        let _ = stream.shutdown(Shutdown::Both);
    })
}

/// RAII guard that decrements active_connections on drop.
struct ConnectionSlot {
    state: Arc<BrokerState>,
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.state.active_connections.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Kill the child process group: SIGTERM first, then SIGKILL after grace period.
fn terminate_child_group(pty_pid: u32) {
    if pty_pid == 0 {
        return;
    }
    let pid = Pid::from_raw(pty_pid as i32);

    let pgid = match nix::unistd::getpgid(Some(pid)) {
        Ok(pg) => pg,
        Err(_) => pid,
    };

    log(&format!("Terminating child process group (pgid={})", pgid));

    let _ = signal::killpg(pgid, Signal::SIGTERM);
    thread::sleep(std::time::Duration::from_secs(2));
    let _ = signal::killpg(pgid, Signal::SIGKILL);
}

/// Global write-end of signal self-pipe.
static SIGNAL_PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn signal_handler(sig: libc::c_int) {
    let fd = SIGNAL_PIPE_WRITE.load(Ordering::SeqCst);
    if fd >= 0 {
        let sig_byte = sig as u8;
        unsafe {
            libc::write(fd, &sig_byte as *const u8 as *const libc::c_void, 1);
        }
    }
}

#[allow(clippy::type_complexity)]
pub fn parse_args() -> Result<(String, Vec<String>, Option<String>, u16, u16)> {
    let args: Vec<String> = std::env::args().collect();
    let mut session_id = String::new();
    let mut working_dir = None;
    let mut cols: u16 = 80;
    let mut rows: u16 = 24;
    let mut argv: Vec<String> = Vec::new();
    let mut after_separator = false;

    let mut i = 1;
    while i < args.len() {
        if after_separator {
            argv.push(args[i].clone());
            i += 1;
            continue;
        }
        match args[i].as_str() {
            "--" => after_separator = true,
            "--session-id" => {
                i += 1;
                session_id = args.get(i).context("missing --session-id value")?.clone();
            }
            "--working-dir" => {
                i += 1;
                working_dir = Some(args.get(i).context("missing --working-dir value")?.clone());
            }
            "--cols" => {
                i += 1;
                cols = args.get(i).context("missing --cols value")?.parse()?;
            }
            "--rows" => {
                i += 1;
                rows = args.get(i).context("missing --rows value")?.parse()?;
            }
            _ => {}
        }
        i += 1;
    }

    if session_id.is_empty() || argv.is_empty() {
        anyhow::bail!(
            "Usage: keepty broker --session-id <ID> [--working-dir <DIR>] [--cols N] [--rows N] -- <command...>"
        );
    }

    Ok((session_id, argv, working_dir, cols, rows))
}

/// Start the PTY read thread that pushes to ring buffer and broadcasts to clients.
fn start_pty_reader(
    mut pty_reader: Box<dyn Read + Send>,
    state: Arc<BrokerState>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = [0u8; 16384];
        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) => {
                    log("PTY EOF");
                    state.running.store(false, Ordering::SeqCst);
                    break;
                }
                Ok(n) => {
                    let data = &buf[..n];

                    // Lock ordering: screen → output (never hold output across socket I/O)
                    {
                        let mut screen = state.screen.lock().unwrap();
                        screen.process(data);
                    }

                    let output_frame = Frame::new(MsgKind::Output, data.to_vec());
                    let encoded: Arc<[u8]> = output_frame.encode().into();

                    let mut out = state.output.lock().unwrap();
                    out.ring.push(data);

                    let mut dead = Vec::new();
                    for (&id, client) in out.clients.iter() {
                        if client.tx.try_send(encoded.clone()).is_err() {
                            dead.push(id);
                        }
                    }
                    for id in dead {
                        if let Some(c) = out.clients.remove(&id) {
                            log(&format!(
                                "Client {} ({:?}) evicted (queue full or disconnected)",
                                id, c.role
                            ));
                        }
                        let _ = state.writer_id.compare_exchange(
                            id,
                            0,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        );
                    }
                }
                Err(e) => {
                    log(&format!("PTY read error: {}", e));
                    state.running.store(false, Ordering::SeqCst);
                    break;
                }
            }
        }
    })
}

pub fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_writer(std::io::stderr)
        .init();

    let (session_id, argv, working_dir, cols, rows) = parse_args()?;
    let sock_path = socket_path(&session_id);

    // Validate socket path length (macOS limit: 104 bytes, Linux: 108)
    if sock_path.len() >= 104 {
        anyhow::bail!(
            "Socket path too long ({} bytes, max 103): {}",
            sock_path.len(),
            sock_path
        );
    }

    // Create socket directory with restricted permissions (0700)
    let sock_dir = keepty_protocol::socket_dir();
    std::fs::create_dir_all(&sock_dir)
        .with_context(|| format!("Failed to create socket directory: {}", sock_dir))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(&sock_dir, perms)
            .with_context(|| format!("Failed to set permissions on {}", sock_dir))?;
    }

    let _ = std::fs::remove_file(&sock_path);

    log(&format!(
        "Starting broker: session={}, argv={:?} (waiting for first writer)",
        session_id, argv
    ));

    // Set up shared state — NO child spawned yet
    let state = Arc::new(BrokerState {
        output: Mutex::new(OutputState {
            clients: HashMap::new(),
            ring: RingBuffer::new(1024 * 1024),
        }),
        writer_id: AtomicU32::new(0),
        next_client_id: AtomicU32::new(1),
        screen: Mutex::new(vt100::Parser::new(rows, cols, 0)),
        running: AtomicBool::new(true),
        pty_pid: AtomicU32::new(0),
        cols: RwLock::new(cols),
        rows: RwLock::new(rows),
        resize_gen: AtomicU32::new(0),
        active_connections: AtomicU32::new(0),
    });

    // Install signal handlers
    let (sig_read_fd, sig_write_fd) =
        nix::unistd::pipe().context("Failed to create signal pipe")?;
    unsafe {
        let flags = libc::fcntl(sig_read_fd.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(
            sig_read_fd.as_raw_fd(),
            libc::F_SETFL,
            flags | libc::O_NONBLOCK,
        );
    }
    unsafe {
        signal::sigaction(
            Signal::SIGTERM,
            &signal::SigAction::new(
                signal::SigHandler::Handler(signal_handler),
                signal::SaFlags::SA_RESTART,
                signal::SigSet::empty(),
            ),
        )?;
        signal::sigaction(
            Signal::SIGINT,
            &signal::SigAction::new(
                signal::SigHandler::Handler(signal_handler),
                signal::SaFlags::SA_RESTART,
                signal::SigSet::empty(),
            ),
        )?;
        // Catch SIGHUP so the broker survives terminal hangup.
        // Don't use SIG_IGN — ignored dispositions survive exec into the child.
        signal::sigaction(
            Signal::SIGHUP,
            &signal::SigAction::new(
                signal::SigHandler::Handler(signal_handler),
                signal::SaFlags::SA_RESTART,
                signal::SigSet::empty(),
            ),
        )?;
    }
    SIGNAL_PIPE_WRITE.store(sig_write_fd.into_raw_fd(), Ordering::SeqCst);

    // Listen on Unix socket with restricted permissions (0600)
    let listener = UnixListener::bind(&sock_path).context("Failed to bind socket")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(&sock_path, perms);
    }
    listener
        .set_nonblocking(true)
        .context("Failed to set nonblocking")?;
    log(&format!("Listening on {}", sock_path));

    // Shared PTY resources — initialized when first Writer connects
    let pty_writer: Arc<Mutex<Option<Arc<Mutex<Box<dyn Write + Send>>>>>> =
        Arc::new(Mutex::new(None));
    let pty_master: Arc<Mutex<Option<Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>>>> =
        Arc::new(Mutex::new(None));
    let child_holder: Arc<Mutex<Option<Box<dyn portable_pty::Child + Send>>>> =
        Arc::new(Mutex::new(None));
    let pty_read_handle: Arc<Mutex<Option<thread::JoinHandle<()>>>> = Arc::new(Mutex::new(None));

    // Shared spawn state
    let argv_clone = argv.clone();
    let working_dir_clone = working_dir.clone();
    let spawned = Arc::new(AtomicBool::new(false));

    // Accept clients
    let state_accept = state.clone();
    let pty_writer_accept = pty_writer.clone();
    let pty_master_accept = pty_master.clone();
    let child_holder_accept = child_holder.clone();
    let pty_read_handle_accept = pty_read_handle.clone();
    let spawned_accept = spawned.clone();

    let accept_thread = thread::spawn(move || {
        loop {
            if !state_accept.running.load(Ordering::SeqCst) {
                break;
            }

            match listener.accept() {
                Ok((stream, _)) => {
                    // Connection cap: reject if too many active connections
                    let count = state_accept
                        .active_connections
                        .fetch_add(1, Ordering::SeqCst);
                    if count >= MAX_CONNECTIONS {
                        state_accept
                            .active_connections
                            .fetch_sub(1, Ordering::SeqCst);
                        log("Connection rejected: max connections reached");
                        drop(stream);
                        continue;
                    }

                    if let Err(e) = stream.set_nonblocking(false) {
                        state_accept
                            .active_connections
                            .fetch_sub(1, Ordering::SeqCst);
                        log(&format!("Failed to set client stream blocking: {}", e));
                        continue;
                    }

                    let state_client = state_accept.clone();
                    let pty_writer_client = pty_writer_accept.clone();
                    let pty_master_client = pty_master_accept.clone();
                    let child_holder_client = child_holder_accept.clone();
                    let pty_read_handle_client = pty_read_handle_accept.clone();
                    let spawned_client = spawned_accept.clone();
                    let cmd_argv = argv_clone.clone();
                    let wd = working_dir_clone.clone();
                    // RAII guard: decrements active_connections when thread exits
                    let _slot = ConnectionSlot {
                        state: state_accept.clone(),
                    };

                    thread::spawn(move || {
                        let _slot = _slot; // Move slot into thread
                        if let Err(e) = handle_client(
                            stream,
                            state_client,
                            pty_writer_client,
                            pty_master_client,
                            child_holder_client,
                            pty_read_handle_client,
                            spawned_client,
                            cmd_argv,
                            wd,
                        ) {
                            log(&format!("Client handler error: {}", e));
                        }
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => {
                    log(&format!("Accept error: {}", e));
                    break;
                }
            }
        }
    });

    // Wait for child to exit OR signal. Poll until child is spawned, then wait on it.
    let exit_code;
    loop {
        // Check if child has been spawned and exited
        if let Ok(mut child_guard) = child_holder.lock() {
            if let Some(ref mut child) = *child_guard {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        exit_code = status.exit_code() as i32;
                        log(&format!("Child exited with code {}", exit_code));
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        log(&format!("try_wait error: {}", e));
                        exit_code = 1;
                        break;
                    }
                }
            }
        }

        if !state.running.load(Ordering::SeqCst) {
            log("Shutdown flag set, terminating child");
            let pid = state.pty_pid.load(Ordering::SeqCst);
            terminate_child_group(pid);
            exit_code = 143;
            break;
        }

        // Check signal pipe
        let mut sig_buf = [0u8; 1];
        let n = unsafe {
            libc::read(
                sig_read_fd.as_raw_fd(),
                sig_buf.as_mut_ptr() as *mut libc::c_void,
                1,
            )
        };
        if n > 0 {
            match sig_buf[0] as libc::c_int {
                libc::SIGHUP => {
                    log("Received SIGHUP; ignoring (broker is a daemon)");
                }
                _ => {
                    log("Received SIGTERM/SIGINT");
                    state.running.store(false, Ordering::SeqCst);
                    let pid = state.pty_pid.load(Ordering::SeqCst);
                    terminate_child_group(pid);
                    exit_code = 143;
                    break;
                }
            }
        }

        thread::sleep(std::time::Duration::from_millis(100));
    }
    state.running.store(false, Ordering::SeqCst);

    // Broadcast Exit to all clients via channels, then drop senders.
    // Dropping senders closes channels, causing drain threads to flush and exit.
    {
        let exit_frame = Frame::new(MsgKind::Exit, encode_exit(exit_code));
        let encoded: Arc<[u8]> = exit_frame.encode().into();
        let mut out = state.output.lock().unwrap();
        for (_, client) in out.clients.iter() {
            // Use blocking send for Exit — we want it delivered, not dropped
            let _ = client.tx.send(encoded.clone());
        }
        out.clients.clear(); // Drops all senders → drain threads will exit after flushing
    }

    // Give drain threads time to flush the Exit frame before process exit
    thread::sleep(std::time::Duration::from_millis(200));

    let _ = std::fs::remove_file(&sock_path);
    log("Broker shutting down");

    let _ = accept_thread.join();

    std::process::exit(exit_code);
}

#[allow(clippy::too_many_arguments)]
fn handle_client(
    mut stream: UnixStream,
    state: Arc<BrokerState>,
    pty_writer_holder: Arc<Mutex<Option<Arc<Mutex<Box<dyn Write + Send>>>>>>,
    pty_master_holder: Arc<Mutex<Option<Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>>>>,
    child_holder: Arc<Mutex<Option<Box<dyn portable_pty::Child + Send>>>>,
    pty_read_handle: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
    spawned: Arc<AtomicBool>,
    argv: Vec<String>,
    working_dir: Option<String>,
) -> Result<()> {
    // Read Hello with timeout to prevent thread exhaustion from silent clients
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(HELLO_TIMEOUT_SECS)))
        .ok();
    let hello_frame = Frame::read_from(&mut stream)
        .context("Failed to read Hello (timed out?)")?
        .context("EOF before Hello")?;
    // Clear timeout after successful Hello
    stream.set_read_timeout(None).ok();

    if hello_frame.kind != MsgKind::Hello {
        anyhow::bail!("Expected Hello, got {:?}", hello_frame.kind);
    }

    let (role, client_cols, client_rows) =
        decode_hello(&hello_frame.payload).context("Invalid Hello payload")?;

    let client_id = state.next_client_id.fetch_add(1, Ordering::SeqCst);
    log(&format!(
        "Client {} connected as {:?} ({}x{})",
        client_id, role, client_cols, client_rows
    ));

    // If this is the first Writer and child hasn't been spawned yet, spawn it now.
    // This ensures the program starts with a connected terminal, so DA1/XTVERSION
    // probes get answered through the client's real terminal.
    if role == Role::Writer
        && spawned
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    {
        log(&format!(
            "First writer connected — spawning command at {}x{}",
            client_cols, client_rows
        ));

        *state.cols.write().unwrap() = client_cols;
        *state.rows.write().unwrap() = client_rows;
        state
            .screen
            .lock()
            .unwrap()
            .set_size(client_rows, client_cols);

        // Open PTY but DON'T spawn command yet
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: client_rows,
                cols: client_cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to open PTY")?;

        // Enable IUTF8 and ensure OPOST+ONLCR on the inner PTY.
        // ONLCR ensures \n → \r\n conversion happens on the inner PTY's slave side.
        // Node.js UV_TTY_MODE_RAW also enables ONLCR, but we set it explicitly for
        // non-Node child programs. The attach tool disables OPOST on the outer terminal
        // and does userspace ONLCR, so having the inner PTY provide proper \r\n means
        // most output passes through the fast path (no conversion needed in userspace).
        if let Some(master_fd) = pair.master.as_raw_fd() {
            unsafe {
                let mut termios: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(master_fd, &mut termios) == 0 {
                    termios.c_iflag |= libc::IUTF8;
                    termios.c_oflag |= libc::OPOST | libc::ONLCR;
                    libc::tcsetattr(master_fd, libc::TCSANOW, &termios);
                }
            }
        }

        let pty_reader = pair
            .master
            .try_clone_reader()
            .context("Failed to clone PTY reader")?;
        let pw = Arc::new(Mutex::new(
            pair.master
                .take_writer()
                .context("Failed to take PTY writer")?,
        ));
        let pm = Arc::new(Mutex::new(pair.master));

        // Store PTY resources
        {
            let mut holder = pty_writer_holder.lock().unwrap();
            *holder = Some(pw);
        }
        {
            let mut holder = pty_master_holder.lock().unwrap();
            *holder = Some(pm);
        }

        state.writer_id.store(client_id, Ordering::SeqCst);

        let ack = Frame::new(
            MsgKind::HelloAck,
            encode_hello_ack(0, client_cols, client_rows), // PID not known yet
        );
        ack.write_to(&mut stream)?;

        // Set up per-client outbound channel + drain thread
        let (tx, rx) = std::sync::mpsc::sync_channel(CLIENT_QUEUE_BOUND);
        let stream_clone = stream.try_clone()?;
        let _drain = spawn_client_drain(client_id, rx, stream_clone, state.clone());
        {
            let mut out = state.output.lock().unwrap();
            out.clients.insert(client_id, ClientSender { tx, role });
        }

        // Start PTY read thread (reads from PTY master, broadcasts to clients)
        let handle = start_pty_reader(pty_reader, state.clone());
        {
            let mut holder = pty_read_handle.lock().unwrap();
            *holder = Some(handle);
        }

        // NOW spawn the command — everything is ready to relay bytes instantly.
        // Claude Code's DA1/XTVERSION probes will be read by the PTY thread,
        // broadcast to the registered client, written to Ghostty, answered,
        // and relayed back — all without any gap.
        let mut cmd = CommandBuilder::new(&argv[0]);
        if argv.len() > 1 {
            cmd.args(&argv[1..]);
        }
        cmd.env("TERM", "xterm-256color");
        if let Some(ref dir) = working_dir {
            cmd.cwd(dir);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("Failed to spawn command")?;
        let pid = child.process_id().unwrap_or(0);

        log(&format!("Child spawned with PID {}", pid));
        state.pty_pid.store(pid, Ordering::SeqCst);

        {
            let mut holder = child_holder.lock().unwrap();
            *holder = Some(child);
        }
    } else {
        // Not the first Writer, or Watcher/Monitor — PTY already running
        if role == Role::Writer {
            state.writer_id.store(client_id, Ordering::SeqCst);
            *state.cols.write().unwrap() = client_cols;
            *state.rows.write().unwrap() = client_rows;

            // Resize PTY
            if let Ok(holder) = pty_master_holder.lock() {
                if let Some(ref master) = *holder {
                    if let Ok(m) = master.lock() {
                        let _ = m.resize(PtySize {
                            rows: client_rows,
                            cols: client_cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                }
            }
        }

        let ack = Frame::new(
            MsgKind::HelloAck,
            encode_hello_ack(
                state.pty_pid.load(Ordering::SeqCst),
                *state.cols.read().unwrap(),
                *state.rows.read().unwrap(),
            ),
        );
        ack.write_to(&mut stream)?;

        // Set up per-client outbound channel + drain thread
        let (tx, rx) = std::sync::mpsc::sync_channel(CLIENT_QUEUE_BOUND);
        let stream_clone = stream.try_clone()?;
        let _drain = spawn_client_drain(client_id, rx, stream_clone, state.clone());

        // Register + replay under the output lock (prevents replay/live duplication)
        {
            let mut out = state.output.lock().unwrap();

            if role == Role::Watcher {
                // Watchers: skip raw ring replay (garbled for cursor-addressed TUIs).
                // After registration, we'll nudge the child to re-render.
            } else {
                // Writers/Monitors: raw ring buffer replay via channel.
                let replay_data = out.ring.recent();
                if !replay_data.is_empty() {
                    let replay = Frame::new(MsgKind::Output, replay_data);
                    let encoded: Arc<[u8]> = replay.encode().into();
                    let _ = tx.try_send(encoded);
                }
            }

            out.clients.insert(client_id, ClientSender { tx, role });
        }

        // Watcher / reattached-writer redraw nudge: raw ring replay alone does
        // not reconstruct cursor-addressed TUI state. Reattached writers keep
        // replay (preserves shell scrollback), then get the same double-resize
        // trick as watchers to force a fresh render from the child.
        if role == Role::Watcher || role == Role::Writer {
            let pid = state.pty_pid.load(Ordering::SeqCst);
            if pid > 0 && spawned.load(Ordering::SeqCst) {
                let current_cols = *state.cols.read().unwrap();
                let current_rows = *state.rows.read().unwrap();

                if current_cols > 1 {
                    // Brief resize to cols-1 to force dimension change
                    if let Ok(holder) = pty_master_holder.lock() {
                        if let Some(ref master) = *holder {
                            if let Ok(m) = master.lock() {
                                let _ = m.resize(PtySize {
                                    rows: current_rows,
                                    cols: current_cols - 1,
                                    pixel_width: 0,
                                    pixel_height: 0,
                                });
                                // Small delay for the signal to be processed
                                thread::sleep(std::time::Duration::from_millis(20));
                                // Resize back to correct dimensions
                                let _ = m.resize(PtySize {
                                    rows: current_rows,
                                    cols: current_cols,
                                    pixel_width: 0,
                                    pixel_height: 0,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    // Read loop: process messages from this client.
    // Input and Resize are only processed after the child is spawned
    // (the PTY writer/master aren't available yet). But we MUST keep
    // reading frames even before spawn — otherwise the client's socket
    // buffer fills and the attach process hangs on join() during detach.
    loop {
        if !state.running.load(Ordering::SeqCst) {
            break;
        }

        let frame = match Frame::read_from(&mut stream) {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(_) => break,
        };

        match frame.kind {
            MsgKind::Input => {
                // Only forward input if child is spawned and we're the writer
                if spawned.load(Ordering::SeqCst)
                    && state.writer_id.load(Ordering::SeqCst) == client_id
                {
                    if let Ok(holder) = pty_writer_holder.lock() {
                        if let Some(ref writer) = *holder {
                            if let Ok(mut w) = writer.lock() {
                                let _ = w.write_all(&frame.payload);
                                let _ = w.flush();
                            }
                        }
                    }
                }
            }
            MsgKind::Resize => {
                if spawned.load(Ordering::SeqCst)
                    && state.writer_id.load(Ordering::SeqCst) == client_id
                {
                    if let Some((new_cols, new_rows)) = decode_resize(&frame.payload) {
                        let old_cols = *state.cols.read().unwrap();
                        *state.cols.write().unwrap() = new_cols;
                        *state.rows.write().unwrap() = new_rows;
                        state.screen.lock().unwrap().set_size(new_rows, new_cols);

                        // Hold output lock across PTY resize + ResizeAck enqueue.
                        // This guarantees ResizeAck arrives before any post-resize
                        // Output frames in every client's channel.
                        let mut out = state.output.lock().unwrap();

                        if let Ok(holder) = pty_master_holder.lock() {
                            if let Some(ref master) = *holder {
                                if let Ok(m) = master.lock() {
                                    let _ = m.resize(PtySize {
                                        rows: new_rows,
                                        cols: new_cols,
                                        pixel_width: 0,
                                        pixel_height: 0,
                                    });
                                }
                            }
                        }

                        let gen = state.resize_gen.fetch_add(1, Ordering::SeqCst) + 1;
                        let ack = Frame::new(
                            MsgKind::ResizeAck,
                            encode_resize_ack(gen, new_cols, new_rows),
                        );
                        let encoded: Arc<[u8]> = ack.encode().into();
                        let mut dead_ack = Vec::new();
                        for (&id, client) in out.clients.iter() {
                            if client.tx.try_send(encoded.clone()).is_err() {
                                dead_ack.push(id);
                            }
                        }
                        for id in dead_ack {
                            out.clients.remove(&id);
                            let _ = state.writer_id.compare_exchange(
                                id,
                                0,
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                            );
                        }

                        // Resize nudge: on widen-only, send a second SIGWINCH after
                        // ~40ms. Some TUIs cache header width and don't fully redraw
                        // on the first SIGWINCH. The nudge gives them a second chance.
                        // Cancel if a newer resize arrives (gen check).
                        if new_cols > old_cols {
                            let nudge_gen = gen;
                            let nudge_state = state.clone();
                            thread::spawn(move || {
                                thread::sleep(std::time::Duration::from_millis(40));
                                // Only nudge if no newer resize has happened
                                if nudge_state.resize_gen.load(Ordering::SeqCst) == nudge_gen {
                                    let pid = nudge_state.pty_pid.load(Ordering::SeqCst);
                                    if pid > 0 {
                                        let _ = signal::kill(
                                            Pid::from_raw(pid as i32),
                                            Signal::SIGWINCH,
                                        );
                                    }
                                }
                            });
                        }
                    }
                } else if role == Role::Watcher && spawned.load(Ordering::SeqCst) {
                    // Watcher resize: don't change PTY dimensions, but trigger a
                    // re-render nudge (double-resize trick) so the watcher gets a
                    // fresh render. Only on widen to avoid spam on drag-narrow.
                    if let Some((watcher_cols, _)) = decode_resize(&frame.payload) {
                        let current_cols = *state.cols.read().unwrap();
                        if watcher_cols >= current_cols {
                            let current_rows = *state.rows.read().unwrap();
                            let pid = state.pty_pid.load(Ordering::SeqCst);
                            if pid > 0 && current_cols > 1 {
                                if let Ok(holder) = pty_master_holder.lock() {
                                    if let Some(ref master) = *holder {
                                        if let Ok(m) = master.lock() {
                                            let _ = m.resize(PtySize {
                                                rows: current_rows,
                                                cols: current_cols - 1,
                                                pixel_width: 0,
                                                pixel_height: 0,
                                            });
                                            thread::sleep(std::time::Duration::from_millis(20));
                                            let _ = m.resize(PtySize {
                                                rows: current_rows,
                                                cols: current_cols,
                                                pixel_width: 0,
                                                pixel_height: 0,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            MsgKind::Shutdown => {
                // Pragmatic role model: Shutdown allowed from current writer,
                // Monitor (used by `keepty stop`), or orphaned session (no writer).
                // Watchers are the only role blocked from Shutdown.
                let writer = state.writer_id.load(Ordering::SeqCst);
                if writer == client_id || role == Role::Monitor || writer == 0 {
                    log(&format!("Shutdown requested by client {}", client_id));
                    state.running.store(false, Ordering::SeqCst);
                    let pid = state.pty_pid.load(Ordering::SeqCst);
                    terminate_child_group(pid);
                    break;
                }
                // Silently ignore — forward compat, don't disconnect
            }
            MsgKind::Ping => {
                let pong = Frame::new(MsgKind::Pong, Vec::new());
                let encoded: Arc<[u8]> = pong.encode().into();
                let out = state.output.lock().unwrap();
                if let Some(client) = out.clients.get(&client_id) {
                    let _ = client.tx.try_send(encoded);
                }
            }
            _ => {}
        }
    }

    // Cleanup this client — remove from output state, drain thread will notice
    log(&format!("Client {} disconnected", client_id));
    {
        let mut out = state.output.lock().unwrap();
        out.clients.remove(&client_id);
        // Dropping the sender closes the channel, drain thread will exit
    }
    let _ = state
        .writer_id
        .compare_exchange(client_id, 0, Ordering::SeqCst, Ordering::SeqCst);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::RingBuffer;

    #[test]
    fn ring_buffer_basic() {
        let mut rb = RingBuffer::new(16);
        rb.push(b"hello");
        assert_eq!(rb.recent(), b"hello");
    }

    #[test]
    fn ring_buffer_wraparound() {
        let mut rb = RingBuffer::new(8);
        rb.push(b"12345678"); // fills exactly
        assert_eq!(rb.recent(), b"12345678");

        rb.push(b"AB"); // wraps: overwrites positions 0,1
        let recent = rb.recent();
        assert_eq!(recent, b"345678AB");
    }

    #[test]
    fn ring_buffer_multiple_wraps() {
        let mut rb = RingBuffer::new(4);
        rb.push(b"ABCD");
        rb.push(b"EFGH");
        rb.push(b"IJ");
        // Buffer should contain last 4 bytes: "GHIJ"
        assert_eq!(rb.recent(), b"GHIJ");
    }

    #[test]
    fn ring_buffer_large_data() {
        let capacity = 1024;
        let mut rb = RingBuffer::new(capacity);

        // Write 3x capacity
        let data: Vec<u8> = (0..capacity * 3).map(|i| (i % 256) as u8).collect();
        rb.push(&data);

        let recent = rb.recent();
        assert_eq!(recent.len(), capacity);
        // Should be last `capacity` bytes
        assert_eq!(recent, &data[data.len() - capacity..]);
    }

    #[test]
    fn ring_buffer_empty() {
        let rb = RingBuffer::new(16);
        assert_eq!(rb.recent(), Vec::<u8>::new());
    }
}
