// Hash-field TTL: HEXPIRE / HPEXPIRE / HPERSIST / HTTL / HPTTL (Redis 7.4
// shape, contract §3.7). Per-field replies come back in request order.
// HExpire is whole-second precision; HPExpire is milliseconds. Codes are
// signed 8-bit (sbyte). Empty fields → InvalidInput. Both backends, both
// faces.

namespace Kevy;

public sealed partial class KevyClient
{
    /// <summary>HEXPIRE (whole-second precision); one code per field.</summary>
    public IReadOnlyList<sbyte> HExpire(KevyBytes key, KevyBytes[] fields, TimeSpan ttl, HExpireCond cond = HExpireCond.Always) =>
        ShapeCodes(Exec(HashTtlArgv("HEXPIRE", key, Argv.I((long)ttl.TotalSeconds), cond, fields)));
    /// <summary>Async HEXPIRE.</summary>
    public async ValueTask<IReadOnlyList<sbyte>> HExpireAsync(KevyBytes key, KevyBytes[] fields, TimeSpan ttl, HExpireCond cond = HExpireCond.Always, CancellationToken ct = default) =>
        ShapeCodes(await ExecAsync(HashTtlArgv("HEXPIRE", key, Argv.I((long)ttl.TotalSeconds), cond, fields), ct));

    /// <summary>HPEXPIRE (millisecond precision).</summary>
    public IReadOnlyList<sbyte> HPExpire(KevyBytes key, KevyBytes[] fields, TimeSpan ttl, HExpireCond cond = HExpireCond.Always) =>
        ShapeCodes(Exec(HashTtlArgv("HPEXPIRE", key, Argv.I(Argv.DurationMs(ttl)), cond, fields)));
    /// <summary>Async HPEXPIRE.</summary>
    public async ValueTask<IReadOnlyList<sbyte>> HPExpireAsync(KevyBytes key, KevyBytes[] fields, TimeSpan ttl, HExpireCond cond = HExpireCond.Always, CancellationToken ct = default) =>
        ShapeCodes(await ExecAsync(HashTtlArgv("HPEXPIRE", key, Argv.I(Argv.DurationMs(ttl)), cond, fields), ct));

    /// <summary>HPERSIST — clear per-field TTLs (-2 missing, -1 no TTL, 1 cleared).</summary>
    public IReadOnlyList<sbyte> HPersist(KevyBytes key, params KevyBytes[] fields) =>
        ShapeCodes(Exec(HashTtlArgv("HPERSIST", key, null, HExpireCond.Always, fields)));
    /// <summary>Async HPERSIST.</summary>
    public async ValueTask<IReadOnlyList<sbyte>> HPersistAsync(KevyBytes key, KevyBytes[] fields, CancellationToken ct = default) =>
        ShapeCodes(await ExecAsync(HashTtlArgv("HPERSIST", key, null, HExpireCond.Always, fields), ct));

    /// <summary>HTTL — remaining TTL per field, seconds.</summary>
    public IReadOnlyList<long> HTtl(KevyBytes key, params KevyBytes[] fields) =>
        ShapeInts(Exec(HashTtlArgv("HTTL", key, null, HExpireCond.Always, fields)));
    /// <summary>Async HTTL.</summary>
    public async ValueTask<IReadOnlyList<long>> HTtlAsync(KevyBytes key, KevyBytes[] fields, CancellationToken ct = default) =>
        ShapeInts(await ExecAsync(HashTtlArgv("HTTL", key, null, HExpireCond.Always, fields), ct));

    /// <summary>HPTTL — remaining TTL per field, milliseconds.</summary>
    public IReadOnlyList<long> HPTtl(KevyBytes key, params KevyBytes[] fields) =>
        ShapeInts(Exec(HashTtlArgv("HPTTL", key, null, HExpireCond.Always, fields)));
    /// <summary>Async HPTTL.</summary>
    public async ValueTask<IReadOnlyList<long>> HPTtlAsync(KevyBytes key, KevyBytes[] fields, CancellationToken ct = default) =>
        ShapeInts(await ExecAsync(HashTtlArgv("HPTTL", key, null, HExpireCond.Always, fields), ct));

    private static byte[][] HashTtlArgv(string verb, KevyBytes key, byte[]? arg, HExpireCond cond, KevyBytes[] fields)
    {
        if (fields.Length == 0)
            throw new KevyInvalidInputException("hash field-TTL verbs need at least one field");
        var argv = new List<byte[]>(fields.Length + 6) { Argv.S(verb), key.Raw };
        if (arg is not null) argv.Add(arg);
        if (cond.Keyword() is { } kw) argv.Add(kw);
        argv.Add(Argv.S("FIELDS"));
        argv.Add(Argv.I(fields.Length));
        foreach (var f in fields) argv.Add(f.Raw);
        return argv.ToArray();
    }

    private static IReadOnlyList<sbyte> ShapeCodes(Reply r)
    {
        var items = ShapeIntArray(r);
        var outp = new List<sbyte>(items.Count);
        foreach (var v in items) outp.Add((sbyte)v);
        return outp;
    }

    private static IReadOnlyList<long> ShapeInts(Reply r) => ShapeIntArray(r);

    private static List<long> ShapeIntArray(Reply r)
    {
        if (r.Kind == ReplyKind.Error) throw Err.ReplyError(r);
        if (r.Kind != ReplyKind.Array) throw Err.Unexpected(r);
        var outp = new List<long>(r.Items.Count);
        foreach (var it in r.Items)
            outp.Add(it.Kind == ReplyKind.Int ? it.Integer : throw Err.Unexpected(it));
        return outp;
    }
}
