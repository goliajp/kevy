// Core string / generic key commands (contract §3.1). Every method has a
// blocking form and an async twin; both build the same argv.

namespace Kevy;

public sealed partial class KevyClient
{
    /// <summary>PING (embedded always OK).</summary>
    public void Ping()
    {
        if (_emb is not null) return;
        var r = Exec([Argv.S("PING")]);
        ExpectPong(r);
    }

    /// <summary>Async PING.</summary>
    public async Task PingAsync(CancellationToken ct = default)
    {
        if (_emb is not null) return;
        ExpectPong(await ExecAsync([Argv.S("PING")], ct));
    }

    private static void ExpectPong(Reply r)
    {
        if (r.Kind == ReplyKind.Simple && r.Str() == "PONG") return;
        throw r.Kind == ReplyKind.Error ? Err.ReplyError(r) : Err.Unexpected(r);
    }

    /// <summary>SET key value (unconditional; contract §3.1).</summary>
    public void Set(KevyBytes key, KevyBytes value) => ExecOk([Argv.S("SET"), key.Raw, value.Raw]);
    /// <summary>Async SET.</summary>
    public Task SetAsync(KevyBytes key, KevyBytes value, CancellationToken ct = default) =>
        ExecOkAsync([Argv.S("SET"), key.Raw, value.Raw], ct);

    /// <summary>GET key; null when absent or expired.</summary>
    public byte[]? Get(KevyBytes key) => ExecOptBulk([Argv.S("GET"), key.Raw]);
    /// <summary>Async GET.</summary>
    public Task<byte[]?> GetAsync(KevyBytes key, CancellationToken ct = default) =>
        ExecOptBulkAsync([Argv.S("GET"), key.Raw], ct);

    /// <summary>DEL keys, returning how many were removed.</summary>
    public long Del(params KevyBytes[] keys) => ExecCount(Argv.Cmd("DEL", keys.Raw()));
    /// <summary>Async DEL.</summary>
    public Task<long> DelAsync(KevyBytes[] keys, CancellationToken ct = default) =>
        ExecCountAsync(Argv.Cmd("DEL", keys.Raw()), ct);

    /// <summary>EXISTS keys (a repeated key counts each time).</summary>
    public long Exists(params KevyBytes[] keys) => ExecCount(Argv.Cmd("EXISTS", keys.Raw()));
    /// <summary>Async EXISTS.</summary>
    public Task<long> ExistsAsync(KevyBytes[] keys, CancellationToken ct = default) =>
        ExecCountAsync(Argv.Cmd("EXISTS", keys.Raw()), ct);

    /// <summary>INCR key, returning the post-increment value.</summary>
    public long Incr(KevyBytes key) => ExecInt([Argv.S("INCR"), key.Raw]);
    /// <summary>Async INCR.</summary>
    public Task<long> IncrAsync(KevyBytes key, CancellationToken ct = default) =>
        ExecIntAsync([Argv.S("INCR"), key.Raw], ct);

    /// <summary>INCRBY key delta (negative = DECRBY).</summary>
    public long IncrBy(KevyBytes key, long delta) => ExecInt([Argv.S("INCRBY"), key.Raw, Argv.I(delta)]);
    /// <summary>Async INCRBY.</summary>
    public Task<long> IncrByAsync(KevyBytes key, long delta, CancellationToken ct = default) =>
        ExecIntAsync([Argv.S("INCRBY"), key.Raw, Argv.I(delta)], ct);

    /// <summary>Set key's TTL (wire PEXPIRE); false when the key is absent.</summary>
    public bool Expire(KevyBytes key, TimeSpan ttl) =>
        ExecBool([Argv.S("PEXPIRE"), key.Raw, Argv.I(Argv.DurationMs(ttl))]);
    /// <summary>Async PEXPIRE.</summary>
    public Task<bool> ExpireAsync(KevyBytes key, TimeSpan ttl, CancellationToken ct = default) =>
        ExecBoolAsync([Argv.S("PEXPIRE"), key.Raw, Argv.I(Argv.DurationMs(ttl))], ct);

    /// <summary>Remove key's TTL; false when there was none.</summary>
    public bool Persist(KevyBytes key) => ExecBool([Argv.S("PERSIST"), key.Raw]);
    /// <summary>Async PERSIST.</summary>
    public Task<bool> PersistAsync(KevyBytes key, CancellationToken ct = default) =>
        ExecBoolAsync([Argv.S("PERSIST"), key.Raw], ct);

    /// <summary>Ms remaining (wire PTTL): -2 no key, -1 no TTL.</summary>
    public long TtlMs(KevyBytes key) => ExecInt([Argv.S("PTTL"), key.Raw]);
    /// <summary>Async PTTL.</summary>
    public Task<long> TtlMsAsync(KevyBytes key, CancellationToken ct = default) =>
        ExecIntAsync([Argv.S("PTTL"), key.Raw], ct);

    /// <summary>The value's type name, or "none".</summary>
    public string TypeOf(KevyBytes key) => ShapeType(Exec([Argv.S("TYPE"), key.Raw]));
    /// <summary>Async TYPE.</summary>
    public async Task<string> TypeOfAsync(KevyBytes key, CancellationToken ct = default) =>
        ShapeType(await ExecAsync([Argv.S("TYPE"), key.Raw], ct));

    private static string ShapeType(Reply r) => r.Kind switch
    {
        ReplyKind.Simple => r.Str(),
        ReplyKind.Error => throw Err.ReplyError(r),
        _ => throw Err.Unexpected(r),
    };

    /// <summary>The live key count.</summary>
    public long DbSize() => ExecCount([Argv.S("DBSIZE")]);
    /// <summary>Async DBSIZE.</summary>
    public Task<long> DbSizeAsync(CancellationToken ct = default) => ExecCountAsync([Argv.S("DBSIZE")], ct);

    /// <summary>Wipe the store.</summary>
    public void FlushAll() => ExecOk([Argv.S("FLUSHALL")]);
    /// <summary>Async FLUSHALL.</summary>
    public Task FlushAllAsync(CancellationToken ct = default) => ExecOkAsync([Argv.S("FLUSHALL")], ct);

    /// <summary>Atomic SET … PX ttl_ms.</summary>
    public void SetWithTtl(KevyBytes key, KevyBytes value, TimeSpan ttl) =>
        ExecOk([Argv.S("SET"), key.Raw, value.Raw, Argv.S("PX"), Argv.I(Argv.DurationMs(ttl))]);
    /// <summary>Async atomic SET … PX.</summary>
    public Task SetWithTtlAsync(KevyBytes key, KevyBytes value, TimeSpan ttl, CancellationToken ct = default) =>
        ExecOkAsync([Argv.S("SET"), key.Raw, value.Raw, Argv.S("PX"), Argv.I(Argv.DurationMs(ttl))], ct);

    /// <summary>MGET keys; a missing/wrong-type key yields null, in order.</summary>
    public IReadOnlyList<byte[]?> MGet(params KevyBytes[] keys) => ShapeBulks(Exec(Argv.Cmd("MGET", keys.Raw())));
    /// <summary>Async MGET.</summary>
    public async Task<IReadOnlyList<byte[]?>> MGetAsync(KevyBytes[] keys, CancellationToken ct = default) =>
        ShapeBulks(await ExecAsync(Argv.Cmd("MGET", keys.Raw()), ct));

    /// <summary>MSET pairs (flat [k,v,k,v,…]) atomically.</summary>
    public void MSet(params KevyBytes[] pairs) => ExecOk(MSetArgv(pairs));
    /// <summary>Async MSET.</summary>
    public Task MSetAsync(KevyBytes[] pairs, CancellationToken ct = default) => ExecOkAsync(MSetArgv(pairs), ct);

    private static byte[][] MSetArgv(KevyBytes[] pairs)
    {
        if (pairs.Length % 2 != 0)
            throw new KevyInvalidInputException("MSet needs an even number of key/value arguments");
        return Argv.Cmd("MSET", pairs.Raw());
    }

    /// <summary>PUBLISH message to channel; subscribers reached.</summary>
    public long Publish(KevyBytes channel, KevyBytes message) =>
        ExecCount([Argv.S("PUBLISH"), channel.Raw, message.Raw]);
    /// <summary>Async PUBLISH.</summary>
    public Task<long> PublishAsync(KevyBytes channel, KevyBytes message, CancellationToken ct = default) =>
        ExecCountAsync([Argv.S("PUBLISH"), channel.Raw, message.Raw], ct);
}
