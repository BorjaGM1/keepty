using System;
using System.Diagnostics;
using System.IO;
using System.Net.Sockets;
using System.Text;

namespace Keepty;

/// <summary>A connection to a keepty broker session.</summary>
public class Session : IDisposable
{
    private readonly Socket _socket;
    private readonly NetworkStream _stream;

    public Role Role { get; }
    public uint PtyPid { get; }
    public string SessionId { get; }

    private Session(Socket socket, NetworkStream stream, Role role, uint ptyPid, string sessionId)
    {
        _socket = socket;
        _stream = stream;
        Role = role;
        PtyPid = ptyPid;
        SessionId = sessionId;
    }

    /// <summary>Connect to a running keepty broker session.</summary>
    public static Session Connect(string sessionId, Role role = Role.Writer, ushort cols = 80, ushort rows = 24)
    {
        string path = Frame.SocketPath(sessionId);
        if (!File.Exists(path))
            throw new FileNotFoundException($"Session '{sessionId}' is not running (no socket at {path})");

        var socket = new Socket(AddressFamily.Unix, SocketType.Stream, ProtocolType.Unspecified);
        socket.Connect(new UnixDomainSocketEndPoint(path));
        var stream = new NetworkStream(socket, ownsSocket: false);

        // Send Hello
        var hello = new Frame(MsgKind.Hello, Messages.EncodeHello(role, cols, rows));
        hello.WriteTo(stream);

        // Read HelloAck
        var ack = Frame.ReadFrom(stream)
            ?? throw new IOException("Broker closed before HelloAck");

        if (ack.Kind != MsgKind.HelloAck)
            throw new InvalidDataException($"Expected HelloAck, got {ack.Kind}");

        var (pid, _, _) = Messages.DecodeHelloAck(ack.Payload);
        return new Session(socket, stream, role, pid, sessionId);
    }

    /// <summary>Read output for up to the given timeout.</summary>
    public byte[] ReadOutput(TimeSpan timeout)
    {
        _socket.ReceiveTimeout = (int)timeout.TotalMilliseconds;
        using var ms = new MemoryStream();

        try
        {
            while (true)
            {
                var frame = Frame.ReadFrom(_stream);
                if (frame == null) break;

                switch (frame.Kind)
                {
                    case MsgKind.Output:
                        ms.Write(frame.Payload);
                        break;
                    case MsgKind.Ping:
                        new Frame(MsgKind.Pong, Array.Empty<byte>()).WriteTo(_stream);
                        break;
                    case MsgKind.Exit:
                        throw new SessionExitedException(Messages.DecodeExit(frame.Payload));
                }
            }
        }
        catch (IOException) { }
        catch (SocketException) { }

        return ms.ToArray();
    }

    /// <summary>Read until expected string appears in output.</summary>
    public string ReadUntil(string expected, TimeSpan timeout)
    {
        var deadline = DateTime.UtcNow + timeout;
        var collected = new StringBuilder();

        while (DateTime.UtcNow < deadline)
        {
            var remaining = deadline - DateTime.UtcNow;
            if (remaining < TimeSpan.FromMilliseconds(50))
                remaining = TimeSpan.FromMilliseconds(50);
            var chunk = remaining > TimeSpan.FromMilliseconds(200)
                ? TimeSpan.FromMilliseconds(200) : remaining;

            var data = ReadOutput(chunk);
            collected.Append(Encoding.UTF8.GetString(data));

            if (collected.ToString().Contains(expected))
                return collected.ToString();
        }

        throw new TimeoutException($"Expected '{expected}' not found within {timeout}");
    }

    /// <summary>Send raw keystrokes. Writer only.</summary>
    public void SendKeys(byte[] keys)
    {
        if (Role != Role.Writer)
            throw new InvalidOperationException("Only Writer role can send input");
        new Frame(MsgKind.Input, keys).WriteTo(_stream);
    }

    /// <summary>Send raw keystrokes from string. Writer only.</summary>
    public void SendKeys(string text) => SendKeys(Encoding.UTF8.GetBytes(text));

    /// <summary>Resize the terminal. Writer only.</summary>
    public void Resize(ushort cols, ushort rows)
    {
        if (Role != Role.Writer)
            throw new InvalidOperationException("Only Writer role can resize");
        new Frame(MsgKind.Resize, Messages.EncodeResize(cols, rows)).WriteTo(_stream);
    }

    /// <summary>Request the broker to shut down the session.</summary>
    public void Shutdown() =>
        new Frame(MsgKind.Shutdown, Array.Empty<byte>()).WriteTo(_stream);

    public void Dispose()
    {
        _stream.Dispose();
        _socket.Dispose();
    }
}

/// <summary>Thrown when the session's process exits.</summary>
public class SessionExitedException : Exception
{
    public int ExitCode { get; }
    public SessionExitedException(int code) : base($"Session exited with code {code}")
    {
        ExitCode = code;
    }
}
