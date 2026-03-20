/**
 * keepty client — connect to a keepty broker session.
 */

import * as net from "net";
import { execSync, spawn } from "child_process";
import { existsSync, readdirSync } from "fs";
import {
  Role,
  MsgKind,
  Frame,
  FrameReader,
  encodeFrame,
  encodeHello,
  decodeHelloAck,
  encodeResize,
  decodeExit,
  socketPath,
  socketDir,
} from "./protocol";

export interface SessionOptions {
  role?: Role;
  cols?: number;
  rows?: number;
}

export class Session {
  private socket: net.Socket;
  private reader: FrameReader;
  private frameQueue: Frame[] = [];
  private waitResolve: ((frame: Frame) => void) | null = null;

  readonly role: Role;
  readonly ptyPid: number;
  readonly sessionId: string;

  private constructor(
    socket: net.Socket,
    reader: FrameReader,
    initialFrames: Frame[],
    role: Role,
    ptyPid: number,
    sessionId: string
  ) {
    this.socket = socket;
    this.reader = reader;
    this.role = role;
    this.ptyPid = ptyPid;
    this.sessionId = sessionId;

    // Queue any frames already received during handshake (e.g. ring buffer replay)
    for (const frame of initialFrames) {
      this.frameQueue.push(frame);
    }

    socket.on("data", (data) => {
      const frames = this.reader.feed(data);
      for (const frame of frames) {
        if (frame.kind === MsgKind.Ping) {
          this.socket.write(encodeFrame(MsgKind.Pong));
          continue;
        }
        if (this.waitResolve) {
          const resolve = this.waitResolve;
          this.waitResolve = null;
          resolve(frame);
        } else {
          this.frameQueue.push(frame);
        }
      }
    });
  }

  /** Connect to a running keepty broker session. */
  static async connect(
    sessionId: string,
    options: SessionOptions = {}
  ): Promise<Session> {
    const { role = Role.Writer, cols = 80, rows = 24 } = options;
    const path = socketPath(sessionId);

    if (!existsSync(path)) {
      throw new Error(`Session '${sessionId}' is not running (no socket at ${path})`);
    }

    const socket = await new Promise<net.Socket>((resolve, reject) => {
      const sock = net.createConnection(path, () => resolve(sock));
      sock.on("error", reject);
    });

    // Send Hello
    socket.write(encodeFrame(MsgKind.Hello, encodeHello(role, cols, rows)));

    // Read HelloAck (and capture any extra frames like ring buffer replay)
    const reader = new FrameReader();
    const { ack, extraFrames } = await new Promise<{ ack: Frame; extraFrames: Frame[] }>(
      (resolve, reject) => {
        const onData = (data: Buffer) => {
          const frames = reader.feed(data);
          if (frames.length > 0) {
            socket.removeListener("data", onData);
            resolve({ ack: frames[0], extraFrames: frames.slice(1) });
          }
        };
        socket.on("data", onData);
        socket.on("error", reject);
        setTimeout(() => reject(new Error("HelloAck timeout")), 5000);
      }
    );

    if (ack.kind !== MsgKind.HelloAck) {
      socket.destroy();
      throw new Error(`Expected HelloAck, got ${MsgKind[ack.kind]}`);
    }

    const { pid } = decodeHelloAck(ack.payload);
    return new Session(socket, reader, extraFrames, role, pid, sessionId);
  }

  /** Read the next frame, waiting up to timeout ms. */
  private nextFrame(timeoutMs: number): Promise<Frame | null> {
    if (this.frameQueue.length > 0) {
      return Promise.resolve(this.frameQueue.shift()!);
    }
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        this.waitResolve = null;
        resolve(null);
      }, timeoutMs);
      this.waitResolve = (frame) => {
        clearTimeout(timer);
        resolve(frame);
      };
    });
  }

  /** Read output, collecting for up to timeoutMs. */
  async readOutput(timeoutMs: number = 1000): Promise<Buffer> {
    const chunks: Buffer[] = [];
    const deadline = Date.now() + timeoutMs;

    while (Date.now() < deadline) {
      const remaining = Math.max(50, deadline - Date.now());
      const frame = await this.nextFrame(remaining);
      if (!frame) break;

      if (frame.kind === MsgKind.Output) {
        chunks.push(frame.payload);
      } else if (frame.kind === MsgKind.Exit) {
        throw new SessionExited(decodeExit(frame.payload));
      }
    }

    return Buffer.concat(chunks);
  }

  /** Read until expected string appears in output. */
  async readUntil(expected: string, timeoutMs: number = 5000): Promise<string> {
    let collected = "";
    const deadline = Date.now() + timeoutMs;

    while (Date.now() < deadline) {
      const remaining = Math.max(50, deadline - Date.now());
      const data = await this.readOutput(Math.min(200, remaining));
      collected += data.toString("utf-8");
      if (collected.includes(expected)) return collected;
    }

    throw new Error(
      `Expected '${expected}' not found within ${timeoutMs}ms. Got: ${collected.slice(0, 200)}`
    );
  }

  /** Send raw keystrokes. Writer only. */
  sendKeys(keys: Buffer | string): void {
    if (this.role !== Role.Writer) {
      throw new Error("Only Writer role can send input");
    }
    const buf = typeof keys === "string" ? Buffer.from(keys) : keys;
    this.socket.write(encodeFrame(MsgKind.Input, buf));
  }

  /** Resize the terminal. Writer only. */
  resize(cols: number, rows: number): void {
    if (this.role !== Role.Writer) {
      throw new Error("Only Writer role can resize");
    }
    this.socket.write(encodeFrame(MsgKind.Resize, encodeResize(cols, rows)));
  }

  /** Request broker to shut down the session. */
  shutdown(): void {
    this.socket.write(encodeFrame(MsgKind.Shutdown));
  }

  /** Close the connection. */
  close(): void {
    this.socket.destroy();
  }
}

export class SessionExited extends Error {
  exitCode: number;
  constructor(code: number) {
    super(`Session exited with code ${code}`);
    this.exitCode = code;
  }
}

// --- Convenience functions ---

/** Start a new keepty session via the CLI. */
export function start(
  sessionId: string,
  options: { command?: string; workingDir?: string; keeptyBin?: string } = {}
): void {
  const bin = options.keeptyBin || "keepty";
  const args = ["start", sessionId];
  if (options.workingDir) args.push("--working-dir", options.workingDir);
  if (options.command) args.push("--", options.command);

  const result = execSync([bin, ...args].join(" "), {
    encoding: "utf-8",
    stdio: ["pipe", "pipe", "pipe"],
  });
}

/** Stop a running session. */
export function stop(sessionId: string, keeptyBin?: string): void {
  const bin = keeptyBin || "keepty";
  try {
    execSync(`${bin} stop ${sessionId}`, { stdio: "pipe" });
  } catch {
    // Session may already be stopped
  }
}

/** List running sessions. */
export function listSessions(): Array<{ id: string; alive: boolean }> {
  const dir = socketDir();
  const sessions: Array<{ id: string; alive: boolean }> = [];

  try {
    const files = readdirSync(dir);
    for (const file of files) {
      if (file.startsWith("keepty-") && file.endsWith(".sock")) {
        const id = file.slice(7, -5);
        let alive = false;
        try {
          const sock = new net.Socket();
          // Quick connect test
          alive = existsSync(`${dir}/${file}`);
        } catch {}
        sessions.push({ id, alive });
      }
    }
  } catch {}

  return sessions.sort((a, b) => a.id.localeCompare(b.id));
}
