package keepty

import (
	"bytes"
	"testing"
)

func TestFrameRoundtrip(t *testing.T) {
	payload := []byte("hello world")
	encoded := EncodeFrame(MsgOutput, payload)
	frame, err := ReadFrame(bytes.NewReader(encoded))
	if err != nil {
		t.Fatalf("ReadFrame: %v", err)
	}
	if frame.Kind != MsgOutput {
		t.Errorf("kind = %d, want %d", frame.Kind, MsgOutput)
	}
	if !bytes.Equal(frame.Payload, payload) {
		t.Errorf("payload = %q, want %q", frame.Payload, payload)
	}
}

func TestHelloRoundtrip(t *testing.T) {
	payload := EncodeHello(RoleWriter, 132, 51)
	if payload[0] != byte(RoleWriter) {
		t.Errorf("role = %d, want %d", payload[0], RoleWriter)
	}
	// Verify by wrapping in a fake HelloAck and decoding
	ack := make([]byte, 8)
	copy(ack[4:], payload[1:5])
	_, cols, rows := DecodeHelloAck(ack)
	if cols != 132 || rows != 51 {
		t.Errorf("cols=%d rows=%d, want 132, 51", cols, rows)
	}
}

func TestResizeRoundtrip(t *testing.T) {
	payload := EncodeResize(80, 24)
	r := bytes.NewReader(payload)
	var cols, rows uint16
	buf := make([]byte, 4)
	r.Read(buf)
	cols = uint16(buf[0])<<8 | uint16(buf[1])
	rows = uint16(buf[2])<<8 | uint16(buf[3])
	if cols != 80 || rows != 24 {
		t.Errorf("cols=%d rows=%d, want 80, 24", cols, rows)
	}
}

func TestExitRoundtrip(t *testing.T) {
	payload := make([]byte, 4)
	payload[0] = 0
	payload[1] = 0
	payload[2] = 0
	payload[3] = 42
	code := DecodeExit(payload)
	if code != 42 {
		t.Errorf("code = %d, want 42", code)
	}
}

func TestSocketPath(t *testing.T) {
	p := SocketPath("abc123")
	suffix := "/keepty/keepty-abc123.sock"
	if len(p) < len(suffix) || p[len(p)-len(suffix):] != suffix {
		t.Errorf("path = %q, want suffix %q", p, suffix)
	}
}

func TestEOFReturnsNil(t *testing.T) {
	frame, err := ReadFrame(bytes.NewReader(nil))
	if err != nil {
		t.Fatalf("ReadFrame: %v", err)
	}
	if frame != nil {
		t.Errorf("expected nil frame on empty input")
	}
}

func TestEmptyPayloadFrame(t *testing.T) {
	encoded := EncodeFrame(MsgPing, nil)
	frame, err := ReadFrame(bytes.NewReader(encoded))
	if err != nil {
		t.Fatalf("ReadFrame: %v", err)
	}
	if frame.Kind != MsgPing {
		t.Errorf("kind = %d, want %d", frame.Kind, MsgPing)
	}
	if len(frame.Payload) != 0 {
		t.Errorf("payload len = %d, want 0", len(frame.Payload))
	}
}
