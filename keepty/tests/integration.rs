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
    let child = start_broker(&id, "echo replay-data-here; sleep 30");

    // First client: connect, receive output, disconnect
    {
        let mut stream1 = connect(&id, Role::Writer);
        let output =
            read_until_output_contains(&mut stream1, "replay-data-here", Duration::from_secs(5));
        assert!(output.contains("replay-data-here"));
    }

    // Wait for disconnect to be processed
    std::thread::sleep(Duration::from_millis(500));

    // Second client: should get ring buffer replay (Monitor gets replay, Watcher doesn't)
    let mut stream2 = connect(&id, Role::Monitor);
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
