// Change feed / CDC: FEED.* (contract §3.10). Both backends serve the same
// cursor contract, but the C ABI kevy_open takes no feed-enable flag, so a
// pure-FFI embedded store cannot turn the feed on — embedded feed_tail/read
// answer Unsupported (matching the mem:// case), while feed_shards is always
// 1. Remote FEED.* requires the server started with feed enabled. An
// unservable cursor surfaces as a Protocol error starting "FEEDRESYNC <gen>
// <tail>". Both faces.

namespace Kevy;

public sealed partial class KevyClient
{
    /// <summary>Number of change-feed shards (embedded: 1).</summary>
    public long FeedShards() => _emb is not null ? 1 : ExecCount([Argv.S("FEED.SHARDS")]);
    /// <summary>Async FEED.SHARDS.</summary>
    public ValueTask<long> FeedShardsAsync(CancellationToken ct = default) =>
        _emb is not null ? new ValueTask<long>(1L) : ExecCountAsync([Argv.S("FEED.SHARDS")], ct);

    /// <summary>The shard's (generation, next_offset) cursor — where a fresh
    /// or resumed consumer begins.</summary>
    public (ulong Generation, ulong NextOffset) FeedTail(int shard)
    {
        EmbeddedFeedGuard(shard);
        return ParseTail(Exec([Argv.S("FEED.TAIL"), Argv.I(shard)]));
    }
    /// <summary>Async FEED.TAIL.</summary>
    public async ValueTask<(ulong Generation, ulong NextOffset)> FeedTailAsync(int shard, CancellationToken ct = default)
    {
        EmbeddedFeedGuard(shard);
        return ParseTail(await ExecAsync([Argv.S("FEED.TAIL"), Argv.I(shard)], ct));
    }

    /// <summary>Up to count frames past the cursor (server default 256, hard
    /// cap 4096), optionally key-prefix filtered (fail-open). count ≤ 0 uses
    /// the server default. Resume from (batch.Generation, batch.NextOffset).</summary>
    public FeedBatch FeedRead(int shard, ulong generation, ulong offset, int count = 0, params KevyBytes[] prefixes)
    {
        EmbeddedFeedGuard(shard);
        return ParseBatch(Exec(FeedReadArgv(shard, generation, offset, count, prefixes)));
    }
    /// <summary>Async FEED.READ.</summary>
    public async ValueTask<FeedBatch> FeedReadAsync(int shard, ulong generation, ulong offset, int count = 0, KevyBytes[]? prefixes = null, CancellationToken ct = default)
    {
        EmbeddedFeedGuard(shard);
        return ParseBatch(await ExecAsync(FeedReadArgv(shard, generation, offset, count, prefixes ?? Array.Empty<KevyBytes>()), ct));
    }

    private void EmbeddedFeedGuard(int shard)
    {
        if (_emb is null) return;
        if (shard != 0) throw new KevyInvalidInputException("embedded feed is single-shard: shard must be 0");
        throw new KevyUnsupportedException(
            "feed disabled: this port's embedded Connect does not enable the change feed");
    }

    private static byte[][] FeedReadArgv(int shard, ulong generation, ulong offset, int count, KevyBytes[] prefixes)
    {
        var argv = new List<byte[]> { Argv.S("FEED.READ"), Argv.I(shard), Argv.U(generation), Argv.U(offset) };
        if (count > 0) { argv.Add(Argv.S("COUNT")); argv.Add(Argv.I(count)); }
        foreach (var p in prefixes) { argv.Add(Argv.S("PREFIX")); argv.Add(p.Raw); }
        return argv.ToArray();
    }

    private static (ulong, ulong) ParseTail(Reply r)
    {
        if (r.Kind == ReplyKind.Array && r.Items.Count == 2 &&
            r.Items[0].Kind == ReplyKind.Int && r.Items[1].Kind == ReplyKind.Int)
            return ((ulong)r.Items[0].Integer, (ulong)r.Items[1].Integer);
        throw r.Kind == ReplyKind.Error ? Err.ReplyError(r) : Err.Unexpected(r);
    }

    private static FeedBatch ParseBatch(Reply r)
    {
        if (r.Kind == ReplyKind.Error) throw Err.ReplyError(r);
        if (r.Kind != ReplyKind.Array || r.Items.Count != 3)
            throw new KevyProtocolException("FEED.READ: expected [gen, next, frames]");
        var g = r.Items[0]; var next = r.Items[1]; var frames = r.Items[2];
        if (g.Kind != ReplyKind.Int || next.Kind != ReplyKind.Int)
            throw new KevyProtocolException("FEED.READ: non-integer cursor");
        if (frames.Kind != ReplyKind.Array) throw new KevyProtocolException("FEED.READ: frames not an array");
        var outFrames = new List<FeedFrame>(frames.Items.Count);
        foreach (var f in frames.Items) outFrames.Add(ParseFrame(f));
        return new FeedBatch((ulong)g.Integer, (ulong)next.Integer, outFrames);
    }

    private static FeedFrame ParseFrame(Reply f)
    {
        if (f.Kind != ReplyKind.Array || f.Items.Count != 2 ||
            f.Items[0].Kind != ReplyKind.Int || f.Items[1].Kind != ReplyKind.Array)
            throw new KevyProtocolException("FEED.READ: frame shape != [offset, argv]");
        var argv = new List<byte[]>(f.Items[1].Items.Count);
        foreach (var a in f.Items[1].Items)
            argv.Add(a.Kind is ReplyKind.Bulk or ReplyKind.Simple ? a.Bytes : throw Err.Unexpected(a));
        return new FeedFrame((ulong)f.Items[0].Integer, argv);
    }
}
