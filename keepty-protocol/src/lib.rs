//! keepty wire protocol
//!
//! Length-prefixed binary frames over Unix sockets.
//! Frame: [u32 len (BE)][u8 version][u8 kind][payload...]
//!
//! Zero external dependencies — implement a client in any language
//! by following this specification.

use std::io::{self, Read, Write};

pub const PROTOCOL_VERSION: u8 = 1;

/// Deprecated: use socket_dir() for the runtime-resolved directory.
/// Kept for backward compatibility with code that imports this constant.
pub const SOCKET_DIR: &str = "/tmp";

/// Maximum payload size (1MB, matches ring buffer capacity).
pub const MAX_PAYLOAD_SIZE: usize = 1_048_576;
/// Maximum frame body size (payload + version byte + kind byte).
pub const MAX_FRAME_BODY_SIZE: usize = MAX_PAYLOAD_SIZE + 2;

/// Client role when connecting to a keepty broker.
///
/// Roles are negotiated in the Hello handshake — the client declares
/// what it wants to do, and the broker enforces access accordingly.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Exclusive control: can send input and resize the terminal.
    /// Only one Writer at a time per broker session.
    Writer = 1,
    /// Read-only: receives the output stream but cannot send input.
    /// Multiple Watchers can connect simultaneously.
    Watcher = 2,
    /// Server-side observation: like Watcher, but intended for
    /// programmatic screen analysis (agents, health checks).
    Monitor = 3,
}

/// Message types in the keepty protocol.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgKind {
    /// Client handshake. Payload: [role: u8, cols: u16 BE, rows: u16 BE]
    Hello = 1,
    /// Broker acknowledgement. Payload: [pty_pid: u32 BE, cols: u16 BE, rows: u16 BE]
    HelloAck = 2,
    /// Raw keystroke data from client to PTY. Payload: raw bytes.
    Input = 3,
    /// Raw PTY output bytes broadcast to all clients. Payload: raw bytes.
    Output = 4,
    /// Terminal resize. Payload: [cols: u16 BE, rows: u16 BE]
    Resize = 5,
    /// Broker acknowledgement of resize. Payload: [resize_gen: u32 BE, cols: u16 BE, rows: u16 BE]
    /// Broadcast to all clients after the broker applies the resize to the PTY.
    /// Acts as an in-band fence: output before this was old geometry.
    ResizeAck = 6,
    /// Process exit. Payload: [exit_code: i32 BE]
    Exit = 10,
    /// Request graceful shutdown.
    Shutdown = 11,
    /// Connection keepalive (client -> broker).
    Ping = 12,
    /// Keepalive response (broker -> client).
    Pong = 13,
    /// Error message. Payload: UTF-8 error string.
    Error = 127,
}

impl TryFrom<u8> for MsgKind {
    type Error = u8;
    fn try_from(v: u8) -> Result<Self, u8> {
        match v {
            1 => Ok(Self::Hello),
            2 => Ok(Self::HelloAck),
            3 => Ok(Self::Input),
            4 => Ok(Self::Output),
            5 => Ok(Self::Resize),
            6 => Ok(Self::ResizeAck),
            10 => Ok(Self::Exit),
            11 => Ok(Self::Shutdown),
            12 => Ok(Self::Ping),
            13 => Ok(Self::Pong),
            127 => Ok(Self::Error),
            other => Err(other),
        }
    }
}

impl TryFrom<u8> for Role {
    type Error = u8;
    fn try_from(v: u8) -> Result<Self, u8> {
        match v {
            1 => Ok(Self::Writer),
            2 => Ok(Self::Watcher),
            3 => Ok(Self::Monitor),
            other => Err(other),
        }
    }
}

/// Resolve the runtime socket directory.
/// Priority: $XDG_RUNTIME_DIR/keepty → $TMPDIR/keepty (via std::env::temp_dir)
///
/// On macOS, $TMPDIR is already per-user (/var/folders/.../T/).
/// On Linux, $XDG_RUNTIME_DIR is per-user (/run/user/$UID).
/// No external dependencies — uses only std.
pub fn socket_dir() -> String {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        if !xdg.is_empty() {
            return format!("{}/keepty", xdg);
        }
    }
    let tmp = std::env::temp_dir();
    format!("{}/keepty", tmp.to_string_lossy().trim_end_matches('/'))
}

/// Construct the broker socket path for a session.
pub fn socket_path(session_id: &str) -> String {
    format!("{}/keepty-{}.sock", socket_dir(), session_id)
}

/// A parsed keepty protocol frame.
#[derive(Debug)]
pub struct Frame {
    pub kind: MsgKind,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(kind: MsgKind, payload: Vec<u8>) -> Self {
        Self { kind, payload }
    }

    /// Encode frame to wire format: [u32 len BE][u8 version][u8 kind][payload]
    pub fn encode(&self) -> Vec<u8> {
        let payload_len = self.payload.len();
        let frame_len = 2 + payload_len; // version + kind + payload
        let mut buf = Vec::with_capacity(4 + frame_len);
        buf.extend_from_slice(&(frame_len as u32).to_be_bytes());
        buf.push(PROTOCOL_VERSION);
        buf.push(self.kind as u8);
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Read one frame from a reader. Returns None on EOF.
    pub fn read_from<R: Read>(reader: &mut R) -> io::Result<Option<Self>> {
        let mut len_buf = [0u8; 4];
        match reader.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let frame_len = u32::from_be_bytes(len_buf) as usize;
        if frame_len < 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame too short",
            ));
        }
        if frame_len > MAX_FRAME_BODY_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "frame too large: {} bytes (max {})",
                    frame_len, MAX_FRAME_BODY_SIZE
                ),
            ));
        }
        let mut data = vec![0u8; frame_len];
        reader.read_exact(&mut data)?;
        let version = data[0];
        if version != PROTOCOL_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported protocol version: {} (expected {})",
                    version, PROTOCOL_VERSION
                ),
            ));
        }
        let kind = MsgKind::try_from(data[1]).map_err(|v| {
            io::Error::new(io::ErrorKind::InvalidData, format!("unknown kind: {}", v))
        })?;
        let payload = data[2..].to_vec();
        Ok(Some(Frame { kind, payload }))
    }

    /// Write this frame to a writer.
    pub fn write_to<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        writer.write_all(&self.encode())?;
        writer.flush()
    }
}

// --- Hello message helpers ---

pub fn encode_hello(role: Role, cols: u16, rows: u16) -> Vec<u8> {
    let mut payload = Vec::with_capacity(5);
    payload.push(role as u8);
    payload.extend_from_slice(&cols.to_be_bytes());
    payload.extend_from_slice(&rows.to_be_bytes());
    payload
}

pub fn decode_hello(payload: &[u8]) -> Option<(Role, u16, u16)> {
    if payload.len() < 5 {
        return None;
    }
    let role = Role::try_from(payload[0]).ok()?;
    let cols = u16::from_be_bytes([payload[1], payload[2]]);
    let rows = u16::from_be_bytes([payload[3], payload[4]]);
    Some((role, cols, rows))
}

// --- HelloAck helpers ---

pub fn encode_hello_ack(pty_pid: u32, cols: u16, rows: u16) -> Vec<u8> {
    let mut payload = Vec::with_capacity(8);
    payload.extend_from_slice(&pty_pid.to_be_bytes());
    payload.extend_from_slice(&cols.to_be_bytes());
    payload.extend_from_slice(&rows.to_be_bytes());
    payload
}

pub fn decode_hello_ack(payload: &[u8]) -> Option<(u32, u16, u16)> {
    if payload.len() < 8 {
        return None;
    }
    let pid = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let cols = u16::from_be_bytes([payload[4], payload[5]]);
    let rows = u16::from_be_bytes([payload[6], payload[7]]);
    Some((pid, cols, rows))
}

// --- Resize helpers ---

pub fn encode_resize(cols: u16, rows: u16) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4);
    payload.extend_from_slice(&cols.to_be_bytes());
    payload.extend_from_slice(&rows.to_be_bytes());
    payload
}

pub fn decode_resize(payload: &[u8]) -> Option<(u16, u16)> {
    if payload.len() < 4 {
        return None;
    }
    let cols = u16::from_be_bytes([payload[0], payload[1]]);
    let rows = u16::from_be_bytes([payload[2], payload[3]]);
    Some((cols, rows))
}

// --- ResizeAck helpers ---

pub fn encode_resize_ack(gen: u32, cols: u16, rows: u16) -> Vec<u8> {
    let mut payload = Vec::with_capacity(8);
    payload.extend_from_slice(&gen.to_be_bytes());
    payload.extend_from_slice(&cols.to_be_bytes());
    payload.extend_from_slice(&rows.to_be_bytes());
    payload
}

pub fn decode_resize_ack(payload: &[u8]) -> Option<(u32, u16, u16)> {
    if payload.len() < 8 {
        return None;
    }
    let gen = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let cols = u16::from_be_bytes([payload[4], payload[5]]);
    let rows = u16::from_be_bytes([payload[6], payload[7]]);
    Some((gen, cols, rows))
}

// --- Exit helpers ---

pub fn encode_exit(code: i32) -> Vec<u8> {
    code.to_be_bytes().to_vec()
}

pub fn decode_exit(payload: &[u8]) -> Option<i32> {
    if payload.len() < 4 {
        return None;
    }
    Some(i32::from_be_bytes([
        payload[0], payload[1], payload[2], payload[3],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let frame = Frame::new(MsgKind::Output, b"hello world".to_vec());
        let encoded = frame.encode();
        let mut cursor = std::io::Cursor::new(encoded);
        let decoded = Frame::read_from(&mut cursor).unwrap().unwrap();
        assert_eq!(decoded.kind, MsgKind::Output);
        assert_eq!(decoded.payload, b"hello world");
    }

    #[test]
    fn hello_roundtrip() {
        let payload = encode_hello(Role::Writer, 132, 51);
        let (role, cols, rows) = decode_hello(&payload).unwrap();
        assert_eq!(role, Role::Writer);
        assert_eq!(cols, 132);
        assert_eq!(rows, 51);
    }

    #[test]
    fn hello_ack_roundtrip() {
        let payload = encode_hello_ack(12345, 80, 24);
        let (pid, cols, rows) = decode_hello_ack(&payload).unwrap();
        assert_eq!(pid, 12345);
        assert_eq!(cols, 80);
        assert_eq!(rows, 24);
    }

    #[test]
    fn resize_roundtrip() {
        let payload = encode_resize(80, 24);
        let (cols, rows) = decode_resize(&payload).unwrap();
        assert_eq!(cols, 80);
        assert_eq!(rows, 24);
    }

    #[test]
    fn exit_roundtrip() {
        let payload = encode_exit(42);
        let code = decode_exit(&payload).unwrap();
        assert_eq!(code, 42);
    }

    #[test]
    fn socket_path_format() {
        let path = socket_path("abc123");
        assert!(path.ends_with("/keepty-abc123.sock"), "path: {}", path);
        assert!(
            path.contains("/keepty"),
            "path should contain /keepty dir: {}",
            path
        );
    }

    #[test]
    fn eof_returns_none() {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        let result = Frame::read_from(&mut cursor).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn all_roles_roundtrip() {
        for role in [Role::Writer, Role::Watcher, Role::Monitor] {
            let v = role as u8;
            assert_eq!(Role::try_from(v).unwrap(), role);
        }
    }

    #[test]
    fn all_msg_kinds_roundtrip() {
        for kind in [
            MsgKind::Hello,
            MsgKind::HelloAck,
            MsgKind::Input,
            MsgKind::Output,
            MsgKind::Resize,
            MsgKind::ResizeAck,
            MsgKind::Exit,
            MsgKind::Shutdown,
            MsgKind::Ping,
            MsgKind::Pong,
            MsgKind::Error,
        ] {
            let v = kind as u8;
            assert_eq!(MsgKind::try_from(v).unwrap(), kind);
        }
    }

    #[test]
    fn invalid_role_returns_err() {
        assert!(Role::try_from(0).is_err());
        assert!(Role::try_from(4).is_err());
        assert!(Role::try_from(255).is_err());
    }

    #[test]
    fn resize_ack_roundtrip() {
        let payload = encode_resize_ack(42, 120, 40);
        let (gen, cols, rows) = decode_resize_ack(&payload).unwrap();
        assert_eq!(gen, 42);
        assert_eq!(cols, 120);
        assert_eq!(rows, 40);
    }

    #[test]
    fn invalid_msg_kind_returns_err() {
        assert!(MsgKind::try_from(0).is_err());
        assert!(MsgKind::try_from(7).is_err());
        assert!(MsgKind::try_from(128).is_err());
    }

    #[test]
    fn frame_too_short_is_error() {
        // Length field says 1 byte, but we need at least 2 (version + kind)
        let data = vec![0, 0, 0, 1, 0xFF];
        let mut cursor = std::io::Cursor::new(data);
        assert!(Frame::read_from(&mut cursor).is_err());
    }

    #[test]
    fn empty_payload_frame() {
        let frame = Frame::new(MsgKind::Ping, vec![]);
        let encoded = frame.encode();
        let mut cursor = std::io::Cursor::new(encoded);
        let decoded = Frame::read_from(&mut cursor).unwrap().unwrap();
        assert_eq!(decoded.kind, MsgKind::Ping);
        assert!(decoded.payload.is_empty());
    }

    #[test]
    fn oversized_frame_rejected() {
        // Frame claiming to be 2MB — should be rejected without allocating
        let len = (MAX_FRAME_BODY_SIZE + 1) as u32;
        let mut data = len.to_be_bytes().to_vec();
        data.push(PROTOCOL_VERSION);
        data.push(MsgKind::Output as u8);
        let mut cursor = std::io::Cursor::new(data);
        let err = Frame::read_from(&mut cursor).unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn max_allowed_frame_accepted() {
        // Frame at exactly MAX_FRAME_BODY_SIZE should be accepted
        let payload = vec![0u8; MAX_PAYLOAD_SIZE];
        let frame = Frame::new(MsgKind::Output, payload);
        let encoded = frame.encode();
        let mut cursor = std::io::Cursor::new(encoded);
        let decoded = Frame::read_from(&mut cursor).unwrap().unwrap();
        assert_eq!(decoded.kind, MsgKind::Output);
        assert_eq!(decoded.payload.len(), MAX_PAYLOAD_SIZE);
    }

    #[test]
    fn wrong_version_rejected() {
        // Valid frame structure but wrong protocol version
        let mut data = Vec::new();
        let frame_len: u32 = 3; // version + kind + 1 byte payload
        data.extend_from_slice(&frame_len.to_be_bytes());
        data.push(99); // wrong version
        data.push(MsgKind::Ping as u8);
        data.push(0); // payload byte
        let mut cursor = std::io::Cursor::new(data);
        let err = Frame::read_from(&mut cursor).unwrap_err();
        assert!(err.to_string().contains("unsupported protocol version"));
    }
}
