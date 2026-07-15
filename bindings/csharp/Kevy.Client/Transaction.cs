// Transactions: MULTI / EXEC / DISCARD + WATCH (contract §3.12). Remote-
// only — the embedded backend rejects multi/watch/unwatch with Unsupported
// (single-connection embedded access is already serial). Wire flow: MULTI →
// +OK; each queued command → +QUEUED; EXEC → array of N typed replies. A
// WATCH violation makes EXEC return Nil (aborted). A Transaction abandoned
// without exec/discard sends an implicit DISCARD via Dispose/DisposeAsync so
// the socket isn't left in MULTI mode.

namespace Kevy;

public sealed partial class KevyClient
{
    /// <summary>WATCH keys for optimistic concurrency (≥1). Remote-only.</summary>
    public void Watch(params KevyBytes[] keys)
    {
        if (keys.Length == 0) throw new KevyInvalidInputException("WATCH needs at least one key");
        SimpleOk(RequireRemote("WATCH").Request(Argv.Cmd("WATCH", keys.Raw())));
    }

    /// <summary>UNWATCH every WATCH on this connection. Remote-only.</summary>
    public void Unwatch() => SimpleOk(RequireRemote("UNWATCH").Request([Argv.S("UNWATCH")]));

    /// <summary>Start a MULTI block. Remote-only.</summary>
    public Transaction Multi()
    {
        var conn = RequireRemote("MULTI/EXEC");
        SimpleOk(conn.Request([Argv.S("MULTI")]));
        return new Transaction(conn);
    }

    internal static void SimpleOk(Reply r)
    {
        if (r.Kind == ReplyKind.Simple && r.Str() == "OK") return;
        throw r.Kind == ReplyKind.Error ? Err.ReplyError(r) : Err.Unexpected(r);
    }
}

/// <summary>One in-flight MULTI block over a remote connection (§3.12).
/// Dispose (or Discard) to send the implicit DISCARD; dropping a live
/// transaction without it is a bug.</summary>
public sealed class Transaction : IDisposable, IAsyncDisposable
{
    private readonly RespConnection _conn;
    private bool _live = true;

    internal Transaction(RespConnection conn) => _conn = conn;

    /// <summary>Queue one raw command (verb + args); expects +QUEUED.</summary>
    public Transaction Queue(params KevyBytes[] parts)
    {
        if (parts.Length == 0) throw new KevyInvalidInputException("Transaction.Queue needs at least a verb");
        QueueArgv(parts.Raw());
        return this;
    }

    /// <summary>Queue SET key value.</summary>
    public Transaction Set(KevyBytes key, KevyBytes value) => QueueChain([Argv.S("SET"), key.Raw, value.Raw]);
    /// <summary>Queue GET key.</summary>
    public Transaction Get(KevyBytes key) => QueueChain([Argv.S("GET"), key.Raw]);
    /// <summary>Queue DEL key…</summary>
    public Transaction Del(params KevyBytes[] keys) => Require(keys, "Del").Pipe(Argv.Cmd("DEL", keys.Raw()));
    /// <summary>Queue EXISTS key…</summary>
    public Transaction Exists(params KevyBytes[] keys) => Require(keys, "Exists").Pipe(Argv.Cmd("EXISTS", keys.Raw()));
    /// <summary>Queue INCR key.</summary>
    public Transaction Incr(KevyBytes key) => QueueChain([Argv.S("INCR"), key.Raw]);
    /// <summary>Queue INCRBY key delta.</summary>
    public Transaction IncrBy(KevyBytes key, long delta) => QueueChain([Argv.S("INCRBY"), key.Raw, Argv.I(delta)]);
    /// <summary>Queue MGET key…</summary>
    public Transaction MGet(params KevyBytes[] keys) => Require(keys, "MGet").Pipe(Argv.Cmd("MGET", keys.Raw()));
    /// <summary>Queue MSET key value…</summary>
    public Transaction MSet(params KevyBytes[] pairs)
    {
        if (pairs.Length == 0 || pairs.Length % 2 != 0)
            throw new KevyInvalidInputException("Transaction.MSet needs a non-empty even key/value list");
        return QueueChain(Argv.Cmd("MSET", pairs.Raw()));
    }

    /// <summary>EXEC; per-command replies. A WATCH abort (Nil) collapses to
    /// an empty list (legacy).</summary>
    public IReadOnlyList<Reply> Exec()
    {
        var r = FinishExec();
        return r.Kind switch
        {
            ReplyKind.Array => r.Items,
            ReplyKind.Nil or ReplyKind.Null => Array.Empty<Reply>(),
            ReplyKind.Error => throw Err.ReplyError(r),
            _ => throw Err.Unexpected(r),
        };
    }

    /// <summary>EXEC; null when a WATCH violation aborted it.</summary>
    public IReadOnlyList<Reply>? ExecWatched()
    {
        var r = FinishExec();
        return r.Kind switch
        {
            ReplyKind.Array => r.Items,
            ReplyKind.Nil or ReplyKind.Null => null,
            ReplyKind.Error => throw Err.ReplyError(r),
            _ => throw Err.Unexpected(r),
        };
    }

    /// <summary>EXEC → a typed reply cursor; a WATCH abort is a Protocol error.</summary>
    public TransactionReplies ExecTyped()
    {
        var r = FinishExec();
        return r.Kind switch
        {
            ReplyKind.Array => new TransactionReplies(r.Items),
            ReplyKind.Nil or ReplyKind.Null => throw new KevyProtocolException("transaction aborted by WATCH"),
            ReplyKind.Error => throw Err.ReplyError(r),
            _ => throw Err.Unexpected(r),
        };
    }

    /// <summary>EXEC → a typed cursor, or null on WATCH abort.</summary>
    public TransactionReplies? ExecWatchedTyped()
    {
        var r = FinishExec();
        return r.Kind switch
        {
            ReplyKind.Array => new TransactionReplies(r.Items),
            ReplyKind.Nil or ReplyKind.Null => null,
            ReplyKind.Error => throw Err.ReplyError(r),
            _ => throw Err.Unexpected(r),
        };
    }

    /// <summary>Abandon the queued commands.</summary>
    public void Discard()
    {
        if (!_live) return;
        _live = false;
        KevyClient.SimpleOk(_conn.Request([Argv.S("DISCARD")]));
    }

    private Reply FinishExec()
    {
        _live = false;
        return _conn.Request([Argv.S("EXEC")]);
    }

    private Transaction QueueChain(byte[][] argv) { QueueArgv(argv); return this; }
    private Transaction Pipe(byte[][] argv) { QueueArgv(argv); return this; }
    private Transaction Require(KevyBytes[] keys, string op) =>
        keys.Length == 0 ? throw new KevyInvalidInputException($"Transaction.{op} needs at least one key") : this;

    private void QueueArgv(byte[][] argv)
    {
        var r = _conn.Request(argv);
        if (r.Kind == ReplyKind.Simple && r.Str() == "QUEUED") return;
        throw r.Kind == ReplyKind.Error ? Err.ReplyError(r) : Err.Unexpected(r);
    }

    /// <summary>Implicit DISCARD if still live (§3.12 drop semantics).</summary>
    public void Dispose()
    {
        if (!_live) return;
        _live = false;
        try { _conn.Request([Argv.S("DISCARD")]); } catch (KevyException) { }
    }

    /// <inheritdoc cref="Dispose"/>
    public ValueTask DisposeAsync() { Dispose(); return ValueTask.CompletedTask; }
}

/// <summary>A typed cursor over a successful EXEC's per-command replies
/// (§4.10). Each Next* consumes one reply; a variant mismatch is a Protocol
/// error (the cursor still advances so ExpectEmpty stays meaningful).</summary>
public sealed class TransactionReplies
{
    private readonly IReadOnlyList<Reply> _items;
    private int _pos;

    internal TransactionReplies(IReadOnlyList<Reply> items) => _items = items;

    /// <summary>How many replies are un-consumed.</summary>
    public int Remaining => _items.Count - _pos;

    /// <summary>Error if any replies remain (an arity gate).</summary>
    public void ExpectEmpty()
    {
        if (Remaining != 0) throw new KevyProtocolException($"transaction reply cursor has {Remaining} un-consumed replies");
    }

    /// <summary>Pop the next reply as-is.</summary>
    public Reply Raw()
    {
        if (_pos >= _items.Count) throw new KevyProtocolException("exhausted");
        return _items[_pos++];
    }

    /// <summary>Expect +OK.</summary>
    public void NextOk()
    {
        var v = Raw();
        if (!(v.Kind == ReplyKind.Simple && v.Str() == "OK")) throw Mismatch("Simple(OK)", v);
    }

    /// <summary>+OK → true, Nil → false (for SET … NX/XX).</summary>
    public bool NextOkOrNil()
    {
        var v = Raw();
        if (v.Kind == ReplyKind.Simple && v.Str() == "OK") return true;
        if (v.IsNil) return false;
        throw Mismatch("Simple(OK) or Nil", v);
    }

    /// <summary>Expect an integer.</summary>
    public long NextInt()
    {
        var v = Raw();
        return v.Kind == ReplyKind.Int ? v.Integer : throw Mismatch("Int", v);
    }

    /// <summary>Expect a bulk (or Nil → null).</summary>
    public byte[]? NextBulk()
    {
        var v = Raw();
        if (v.Kind == ReplyKind.Bulk) return v.Bytes;
        if (v.IsNil) return null;
        throw Mismatch("Bulk or Nil", v);
    }

    /// <summary>Expect an array of Bulk/Nil (MGET), in order.</summary>
    public IReadOnlyList<byte[]?> NextArrayOfBulks()
    {
        var v = Raw();
        if (v.IsNil) return Array.Empty<byte[]?>();
        if (v.Kind != ReplyKind.Array) throw Mismatch("Array", v);
        var outp = new List<byte[]?>(v.Items.Count);
        foreach (var it in v.Items)
        {
            if (it.Kind == ReplyKind.Bulk) outp.Add(it.Bytes);
            else if (it.IsNil) outp.Add(null);
            else throw Mismatch("Array element Bulk/Nil", it);
        }
        return outp;
    }

    /// <summary>Expect any simple string (e.g. +PONG).</summary>
    public byte[] NextSimple()
    {
        var v = Raw();
        return v.Kind == ReplyKind.Simple ? v.Bytes : throw Mismatch("Simple", v);
    }

    private static KevyProtocolException Mismatch(string want, Reply got) =>
        new($"transaction reply mismatch: expected {want}, got {got.Shape()}");
}
