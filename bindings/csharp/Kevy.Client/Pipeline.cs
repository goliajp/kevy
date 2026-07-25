// Non-atomic pipeline batching (contract §3.13). Remote-only: the embedded
// backend answers Unsupported. Queue N commands client-side, send as one
// write, read N replies in order. NOT atomic. Per-command errors come back
// as Reply.Error entries INLINE (they do not abort the batch). An empty
// batch returns [] without touching the wire; an empty-argv command poisons
// the batch → InvalidInput at send.

using System.Buffers;

namespace Kevy;

/// <summary>Accumulates commands for <see cref="KevyClient.Pipeline"/>.</summary>
public sealed class PipelineBuf
{
    private readonly ArrayBufferWriter<byte> _buf = new();
    private int _count;
    private bool _poisoned;

    /// <summary>Queue one command (verb + args). Chainable; empty parts
    /// poisons the batch.</summary>
    public PipelineBuf Cmd(params KevyBytes[] parts)
    {
        if (parts.Length == 0) { _poisoned = true; return this; }
        var argv = parts.Raw();
        var size = Resp.EncodedSize(argv);
        Resp.EncodeInto(_buf.GetSpan(size), argv);
        _buf.Advance(size);
        _count++;
        return this;
    }

    /// <summary>How many commands are queued.</summary>
    public int Len => _count;

    /// <summary>Whether nothing is queued.</summary>
    public bool IsEmpty => _count == 0;

    internal bool Poisoned => _poisoned;
    internal byte[] Wire() => _buf.WrittenSpan.ToArray();
}

public sealed partial class KevyClient
{
    /// <summary>Run a pipelined batch: build queues commands, then they are
    /// sent as one write and the per-command replies returned in queue
    /// order. Remote-only.</summary>
    public IReadOnlyList<Reply> Pipeline(Action<PipelineBuf> build)
    {
        var (conn, p) = PreparePipeline(build);
        return p.Len == 0 ? Array.Empty<Reply>() : conn.PipelineRaw(p.Wire(), p.Len);
    }

    /// <summary>Async <see cref="Pipeline"/>.</summary>
    public async Task<IReadOnlyList<Reply>> PipelineAsync(Action<PipelineBuf> build, CancellationToken ct = default)
    {
        var (conn, p) = PreparePipeline(build);
        return p.Len == 0 ? Array.Empty<Reply>() : await conn.PipelineRawAsync(p.Wire(), p.Len, ct);
    }

    private (RespConnection, PipelineBuf) PreparePipeline(Action<PipelineBuf> build)
    {
        var conn = RequireRemote("pipeline");
        var p = new PipelineBuf();
        build(p);
        if (p.Poisoned) throw new KevyInvalidInputException("pipeline: a queued command had an empty argv");
        return (conn, p);
    }
}
