// The native remote transport (contract §1.1): a blocking-and-async RESP2/
// RESP3 connection over a raw TCP socket. One request at a time; a read
// buffer reassembles multi-segment replies across reads. Every command has
// both a synchronous path (blocking Send/Receive) and an async path
// (SendAsync/ReceiveAsync) so the unified client can offer both faces over
// this one backend. Not safe for concurrent use from multiple threads.

using System.Net.Sockets;

namespace Kevy;

internal sealed class RespConnection : IDisposable
{
    private Socket? _sock;
    private byte[] _rbuf = new byte[8192];
    private int _rStart;
    private int _rEnd;

    private RespConnection(Socket sock) => _sock = sock;

    internal bool IsOpen => _sock is not null;

    // --- dialing --------------------------------------------------------

    internal static RespConnection Dial(string host, ushort port)
    {
        try
        {
            var sock = new Socket(SocketType.Stream, ProtocolType.Tcp) { NoDelay = true };
            sock.Connect(host, port);
            return new RespConnection(sock);
        }
        catch (SocketException e) { throw new KevyIoException(e.Message, e); }
    }

    internal static async Task<RespConnection> DialAsync(string host, ushort port, CancellationToken ct)
    {
        try
        {
            var sock = new Socket(SocketType.Stream, ProtocolType.Tcp) { NoDelay = true };
            await sock.ConnectAsync(host, port, ct).ConfigureAwait(false);
            return new RespConnection(sock);
        }
        catch (SocketException e) { throw new KevyIoException(e.Message, e); }
    }

    public void Dispose()
    {
        _sock?.Dispose();
        _sock = null;
    }

    // --- request / pipeline --------------------------------------------

    internal Reply Request(IReadOnlyList<byte[]> argv)
    {
        WriteWire(Encode(argv));
        return ReadOne();
    }

    internal async Task<Reply> RequestAsync(IReadOnlyList<byte[]> argv, CancellationToken ct)
    {
        await WriteWireAsync(Encode(argv), ct).ConfigureAwait(false);
        return await ReadOneAsync(ct).ConfigureAwait(false);
    }

    internal IReadOnlyList<Reply> PipelineRaw(byte[] wire, int n)
    {
        WriteWire(wire);
        var outp = new List<Reply>(n);
        while (outp.Count < n) outp.Add(ReadOne());
        return outp;
    }

    internal async Task<IReadOnlyList<Reply>> PipelineRawAsync(byte[] wire, int n, CancellationToken ct)
    {
        await WriteWireAsync(wire, ct).ConfigureAwait(false);
        var outp = new List<Reply>(n);
        while (outp.Count < n) outp.Add(await ReadOneAsync(ct).ConfigureAwait(false));
        return outp;
    }

    // Write sends argv without reading a reply — the decoupled send side
    // used by the pub/sub consumer, which then drains frames with ReadReply.
    internal void Write(IReadOnlyList<byte[]> argv) => WriteWire(Encode(argv));

    internal Task WriteAsync(IReadOnlyList<byte[]> argv, CancellationToken ct) =>
        WriteWireAsync(Encode(argv), ct);

    internal Reply ReadReply() => ReadOne();

    internal Task<Reply> ReadReplyAsync(CancellationToken ct) => ReadOneAsync(ct);

    // SetReadTimeout bounds a blocking Receive (null clears it). A timeout
    // surfaces as KevyTimedOutException from the sync read path.
    internal void SetReadTimeout(TimeSpan? d)
    {
        var s = Live();
        s.ReceiveTimeout = d is null ? 0 : Math.Max(1, (int)d.Value.TotalMilliseconds);
    }

    internal void SelectDb(uint db)
    {
        var r = Request(["SELECT"u8.ToArray(), Argv.I((long)db)]);
        if (r.Kind == ReplyKind.Error)
            throw new KevyProtocolException($"SELECT {db} rejected: {r.Str()}");
    }

    internal async Task SelectDbAsync(uint db, CancellationToken ct)
    {
        var r = await RequestAsync(["SELECT"u8.ToArray(), Argv.I((long)db)], ct).ConfigureAwait(false);
        if (r.Kind == ReplyKind.Error)
            throw new KevyProtocolException($"SELECT {db} rejected: {r.Str()}");
    }

    // --- wire I/O -------------------------------------------------------

    private static byte[] Encode(IReadOnlyList<byte[]> argv)
    {
        var buf = new List<byte>(16 + argv.Count * 16);
        Resp.EncodeCommand(buf, argv);
        return buf.ToArray();
    }

    private void WriteWire(byte[] wire)
    {
        var s = Live();
        try
        {
            var sent = 0;
            while (sent < wire.Length)
                sent += s.Send(wire, sent, wire.Length - sent, SocketFlags.None);
        }
        catch (SocketException e) { throw MapIo(e); }
    }

    private async Task WriteWireAsync(byte[] wire, CancellationToken ct)
    {
        var s = Live();
        try
        {
            var sent = 0;
            while (sent < wire.Length)
                sent += await s.SendAsync(wire.AsMemory(sent), SocketFlags.None, ct).ConfigureAwait(false);
        }
        catch (SocketException e) { throw MapIo(e); }
    }

    private Reply ReadOne()
    {
        while (true)
        {
            var used = Resp.Parse(_rbuf.AsSpan(_rStart, _rEnd - _rStart), out var r);
            if (used > 0 && r is not null) { _rStart += used; return r; }
            var n = ReceiveMore();
            if (n == 0) { Dispose(); throw new KevyClosedException(); }
        }
    }

    private async Task<Reply> ReadOneAsync(CancellationToken ct)
    {
        while (true)
        {
            var used = Resp.Parse(_rbuf.AsSpan(_rStart, _rEnd - _rStart), out var r);
            if (used > 0 && r is not null) { _rStart += used; return r; }
            var n = await ReceiveMoreAsync(ct).ConfigureAwait(false);
            if (n == 0) { Dispose(); throw new KevyClosedException(); }
        }
    }

    private int ReceiveMore()
    {
        var s = Live();
        Compact();
        try
        {
            var n = s.Receive(_rbuf, _rEnd, _rbuf.Length - _rEnd, SocketFlags.None);
            if (n > 0) _rEnd += n;
            return n;
        }
        catch (SocketException e) { throw MapIo(e); }
    }

    private async Task<int> ReceiveMoreAsync(CancellationToken ct)
    {
        var s = Live();
        Compact();
        try
        {
            var n = await s.ReceiveAsync(_rbuf.AsMemory(_rEnd), SocketFlags.None, ct).ConfigureAwait(false);
            if (n > 0) _rEnd += n;
            return n;
        }
        catch (SocketException e) { throw MapIo(e); }
        catch (OperationCanceledException) { throw new KevyTimedOutException(); }
    }

    // Compact reclaims consumed bytes; grows the buffer when a single frame
    // exceeds it so an unbounded reply still parses.
    private void Compact()
    {
        if (_rStart > 0)
        {
            Array.Copy(_rbuf, _rStart, _rbuf, 0, _rEnd - _rStart);
            _rEnd -= _rStart;
            _rStart = 0;
        }
        if (_rEnd == _rbuf.Length) Array.Resize(ref _rbuf, _rbuf.Length * 2);
    }

    private Socket Live() => _sock ?? throw new KevyClosedException();

    private KevyException MapIo(SocketException e)
    {
        if (e.SocketErrorCode is SocketError.TimedOut or SocketError.WouldBlock)
            return new KevyTimedOutException();
        Dispose();
        return new KevyIoException(e.Message, e);
    }
}
