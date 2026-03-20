/**
 * keepty wire protocol — pure TypeScript implementation.
 *
 * Length-prefixed binary frames over Unix sockets.
 * Frame: [u32 len BE][u8 version][u8 kind][payload...]
 */

import * as net from "net";

import * as os from "os";
import * as path from "path";

export const PROTOCOL_VERSION = 1;
/** @deprecated Use socketDir() instead */
export const SOCKET_DIR = "/tmp";
export const MAX_PAYLOAD_SIZE = 1_048_576; // 1MB
export const MAX_FRAME_BODY_SIZE = MAX_PAYLOAD_SIZE + 2; // payload + version + kind

export function socketDir(): string {
  const xdg = process.env.XDG_RUNTIME_DIR;
  if (xdg) return path.join(xdg, "keepty");
  return path.join(os.tmpdir(), "keepty");
}

export enum Role {
  /** Exclusive: can send input + resize */
  Writer = 1,
  /** Read-only: receives output broadcast */
  Watcher = 2,
  /** Read-only: for programmatic screen analysis */
  Monitor = 3,
}

export enum MsgKind {
  Hello = 1,
  HelloAck = 2,
  Input = 3,
  Output = 4,
  Resize = 5,
  ResizeAck = 6,
  Exit = 10,
  Shutdown = 11,
  Ping = 12,
  Pong = 13,
  Error = 127,
}

export interface Frame {
  kind: MsgKind;
  payload: Buffer;
}

export function encodeFrame(kind: MsgKind, payload: Buffer = Buffer.alloc(0)): Buffer {
  const frameLen = 2 + payload.length; // version + kind + payload
  const buf = Buffer.alloc(4 + frameLen);
  buf.writeUInt32BE(frameLen, 0);
  buf.writeUInt8(PROTOCOL_VERSION, 4);
  buf.writeUInt8(kind, 5);
  payload.copy(buf, 6);
  return buf;
}

export function socketPath(sessionId: string): string {
  return path.join(socketDir(), `keepty-${sessionId}.sock`);
}

// --- Message helpers ---

export function encodeHello(role: Role, cols: number, rows: number): Buffer {
  const buf = Buffer.alloc(5);
  buf.writeUInt8(role, 0);
  buf.writeUInt16BE(cols, 1);
  buf.writeUInt16BE(rows, 3);
  return buf;
}

export function decodeHelloAck(payload: Buffer): { pid: number; cols: number; rows: number } {
  return {
    pid: payload.readUInt32BE(0),
    cols: payload.readUInt16BE(4),
    rows: payload.readUInt16BE(6),
  };
}

export function encodeResize(cols: number, rows: number): Buffer {
  const buf = Buffer.alloc(4);
  buf.writeUInt16BE(cols, 0);
  buf.writeUInt16BE(rows, 2);
  return buf;
}

export function decodeHello(payload: Buffer): { role: Role; cols: number; rows: number } {
  return {
    role: payload.readUInt8(0) as Role,
    cols: payload.readUInt16BE(1),
    rows: payload.readUInt16BE(3),
  };
}

export function encodeResizeAck(gen: number, cols: number, rows: number): Buffer {
  const buf = Buffer.alloc(8);
  buf.writeUInt32BE(gen, 0);
  buf.writeUInt16BE(cols, 4);
  buf.writeUInt16BE(rows, 6);
  return buf;
}

export function decodeResizeAck(payload: Buffer): { gen: number; cols: number; rows: number } {
  return {
    gen: payload.readUInt32BE(0),
    cols: payload.readUInt16BE(4),
    rows: payload.readUInt16BE(6),
  };
}

export function decodeExit(payload: Buffer): number {
  return payload.readInt32BE(0);
}

/**
 * Frame reader that handles partial reads from a stream.
 * Accumulates data and emits complete frames.
 */
export class FrameReader {
  private buffer: Buffer = Buffer.alloc(0);

  /** Feed raw data and return any complete frames. */
  feed(data: Buffer): Frame[] {
    this.buffer = Buffer.concat([this.buffer, data]);
    const frames: Frame[] = [];

    while (this.buffer.length >= 4) {
      const frameLen = this.buffer.readUInt32BE(0);
      if (frameLen < 2) {
        throw new Error(`Frame too short: ${frameLen}`);
      }
      if (frameLen > MAX_FRAME_BODY_SIZE) {
        throw new Error(`Frame too large: ${frameLen} bytes (max ${MAX_FRAME_BODY_SIZE})`);
      }
      const totalLen = 4 + frameLen;
      if (this.buffer.length < totalLen) break;

      const version = this.buffer.readUInt8(4);
      if (version !== PROTOCOL_VERSION) {
        throw new Error(`Unsupported protocol version: ${version} (expected ${PROTOCOL_VERSION})`);
      }
      const kind = this.buffer.readUInt8(5) as MsgKind;
      const payload = Buffer.from(this.buffer.subarray(6, totalLen));

      frames.push({ kind, payload });
      this.buffer = Buffer.from(this.buffer.subarray(totalLen));
    }

    return frames;
  }
}
