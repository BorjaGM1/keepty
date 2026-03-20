//! keepty-screen: Agent screen-reading layer for keepty.
//!
//! Connects to a keepty broker as Monitor (or Writer), feeds the raw byte stream
//! into a vt100 terminal parser, and exposes a clean API for reading parsed screen state.
//!
//! # Example
//!
//! ```no_run
//! use keepty_screen::{ScreenReader, Role};
//!
//! let mut reader = ScreenReader::connect("/tmp/keepty-my-session.sock", Role::Monitor)?;
//!
//! // Process incoming frames (non-blocking with timeout)
//! reader.poll(std::time::Duration::from_millis(100))?;
//!
//! // Read the current screen state
//! let contents = reader.contents();
//! let (row, col) = reader.cursor_position();
//! let size = reader.size();
//! # Ok::<(), std::io::Error>(())
//! ```

use keepty_protocol::{self as protocol, Frame, MsgKind};
use std::io;
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// A connection to a keepty broker with integrated screen parsing.
///
/// ScreenReader connects to a broker session, processes Output frames through
/// a vt100 terminal parser, and provides methods to read the parsed screen state.
pub struct ScreenReader {
    stream: UnixStream,
    parser: vt100::Parser,
    role: Role,
    pty_pid: u32,
}

impl ScreenReader {
    /// Connect to a keepty broker at the given socket path.
    ///
    /// The `role` determines what this client can do:
    /// - `Role::Monitor` — read-only, for observation and screen analysis
    /// - `Role::Watcher` — read-only, same as Monitor at the protocol level
    /// - `Role::Writer` — can also send input and resize
    ///
    /// The `cols` and `rows` parameters are sent in the Hello request so the broker
    /// knows the client's requested terminal size. The parser initializes from the
    /// broker's HelloAck dimensions, which may differ from what was requested.
    pub fn connect_with_size(
        socket_path: &str,
        role: Role,
        cols: u16,
        rows: u16,
    ) -> io::Result<Self> {
        let mut stream = UnixStream::connect(socket_path)?;
        stream.set_nonblocking(false)?;

        // Send Hello
        let hello = Frame::new(MsgKind::Hello, protocol::encode_hello(role, cols, rows));
        hello.write_to(&mut stream)?;

        // Read HelloAck
        let ack = Frame::read_from(&mut stream)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "broker closed before HelloAck",
            )
        })?;

        if ack.kind != MsgKind::HelloAck {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected HelloAck, got {:?}", ack.kind),
            ));
        }

        let (pty_pid, ack_cols, ack_rows) =
            protocol::decode_hello_ack(&ack.payload).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid HelloAck payload")
            })?;

        let parser = vt100::Parser::new(ack_rows, ack_cols, 0);

        let mut reader = Self {
            stream,
            parser,
            role,
            pty_pid,
        };

        // Process one immediately queued frame (ring replay Output or a racey ResizeAck).
        reader
            .stream
            .set_read_timeout(Some(Duration::from_secs(2)))?;
        if let Ok(Some(frame)) = Frame::read_from(&mut reader.stream) {
            match frame.kind {
                MsgKind::Output => reader.parser.process(&frame.payload),
                MsgKind::ResizeAck => {
                    reader.apply_resize_ack(&frame.payload)?;
                }
                _ => {}
            }
        }

        Ok(reader)
    }

    /// Connect with default requested terminal size (80x24).
    pub fn connect(socket_path: &str, role: Role) -> io::Result<Self> {
        Self::connect_with_size(socket_path, role, 80, 24)
    }

    fn apply_resize_ack(&mut self, payload: &[u8]) -> io::Result<()> {
        let (_gen, cols, rows) = protocol::decode_resize_ack(payload).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid ResizeAck payload")
        })?;
        self.parser.set_size(rows, cols);
        Ok(())
    }

    /// Process incoming frames for up to `timeout` duration.
    ///
    /// Returns the number of frames that changed local parser state
    /// (Output and ResizeAck). Call this in a loop to keep the screen state up to date.
    pub fn poll(&mut self, timeout: Duration) -> io::Result<usize> {
        self.stream.set_read_timeout(Some(timeout))?;
        let mut count = 0;

        loop {
            match Frame::read_from(&mut self.stream) {
                Ok(Some(frame)) => match frame.kind {
                    MsgKind::Output => {
                        self.parser.process(&frame.payload);
                        count += 1;
                    }
                    MsgKind::ResizeAck => {
                        self.apply_resize_ack(&frame.payload)?;
                        count += 1;
                    }
                    MsgKind::Ping => {
                        let pong = Frame::new(MsgKind::Pong, vec![]);
                        let _ = pong.write_to(&mut self.stream);
                    }
                    MsgKind::Exit => {
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionAborted,
                            "session exited",
                        ));
                    }
                    _ => {}
                },
                Ok(None) => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "broker disconnected",
                    ));
                }
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(e) => return Err(e),
            }
        }

        Ok(count)
    }

    /// Get the full text content of the screen (all rows joined with newlines).
    pub fn contents(&self) -> String {
        self.parser.screen().contents()
    }

    /// Get the text content of a specific row (0-indexed).
    pub fn row_text(&self, row: u16) -> String {
        self.parser.screen().rows(row, 1).next().unwrap_or_default()
    }

    /// Get the cursor position as (row, col), both 0-indexed.
    pub fn cursor_position(&self) -> (u16, u16) {
        let screen = self.parser.screen();
        (screen.cursor_position().0, screen.cursor_position().1)
    }

    /// Get the parser's current terminal size as (rows, cols).
    /// This tracks the broker's acknowledged size from HelloAck and ResizeAck.
    pub fn size(&self) -> (u16, u16) {
        let screen = self.parser.screen();
        (screen.size().0, screen.size().1)
    }

    /// Get the PID of the process running in the PTY.
    pub fn pty_pid(&self) -> u32 {
        self.pty_pid
    }

    /// Get the role this client connected with.
    pub fn role(&self) -> Role {
        self.role
    }

    /// Get direct access to the underlying vt100 screen for advanced queries.
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Send raw keystrokes to the PTY. Only works if connected as Writer.
    pub fn send_keys(&mut self, keys: &[u8]) -> io::Result<()> {
        if self.role != Role::Writer {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "only Writer role can send input",
            ));
        }
        let frame = Frame::new(MsgKind::Input, keys.to_vec());
        frame.write_to(&mut self.stream)
    }

    /// Send a resize request. Only works if connected as Writer.
    /// The parser size updates when poll() receives the broker's ResizeAck.
    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        if self.role != Role::Writer {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "only Writer role can resize",
            ));
        }
        let frame = Frame::new(MsgKind::Resize, protocol::encode_resize(cols, rows));
        frame.write_to(&mut self.stream)
    }

    /// Request the broker to shut down the session.
    pub fn shutdown(&mut self) -> io::Result<()> {
        let frame = Frame::new(MsgKind::Shutdown, vec![]);
        frame.write_to(&mut self.stream)
    }
}

// Re-export from protocol for convenience
pub use keepty_protocol;
pub use keepty_protocol::Role;

#[cfg(test)]
mod tests {
    use super::{Role, ScreenReader};
    use keepty_protocol::{self as protocol, Frame, MsgKind};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    /// Helper: create a ScreenReader + peer socket without a broker.
    fn reader_with_peer(role: Role, cols: u16, rows: u16) -> (ScreenReader, UnixStream) {
        let (stream, peer) = UnixStream::pair().unwrap();
        (
            ScreenReader {
                stream,
                parser: vt100::Parser::new(rows, cols, 0),
                role,
                pty_pid: 1234,
            },
            peer,
        )
    }

    /// Helper: create a vt100 parser and process bytes, simulating what
    /// ScreenReader does internally when it receives Output frames.
    fn parse_output(cols: u16, rows: u16, data: &[u8]) -> vt100::Parser {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(data);
        parser
    }

    #[test]
    fn plain_text_appears_on_screen() {
        let parser = parse_output(80, 24, b"Hello, world!");
        let contents = parser.screen().contents();
        assert!(contents.contains("Hello, world!"));
    }

    #[test]
    fn cursor_moves_with_output() {
        let parser = parse_output(80, 24, b"AB");
        let (row, col) = parser.screen().cursor_position();
        assert_eq!(row, 0);
        assert_eq!(col, 2);
    }

    #[test]
    fn newline_moves_to_next_row() {
        let parser = parse_output(80, 24, b"line1\r\nline2");
        let contents = parser.screen().contents();
        assert!(contents.contains("line1"));
        assert!(contents.contains("line2"));
        let (row, _) = parser.screen().cursor_position();
        assert_eq!(row, 1);
    }

    #[test]
    fn ansi_escape_sequences_parsed_correctly() {
        // Bold + text + reset
        let parser = parse_output(80, 24, b"\x1b[1mBold\x1b[0m Normal");
        let contents = parser.screen().contents();
        assert!(contents.contains("Bold"));
        assert!(contents.contains("Normal"));
    }

    #[test]
    fn cursor_positioning_escape() {
        // Move cursor to row 3, col 5 (1-indexed in ANSI)
        let parser = parse_output(80, 24, b"\x1b[3;5Hhere");
        let contents = parser.screen().contents();
        assert!(contents.contains("here"));
        // After writing "here" at (2,4) 0-indexed, cursor is at (2,8)
        let (row, col) = parser.screen().cursor_position();
        assert_eq!(row, 2);
        assert_eq!(col, 8);
    }

    #[test]
    fn screen_clear_works() {
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"old content");
        parser.process(b"\x1b[2J\x1b[H"); // clear screen + home
        parser.process(b"new content");
        let contents = parser.screen().contents();
        assert!(contents.contains("new content"));
        assert!(!contents.contains("old content"));
    }

    #[test]
    fn tui_box_drawing_characters() {
        // Unicode box drawing (common in TUIs like vim, htop)
        let parser = parse_output(80, 24, "┌──────┐\r\n│ test │\r\n└──────┘".as_bytes());
        let contents = parser.screen().contents();
        assert!(contents.contains("┌──────┐"));
        assert!(contents.contains("│ test │"));
        assert!(contents.contains("└──────┘"));
    }

    #[test]
    fn screen_size_matches_constructor() {
        let parser = parse_output(132, 50, b"");
        let (rows, cols) = parser.screen().size();
        assert_eq!(rows, 50);
        assert_eq!(cols, 132);
    }

    #[test]
    fn incremental_output_accumulates() {
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"first ");
        parser.process(b"second ");
        parser.process(b"third");
        let contents = parser.screen().contents();
        assert!(contents.contains("first second third"));
    }

    #[test]
    fn color_output_preserves_text() {
        // Red text: \x1b[31m
        let parser = parse_output(80, 24, b"\x1b[31mError\x1b[0m: something broke");
        let contents = parser.screen().contents();
        assert!(contents.contains("Error: something broke"));
    }

    #[test]
    fn multiline_content_readable() {
        let parser = parse_output(80, 24, b"line0\r\nline1\r\nline2");
        let contents = parser.screen().contents();
        assert!(contents.contains("line0"));
        assert!(contents.contains("line1"));
        assert!(contents.contains("line2"));
    }

    // --- Resize/HelloAck tests ---

    #[test]
    fn connect_uses_hello_ack_dimensions_for_parser_size() {
        // Simulate the handshake using UnixStream::pair() and manual construction.
        // We can't use connect_with_size() directly in unit tests (needs a real socket),
        // so we test the core logic: parser init uses HelloAck dims, not requested dims.
        let (stream, mut broker) = UnixStream::pair().unwrap();

        // Simulate broker side in a thread
        let server = std::thread::spawn(move || {
            // Read Hello
            let hello = Frame::read_from(&mut broker).unwrap().unwrap();
            assert_eq!(hello.kind, MsgKind::Hello);
            let (role, cols, rows) = protocol::decode_hello(&hello.payload).unwrap();
            assert_eq!(role, Role::Monitor);
            assert_eq!(cols, 80);
            assert_eq!(rows, 24);

            // Reply with different dimensions than requested
            let ack = Frame::new(MsgKind::HelloAck, protocol::encode_hello_ack(4321, 132, 50));
            ack.write_to(&mut broker).unwrap();
        });

        // Client side: manually do what connect_with_size does
        let mut stream = stream;
        stream.set_nonblocking(false).unwrap();
        let hello = Frame::new(
            MsgKind::Hello,
            protocol::encode_hello(Role::Monitor, 80, 24),
        );
        hello.write_to(&mut stream).unwrap();

        let ack = Frame::read_from(&mut stream).unwrap().unwrap();
        assert_eq!(ack.kind, MsgKind::HelloAck);
        let (pty_pid, ack_cols, ack_rows) = protocol::decode_hello_ack(&ack.payload).unwrap();

        // Key assertion: parser should use HelloAck dims, not requested dims
        let parser = vt100::Parser::new(ack_rows, ack_cols, 0);
        assert_eq!(pty_pid, 4321);
        assert_eq!(parser.screen().size(), (50, 132));

        server.join().unwrap();
    }

    #[test]
    fn connect_applies_immediate_resize_ack_after_hello_ack() {
        let (stream, mut broker) = UnixStream::pair().unwrap();

        let server = std::thread::spawn(move || {
            let _hello = Frame::read_from(&mut broker).unwrap().unwrap();

            let ack = Frame::new(MsgKind::HelloAck, protocol::encode_hello_ack(4321, 80, 24));
            ack.write_to(&mut broker).unwrap();

            // Send ResizeAck immediately after HelloAck
            let resize_ack =
                Frame::new(MsgKind::ResizeAck, protocol::encode_resize_ack(1, 132, 50));
            resize_ack.write_to(&mut broker).unwrap();
        });

        // Client side: do the handshake manually
        let mut stream = stream;
        stream.set_nonblocking(false).unwrap();
        let hello = Frame::new(
            MsgKind::Hello,
            protocol::encode_hello(Role::Monitor, 80, 24),
        );
        hello.write_to(&mut stream).unwrap();

        let ack = Frame::read_from(&mut stream).unwrap().unwrap();
        let (_pid, ack_cols, ack_rows) = protocol::decode_hello_ack(&ack.payload).unwrap();
        let mut parser = vt100::Parser::new(ack_rows, ack_cols, 0);
        assert_eq!(parser.screen().size(), (24, 80)); // starts at HelloAck size

        // Read the immediate ResizeAck (data is already buffered from the server thread)
        let frame = Frame::read_from(&mut stream).unwrap().unwrap();
        assert_eq!(frame.kind, MsgKind::ResizeAck);
        let (_gen, cols, rows) = protocol::decode_resize_ack(&frame.payload).unwrap();
        parser.set_size(rows, cols);

        // Parser should now reflect the ResizeAck dimensions
        assert_eq!(parser.screen().size(), (50, 132));

        server.join().unwrap();
    }

    #[test]
    fn poll_applies_resize_ack_to_parser_size() {
        let (mut reader, mut peer) = reader_with_peer(Role::Monitor, 80, 24);
        assert_eq!(reader.size(), (24, 80));

        let ack = Frame::new(MsgKind::ResizeAck, protocol::encode_resize_ack(7, 120, 40));
        ack.write_to(&mut peer).unwrap();

        let count = reader.poll(Duration::from_millis(100)).unwrap();
        assert_eq!(count, 1);
        assert_eq!(reader.size(), (40, 120));
    }

    #[test]
    fn resize_does_not_change_parser_size_before_ack() {
        let (mut reader, mut peer) = reader_with_peer(Role::Writer, 80, 24);

        // resize() sends frame but doesn't change parser size
        reader.resize(120, 40).unwrap();
        assert_eq!(reader.size(), (24, 80));

        // Verify the Resize frame was sent
        let frame = Frame::read_from(&mut peer).unwrap().unwrap();
        assert_eq!(frame.kind, MsgKind::Resize);
        let (cols, rows) = protocol::decode_resize(&frame.payload).unwrap();
        assert_eq!(cols, 120);
        assert_eq!(rows, 40);
    }
}
