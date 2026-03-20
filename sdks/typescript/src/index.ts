/**
 * keepty — TypeScript SDK for persistent terminal sessions.
 *
 * Usage:
 *   import { Session, Role, start, stop } from "keepty";
 *
 *   start("my-session", { command: "bash" });
 *   const session = await Session.connect("my-session");
 *   session.sendKeys("echo hello\n");
 *   const output = await session.readUntil("hello");
 *   session.close();
 */

export { Role, MsgKind, Frame, FrameReader, encodeFrame, socketPath } from "./protocol";
export { encodeHello, decodeHelloAck, encodeResize, decodeExit } from "./protocol";
export { Session, SessionExited, start, stop, listSessions } from "./client";
