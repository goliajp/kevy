// Collection commands: hash, list, set, sorted set (contract §3.2–§3.5).
// Each runs the same argv against either backend; the embedded engine
// dispatches the full verb surface through the FFI cmd path, so set
// combines (SINTER/SUNION/SDIFF) run server-side there too. Both faces.

namespace Kevy;

public sealed partial class KevyClient
{
    // --- Hash (§3.2) -------------------------------------------------------

    /// <summary>HSET field/value pairs; returns newly-added (non-overwrite) fields.</summary>
    public long HSet(KevyBytes key, params KevyBytes[] pairs) => ExecCount(HSetArgv(key, pairs));
    /// <summary>Async HSET.</summary>
    public ValueTask<long> HSetAsync(KevyBytes key, KevyBytes[] pairs, CancellationToken ct = default) =>
        ExecCountAsync(HSetArgv(key, pairs), ct);

    private static byte[][] HSetArgv(KevyBytes key, KevyBytes[] pairs)
    {
        if (pairs.Length % 2 != 0)
            throw new KevyInvalidInputException("HSet needs an even number of field/value arguments");
        return Argv.Cmd2("HSET", key.Raw, pairs.Raw());
    }

    /// <summary>HGET field; null when key or field is absent.</summary>
    public byte[]? HGet(KevyBytes key, KevyBytes field) => ExecOptBulk([Argv.S("HGET"), key.Raw, field.Raw]);
    /// <summary>Async HGET.</summary>
    public ValueTask<byte[]?> HGetAsync(KevyBytes key, KevyBytes field, CancellationToken ct = default) =>
        ExecOptBulkAsync([Argv.S("HGET"), key.Raw, field.Raw], ct);

    /// <summary>HDEL fields; returns how many were removed.</summary>
    public long HDel(KevyBytes key, params KevyBytes[] fields) => ExecCount(Argv.Cmd2("HDEL", key.Raw, fields.Raw()));
    /// <summary>Async HDEL.</summary>
    public ValueTask<long> HDelAsync(KevyBytes key, KevyBytes[] fields, CancellationToken ct = default) =>
        ExecCountAsync(Argv.Cmd2("HDEL", key.Raw, fields.Raw()), ct);

    /// <summary>HLEN (0 if absent).</summary>
    public long HLen(KevyBytes key) => ExecCount([Argv.S("HLEN"), key.Raw]);
    /// <summary>Async HLEN.</summary>
    public ValueTask<long> HLenAsync(KevyBytes key, CancellationToken ct = default) =>
        ExecCountAsync([Argv.S("HLEN"), key.Raw], ct);

    /// <summary>HGETALL as flat [f0,v0,f1,v1,…] (empty if absent).</summary>
    public IReadOnlyList<byte[]> HGetAll(KevyBytes key) => ExecByteList([Argv.S("HGETALL"), key.Raw]);
    /// <summary>Async HGETALL.</summary>
    public async ValueTask<IReadOnlyList<byte[]>> HGetAllAsync(KevyBytes key, CancellationToken ct = default) =>
        await ExecByteListAsync([Argv.S("HGETALL"), key.Raw], ct);

    /// <summary>HKEYS (empty if absent).</summary>
    public IReadOnlyList<byte[]> HKeys(KevyBytes key) => ExecByteList([Argv.S("HKEYS"), key.Raw]);
    /// <summary>Async HKEYS.</summary>
    public async ValueTask<IReadOnlyList<byte[]>> HKeysAsync(KevyBytes key, CancellationToken ct = default) =>
        await ExecByteListAsync([Argv.S("HKEYS"), key.Raw], ct);

    /// <summary>HVALS (empty if absent).</summary>
    public IReadOnlyList<byte[]> HVals(KevyBytes key) => ExecByteList([Argv.S("HVALS"), key.Raw]);
    /// <summary>Async HVALS.</summary>
    public async ValueTask<IReadOnlyList<byte[]>> HValsAsync(KevyBytes key, CancellationToken ct = default) =>
        await ExecByteListAsync([Argv.S("HVALS"), key.Raw], ct);

    // --- List (§3.3) -------------------------------------------------------

    /// <summary>LPUSH values, returning the new length.</summary>
    public long LPush(KevyBytes key, params KevyBytes[] values) => ExecCount(Argv.Cmd2("LPUSH", key.Raw, values.Raw()));
    /// <summary>Async LPUSH.</summary>
    public ValueTask<long> LPushAsync(KevyBytes key, KevyBytes[] values, CancellationToken ct = default) =>
        ExecCountAsync(Argv.Cmd2("LPUSH", key.Raw, values.Raw()), ct);

    /// <summary>RPUSH values, returning the new length.</summary>
    public long RPush(KevyBytes key, params KevyBytes[] values) => ExecCount(Argv.Cmd2("RPUSH", key.Raw, values.Raw()));
    /// <summary>Async RPUSH.</summary>
    public ValueTask<long> RPushAsync(KevyBytes key, KevyBytes[] values, CancellationToken ct = default) =>
        ExecCountAsync(Argv.Cmd2("RPUSH", key.Raw, values.Raw()), ct);

    /// <summary>LPOP up to count from the head (empty if drained).</summary>
    public IReadOnlyList<byte[]> LPop(KevyBytes key, int count) => ListPop("LPOP", key, count, Exec);
    /// <summary>Async LPOP.</summary>
    public async ValueTask<IReadOnlyList<byte[]>> LPopAsync(KevyBytes key, int count, CancellationToken ct = default) =>
        ShapePop(await ExecAsync([Argv.S("LPOP"), key.Raw, Argv.I(count)], ct));

    /// <summary>RPOP up to count from the tail.</summary>
    public IReadOnlyList<byte[]> RPop(KevyBytes key, int count) => ListPop("RPOP", key, count, Exec);
    /// <summary>Async RPOP.</summary>
    public async ValueTask<IReadOnlyList<byte[]>> RPopAsync(KevyBytes key, int count, CancellationToken ct = default) =>
        ShapePop(await ExecAsync([Argv.S("RPOP"), key.Raw, Argv.I(count)], ct));

    private IReadOnlyList<byte[]> ListPop(string verb, KevyBytes key, int count, Func<byte[][], Reply> run) =>
        ShapePop(run([Argv.S(verb), key.Raw, Argv.I(count)]));

    private static List<byte[]> ShapePop(Reply r) => r.Kind switch
    {
        ReplyKind.Array or ReplyKind.Set => ToNonNull(r.Items),
        ReplyKind.Bulk => new List<byte[]> { r.Bytes },
        ReplyKind.Nil or ReplyKind.Null => new List<byte[]>(),
        ReplyKind.Error => throw Err.ReplyError(r),
        _ => throw Err.Unexpected(r),
    };

    private static List<byte[]> ToNonNull(IReadOnlyList<Reply> items)
    {
        var outp = new List<byte[]>(items.Count);
        foreach (var it in items) outp.Add(it.Kind is ReplyKind.Nil or ReplyKind.Null ? Array.Empty<byte>() : it.Bytes);
        return outp;
    }

    /// <summary>LLEN (0 if absent).</summary>
    public long LLen(KevyBytes key) => ExecCount([Argv.S("LLEN"), key.Raw]);
    /// <summary>Async LLEN.</summary>
    public ValueTask<long> LLenAsync(KevyBytes key, CancellationToken ct = default) =>
        ExecCountAsync([Argv.S("LLEN"), key.Raw], ct);

    /// <summary>LRANGE start..stop with Redis negative indexing.</summary>
    public IReadOnlyList<byte[]> LRange(KevyBytes key, long start, long stop) =>
        ExecByteList([Argv.S("LRANGE"), key.Raw, Argv.I(start), Argv.I(stop)]);
    /// <summary>Async LRANGE.</summary>
    public async ValueTask<IReadOnlyList<byte[]>> LRangeAsync(KevyBytes key, long start, long stop, CancellationToken ct = default) =>
        await ExecByteListAsync([Argv.S("LRANGE"), key.Raw, Argv.I(start), Argv.I(stop)], ct);

    // --- Set (§3.4) --------------------------------------------------------

    /// <summary>SADD members, returning the newly-added count.</summary>
    public long SAdd(KevyBytes key, params KevyBytes[] members) => ExecCount(Argv.Cmd2("SADD", key.Raw, members.Raw()));
    /// <summary>Async SADD.</summary>
    public ValueTask<long> SAddAsync(KevyBytes key, KevyBytes[] members, CancellationToken ct = default) =>
        ExecCountAsync(Argv.Cmd2("SADD", key.Raw, members.Raw()), ct);

    /// <summary>SREM members, returning the removed count.</summary>
    public long SRem(KevyBytes key, params KevyBytes[] members) => ExecCount(Argv.Cmd2("SREM", key.Raw, members.Raw()));
    /// <summary>Async SREM.</summary>
    public ValueTask<long> SRemAsync(KevyBytes key, KevyBytes[] members, CancellationToken ct = default) =>
        ExecCountAsync(Argv.Cmd2("SREM", key.Raw, members.Raw()), ct);

    /// <summary>SMEMBERS (order implementation-defined).</summary>
    public IReadOnlyList<byte[]> SMembers(KevyBytes key) => ExecByteList([Argv.S("SMEMBERS"), key.Raw]);
    /// <summary>Async SMEMBERS.</summary>
    public async ValueTask<IReadOnlyList<byte[]>> SMembersAsync(KevyBytes key, CancellationToken ct = default) =>
        await ExecByteListAsync([Argv.S("SMEMBERS"), key.Raw], ct);

    /// <summary>SCARD (0 if absent).</summary>
    public long SCard(KevyBytes key) => ExecCount([Argv.S("SCARD"), key.Raw]);
    /// <summary>Async SCARD.</summary>
    public ValueTask<long> SCardAsync(KevyBytes key, CancellationToken ct = default) =>
        ExecCountAsync([Argv.S("SCARD"), key.Raw], ct);

    /// <summary>SISMEMBER membership.</summary>
    public bool SIsMember(KevyBytes key, KevyBytes member) => ExecBool([Argv.S("SISMEMBER"), key.Raw, member.Raw]);
    /// <summary>Async SISMEMBER.</summary>
    public ValueTask<bool> SIsMemberAsync(KevyBytes key, KevyBytes member, CancellationToken ct = default) =>
        ExecBoolAsync([Argv.S("SISMEMBER"), key.Raw, member.Raw], ct);

    /// <summary>SINTER of the given sets.</summary>
    public IReadOnlyList<byte[]> SInter(params KevyBytes[] keys) => ExecByteList(Argv.Cmd("SINTER", keys.Raw()));
    /// <summary>Async SINTER.</summary>
    public async ValueTask<IReadOnlyList<byte[]>> SInterAsync(KevyBytes[] keys, CancellationToken ct = default) =>
        await ExecByteListAsync(Argv.Cmd("SINTER", keys.Raw()), ct);

    /// <summary>SUNION of the given sets.</summary>
    public IReadOnlyList<byte[]> SUnion(params KevyBytes[] keys) => ExecByteList(Argv.Cmd("SUNION", keys.Raw()));
    /// <summary>Async SUNION.</summary>
    public async ValueTask<IReadOnlyList<byte[]>> SUnionAsync(KevyBytes[] keys, CancellationToken ct = default) =>
        await ExecByteListAsync(Argv.Cmd("SUNION", keys.Raw()), ct);

    /// <summary>SDIFF: the first set minus the rest.</summary>
    public IReadOnlyList<byte[]> SDiff(params KevyBytes[] keys) => ExecByteList(Argv.Cmd("SDIFF", keys.Raw()));
    /// <summary>Async SDIFF.</summary>
    public async ValueTask<IReadOnlyList<byte[]>> SDiffAsync(KevyBytes[] keys, CancellationToken ct = default) =>
        await ExecByteListAsync(Argv.Cmd("SDIFF", keys.Raw()), ct);

    // --- Sorted set (§3.5) -------------------------------------------------

    /// <summary>ZADD scored members, returning the newly-added count.</summary>
    public long ZAdd(KevyBytes key, params ZMember[] members) => ExecCount(ZAddArgv(key, members));
    /// <summary>Async ZADD.</summary>
    public ValueTask<long> ZAddAsync(KevyBytes key, ZMember[] members, CancellationToken ct = default) =>
        ExecCountAsync(ZAddArgv(key, members), ct);

    private static byte[][] ZAddArgv(KevyBytes key, ZMember[] members)
    {
        var argv = new byte[members.Length * 2 + 2][];
        argv[0] = Argv.S("ZADD");
        argv[1] = key.Raw;
        for (var i = 0; i < members.Length; i++)
        {
            argv[2 + i * 2] = Argv.F(members[i].Score);
            argv[3 + i * 2] = members[i].Member;
        }
        return argv;
    }

    /// <summary>ZREM members, returning the removed count.</summary>
    public long ZRem(KevyBytes key, params KevyBytes[] members) => ExecCount(Argv.Cmd2("ZREM", key.Raw, members.Raw()));
    /// <summary>Async ZREM.</summary>
    public ValueTask<long> ZRemAsync(KevyBytes key, KevyBytes[] members, CancellationToken ct = default) =>
        ExecCountAsync(Argv.Cmd2("ZREM", key.Raw, members.Raw()), ct);

    /// <summary>ZSCORE; null when absent.</summary>
    public double? ZScore(KevyBytes key, KevyBytes member) => ShapeScore(Exec([Argv.S("ZSCORE"), key.Raw, member.Raw]));
    /// <summary>Async ZSCORE.</summary>
    public async ValueTask<double?> ZScoreAsync(KevyBytes key, KevyBytes member, CancellationToken ct = default) =>
        ShapeScore(await ExecAsync([Argv.S("ZSCORE"), key.Raw, member.Raw], ct));

    private static double? ShapeScore(Reply r) => r.Kind switch
    {
        ReplyKind.Bulk or ReplyKind.Simple => ParseFloat(r.Bytes),
        ReplyKind.Double => r.Double,
        ReplyKind.Nil or ReplyKind.Null => null,
        ReplyKind.Error => throw Err.ReplyError(r),
        _ => throw Err.Unexpected(r),
    };

    /// <summary>ZCARD (0 if absent).</summary>
    public long ZCard(KevyBytes key) => ExecCount([Argv.S("ZCARD"), key.Raw]);
    /// <summary>Async ZCARD.</summary>
    public ValueTask<long> ZCardAsync(KevyBytes key, CancellationToken ct = default) =>
        ExecCountAsync([Argv.S("ZCARD"), key.Raw], ct);

    /// <summary>ZRANGE start..stop ascending (members only; negative indexing).</summary>
    public IReadOnlyList<byte[]> ZRange(KevyBytes key, long start, long stop) =>
        ExecByteList([Argv.S("ZRANGE"), key.Raw, Argv.I(start), Argv.I(stop)]);
    /// <summary>Async ZRANGE.</summary>
    public async ValueTask<IReadOnlyList<byte[]>> ZRangeAsync(KevyBytes key, long start, long stop, CancellationToken ct = default) =>
        await ExecByteListAsync([Argv.S("ZRANGE"), key.Raw, Argv.I(start), Argv.I(stop)], ct);
}
