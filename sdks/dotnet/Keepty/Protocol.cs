using System;
using System.Buffers.Binary;
using System.IO;
using System.Net.Sockets;

namespace Keepty;

/// <summary>Client role when connecting to a keepty broker.</summary>
public enum Role : byte
{
    /// <summary>Exclusive: can send input + resize.</summary>
    Writer = 1,
    /// <summary>Read-only: receives output broadcast.</summary>
    Watcher = 2,
    /// <summary>Read-only: for programmatic screen analysis.</summary>
    Monitor = 3,
}

/// <summary>Message types in the keepty protocol.</summary>
public enum MsgKind : byte
{
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

/// <summary>A keepty protocol frame.</summary>
public record Frame(MsgKind Kind, byte[] Payload)
{
    /// <summary>Deprecated: use SocketDirResolved() instead.</summary>
    public static readonly string SocketDir = "/tmp";

    /// <summary>Resolve the runtime socket directory.</summary>
    public static string SocketDirResolved()
    {
        var xdg = Environment.GetEnvironmentVariable("XDG_RUNTIME_DIR");
        if (!string.IsNullOrEmpty(xdg))
            return Path.Combine(xdg, "keepty");
        return Path.Combine(Path.GetTempPath().TrimEnd(Path.DirectorySeparatorChar), "keepty");
    }
    public const int MaxPayloadSize = 1_048_576; // 1MB
    public const int MaxFrameBodySize = MaxPayloadSize + 2; // payload + version + kind

    /// <summary>Construct the broker socket path for a session.</summary>
    public static string SocketPath(string sessionId) =>
        Path.Combine(SocketDirResolved(), $"keepty-{sessionId}.sock");

    /// <summary>Encode to wire format: [u32 len BE][u8 version][u8 kind][payload]</summary>
    public byte[] Encode()
    {
        int frameLen = 2 + Payload.Length;
        byte[] buf = new byte[4 + frameLen];
        BinaryPrimitives.WriteUInt32BigEndian(buf.AsSpan(0, 4), (uint)frameLen);
        buf[4] = 1; // protocol version
        buf[5] = (byte)Kind;
        Payload.CopyTo(buf.AsSpan(6));
        return buf;
    }

    /// <summary>Read one frame from a stream. Returns null on EOF.</summary>
    public static Frame? ReadFrom(Stream stream)
    {
        byte[] lenBuf = new byte[4];
        if (!ReadExact(stream, lenBuf)) return null;

        uint frameLen = BinaryPrimitives.ReadUInt32BigEndian(lenBuf);
        if (frameLen < 2)
            throw new InvalidDataException($"Frame too short: {frameLen}");
        if (frameLen > (uint)MaxFrameBodySize)
            throw new InvalidDataException($"Frame too large: {frameLen} bytes (max {MaxFrameBodySize})");

        byte[] data = new byte[frameLen];
        if (!ReadExact(stream, data))
            throw new EndOfStreamException("Connection closed mid-frame");

        byte version = data[0];
        if (version != 1)
            throw new InvalidDataException($"Unsupported protocol version: {version} (expected 1)");
        var kind = (MsgKind)data[1];
        byte[] payload = new byte[frameLen - 2];
        Array.Copy(data, 2, payload, 0, payload.Length);
        return new Frame(kind, payload);
    }

    /// <summary>Write this frame to a stream.</summary>
    public void WriteTo(Stream stream)
    {
        byte[] encoded = Encode();
        stream.Write(encoded);
        stream.Flush();
    }

    private static bool ReadExact(Stream stream, byte[] buffer)
    {
        int offset = 0;
        while (offset < buffer.Length)
        {
            int read = stream.Read(buffer, offset, buffer.Length - offset);
            if (read == 0)
            {
                if (offset == 0) return false;
                throw new EndOfStreamException($"Connection closed (got {offset}/{buffer.Length} bytes)");
            }
            offset += read;
        }
        return true;
    }
}

/// <summary>Message encoding/decoding helpers.</summary>
public static class Messages
{
    public static byte[] EncodeHello(Role role, ushort cols, ushort rows)
    {
        byte[] buf = new byte[5];
        buf[0] = (byte)role;
        BinaryPrimitives.WriteUInt16BigEndian(buf.AsSpan(1, 2), cols);
        BinaryPrimitives.WriteUInt16BigEndian(buf.AsSpan(3, 2), rows);
        return buf;
    }

    public static (uint pid, ushort cols, ushort rows) DecodeHelloAck(byte[] payload) => (
        BinaryPrimitives.ReadUInt32BigEndian(payload.AsSpan(0, 4)),
        BinaryPrimitives.ReadUInt16BigEndian(payload.AsSpan(4, 2)),
        BinaryPrimitives.ReadUInt16BigEndian(payload.AsSpan(6, 2))
    );

    public static byte[] EncodeResize(ushort cols, ushort rows)
    {
        byte[] buf = new byte[4];
        BinaryPrimitives.WriteUInt16BigEndian(buf.AsSpan(0, 2), cols);
        BinaryPrimitives.WriteUInt16BigEndian(buf.AsSpan(2, 2), rows);
        return buf;
    }

    public static (Role role, ushort cols, ushort rows) DecodeHello(byte[] payload) => (
        (Role)payload[0],
        BinaryPrimitives.ReadUInt16BigEndian(payload.AsSpan(1, 2)),
        BinaryPrimitives.ReadUInt16BigEndian(payload.AsSpan(3, 2))
    );

    public static byte[] EncodeResizeAck(uint gen, ushort cols, ushort rows)
    {
        byte[] buf = new byte[8];
        BinaryPrimitives.WriteUInt32BigEndian(buf.AsSpan(0, 4), gen);
        BinaryPrimitives.WriteUInt16BigEndian(buf.AsSpan(4, 2), cols);
        BinaryPrimitives.WriteUInt16BigEndian(buf.AsSpan(6, 2), rows);
        return buf;
    }

    public static (uint gen, ushort cols, ushort rows) DecodeResizeAck(byte[] payload) => (
        BinaryPrimitives.ReadUInt32BigEndian(payload.AsSpan(0, 4)),
        BinaryPrimitives.ReadUInt16BigEndian(payload.AsSpan(4, 2)),
        BinaryPrimitives.ReadUInt16BigEndian(payload.AsSpan(6, 2))
    );

    public static int DecodeExit(byte[] payload) =>
        BinaryPrimitives.ReadInt32BigEndian(payload.AsSpan(0, 4));
}
