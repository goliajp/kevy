// Cluster-aware client (contract §3.15). One connection per shard, CRC16-
// slot routed so single-key commands hit their owner shard directly (no
// server -MOVED / forwarding hop). Requires the server in cluster mode.
// Topology discovered once at connect via CLUSTER SLOTS (16384 slots). CRC16
// matches Redis's key_hash_slot exactly so routing agrees with the server.

namespace Kevy;

/// <summary>A CRC16-routed multi-shard client (§3.15).</summary>
public sealed class ClusterClient : IDisposable
{
    private const int NumSlots = 16384;
    private RespConnection[] _shards;
    private readonly ushort[] _slotToShard;

    private ClusterClient(RespConnection[] shards, ushort[] table) { _shards = shards; _slotToShard = table; }

    /// <summary>Connect via a seed node, discover the topology, and open one
    /// connection per shard.</summary>
    public static ClusterClient Connect(string host, ushort port)
    {
        using var seed = RespConnection.Dial(host, port);
        var reply = seed.Request(["CLUSTER"u8.ToArray(), "SLOTS"u8.ToArray()]);
        var ranges = ParseSlots(reply);
        var (nodes, table) = BuildTopology(ranges);
        var shards = new List<RespConnection>(nodes.Count);
        try
        {
            foreach (var n in nodes) shards.Add(RespConnection.Dial(n.Host, n.Port));
        }
        catch
        {
            foreach (var s in shards) s.Dispose();
            throw;
        }
        return new ClusterClient(shards.ToArray(), table);
    }

    /// <summary>Number of distinct shard nodes.</summary>
    public int ShardCount => _shards.Length;

    private RespConnection ShardFor(KevyBytes key) => _shards[_slotToShard[Crc16.KeyHashSlot(key.Raw)]];

    /// <summary>Route a single-key command to the key's owner shard.</summary>
    public Reply RequestKeyed(KevyBytes key, params KevyBytes[] argv) => ShardFor(key).Request(argv.Raw());
    /// <summary>Async <see cref="RequestKeyed"/>.</summary>
    public Task<Reply> RequestKeyedAsync(KevyBytes key, KevyBytes[] argv, CancellationToken ct = default) =>
        ShardFor(key).RequestAsync(argv.Raw(), ct);

    /// <summary>Send a keyless command to shard 0.</summary>
    public Reply RequestUnkeyed(params KevyBytes[] argv) => _shards[0].Request(argv.Raw());
    /// <summary>Async <see cref="RequestUnkeyed"/>.</summary>
    public Task<Reply> RequestUnkeyedAsync(KevyBytes[] argv, CancellationToken ct = default) =>
        _shards[0].RequestAsync(argv.Raw(), ct);

    /// <summary>PING (any shard; accepts +PONG or +OK).</summary>
    public void Ping()
    {
        var r = RequestUnkeyed("PING");
        if (r.Kind == ReplyKind.Simple && (r.Str() == "PONG" || r.Str() == "OK")) return;
        throw r.Kind == ReplyKind.Error ? Err.ReplyError(r) : Err.Unexpected(r);
    }

    /// <summary>PUBLISH (process-global; any shard).</summary>
    public long Publish(KevyBytes channel, KevyBytes message) => Count(RequestUnkeyed("PUBLISH", channel, message));

    /// <summary>SET (key-routed).</summary>
    public void Set(KevyBytes key, KevyBytes value) => Ok(RequestKeyed(key, "SET", key, value));
    /// <summary>SET … PX (key-routed).</summary>
    public void SetWithTtl(KevyBytes key, KevyBytes value, TimeSpan ttl) =>
        Ok(RequestKeyed(key, "SET", key, value, "PX", Argv.I(Argv.DurationMs(ttl))));

    /// <summary>GET (key-routed); null on miss.</summary>
    public byte[]? Get(KevyBytes key)
    {
        var r = RequestKeyed(key, "GET", key);
        return r.Kind switch
        {
            ReplyKind.Bulk => r.Bytes,
            ReplyKind.Nil or ReplyKind.Null => null,
            ReplyKind.Error => throw Err.ReplyError(r),
            _ => throw Err.Unexpected(r),
        };
    }

    /// <summary>INCR (key-routed).</summary>
    public long Incr(KevyBytes key) => Int(RequestKeyed(key, "INCR", key));
    /// <summary>INCRBY (key-routed).</summary>
    public long IncrBy(KevyBytes key, long delta) => Int(RequestKeyed(key, "INCRBY", key, Argv.I(delta)));
    /// <summary>PEXPIRE (key-routed).</summary>
    public bool Expire(KevyBytes key, TimeSpan ttl) => Bool(RequestKeyed(key, "PEXPIRE", key, Argv.I(Argv.DurationMs(ttl))));
    /// <summary>PERSIST (key-routed).</summary>
    public bool Persist(KevyBytes key) => Bool(RequestKeyed(key, "PERSIST", key));
    /// <summary>PTTL (key-routed).</summary>
    public long TtlMs(KevyBytes key) => Int(RequestKeyed(key, "PTTL", key));

    /// <summary>DEL — routed per key and summed (cross-shard OK).</summary>
    public long Del(params KevyBytes[] keys) => PerKeySum("DEL", keys);
    /// <summary>EXISTS — routed per key and summed.</summary>
    public long Exists(params KevyBytes[] keys) => PerKeySum("EXISTS", keys);

    /// <summary>DBSIZE (whole-cluster; server fans out).</summary>
    public long DbSize() => Count(RequestUnkeyed("DBSIZE"));
    /// <summary>FLUSHALL (whole-cluster; server fans out).</summary>
    public void FlushAll() => Ok(RequestUnkeyed("FLUSHALL"));

    private long PerKeySum(string verb, KevyBytes[] keys)
    {
        long total = 0;
        foreach (var k in keys)
        {
            var n = Int(RequestKeyed(k, verb, k));
            if (n > 0) total += n;
        }
        return total;
    }

    // --- CLUSTER SLOTS parsing + topology ----------------------------------

    private readonly record struct Node(string Host, ushort Port);
    private readonly record struct Range(ushort Start, ushort End, string Host, ushort Port);

    private static (List<Node>, ushort[]) BuildTopology(List<Range> ranges)
    {
        if (ranges.Count == 0) throw new KevyProtocolException("CLUSTER SLOTS returned no ranges");
        var nodes = new List<Node>();
        var table = new ushort[NumSlots];
        foreach (var r in ranges)
        {
            var idx = nodes.FindIndex(n => n.Host == r.Host && n.Port == r.Port);
            if (idx < 0) { nodes.Add(new Node(r.Host, r.Port)); idx = nodes.Count - 1; }
            for (int slot = r.Start; slot <= r.End; slot++) table[slot] = (ushort)idx;
        }
        return (nodes, table);
    }

    private static List<Range> ParseSlots(Reply reply)
    {
        static KevyProtocolException Bad() => new("malformed CLUSTER SLOTS reply");
        if (reply.Kind != ReplyKind.Array) throw Bad();
        var outp = new List<Range>(reply.Items.Count);
        foreach (var row in reply.Items)
        {
            if (row.Kind != ReplyKind.Array || row.Items.Count < 3) throw Bad();
            if (row.Items[0].Kind != ReplyKind.Int || row.Items[1].Kind != ReplyKind.Int) throw Bad();
            var start = row.Items[0].Integer; var end = row.Items[1].Integer;
            var node = row.Items[2];
            if (node.Kind != ReplyKind.Array || node.Items.Count < 2) throw Bad();
            if (node.Items[0].Kind is not (ReplyKind.Bulk or ReplyKind.Simple) || node.Items[1].Kind != ReplyKind.Int) throw Bad();
            var port = node.Items[1].Integer;
            if (!InU16(start) || !InU16(end) || !InU16(port)) throw Bad();
            outp.Add(new Range((ushort)start, (ushort)end, node.Items[0].Str(), (ushort)port));
        }
        return outp;
    }

    private static bool InU16(long n) => n is >= 0 and <= 65535;

    private static long Int(Reply r) => r.Kind switch
    {
        ReplyKind.Int => r.Integer,
        ReplyKind.Error => throw Err.ReplyError(r),
        _ => throw Err.Unexpected(r),
    };

    private static long Count(Reply r)
    {
        var n = Int(r);
        return n < 0 ? throw new KevyProtocolException($"expected non-negative count, got {n}") : n;
    }

    private static bool Bool(Reply r) => r.Kind switch
    {
        ReplyKind.Int => r.Integer == 1,
        ReplyKind.Error => throw Err.ReplyError(r),
        _ => throw Err.Unexpected(r),
    };

    private static void Ok(Reply r)
    {
        if (r.Kind == ReplyKind.Simple && r.Str() == "OK") return;
        throw r.Kind == ReplyKind.Error ? Err.ReplyError(r) : Err.Unexpected(r);
    }

    /// <summary>Close every shard connection.</summary>
    public void Dispose()
    {
        foreach (var s in _shards) s.Dispose();
        _shards = Array.Empty<RespConnection>();
    }
}
