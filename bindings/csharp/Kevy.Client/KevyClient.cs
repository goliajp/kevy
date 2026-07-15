// The unified kevy client (contract §1–§4). ONE type, BOTH faces: every
// command is exposed as a blocking method (Get) AND an async Task method
// (GetAsync), and they agree because both run the same argv against the
// same backend. A single Connect(url) chooses that backend — the in-process
// embedded engine (mem:// / file://) or a native RESP TCP server (kevy:// /
// redis:// / tcp://) — so the same business code runs against either by
// changing only the URL string. C# has blocking sockets, so both faces work
// over both backends; the async face over the (synchronous, in-process)
// embedded engine simply runs the blocking op (§1.4).
//
// A typed method maps a -ERR reply to a KevyException, because a typed call
// has exactly one meaning; the raw Do() escape hatch returns Reply.Error as
// a value so no server capability is unreachable (§7).

using Kevy.Embedded;

namespace Kevy;

/// <summary>One open connection to a kevy backend — embedded or remote.</summary>
public sealed partial class KevyClient : IDisposable
{
    private RespConnection? _remote;
    private KevyDb? _emb;
    private string _embKey = "";
    private readonly string _url;

    private KevyClient(string url) => _url = url;

    /// <summary>Open a backend chosen by URL scheme (contract §1.1). Rejects
    /// TLS/AUTH and unknown schemes before any I/O. For kevy:// / redis://
    /// with a /db it issues one SELECT round-trip.</summary>
    public static KevyClient Connect(string url)
    {
        var t = Url.Parse(url);
        var c = new KevyClient(url);
        if (t.Kind == TargetKind.Remote)
        {
            c._remote = RespConnection.Dial(t.Host, t.Port);
            if (t.Db is { } db) c._remote.SelectDb(db);
        }
        else
        {
            (c._emb, c._embKey) = Registry.Resolve(t);
        }
        return c;
    }

    /// <summary>The async twin of <see cref="Connect"/>.</summary>
    public static async Task<KevyClient> ConnectAsync(string url, CancellationToken ct = default)
    {
        var t = Url.Parse(url);
        var c = new KevyClient(url);
        if (t.Kind == TargetKind.Remote)
        {
            c._remote = await RespConnection.DialAsync(t.Host, t.Port, ct).ConfigureAwait(false);
            if (t.Db is { } db) await c._remote.SelectDbAsync(db, ct).ConfigureAwait(false);
        }
        else
        {
            (c._emb, c._embKey) = Registry.Resolve(t);
        }
        return c;
    }

    /// <summary>Whether this client is backed by the in-process engine.</summary>
    public bool IsEmbedded => _emb is not null;

    /// <summary>Release the connection, or drop one reference to the shared
    /// embedded store. Safe to call once.</summary>
    public void Dispose()
    {
        if (_remote is not null) { _remote.Dispose(); _remote = null; }
        if (_emb is not null) { Registry.Release(_embKey, _emb); _emb = null; }
    }

    // --- exec: one argv against whichever backend --------------------------

    private Reply Exec(byte[][] argv)
    {
        if (_emb is not null) return EmbExec(argv);
        if (_remote is not null) return _remote.Request(argv);
        throw new KevyClosedException();
    }

    private async Task<Reply> ExecAsync(byte[][] argv, CancellationToken ct)
    {
        if (_emb is not null) return EmbExec(argv);
        if (_remote is not null) return await _remote.RequestAsync(argv, ct).ConfigureAwait(false);
        throw new KevyClosedException();
    }

    private Reply EmbExec(byte[][] argv)
    {
        try { return Reply.Decode(_emb!.CmdRaw(argv)); }
        catch (Kevy.Embedded.KevyException) { throw new KevyClosedException(); }
    }

    internal RespConnection RequireRemote(string feature) =>
        _remote ?? throw new KevyUnsupportedException(
            feature + " is remote-only; on the embedded backend use the raw Cmd path via KevyDb");

    // --- raw escape hatch (contract §7) ------------------------------------

    /// <summary>Run any verb and get the raw <see cref="Reply"/> back — a
    /// -ERR arrives as <see cref="ReplyKind.Error"/> data, not an exception,
    /// matching the pipeline inline-error convention.</summary>
    public Reply Do(params KevyBytes[] argv)
    {
        if (argv.Length == 0) throw new KevyInvalidInputException("Do needs at least a verb");
        return Exec(argv.Raw());
    }

    /// <summary>The async twin of <see cref="Do"/>.</summary>
    public Task<Reply> DoAsync(KevyBytes[] argv, CancellationToken ct = default)
    {
        if (argv.Length == 0) throw new KevyInvalidInputException("Do needs at least a verb");
        return ExecAsync(argv.Raw(), ct);
    }

    // --- reply shapers (shared by both faces) ------------------------------

    private static long ShapeInt(Reply r) => r.Kind switch
    {
        ReplyKind.Int => r.Integer,
        ReplyKind.Error => throw Err.ReplyError(r),
        _ => throw Err.Unexpected(r),
    };

    private static long ShapeCount(Reply r)
    {
        var n = ShapeInt(r);
        return n < 0 ? throw new KevyProtocolException($"expected non-negative count, got {n}") : n;
    }

    private static bool ShapeBool(Reply r) => r.Kind switch
    {
        ReplyKind.Int => r.Integer == 1,
        ReplyKind.Error => throw Err.ReplyError(r),
        _ => throw Err.Unexpected(r),
    };

    private static void ShapeOk(Reply r)
    {
        if (r.Kind == ReplyKind.Simple && r.Str() == "OK") return;
        throw r.Kind == ReplyKind.Error ? Err.ReplyError(r) : Err.Unexpected(r);
    }

    private static byte[]? ShapeOptBulk(Reply r) => r.Kind switch
    {
        ReplyKind.Bulk => r.Bytes,
        ReplyKind.Nil or ReplyKind.Null => null,
        ReplyKind.Error => throw Err.ReplyError(r),
        _ => throw Err.Unexpected(r),
    };

    private static IReadOnlyList<byte[]?> ShapeBulks(Reply r)
    {
        IReadOnlyList<Reply> items = r.Kind switch
        {
            ReplyKind.Array or ReplyKind.Set => r.Items,
            ReplyKind.Error => throw Err.ReplyError(r),
            _ => throw Err.Unexpected(r),
        };
        return ArrayToBulks(items);
    }

    private static List<byte[]?> ArrayToBulks(IReadOnlyList<Reply> items)
    {
        var outp = new List<byte[]?>(items.Count);
        foreach (var it in items)
            outp.Add(it.Kind switch
            {
                ReplyKind.Bulk or ReplyKind.Simple => it.Bytes,
                ReplyKind.Nil or ReplyKind.Null => null,
                _ => throw Err.Unexpected(it),
            });
        return outp;
    }

    // Non-null bulk list (SMEMBERS/HGETALL/…: no nils expected but tolerated).
    private static List<byte[]> ShapeByteList(Reply r)
    {
        var outp = new List<byte[]>();
        foreach (var v in ShapeBulks(r)) outp.Add(v ?? Array.Empty<byte>());
        return outp;
    }

    // --- adapters: sync + async over the shapers ---------------------------

    private long ExecInt(byte[][] a) => ShapeInt(Exec(a));
    private async Task<long> ExecIntAsync(byte[][] a, CancellationToken ct) => ShapeInt(await ExecAsync(a, ct));
    private long ExecCount(byte[][] a) => ShapeCount(Exec(a));
    private async Task<long> ExecCountAsync(byte[][] a, CancellationToken ct) => ShapeCount(await ExecAsync(a, ct));
    private bool ExecBool(byte[][] a) => ShapeBool(Exec(a));
    private async Task<bool> ExecBoolAsync(byte[][] a, CancellationToken ct) => ShapeBool(await ExecAsync(a, ct));
    private void ExecOk(byte[][] a) => ShapeOk(Exec(a));
    private async Task ExecOkAsync(byte[][] a, CancellationToken ct) => ShapeOk(await ExecAsync(a, ct));
    private byte[]? ExecOptBulk(byte[][] a) => ShapeOptBulk(Exec(a));
    private async Task<byte[]?> ExecOptBulkAsync(byte[][] a, CancellationToken ct) => ShapeOptBulk(await ExecAsync(a, ct));
    private List<byte[]> ExecByteList(byte[][] a) => ShapeByteList(Exec(a));
    private async Task<List<byte[]>> ExecByteListAsync(byte[][] a, CancellationToken ct) => ShapeByteList(await ExecAsync(a, ct));

    internal static double ScoreOf(Reply r) => r.Kind switch
    {
        ReplyKind.Bulk or ReplyKind.Simple => ParseFloat(r.Bytes),
        ReplyKind.Double => r.Double,
        _ => throw Err.Unexpected(r),
    };

    internal static double ParseFloat(byte[] b)
    {
        var s = System.Text.Encoding.ASCII.GetString(b);
        return double.TryParse(s, System.Globalization.NumberStyles.Float,
                System.Globalization.CultureInfo.InvariantCulture, out var f)
            ? f
            : throw new KevyProtocolException("bad float reply: " + s);
    }
}
