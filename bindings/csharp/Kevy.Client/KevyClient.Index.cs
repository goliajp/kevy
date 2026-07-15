// Declarative secondary indexes: IDX.* (contract §3.8). Remote-only — the
// embedded backend answers Unsupported. Two-layered: typed shortcuts plus
// *Raw argv passthroughs that keep every server capability (COMPOSE /
// HYBRID / GROUPS / ANN / text) reachable. Both faces.

using System.Buffers.Binary;

namespace Kevy;

public sealed partial class KevyClient
{
    /// <summary>Declare a plain range index:
    /// IDX.CREATE name ON PREFIX prefix FIELD field TYPE ty KIND range.</summary>
    public void IdxCreateRange(KevyBytes name, KevyBytes prefix, KevyBytes field, IdxType ty) =>
        IdxCreateRaw(name, "ON", "PREFIX", prefix, "FIELD", field, "TYPE", ty.Tag(), "KIND", "range");
    /// <summary>Async IDX.CREATE range.</summary>
    public Task IdxCreateRangeAsync(KevyBytes name, KevyBytes prefix, KevyBytes field, IdxType ty, CancellationToken ct = default) =>
        IdxCreateRawAsync(new KevyBytes[] { name, "ON", "PREFIX", prefix, "FIELD", field, "TYPE", ty.Tag(), "KIND", "range" }, ct);

    /// <summary>IDX.CREATE argv passthrough (everything after the verb).</summary>
    public void IdxCreateRaw(params KevyBytes[] args) { RequireRemote("IDX.CREATE"); ExecOk(Argv.Cmd("IDX.CREATE", args.Raw())); }
    /// <summary>Async IDX.CREATE passthrough.</summary>
    public Task IdxCreateRawAsync(KevyBytes[] args, CancellationToken ct = default)
    {
        RequireRemote("IDX.CREATE");
        return ExecOkAsync(Argv.Cmd("IDX.CREATE", args.Raw()), ct);
    }

    /// <summary>IDX.DROP; whether it existed.</summary>
    public bool IdxDrop(KevyBytes name) { RequireRemote("IDX.DROP"); return ExecBool([Argv.S("IDX.DROP"), name.Raw]); }
    /// <summary>Async IDX.DROP.</summary>
    public Task<bool> IdxDropAsync(KevyBytes name, CancellationToken ct = default)
    { RequireRemote("IDX.DROP"); return ExecBoolAsync([Argv.S("IDX.DROP"), name.Raw], ct); }

    /// <summary>IDX.LIST (unknown labels skipped).</summary>
    public IReadOnlyList<IdxInfo> IdxList() { RequireRemote("IDX.LIST"); return ParseIdxList(Exec([Argv.S("IDX.LIST")])); }
    /// <summary>Async IDX.LIST.</summary>
    public async Task<IReadOnlyList<IdxInfo>> IdxListAsync(CancellationToken ct = default)
    { RequireRemote("IDX.LIST"); return ParseIdxList(await ExecAsync([Argv.S("IDX.LIST")], ct)); }

    /// <summary>IDX.QUERY name RANGE min max [LIMIT n] [CURSOR c] → one page
    /// in (value, key) order; resume with <see cref="IdxPage.Cursor"/>.</summary>
    public IdxPage IdxQueryRange(KevyBytes name, KevyBytes min, KevyBytes max, int limit, byte[]? cursor = null) =>
        ParseIdxPage(IdxQueryRaw(RangeArgs(name, min, max, limit, cursor)));
    /// <summary>Async IDX.QUERY RANGE.</summary>
    public async Task<IdxPage> IdxQueryRangeAsync(KevyBytes name, KevyBytes min, KevyBytes max, int limit, byte[]? cursor = null, CancellationToken ct = default) =>
        ParseIdxPage(await IdxQueryRawAsync(RangeArgs(name, min, max, limit, cursor), ct));

    /// <summary>IDX.QUERY name EQ value [LIMIT n].</summary>
    public IdxPage IdxQueryEq(KevyBytes name, KevyBytes value, int limit) =>
        ParseIdxPage(IdxQueryRaw(new KevyBytes[] { name, "EQ", value, "LIMIT", Argv.I(limit) }));
    /// <summary>Async IDX.QUERY EQ.</summary>
    public async Task<IdxPage> IdxQueryEqAsync(KevyBytes name, KevyBytes value, int limit, CancellationToken ct = default) =>
        ParseIdxPage(await IdxQueryRawAsync(new KevyBytes[] { name, "EQ", value, "LIMIT", Argv.I(limit) }, ct));

    /// <summary>IDX.QUERY name MATCH text [LIMIT n] → BM25 (key, score), best first.</summary>
    public IReadOnlyList<Ranked> IdxQueryMatch(KevyBytes name, KevyBytes text, int limit) =>
        ParseRanked(IdxQueryRaw(new KevyBytes[] { name, "MATCH", text, "LIMIT", Argv.I(limit) }));
    /// <summary>Async IDX.QUERY MATCH.</summary>
    public async Task<IReadOnlyList<Ranked>> IdxQueryMatchAsync(KevyBytes name, KevyBytes text, int limit, CancellationToken ct = default) =>
        ParseRanked(await IdxQueryRawAsync(new KevyBytes[] { name, "MATCH", text, "LIMIT", Argv.I(limit) }, ct));

    /// <summary>IDX.QUERY name KNN vector [LIMIT k] → (key, distance) nearest
    /// first; vector encoded as an f32 little-endian blob.</summary>
    public IReadOnlyList<Ranked> IdxQueryKnn(KevyBytes name, float[] vector, int k) =>
        ParseRanked(IdxQueryRaw(new KevyBytes[] { name, "KNN", VectorBlob(vector), "LIMIT", Argv.I(k) }));
    /// <summary>Async IDX.QUERY KNN.</summary>
    public async Task<IReadOnlyList<Ranked>> IdxQueryKnnAsync(KevyBytes name, float[] vector, int k, CancellationToken ct = default) =>
        ParseRanked(await IdxQueryRawAsync(new KevyBytes[] { name, "KNN", VectorBlob(vector), "LIMIT", Argv.I(k) }, ct));

    /// <summary>IDX.QUERY argv passthrough returning the raw Reply — the
    /// escape hatch for COMPOSE / HYBRID / GROUPS / FIELDS.</summary>
    public Reply IdxQueryRaw(params KevyBytes[] args)
    {
        RequireRemote("IDX.QUERY");
        var r = Exec(Argv.Cmd("IDX.QUERY", args.Raw()));
        return r.Kind == ReplyKind.Error ? throw Err.ReplyError(r) : r;
    }
    /// <summary>Async IDX.QUERY passthrough.</summary>
    public async Task<Reply> IdxQueryRawAsync(KevyBytes[] args, CancellationToken ct = default)
    {
        RequireRemote("IDX.QUERY");
        var r = await ExecAsync(Argv.Cmd("IDX.QUERY", args.Raw()), ct);
        return r.Kind == ReplyKind.Error ? throw Err.ReplyError(r) : r;
    }

    private static KevyBytes[] RangeArgs(KevyBytes name, KevyBytes min, KevyBytes max, int limit, byte[]? cursor)
    {
        var args = new List<KevyBytes> { name, "RANGE", min, max, "LIMIT", Argv.I(limit) };
        if (cursor is not null) { args.Add("CURSOR"); args.Add(cursor); }
        return args.ToArray();
    }

    private static byte[] VectorBlob(float[] vector)
    {
        var blob = new byte[vector.Length * 4];
        for (var i = 0; i < vector.Length; i++)
            BinaryPrimitives.WriteSingleLittleEndian(blob.AsSpan(i * 4), vector[i]);
        return blob;
    }

    private static IReadOnlyList<IdxInfo> ParseIdxList(Reply r)
    {
        if (r.Kind == ReplyKind.Error) throw Err.ReplyError(r);
        if (r.Kind != ReplyKind.Array) throw Err.Unexpected(r);
        var outp = new List<IdxInfo>(r.Items.Count);
        foreach (var entry in r.Items) outp.Add(ParseIdxInfo(entry));
        return outp;
    }

    private static IdxInfo ParseIdxInfo(Reply entry)
    {
        if (entry.Kind != ReplyKind.Array) throw Err.Unexpected(entry);
        byte[] name = [], prefix = [];
        string kind = "", state = "";
        ulong entries = 0, bytes = 0;
        var cells = entry.Items;
        for (var i = 0; i + 1 < cells.Count; i += 2)
        {
            if (cells[i].Kind != ReplyKind.Bulk || cells[i + 1].Kind != ReplyKind.Bulk) continue;
            var val = cells[i + 1];
            switch (cells[i].Str())
            {
                case "name": name = val.Bytes; break;
                case "prefix": prefix = val.Bytes; break;
                case "kind": kind = val.Str(); break;
                case "state": state = val.Str(); break;
                case "entries": entries = ParseUlong(val.Bytes); break;
                case "bytes": bytes = ParseUlong(val.Bytes); break;
                // forward-compatible: skip labels this version doesn't know
            }
        }
        return new IdxInfo(name, prefix, kind, state, entries, bytes);
    }

    private static IdxPage ParseIdxPage(Reply r)
    {
        if (r.Kind != ReplyKind.Array || r.Items.Count != 2)
            throw new KevyProtocolException("IDX.QUERY page: expected [cursor, rows]");
        var cur = r.Items[0];
        if (cur.Kind != ReplyKind.Bulk) throw Err.Unexpected(cur);
        byte[]? cursor = cur.Str() == "0" ? null : cur.Bytes;
        var flat = r.Items[1];
        if (flat.Kind != ReplyKind.Array) throw new KevyProtocolException("IDX.QUERY page: rows not an array");
        if (flat.Items.Count % 2 != 0) throw new KevyProtocolException("IDX.QUERY page: odd row pair");
        var rows = new List<IdxRow>(flat.Items.Count / 2);
        for (var i = 0; i < flat.Items.Count; i += 2)
        {
            var k = flat.Items[i]; var v = flat.Items[i + 1];
            if (k.Kind != ReplyKind.Bulk || v.Kind != ReplyKind.Bulk)
                throw new KevyProtocolException("IDX.QUERY page: non-bulk row pair");
            rows.Add(new IdxRow(k.Bytes, v.Bytes));
        }
        return new IdxPage(cursor, rows);
    }

    private static IReadOnlyList<Ranked> ParseRanked(Reply r)
    {
        if (r.Kind != ReplyKind.Array) throw Err.Unexpected(r);
        var outp = new List<Ranked>(r.Items.Count);
        foreach (var row in r.Items)
        {
            if (row.Kind != ReplyKind.Array || row.Items.Count < 2)
                throw new KevyProtocolException("IDX.QUERY ranked row: expected [key, score]");
            if (row.Items[0].Kind != ReplyKind.Bulk) throw Err.Unexpected(row.Items[0]);
            outp.Add(new Ranked(row.Items[0].Bytes, ScoreOf(row.Items[1])));
        }
        return outp;
    }

    private static ulong ParseUlong(byte[] b)
    {
        var s = System.Text.Encoding.ASCII.GetString(b);
        return ulong.TryParse(s, out var n) ? n : throw new KevyProtocolException("bad integer reply: " + s);
    }
}
