/**
 * Integration tests for the keepty TypeScript SDK.
 * Uses Node's built-in test runner (node --test).
 */

import { describe, it, before, after } from "node:test";
import assert from "node:assert/strict";
import { execSync, spawn } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

// Import compiled SDK
const __dirname = path.dirname(fileURLToPath(import.meta.url));
const distPath = path.join(__dirname, "..", "dist");
const { Session, Role, MsgKind, encodeFrame, FrameReader, socketPath } = await import(
  path.join(distPath, "index.js")
);

// Find keepty binary
const keeptyBin = (() => {
  const release = path.join(__dirname, "..", "..", "..", "target", "release", "keepty");
  const debug = path.join(__dirname, "..", "..", "..", "target", "debug", "keepty");
  if (existsSync(release)) return release;
  if (existsSync(debug)) return debug;
  throw new Error("keepty binary not found — run: cargo build -p keepty");
})();

let counter = 0;
function uniqueId(label) {
  return `tstest-${process.pid}-${++counter}-${label}`;
}

function startBroker(sessionId, command) {
  const child = spawn(keeptyBin, [
    "broker", "--session-id", sessionId,
    "--cols", "80", "--rows", "24", "--", "sh", "-c", command,
  ], { stdio: ["pipe", "pipe", "pipe"] });

  const sock = socketPath(sessionId);
  return new Promise((resolve, reject) => {
    let attempts = 0;
    const check = setInterval(() => {
      if (existsSync(sock)) {
        clearInterval(check);
        setTimeout(() => resolve(child), 50);
      }
      if (++attempts > 100) {
        clearInterval(check);
        child.kill();
        reject(new Error("Broker socket never appeared"));
      }
    }, 50);
  });
}

function cleanup(child, sessionId) {
  child.kill();
  try { execSync(`rm -f ${socketPath(sessionId)}`); } catch {}
}

// --- Protocol unit tests ---

describe("Protocol", () => {
  it("FrameReader roundtrip", () => {
    const encoded = encodeFrame(MsgKind.Output, Buffer.from("hello"));
    const reader = new FrameReader();
    const frames = reader.feed(encoded);
    assert.equal(frames.length, 1);
    assert.equal(frames[0].kind, MsgKind.Output);
    assert.deepEqual(frames[0].payload, Buffer.from("hello"));
  });

  it("FrameReader handles partial data", () => {
    const encoded = encodeFrame(MsgKind.Ping);
    const reader = new FrameReader();

    // Feed first 3 bytes
    let frames = reader.feed(encoded.subarray(0, 3));
    assert.equal(frames.length, 0);

    // Feed the rest
    frames = reader.feed(encoded.subarray(3));
    assert.equal(frames.length, 1);
    assert.equal(frames[0].kind, MsgKind.Ping);
  });

  it("socketPath format", () => {
    const p = socketPath("abc123");
    assert.ok(p.endsWith("/keepty/keepty-abc123.sock"), `unexpected path: ${p}`);
  });
});

// --- Integration tests ---

describe("Integration", () => {
  it("connect and receive output", async () => {
    const sid = uniqueId("output");
    const child = await startBroker(sid, "echo 'hello from typescript'; sleep 30");
    try {
      const session = await Session.connect(sid);
      const output = await session.readUntil("hello from typescript", 5000);
      assert.ok(output.includes("hello from typescript"));
      session.close();
    } finally {
      cleanup(child, sid);
    }
  });

  it("send input", async () => {
    const sid = uniqueId("input");
    const child = await startBroker(sid, "cat");
    try {
      const session = await Session.connect(sid);
      await new Promise((r) => setTimeout(r, 200));
      session.sendKeys("ts sdk works!\n");
      const output = await session.readUntil("ts sdk works!", 5000);
      assert.ok(output.includes("ts sdk works!"));
      session.close();
    } finally {
      cleanup(child, sid);
    }
  });

  it("watcher receives output", async () => {
    const sid = uniqueId("watcher");
    // Delay echo so it runs AFTER watcher connects (watchers skip ring replay)
    const child = await startBroker(sid, "sleep 2 && echo 'watcher test'; sleep 30");
    try {
      // Writer must connect first to trigger deferred spawn
      const writer = await Session.connect(sid);
      await new Promise((r) => setTimeout(r, 500));
      const session = await Session.connect(sid, { role: Role.Watcher });
      const output = await session.readUntil("watcher test", 10000);
      assert.ok(output.includes("watcher test"));
      session.close();
      writer.close();
    } finally {
      cleanup(child, sid);
    }
  });

  it("watcher cannot send input", async () => {
    const sid = uniqueId("watcher-input");
    const child = await startBroker(sid, "cat");
    try {
      // Writer must connect first to trigger deferred spawn
      const writer = await Session.connect(sid);
      const session = await Session.connect(sid, { role: Role.Watcher });
      assert.throws(() => session.sendKeys("test\n"), /Writer/);
      session.close();
      writer.close();
    } finally {
      cleanup(child, sid);
    }
  });
});
