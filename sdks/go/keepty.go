// Package keepty provides a Go client for the keepty persistent terminal protocol.
//
// Usage:
//
//	conn, err := keepty.Connect("my-session", keepty.RoleWriter, 80, 24)
//	conn.SendKeys([]byte("echo hello\n"))
//	output, _ := conn.ReadOutput(time.Second)
//	conn.Close()
package keepty

import (
	"encoding/binary"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
	"time"
)

const (
	ProtocolVersion  = 1
	MaxPayloadSize   = 1_048_576         // 1MB
	MaxFrameBodySize = MaxPayloadSize + 2 // payload + version + kind
)

// Deprecated: use SocketDirResolved() instead.
const SocketDir = "/tmp"

// SocketDirResolved returns the runtime socket directory.
// Priority: $XDG_RUNTIME_DIR/keepty → os.TempDir()/keepty
func SocketDirResolved() string {
	if xdg := os.Getenv("XDG_RUNTIME_DIR"); xdg != "" {
		return filepath.Join(xdg, "keepty")
	}
	return filepath.Join(os.TempDir(), "keepty")
}

// Role defines the client's access level.
type Role uint8

const (
	RoleWriter  Role = 1 // Exclusive: can send input + resize
	RoleWatcher Role = 2 // Read-only: receives output broadcast
	RoleMonitor Role = 3 // Read-only: for programmatic screen analysis
)

// MsgKind identifies the type of a protocol frame.
type MsgKind uint8

const (
	MsgHello    MsgKind = 1
	MsgHelloAck MsgKind = 2
	MsgInput    MsgKind = 3
	MsgOutput   MsgKind = 4
	MsgResize    MsgKind = 5
	MsgResizeAck MsgKind = 6
	MsgExit      MsgKind = 10
	MsgShutdown MsgKind = 11
	MsgPing     MsgKind = 12
	MsgPong     MsgKind = 13
	MsgError    MsgKind = 127
)

// Frame is a keepty protocol frame.
type Frame struct {
	Kind    MsgKind
	Payload []byte
}

// SocketPath returns the broker socket path for a session.
func SocketPath(sessionID string) string {
	return filepath.Join(SocketDirResolved(), fmt.Sprintf("keepty-%s.sock", sessionID))
}

// EncodeFrame encodes a frame to wire format.
func EncodeFrame(kind MsgKind, payload []byte) []byte {
	frameLen := 2 + len(payload) // version + kind + payload
	buf := make([]byte, 4+frameLen)
	binary.BigEndian.PutUint32(buf[0:4], uint32(frameLen))
	buf[4] = ProtocolVersion
	buf[5] = byte(kind)
	copy(buf[6:], payload)
	return buf
}

// ReadFrame reads one frame from a reader. Returns nil on EOF.
func ReadFrame(r io.Reader) (*Frame, error) {
	lenBuf := make([]byte, 4)
	if _, err := io.ReadFull(r, lenBuf); err != nil {
		if err == io.EOF || err == io.ErrUnexpectedEOF {
			return nil, nil
		}
		return nil, err
	}
	frameLen := binary.BigEndian.Uint32(lenBuf)
	if frameLen < 2 {
		return nil, fmt.Errorf("frame too short: %d", frameLen)
	}
	if frameLen > MaxFrameBodySize {
		return nil, fmt.Errorf("frame too large: %d bytes (max %d)", frameLen, MaxFrameBodySize)
	}
	data := make([]byte, frameLen)
	if _, err := io.ReadFull(r, data); err != nil {
		return nil, err
	}
	version := data[0]
	if version != ProtocolVersion {
		return nil, fmt.Errorf("unsupported protocol version: %d (expected %d)", version, ProtocolVersion)
	}
	kind := MsgKind(data[1])
	payload := data[2:]
	return &Frame{Kind: kind, Payload: payload}, nil
}

// EncodeHello encodes a Hello message payload.
func EncodeHello(role Role, cols, rows uint16) []byte {
	buf := make([]byte, 5)
	buf[0] = byte(role)
	binary.BigEndian.PutUint16(buf[1:3], cols)
	binary.BigEndian.PutUint16(buf[3:5], rows)
	return buf
}

// DecodeHelloAck decodes a HelloAck payload.
func DecodeHelloAck(payload []byte) (pid uint32, cols, rows uint16) {
	pid = binary.BigEndian.Uint32(payload[0:4])
	cols = binary.BigEndian.Uint16(payload[4:6])
	rows = binary.BigEndian.Uint16(payload[6:8])
	return
}

// EncodeResize encodes a Resize message payload.
func EncodeResize(cols, rows uint16) []byte {
	buf := make([]byte, 4)
	binary.BigEndian.PutUint16(buf[0:2], cols)
	binary.BigEndian.PutUint16(buf[2:4], rows)
	return buf
}

// DecodeHello decodes a Hello message payload.
func DecodeHello(payload []byte) (role Role, cols, rows uint16) {
	role = Role(payload[0])
	cols = binary.BigEndian.Uint16(payload[1:3])
	rows = binary.BigEndian.Uint16(payload[3:5])
	return
}

// EncodeResizeAck encodes a ResizeAck message payload.
func EncodeResizeAck(gen uint32, cols, rows uint16) []byte {
	buf := make([]byte, 8)
	binary.BigEndian.PutUint32(buf[0:4], gen)
	binary.BigEndian.PutUint16(buf[4:6], cols)
	binary.BigEndian.PutUint16(buf[6:8], rows)
	return buf
}

// DecodeResizeAck decodes a ResizeAck payload.
func DecodeResizeAck(payload []byte) (gen uint32, cols, rows uint16) {
	gen = binary.BigEndian.Uint32(payload[0:4])
	cols = binary.BigEndian.Uint16(payload[4:6])
	rows = binary.BigEndian.Uint16(payload[6:8])
	return
}

// DecodeExit decodes an Exit message payload.
func DecodeExit(payload []byte) int32 {
	return int32(binary.BigEndian.Uint32(payload[0:4]))
}

// Session is a connection to a keepty broker.
type Session struct {
	conn      net.Conn
	Role      Role
	PtyPid    uint32
	SessionID string
}

// Connect connects to a running keepty broker session.
func Connect(sessionID string, role Role, cols, rows uint16) (*Session, error) {
	path := SocketPath(sessionID)
	if _, err := os.Stat(path); os.IsNotExist(err) {
		return nil, fmt.Errorf("session '%s' is not running (no socket at %s)", sessionID, path)
	}

	conn, err := net.Dial("unix", path)
	if err != nil {
		return nil, fmt.Errorf("failed to connect: %w", err)
	}

	// Send Hello
	hello := EncodeFrame(MsgHello, EncodeHello(role, cols, rows))
	if _, err := conn.Write(hello); err != nil {
		conn.Close()
		return nil, fmt.Errorf("failed to send Hello: %w", err)
	}

	// Read HelloAck
	conn.SetReadDeadline(time.Now().Add(5 * time.Second))
	ack, err := ReadFrame(conn)
	if err != nil {
		conn.Close()
		return nil, fmt.Errorf("failed to read HelloAck: %w", err)
	}
	if ack == nil {
		conn.Close()
		return nil, fmt.Errorf("broker closed before HelloAck")
	}
	if ack.Kind != MsgHelloAck {
		conn.Close()
		return nil, fmt.Errorf("expected HelloAck, got %d", ack.Kind)
	}

	pid, _, _ := DecodeHelloAck(ack.Payload)
	conn.SetReadDeadline(time.Time{}) // clear deadline

	return &Session{
		conn:      conn,
		Role:      role,
		PtyPid:    pid,
		SessionID: sessionID,
	}, nil
}

// ReadOutput reads output frames for up to the given timeout.
func (s *Session) ReadOutput(timeout time.Duration) ([]byte, error) {
	s.conn.SetReadDeadline(time.Now().Add(timeout))
	var collected []byte

	for {
		frame, err := ReadFrame(s.conn)
		if err != nil {
			if netErr, ok := err.(net.Error); ok && netErr.Timeout() {
				break
			}
			return collected, err
		}
		if frame == nil {
			break
		}

		switch frame.Kind {
		case MsgOutput:
			collected = append(collected, frame.Payload...)
		case MsgPing:
			s.conn.Write(EncodeFrame(MsgPong, nil))
		case MsgExit:
			code := DecodeExit(frame.Payload)
			return collected, &SessionExitedError{ExitCode: code}
		}
	}

	return collected, nil
}

// ReadUntil reads output until the expected string appears.
func (s *Session) ReadUntil(expected string, timeout time.Duration) (string, error) {
	deadline := time.Now().Add(timeout)
	var collected string

	for time.Now().Before(deadline) {
		remaining := time.Until(deadline)
		if remaining < 50*time.Millisecond {
			remaining = 50 * time.Millisecond
		}
		chunk := remaining
		if chunk > 200*time.Millisecond {
			chunk = 200 * time.Millisecond
		}
		data, err := s.ReadOutput(chunk)
		collected += string(data)
		if err != nil {
			return collected, err
		}
		if contains(collected, expected) {
			return collected, nil
		}
	}

	return collected, fmt.Errorf("expected '%s' not found within %v", expected, timeout)
}

// SendKeys sends raw keystrokes. Writer only.
func (s *Session) SendKeys(keys []byte) error {
	if s.Role != RoleWriter {
		return fmt.Errorf("only Writer role can send input")
	}
	_, err := s.conn.Write(EncodeFrame(MsgInput, keys))
	return err
}

// Resize sends a terminal resize. Writer only.
func (s *Session) Resize(cols, rows uint16) error {
	if s.Role != RoleWriter {
		return fmt.Errorf("only Writer role can resize")
	}
	_, err := s.conn.Write(EncodeFrame(MsgResize, EncodeResize(cols, rows)))
	return err
}

// Shutdown requests the broker to terminate the session.
func (s *Session) Shutdown() error {
	_, err := s.conn.Write(EncodeFrame(MsgShutdown, nil))
	return err
}

// Close closes the connection.
func (s *Session) Close() error {
	return s.conn.Close()
}

// SessionExitedError is returned when the session's process exits.
type SessionExitedError struct {
	ExitCode int32
}

func (e *SessionExitedError) Error() string {
	return fmt.Sprintf("session exited with code %d", e.ExitCode)
}

func contains(s, substr string) bool {
	return len(substr) > 0 && len(s) >= len(substr) && (s == substr || len(s) > 0 && containsStr(s, substr))
}

func containsStr(s, substr string) bool {
	for i := 0; i <= len(s)-len(substr); i++ {
		if s[i:i+len(substr)] == substr {
			return true
		}
	}
	return false
}
