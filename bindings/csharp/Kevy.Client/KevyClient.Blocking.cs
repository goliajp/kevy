// Blocking pops: BLPOP / BRPOP / BZPOPMIN (contract §3.14). Remote lets the
// server park the connection. The C ABI exposes no blocking-pop symbol, so
// the embedded backend EMULATES by polling the non-blocking pop on a short
// interval (observably correct; only a bounded poll latency differs). null
// timeout waits forever (wire 0); Some(zero) → InvalidInput (ambiguous, wire
// 0 already means forever). Empty keys → InvalidInput. Timeout is sent as
// fractional seconds. Both faces.

using System.Globalization;

namespace Kevy;

public sealed partial class KevyClient
{
    /// <summary>BLPOP: block for a head element across keys; null on timeout.</summary>
    public Kv? BLPop(KevyBytes[] keys, TimeSpan? timeout)
    {
        CheckBlocking(keys, timeout);
        return _emb is not null ? EmbPopKv("LPOP", keys, timeout) : PopKv("BLPOP", keys, timeout);
    }
    /// <summary>Async BLPOP.</summary>
    public Task<Kv?> BLPopAsync(KevyBytes[] keys, TimeSpan? timeout, CancellationToken ct = default)
    {
        CheckBlocking(keys, timeout);
        return _emb is not null ? EmbPopKvAsync("LPOP", keys, timeout, ct) : PopKvAsync("BLPOP", keys, timeout, ct);
    }

    /// <summary>BRPOP: block for a tail element; null on timeout.</summary>
    public Kv? BRPop(KevyBytes[] keys, TimeSpan? timeout)
    {
        CheckBlocking(keys, timeout);
        return _emb is not null ? EmbPopKv("RPOP", keys, timeout) : PopKv("BRPOP", keys, timeout);
    }
    /// <summary>Async BRPOP.</summary>
    public Task<Kv?> BRPopAsync(KevyBytes[] keys, TimeSpan? timeout, CancellationToken ct = default)
    {
        CheckBlocking(keys, timeout);
        return _emb is not null ? EmbPopKvAsync("RPOP", keys, timeout, ct) : PopKvAsync("BRPOP", keys, timeout, ct);
    }

    /// <summary>BZPOPMIN: block for the lowest-scored member; null on timeout.</summary>
    public ZPopHit? BZPopMin(KevyBytes[] keys, TimeSpan? timeout)
    {
        CheckBlocking(keys, timeout);
        return _emb is not null ? EmbPopZ(keys, timeout) : ShapeZPop(Exec(BlockingArgv("BZPOPMIN", keys, timeout)));
    }
    /// <summary>Async BZPOPMIN.</summary>
    public async Task<ZPopHit?> BZPopMinAsync(KevyBytes[] keys, TimeSpan? timeout, CancellationToken ct = default)
    {
        CheckBlocking(keys, timeout);
        if (_emb is not null) return await EmbPopZAsync(keys, timeout, ct);
        return ShapeZPop(await ExecAsync(BlockingArgv("BZPOPMIN", keys, timeout), ct));
    }

    private Kv? PopKv(string verb, KevyBytes[] keys, TimeSpan? timeout) =>
        ShapeKv(Exec(BlockingArgv(verb, keys, timeout)));
    private async Task<Kv?> PopKvAsync(string verb, KevyBytes[] keys, TimeSpan? timeout, CancellationToken ct) =>
        ShapeKv(await ExecAsync(BlockingArgv(verb, keys, timeout), ct));

    private static Kv? ShapeKv(Reply r)
    {
        if (r.Kind == ReplyKind.Array && r.Items.Count == 2 &&
            r.Items[0].Kind == ReplyKind.Bulk && r.Items[1].Kind == ReplyKind.Bulk)
            return new Kv(r.Items[0].Bytes, r.Items[1].Bytes);
        if (r.IsNil) return null;
        throw r.Kind == ReplyKind.Error ? Err.ReplyError(r) : Err.Unexpected(r);
    }

    private static ZPopHit? ShapeZPop(Reply r)
    {
        if (r.Kind == ReplyKind.Array && r.Items.Count == 3 &&
            r.Items[0].Kind == ReplyKind.Bulk && r.Items[1].Kind == ReplyKind.Bulk)
            return new ZPopHit(r.Items[0].Bytes, r.Items[1].Bytes, ScoreOf(r.Items[2]));
        if (r.IsNil) return null;
        throw r.Kind == ReplyKind.Error ? Err.ReplyError(r) : Err.Unexpected(r);
    }

    // --- embedded emulation (poll the non-blocking pop) --------------------

    private Kv? EmbPopKv(string verb, KevyBytes[] keys, TimeSpan? timeout)
    {
        var deadline = Deadline(timeout);
        while (true)
        {
            foreach (var k in keys)
                if (PopOne(verb, k) is { } v) return new Kv(k.Raw, v);
            if (Elapsed(deadline)) return null;
            Thread.Sleep(5);
        }
    }

    private async Task<Kv?> EmbPopKvAsync(string verb, KevyBytes[] keys, TimeSpan? timeout, CancellationToken ct)
    {
        var deadline = Deadline(timeout);
        while (true)
        {
            foreach (var k in keys)
                if (PopOne(verb, k) is { } v) return new Kv(k.Raw, v);
            if (Elapsed(deadline)) return null;
            await Task.Delay(5, ct).ConfigureAwait(false);
        }
    }

    private byte[]? PopOne(string verb, KevyBytes k)
    {
        var r = Exec([Argv.S(verb), k.Raw, Argv.S("1")]);
        if (r.Kind == ReplyKind.Array && r.Items.Count > 0 && r.Items[0].Kind == ReplyKind.Bulk) return r.Items[0].Bytes;
        if (r.Kind == ReplyKind.Bulk) return r.Bytes;
        return null;
    }

    private ZPopHit? EmbPopZ(KevyBytes[] keys, TimeSpan? timeout)
    {
        var deadline = Deadline(timeout);
        while (true)
        {
            foreach (var k in keys)
                if (PopZOne(k) is { } hit) return hit;
            if (Elapsed(deadline)) return null;
            Thread.Sleep(5);
        }
    }

    private async Task<ZPopHit?> EmbPopZAsync(KevyBytes[] keys, TimeSpan? timeout, CancellationToken ct)
    {
        var deadline = Deadline(timeout);
        while (true)
        {
            foreach (var k in keys)
                if (PopZOne(k) is { } hit) return hit;
            if (Elapsed(deadline)) return null;
            await Task.Delay(5, ct).ConfigureAwait(false);
        }
    }

    private ZPopHit? PopZOne(KevyBytes k)
    {
        var r = Exec([Argv.S("ZPOPMIN"), k.Raw, Argv.S("1")]);
        if (r.Kind == ReplyKind.Array && r.Items.Count >= 2 && r.Items[0].Kind == ReplyKind.Bulk)
            return new ZPopHit(k.Raw, r.Items[0].Bytes, ScoreOf(r.Items[1]));
        return null;
    }

    private static void CheckBlocking(KevyBytes[] keys, TimeSpan? timeout)
    {
        if (keys.Length == 0) throw new KevyInvalidInputException("blocking pop needs at least one key");
        if (timeout is { } t && t == TimeSpan.Zero)
            throw new KevyInvalidInputException("timeout Some(0) is ambiguous (wire 0 = wait forever); use null to wait forever");
    }

    private static byte[][] BlockingArgv(string verb, KevyBytes[] keys, TimeSpan? timeout)
    {
        var argv = new List<byte[]>(keys.Length + 2) { Argv.S(verb) };
        foreach (var k in keys) argv.Add(k.Raw);
        argv.Add(timeout is { } t
            ? Argv.S(t.TotalSeconds.ToString("R", CultureInfo.InvariantCulture))
            : Argv.S("0"));
        return argv.ToArray();
    }

    private static DateTime? Deadline(TimeSpan? timeout) => timeout is { } t ? DateTime.UtcNow.Add(t) : null;
    private static bool Elapsed(DateTime? deadline) => deadline is { } d && DateTime.UtcNow >= d;
}
