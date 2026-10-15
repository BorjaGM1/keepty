//! Integration tests for the keepty broker.
//!
//! These tests spawn a real broker process, connect to it via the protocol,
//! and verify end-to-end behavior.

use keepty_protocol::*;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// Monotonic counter to ensure unique session IDs across parallel tests
static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Helper: get the path to the keepty binary (built by cargo test)
fn keepty_bin() -> String {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // remove test binary name
    path.pop(); // remove deps/
    path.push("keepty");
    path.to_string_lossy().to_string()
}

/// Helper: generate a unique session ID for each test
fn unique_id(label: &str) -> String {
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("test-{}-{}-{}", std::process::id(), n, label)
}

/// Helper: start a broker and wait for the socket to appear.
/// Command is passed as a shell string via `sh -c` for test convenience.
#[allow(clippy::zombie_processes)]
fn start_broker(session_id: &str, command: &str) -> Child {
    let child = Command::new(keepty_bin())
        .args([
            "broker",
            "--session-id",
            session_id,
            "--cols",
            "80",
            "--rows",
            "24",
            "--",
            "sh",
            "-c",
            command,
        ])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to start keepty broker");

    // Wait for socket to appear
    let sock = socket_path(session_id);
    for _ in 0..100 {
        if std::path::Path::new(&sock).exists() {
            std::thread::sleep(Duration::from_millis(50));
            return child;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("Broker socket never appeared at {}", sock);
}

/// Helper: connect to broker as a given role, with retries
fn connect(session_id: &str, role: Role) -> UnixStream {
    let sock_path = socket_path(session_id);

    let mut stream = None;
    for _ in 0..10 {
        match UnixStream::connect(&sock_path) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    let mut stream =
        stream.unwrap_or_else(|| panic!("Failed to connect to broker at {}", sock_path));
    stream.set_nonblocking(false).unwrap();

    // Send Hello
    let hello = Frame::new(MsgKind::Hello, encode_hello(role, 80, 24));
    hello.write_to(&mut stream).unwrap();

    // Read HelloAck
    let ack = Frame::read_from(&mut stream).unwrap().unwrap();
    assert_eq!(ack.kind, MsgKind::HelloAck);
    let (_pid, cols, rows) = decode_hello_ack(&ack.payload).unwrap();
    // PID may be 0 for the first Writer (deferred spawn sends HelloAck before command starts)
    assert_eq!(cols, 80);
    assert_eq!(rows, 24);

    stream
}

/// Helper: read frames until we find Output containing expected text (with timeout)
fn read_until_output_contains(
    stream: &mut UnixStream,
    expected: &str,
    timeout: Duration,
) -> String {
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let deadline = std::time::Instant::now() + timeout;
    let mut collected = String::new();

    while std::time::Instant::now() < deadline {
        match Frame::read_from(stream) {
            Ok(Some(frame)) if frame.kind == MsgKind::Output => {
                collected.push_str(&String::from_utf8_lossy(&frame.payload));
                if collected.contains(expected) {
                    return collected;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        }
    }
    collected
}

/// Cleanup helper
fn cleanup(mut child: Child, session_id: &str) {
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(socket_path(session_id));
}

// Use sleep to keep echo-based sessions alive long enough for tests.
// The broker exits when the child process exits, so "echo X" alone
// would exit before the client connects.

#[test]
fn broker_starts_and_accepts_connection() {
    let id = unique_id("start");
    let child = start_broker(&id, "echo hello; sleep 30");

    let mut stream = connect(&id, Role::Writer);
    let output = read_until_output_contains(&mut stream, "hello", Duration::from_secs(5));
    assert!(
        output.contains("hello"),
        "Expected 'hello' in output, got: {}",
        output
    );

    cleanup(child, &id);
}

#[test]
fn writer_can_send_input() {
    let id = unique_id("input");
    let child = start_broker(&id, "cat");

    let mut stream = connect(&id, Role::Writer);
    std::thread::sleep(Duration::from_millis(200));

    // Send input — cat echoes it back
    let input = Frame::new(MsgKind::Input, b"keepty works!\n".to_vec());
    input.write_to(&mut stream).unwrap();

    let output = read_until_output_contains(&mut stream, "keepty works!", Duration::from_secs(5));
    assert!(
        output.contains("keepty works!"),
        "Expected echo back, got: {}",
        output
    );

    cleanup(child, &id);
}

#[test]
fn watcher_receives_output() {
    let id = unique_id("watcher");
    let child = start_broker(&id, "cat");

    // Writer must connect first to trigger deferred spawn
    let mut writer = connect(&id, Role::Writer);
    std::thread::sleep(Duration::from_millis(200));

    // Watcher connects — no replay, sees live output only
    let mut watcher = connect(&id, Role::Watcher);
    std::thread::sleep(Duration::from_millis(200));

    // Writer sends input — cat echoes it, watcher sees the live broadcast
    let input = Frame::new(MsgKind::Input, b"watcher-live\n".to_vec());
    input.write_to(&mut writer).unwrap();

    let output = read_until_output_contains(&mut watcher, "watcher-live", Duration::from_secs(5));
    assert!(
        output.contains("watcher-live"),
        "Watcher should see live output, got: {}",
        output
    );

    cleanup(child, &id);
}

#[test]
fn multiple_clients_receive_same_output() {
    let id = unique_id("multi");
    let child = start_broker(&id, "cat");

    let mut writer = connect(&id, Role::Writer);
    let mut watcher = connect(&id, Role::Watcher);
    std::thread::sleep(Duration::from_millis(200));

    let input = Frame::new(MsgKind::Input, b"broadcast test\n".to_vec());
    input.write_to(&mut writer).unwrap();

    let writer_output =
        read_until_output_contains(&mut writer, "broadcast test", Duration::from_secs(5));
    let watcher_output =
        read_until_output_contains(&mut watcher, "broadcast test", Duration::from_secs(5));

    assert!(
        writer_output.contains("broadcast test"),
        "Writer should see output"
    );
    assert!(
        watcher_output.contains("broadcast test"),
        "Watcher should see output"
    );

    cleanup(child, &id);
}

#[test]
fn resize_is_accepted() {
    let id = unique_id("resize");
    let child = start_broker(&id, "cat");

    let mut stream = connect(&id, Role::Writer);

    let resize = Frame::new(MsgKind::Resize, encode_resize(132, 50));
    resize.write_to(&mut stream).unwrap();

    // No error = resize accepted and forwarded as SIGWINCH
    std::thread::sleep(Duration::from_millis(100));

    cleanup(child, &id);
}

#[test]
fn shutdown_terminates_session() {
    let id = unique_id("shutdown");
    let child = start_broker(&id, "cat");

    let mut stream = connect(&id, Role::Writer);

    let shutdown = Frame::new(MsgKind::Shutdown, vec![]);
    shutdown.write_to(&mut stream).unwrap();

    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut got_exit = false;
    for _ in 0..50 {
        match Frame::read_from(&mut stream) {
            Ok(Some(frame)) if frame.kind == MsgKind::Exit => {
                got_exit = true;
                break;
            }
            Ok(Some(_)) => continue,
            _ => break,
        }
    }
    assert!(got_exit, "Should receive Exit frame after shutdown");

    let status = child.wait_with_output().expect("Failed to wait for broker");
    assert!(
        status.status.code().is_some(),
        "Broker should exit with a code"
    );

    let _ = std::fs::remove_file(socket_path(&id));
}

#[test]
fn ring_buffer_replays_on_reconnect() {
    let id = unique_id("replay");
    let child = start_broker(&id, "echo replay-data-here; i=0; while [ $i -lt 60 ]; do echo scroll-line-$i; i=$((i + 1)); done; echo replay-ready; sleep 30");

    // First client: connect, receive output, disconnect
    {
        let mut stream1 = connect(&id, Role::Writer);
        let output =
            read_until_output_contains(&mut stream1, "replay-ready", Duration::from_secs(5));
        assert!(output.contains("replay-data-here"));
    }

    // Wait for disconnect to be processed
    std::thread::sleep(Duration::from_millis(500));

    // The marker has scrolled off the viewport; both read-only roles need the ring.
    for role in [Role::Monitor, Role::Watcher] {
        let mut stream2 = connect(&id, role);
        // The first frame after HelloAck is the ring buffer replay
        stream2
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut replay_contents = String::new();
        for _ in 0..10 {
            match Frame::read_from(&mut stream2) {
                Ok(Some(frame)) if frame.kind == MsgKind::Output => {
                    replay_contents.push_str(&String::from_utf8_lossy(&frame.payload));
                    if replay_contents.contains("replay-data-here") {
                        break;
                    }
                }
                _ => break,
            }
        }
        assert!(
            replay_contents.contains("replay-data-here"),
            "Ring buffer replay should contain previous output, got: {}",
            replay_contents
        );
    }
    cleanup(child, &id);
}

#[test]
fn monitor_with_vt100_reads_screen() {
    let id = unique_id("screen");
    let child = start_broker(&id, "echo 'screen test output'; sleep 30");

    // Writer must connect first to trigger deferred spawn
    let _writer = connect(&id, Role::Writer);
    std::thread::sleep(Duration::from_millis(500));

    let mut stream = connect(&id, Role::Monitor);

    // Feed output into vt100 parser (this is what keepty-screen does)
    let mut parser = vt100::Parser::new(24, 80, 0);
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    for _ in 0..20 {
        match Frame::read_from(&mut stream) {
            Ok(Some(frame)) if frame.kind == MsgKind::Output => {
                parser.process(&frame.payload);
                let contents = parser.screen().contents();
                if contents.contains("screen test output") {
                    cleanup(child, &id);
                    return;
                }
            }
            Ok(Some(_)) => continue,
            _ => break,
        }
    }

    let final_contents = parser.screen().contents();
    cleanup(child, &id);
    panic!(
        "Expected 'screen test output' on parsed screen, got: {}",
        final_contents
    );
}

#[test]
fn writer_handoff_rejects_old_writer_input() {
    let id = unique_id("handoff");
    let child = start_broker(&id, "cat");

    let mut writer1 = connect(&id, Role::Writer);
    std::thread::sleep(Duration::from_millis(200));

    // Writer 1 can send input
    let input1 = Frame::new(MsgKind::Input, b"from-writer1\n".to_vec());
    input1.write_to(&mut writer1).unwrap();
    let output = read_until_output_contains(&mut writer1, "from-writer1", Duration::from_secs(5));
    assert!(output.contains("from-writer1"), "Writer1 input should work");

    // Writer 2 connects — takes over
    let mut writer2 = connect(&id, Role::Writer);
    std::thread::sleep(Duration::from_millis(200));

    // Writer 2 can send input
    let input2 = Frame::new(MsgKind::Input, b"from-writer2\n".to_vec());
    input2.write_to(&mut writer2).unwrap();
    let output2 = read_until_output_contains(&mut writer2, "from-writer2", Duration::from_secs(5));
    assert!(
        output2.contains("from-writer2"),
        "Writer2 input should work"
    );

    // Writer 1's input should be silently ignored (writer_id check fails)
    let input3 = Frame::new(MsgKind::Input, b"writer1-after-handoff\n".to_vec());
    input3.write_to(&mut writer1).unwrap();
    std::thread::sleep(Duration::from_millis(500));

    // Verify writer1's text did NOT appear — send a marker from writer2
    let marker = Frame::new(MsgKind::Input, b"MARKER\n".to_vec());
    marker.write_to(&mut writer2).unwrap();
    let output3 = read_until_output_contains(&mut writer2, "MARKER", Duration::from_secs(5));
    assert!(
        !output3.contains("writer1-after-handoff"),
        "Writer1 input after handoff should be rejected"
    );

    cleanup(child, &id);
}

#[test]
fn resize_sends_resize_ack() {
    let id = unique_id("resize-ack");
    let child = start_broker(&id, "sleep 30");

    let mut stream = connect(&id, Role::Writer);
    std::thread::sleep(Duration::from_millis(200));

    let resize = Frame::new(MsgKind::Resize, encode_resize(132, 50));
    resize.write_to(&mut stream).unwrap();

    // Should receive ResizeAck
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut got_ack = false;
    for _ in 0..20 {
        match Frame::read_from(&mut stream) {
            Ok(Some(frame)) if frame.kind == MsgKind::ResizeAck => {
                let (gen, cols, rows) = decode_resize_ack(&frame.payload).unwrap();
                assert!(gen > 0, "Gen should be > 0");
                assert_eq!(cols, 132);
                assert_eq!(rows, 50);
                got_ack = true;
                break;
            }
            Ok(Some(_)) => continue,
            _ => break,
        }
    }
    assert!(got_ack, "Should receive ResizeAck after Resize");

    cleanup(child, &id);
}

#[test]
fn watcher_input_is_ignored() {
    let id = unique_id("watcher-input");
    let child = start_broker(&id, "cat");

    let mut writer = connect(&id, Role::Writer);
    std::thread::sleep(Duration::from_millis(200));
    let mut watcher = connect(&id, Role::Watcher);

    // Watcher sends input — should be silently ignored
    let input = Frame::new(MsgKind::Input, b"from-watcher\n".to_vec());
    input.write_to(&mut watcher).unwrap();
    std::thread::sleep(Duration::from_millis(300));

    // Writer sends a marker to check watcher's input didn't arrive
    let marker = Frame::new(MsgKind::Input, b"MARKER\n".to_vec());
    marker.write_to(&mut writer).unwrap();
    let output = read_until_output_contains(&mut writer, "MARKER", Duration::from_secs(5));
    assert!(
        !output.contains("from-watcher"),
        "Watcher input should be silently ignored"
    );

    cleanup(child, &id);
}

#[test]
fn readonly_clients_replay_current_alternate_screen() {
    let id = unique_id("alternate-replay");
    // Draw once and remain idle: no application redraw can supply the replay.
    let child = start_broker(
        &id,
        "printf '\\033[?1049h\\033[2J\\033[5;10Halternate screen'; sleep 30",
    );
    let mut writer = connect(&id, Role::Writer);
    let initial =
        read_until_output_contains(&mut writer, "alternate screen", Duration::from_secs(5));
    assert!(initial.contains("alternate screen"));

    for role in [Role::Monitor, Role::Watcher] {
        let mut client = connect(&id, role);
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let replay = Frame::read_from(&mut client).unwrap().unwrap();
        assert_eq!(replay.kind, MsgKind::Output);
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(&replay.payload);
        assert!(parser.screen().alternate_screen());
        assert_eq!(parser.screen().contents().trim(), "alternate screen");
        assert_eq!(parser.screen().cell(4, 9).unwrap().contents(), "a");
    }
    cleanup(child, &id);
}

#[cfg(target_os = "linux")]
fn assert_descendant_cleanup(detached: bool, natural_exit: bool) {
    let id = unique_id("descendants");
    let pidfile = std::env::temp_dir().join(format!("{id}.pid"));
    // Ignore the graceful signals to exercise SIGKILL escalation too. The
    // detached shell writes its own PID (setsid may fork internally).
    let spawn = if detached { "setsid sh" } else { "sh" };
    let command = format!(
        "{spawn} -c 'trap \"\" HUP TERM; echo $$ > {}; exec sleep 1000' & while [ ! -s {} ]; do sleep 0.01; done; echo READY; {}",
        pidfile.display(), pidfile.display(),
        if natural_exit { "exit 7" } else { "wait" }
    );
    let mut broker = start_broker(&id, &command);
    let mut writer = connect(&id, Role::Writer);
    let output = read_until_output_contains(&mut writer, "READY", Duration::from_secs(5));
    assert!(output.contains("READY"), "{output}");
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    if !natural_exit {
        assert!(Command::new(keepty_bin())
            .args(["stop", &id])
            .status()
            .unwrap()
            .success());
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = broker.try_wait().unwrap() {
            assert_eq!(status.code(), Some(if natural_exit { 7 } else { 143 }));
            break;
        }
        if std::time::Instant::now() >= deadline {
            cleanup(broker, &id);
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
            panic!("Broker did not finish cleanup");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "Descendant {pid} survived or was not reaped"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    assert!(!std::path::Path::new(&socket_path(&id)).exists());
    std::fs::remove_file(pidfile).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn stop_kills_background_grandchild() {
    assert_descendant_cleanup(false, false);
}

#[cfg(target_os = "linux")]
#[test]
fn stop_kills_setsid_descendant() {
    assert_descendant_cleanup(true, false);
}

#[cfg(target_os = "linux")]
#[test]
fn child_exit_kills_setsid_descendant() {
    assert_descendant_cleanup(true, true);
}

#[test]
fn list_prunes_refused_sockets_and_preserves_live_sessions() {
    use std::os::unix::net::UnixListener;
    let id = unique_id("list");
    let root = std::env::temp_dir().join(&id);
    let dir = root.join("keepty");
    std::fs::create_dir_all(&dir).unwrap();
    let stale = dir.join("keepty-stale.sock");
    drop(UnixListener::bind(&stale).unwrap());
    let live = dir.join("keepty-live.sock");
    let _listener = UnixListener::bind(&live).unwrap();
    let output = Command::new(keepty_bin())
        .arg("list")
        .env("XDG_RUNTIME_DIR", &root)
        .env("TMPDIR", &root)
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("live  (running)"), "{stdout}");
    assert!(!stdout.contains("stale"), "{stdout}");
    assert!(!stale.exists());
    assert!(live.exists());
    std::fs::remove_dir_all(root).unwrap();
}

fn agent_cli(args: &[&str]) -> std::process::Output {
    Command::new(keepty_bin()).args(args).output().unwrap()
}

#[test]
fn cli_screen_replays_without_resize_or_redraw() {
    let id = unique_id("cli-screen");
    let child = start_broker(
        &id,
        "trap 'echo REDRAW' WINCH; printf '\\033[31mready   \\033[0m'; sleep 30",
    );
    let mut writer = connect(&id, Role::Writer);
    assert!(
        read_until_output_contains(&mut writer, "ready", Duration::from_secs(5)).contains("ready")
    );
    let output = agent_cli(&["screen", &id, "--cols", "120", "--rows", "40"]);
    assert!(output.status.success(), "{:?}", output);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("ready"));
    assert!(!text.contains('\x1b'));
    assert!(!text.contains("REDRAW"));
    assert!(text.lines().all(|line| line == line.trim_end()));
    let monitor = connect(&id, Role::Monitor); // helper asserts size remains 80x24
    drop(monitor);
    cleanup(child, &id);
}

// A protocol peer makes Input frame boundaries and submission timing observable.
fn cli_peer<F>(label: &str, args: &[&str], replay: &[u8], check: F)
where
    F: FnOnce(&mut UnixStream) + Send + 'static,
{
    use std::os::unix::net::UnixListener;
    let id = unique_id(label);
    let path = socket_path(&id);
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let replay = replay.to_vec();
    let server = std::thread::spawn(move || {
        let accept = || {
            let started = std::time::Instant::now();
            loop {
                match listener.accept() {
                    Ok(pair) => return pair.0,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && started.elapsed() < Duration::from_secs(5) =>
                    {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("accept timed out or failed: {e}"),
                }
            }
        };
        let mut monitor = accept();
        monitor
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let hello = Frame::read_from(&mut monitor).unwrap().unwrap();
        assert_eq!(decode_hello(&hello.payload).unwrap().0, Role::Monitor);
        Frame::new(MsgKind::HelloAck, encode_hello_ack(123, 132, 40))
            .write_to(&mut monitor)
            .unwrap();
        Frame::new(MsgKind::Output, b"ready".to_vec())
            .write_to(&mut monitor)
            .unwrap();
        let mut stream = accept();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let hello = Frame::read_from(&mut stream).unwrap().unwrap();
        assert_eq!(
            decode_hello(&hello.payload).unwrap(),
            (Role::Writer, 132, 40)
        );
        Frame::new(MsgKind::HelloAck, encode_hello_ack(123, 80, 24))
            .write_to(&mut stream)
            .unwrap();
        Frame::new(MsgKind::Output, replay)
            .write_to(&mut stream)
            .unwrap();
        let ping = Frame::read_from(&mut stream).unwrap().unwrap();
        assert_eq!(ping.kind, MsgKind::Ping);
        Frame::new(MsgKind::Pong, vec![])
            .write_to(&mut stream)
            .unwrap();
        check(&mut stream);
        let ping = Frame::read_from(&mut stream).unwrap().unwrap();
        assert_eq!(ping.kind, MsgKind::Ping);
        Frame::new(MsgKind::Pong, vec![])
            .write_to(&mut stream)
            .unwrap();
        assert!(Frame::read_from(&mut stream).unwrap().is_none());
    });
    let mut cli_args = vec![args[0], &id];
    cli_args.extend_from_slice(&args[1..]);
    let output = agent_cli(&cli_args);
    server.join().unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(output.status.success(), "{:?}", output);
}

#[test]
fn cli_send_paste_and_separate_delayed_submit() {
    cli_peer(
        "cli-send",
        &["send", "one\ntwo", "--submit", "--submit-delay", "150"],
        b"\x1b[?2004hready",
        |stream| {
            let first = Frame::read_from(stream).unwrap().unwrap();
            assert_eq!(first.kind, MsgKind::Input);
            assert_eq!(first.payload, b"\x1b[200~one\ntwo\x1b[201~");
            let started = std::time::Instant::now();
            let enter = Frame::read_from(stream).unwrap().unwrap();
            assert_eq!(enter.kind, MsgKind::Input);
            assert_eq!(enter.payload, b"\r");
            assert!(started.elapsed() >= Duration::from_millis(120));
        },
    );
    cli_peer("cli-raw", &["send", "hello"], b"ready", |stream| {
        assert_eq!(Frame::read_from(stream).unwrap().unwrap().payload, b"hello");
    });
}

#[test]
fn cli_keys_encodes_named_keys_and_rejects_unknown() {
    let names = [
        "keys",
        "enter",
        "tab",
        "esc",
        "backspace",
        "delete",
        "up",
        "down",
        "left",
        "right",
        "home",
        "end",
        "pageup",
        "pagedown",
        "f1",
        "f2",
        "f3",
        "f4",
        "f5",
        "f6",
        "f7",
        "f8",
        "f9",
        "f10",
        "f11",
        "f12",
        "ctrl-a",
        "ctrl-z",
        "space",
    ];
    cli_peer("cli-keys", &names, b"ready", |stream| {
        for expected in [
            b"\r".as_slice(),
            b"\t",
            b"\x1b",
            b"\x7f",
            b"\x1b[3~",
            b"\x1b[A",
            b"\x1b[B",
            b"\x1b[D",
            b"\x1b[C",
            b"\x1b[H",
            b"\x1b[F",
            b"\x1b[5~",
            b"\x1b[6~",
            b"\x1bOP",
            b"\x1bOQ",
            b"\x1bOR",
            b"\x1bOS",
            b"\x1b[15~",
            b"\x1b[17~",
            b"\x1b[18~",
            b"\x1b[19~",
            b"\x1b[20~",
            b"\x1b[21~",
            b"\x1b[23~",
            b"\x1b[24~",
            b"\x01",
            b"\x1a",
            b" ",
        ] {
            let frame = Frame::read_from(stream).unwrap().unwrap();
            assert_eq!(frame.kind, MsgKind::Input);
            assert_eq!(frame.payload, expected);
        }
    });
    assert_eq!(
        agent_cli(&["keys", "missing", "enter", "unknown"])
            .status
            .code(),
        Some(2)
    );
    cli_peer(
        "cli-appkeys",
        &["keys", "up", "home"],
        b"\x1b[?1h",
        |stream| {
            assert_eq!(
                Frame::read_from(stream).unwrap().unwrap().payload,
                b"\x1bOA"
            );
            assert_eq!(
                Frame::read_from(stream).unwrap().unwrap().payload,
                b"\x1bOH"
            );
        },
    );
}

#[test]
fn cli_wait_text_quiet_timeout_and_exit() {
    let id = unique_id("cli-wait");
    let child = start_broker(
        &id,
        "printf ready; sleep 1; printf '\\r\\033[Kdone'; sleep 30",
    );
    let mut writer = connect(&id, Role::Writer);
    assert!(
        read_until_output_contains(&mut writer, "ready", Duration::from_secs(5)).contains("ready")
    );
    assert!(
        agent_cli(&["wait", &id, "--text", "done", "--timeout", "5"])
            .status
            .success()
    );
    assert!(agent_cli(&["wait", &id, "--quiet", "100"]).status.success());
    assert_eq!(
        agent_cli(&["wait", &id, "--text", "absent", "--timeout", "1"])
            .status
            .code(),
        Some(1)
    );
    assert_eq!(
        agent_cli(&["wait", &id, "--text", "done", "--quiet", "100"])
            .status
            .code(),
        Some(2)
    );
    cleanup(child, &id);

    let id = unique_id("cli-exit");
    let child = start_broker(&id, "echo ready; sleep 1");
    let mut writer = connect(&id, Role::Writer);
    read_until_output_contains(&mut writer, "ready", Duration::from_secs(5));
    assert_eq!(
        agent_cli(&["wait", &id, "--text", "absent", "--timeout", "5"])
            .status
            .code(),
        Some(3)
    );
    cleanup(child, &id);
}

#[test]
fn cli_wait_quiet_ignores_continuous_braille_spinner() {
    let id = unique_id("cli-spinner");
    let child = start_broker(
        &id,
        "while :; do printf '\\rworking ⠋'; sleep 0.03; printf '\\rworking ⠙'; sleep 0.03; done",
    );
    let mut writer = connect(&id, Role::Writer);
    read_until_output_contains(&mut writer, "working", Duration::from_secs(5));
    let started = std::time::Instant::now();
    assert!(
        agent_cli(&["wait", &id, "--quiet", "200", "--timeout", "2"])
            .status
            .success()
    );
    assert!(started.elapsed() >= Duration::from_millis(200));
    assert_eq!(
        agent_cli(&["wait", &id, "--text", "absent", "--timeout", "1"])
            .status
            .code(),
        Some(1)
    );
    cleanup(child, &id);
}

#[test]
fn cli_send_real_broker_preserves_size_separates_submit_and_restores_writer() {
    let id = unique_id("cli-real-send");
    let child = start_broker(
        &id,
        r#"stty raw -echo; python3 -u -c 'import os,time
print("READY",flush=True)
a=os.read(0,4096); t=time.monotonic()
print("FIRST:"+a.hex(),flush=True)
b=os.read(0,4096)
assert a==b"hello" and b==b"\r",(a,b)
assert time.monotonic()-t >= 0.10
assert os.get_terminal_size(0)==(132,40),os.get_terminal_size(0)
print("SEPARATE_SIZE_OK",flush=True)
c=os.read(0,4096)
assert c==b"z",c
print("RESTORED",flush=True)
time.sleep(30)'"#,
    );
    let mut writer = UnixStream::connect(socket_path(&id)).unwrap();
    writer
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    Frame::new(MsgKind::Hello, encode_hello(Role::Writer, 132, 40))
        .write_to(&mut writer)
        .unwrap();
    let ack = Frame::read_from(&mut writer).unwrap().unwrap();
    assert_eq!(decode_hello_ack(&ack.payload).unwrap().1, 132);
    assert!(
        read_until_output_contains(&mut writer, "READY", Duration::from_secs(5)).contains("READY")
    );
    let output = agent_cli(&["send", &id, "hello", "--submit", "--submit-delay", "200"]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        read_until_output_contains(&mut writer, "SEPARATE_SIZE_OK", Duration::from_secs(5))
            .contains("SEPARATE_SIZE_OK")
    );
    // Allow the broker's client read thread to observe EOF and restore ownership.
    std::thread::sleep(Duration::from_millis(100));
    Frame::new(MsgKind::Input, b"z".to_vec())
        .write_to(&mut writer)
        .unwrap();
    assert!(
        read_until_output_contains(&mut writer, "RESTORED", Duration::from_secs(5))
            .contains("RESTORED")
    );
    cleanup(child, &id);
}

#[test]
fn cli_wait_changing_letters_timeout_and_invalid_arguments() {
    let id = unique_id("cli-changing");
    let child = start_broker(
        &id,
        "while :; do printf '\\rA'; sleep 0.03; printf '\\rB'; sleep 0.03; done",
    );
    let mut writer = connect(&id, Role::Writer);
    read_until_output_contains(&mut writer, "A", Duration::from_secs(5));
    assert_eq!(
        agent_cli(&["wait", &id, "--quiet", "200", "--timeout", "1"])
            .status
            .code(),
        Some(1)
    );
    for args in [
        vec!["wait", &id, "--quiet", "0"],
        vec!["wait", &id, "--text", ""],
        vec!["send", &id, "hello", "--submit-delay", "49"],
    ] {
        assert_eq!(agent_cli(&args).status.code(), Some(2));
    }
    cleanup(child, &id);
}

#[test]
fn cli_send_carriage_return_paste_and_drains_noisy_output() {
    cli_peer(
        "cli-cr-paste",
        &["send", "a\rb", "--submit", "--submit-delay", "200"],
        b"\x1b[?2004h",
        |stream| {
            assert_eq!(
                Frame::read_from(stream).unwrap().unwrap().payload,
                b"\x1b[200~a\rb\x1b[201~"
            );
            for _ in 0..512 {
                Frame::new(MsgKind::Output, vec![b'x'; 4096])
                    .write_to(stream)
                    .unwrap();
            }
            assert_eq!(Frame::read_from(stream).unwrap().unwrap().payload, b"\r");
        },
    );
    cli_peer(
        "cli-paste-off",
        &["send", "a\nb"],
        b"\x1b[?2004l",
        |stream| {
            assert_eq!(Frame::read_from(stream).unwrap().unwrap().payload, b"a\nb");
        },
    );
    cli_peer(
        "cli-aliases",
        &["keys", "RETURN", "Escape", "PAGE-UP", "Page-Down", "CTRL-Z"],
        b"",
        |stream| {
            for expected in [b"\r".as_slice(), b"\x1b", b"\x1b[5~", b"\x1b[6~", b"\x1a"] {
                assert_eq!(Frame::read_from(stream).unwrap().unwrap().payload, expected);
            }
        },
    );
}

#[test]
fn cli_wait_matches_final_output_before_exit_and_screen_reports_exit() {
    use std::os::unix::net::UnixListener;
    for command in ["wait", "screen"] {
        let id = unique_id("cli-final-output");
        let path = socket_path(&id);
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && started.elapsed() < Duration::from_secs(5) =>
                    {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("accept timed out or failed: {e}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let hello = Frame::read_from(&mut stream).unwrap().unwrap();
            assert_eq!(decode_hello(&hello.payload).unwrap().0, Role::Monitor);
            Frame::new(MsgKind::HelloAck, encode_hello_ack(123, 80, 24))
                .write_to(&mut stream)
                .unwrap();
            Frame::new(MsgKind::Output, b"finished".to_vec())
                .write_to(&mut stream)
                .unwrap();
            Frame::new(MsgKind::Exit, encode_exit(0))
                .write_to(&mut stream)
                .unwrap();
        });
        let output = if command == "wait" {
            agent_cli(&[command, &id, "--text", "finished"])
        } else {
            agent_cli(&[command, &id])
        };
        server.join().unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(
            output.status.code(),
            Some(if command == "wait" { 0 } else { 3 })
        );
    }
}

#[test]
fn cli_send_restores_old_paste_mode_and_delivers_submit_before_exit() {
    // Model a TUI that enabled bracketed paste once, then produced enough output
    // to evict that DECSET from the ring. Raw bursts must never reach this app.
    for text in ["single line with spaces", "first line\nsecond line"] {
        let id = unique_id("cli-old-paste-mode");
        let child = start_broker(
            &id,
            r#"stty raw -echo; python3 -u -c 'import os,time
os.write(1,b"\x1b[?2004h\x1b[?1h")
os.write(1,b"x"*(1048576+65536))
os.write(1,b"\r\nREADY\r\n\x1b]0;unfinished")
a=os.read(0,4096); t=time.monotonic_ns()
os.write(1,b"TEXT:"+a.hex().encode()+b"\r\n")
b=os.read(0,4096); cr=time.monotonic_ns()
assert a.startswith(b"\x1b[200~") and a.endswith(b"\x1b[201~"),a
assert b==b"\r",b
assert cr-t >= 100000000
os.write(1,b"CR_OK\r\n")
time.sleep(30)'"#,
        );
        let mut writer = connect(&id, Role::Writer);
        let output = read_until_output_contains(&mut writer, "READY", Duration::from_secs(10));
        assert!(output.contains("READY"));
        let result = agent_cli(&["send", &id, text, "--submit", "--submit-delay", "200"]);
        assert!(result.status.success(), "{result:?}");
        let received = read_until_output_contains(&mut writer, "CR_OK", Duration::from_secs(5));
        let expected = format!("\x1b[200~{text}\x1b[201~");
        let hex: String = expected
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(received.contains(&format!("TEXT:{hex}")), "{received}");
        // The child reports success only after checking paste, CR, and their
        // gap on its own clock. Allow the same five-second deadline as sync
        // for the child to run after the broker has acknowledged the PTY write.
        assert!(received.contains("CR_OK"), "{received}");
        cleanup(child, &id);
    }
}
