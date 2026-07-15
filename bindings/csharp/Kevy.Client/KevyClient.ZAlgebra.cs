// Sorted-set algebra: ZINTERSTORE / ZUNIONSTORE / ZINTERCARD (contract
// §3.6). Available on both backends. AGGREGATE is emitted only for the
// non-default modes; WEIGHTS (when given) must be one-per-key. Empty keys
// → InvalidInput. Both faces.

namespace Kevy;

public sealed partial class KevyClient
{
    /// <summary>ZINTERSTORE (unweighted SUM) into dest; dest cardinality.</summary>
    public long ZInterStore(KevyBytes dest, params KevyBytes[] keys) =>
        ExecCount(ZStoreArgv("ZINTERSTORE", dest, keys, null, ZAggregate.Sum));
    /// <summary>Full-option ZINTERSTORE.</summary>
    public long ZInterStoreWith(KevyBytes dest, KevyBytes[] keys, double[]? weights, ZAggregate agg) =>
        ExecCount(ZStoreArgv("ZINTERSTORE", dest, keys, weights, agg));
    /// <summary>Async ZINTERSTORE.</summary>
    public Task<long> ZInterStoreAsync(KevyBytes dest, KevyBytes[] keys, double[]? weights = null, ZAggregate agg = ZAggregate.Sum, CancellationToken ct = default) =>
        ExecCountAsync(ZStoreArgv("ZINTERSTORE", dest, keys, weights, agg), ct);

    /// <summary>ZUNIONSTORE (unweighted SUM) into dest; dest cardinality.</summary>
    public long ZUnionStore(KevyBytes dest, params KevyBytes[] keys) =>
        ExecCount(ZStoreArgv("ZUNIONSTORE", dest, keys, null, ZAggregate.Sum));
    /// <summary>Full-option ZUNIONSTORE.</summary>
    public long ZUnionStoreWith(KevyBytes dest, KevyBytes[] keys, double[]? weights, ZAggregate agg) =>
        ExecCount(ZStoreArgv("ZUNIONSTORE", dest, keys, weights, agg));
    /// <summary>Async ZUNIONSTORE.</summary>
    public Task<long> ZUnionStoreAsync(KevyBytes dest, KevyBytes[] keys, double[]? weights = null, ZAggregate agg = ZAggregate.Sum, CancellationToken ct = default) =>
        ExecCountAsync(ZStoreArgv("ZUNIONSTORE", dest, keys, weights, agg), ct);

    /// <summary>ZINTERCARD; <paramref name="limit"/> null counts everything,
    /// otherwise short-circuits at limit.</summary>
    public long ZInterCard(KevyBytes[] keys, long? limit) => ExecCount(ZInterCardArgv(keys, limit));
    /// <summary>Async ZINTERCARD.</summary>
    public Task<long> ZInterCardAsync(KevyBytes[] keys, long? limit, CancellationToken ct = default) =>
        ExecCountAsync(ZInterCardArgv(keys, limit), ct);

    private static void CheckSourceKeys(KevyBytes[] keys)
    {
        if (keys.Length == 0)
            throw new KevyInvalidInputException("zset algebra needs at least one source key");
    }

    private static byte[][] ZInterCardArgv(KevyBytes[] keys, long? limit)
    {
        CheckSourceKeys(keys);
        var argv = new List<byte[]>(keys.Length + 4) { Argv.S("ZINTERCARD"), Argv.I(keys.Length) };
        foreach (var k in keys) argv.Add(k.Raw);
        if (limit is { } lim) { argv.Add(Argv.S("LIMIT")); argv.Add(Argv.I(lim)); }
        return argv.ToArray();
    }

    private static byte[][] ZStoreArgv(string verb, KevyBytes dest, KevyBytes[] keys, double[]? weights, ZAggregate agg)
    {
        CheckSourceKeys(keys);
        var argv = new List<byte[]>(keys.Length * 2 + 6) { Argv.S(verb), dest.Raw, Argv.I(keys.Length) };
        foreach (var k in keys) argv.Add(k.Raw);
        if (weights is not null)
        {
            argv.Add(Argv.S("WEIGHTS"));
            foreach (var w in weights) argv.Add(Argv.F(w));
        }
        if (agg != ZAggregate.Sum) { argv.Add(Argv.S("AGGREGATE")); argv.Add(agg.Tag()); }
        return argv.ToArray();
    }
}
