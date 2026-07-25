// Pipeline (remote-only) — contract §6 "Pipeline", §3.13.

using Kevy;
using Xunit;

namespace Kevy.Tests;

[Collection("server")]
public class PipelineTests(ServerFixture fx)
{
    [Fact]
    public void NCommandsOneRoundTrip()
    {
        if (!fx.HasServer) return;
        using var c = Fresh();
        var replies = c.Pipeline(p => p.Cmd("SET", "a", "1").Cmd("INCR", "n").Cmd("GET", "a"));
        Assert.Equal(3, replies.Count);
        Assert.Equal(1, replies[1].Integer);
        Assert.Equal("1", replies[2].Str());
    }

    [Fact]
    public void PerCommandErrorLandsInline()
    {
        if (!fx.HasServer) return;
        using var c = Fresh();
        // INCR on a string errors inline; the batch is NOT aborted.
        var replies = c.Pipeline(p => p.Cmd("SET", "s", "str").Cmd("INCR", "s").Cmd("GET", "s"));
        Assert.Equal(3, replies.Count);
        Assert.True(replies[1].IsError);
        Assert.Equal("str", replies[2].Str());
    }

    [Fact]
    public void EmptyBatchNoWireIo()
    {
        if (!fx.HasServer) return;
        using var c = Fresh();
        Assert.Empty(c.Pipeline(_ => { }));
    }

    [Fact]
    public void EmptyArgvIsInvalidInput()
    {
        if (!fx.HasServer) return;
        using var c = Fresh();
        Assert.Throws<KevyInvalidInputException>(() => c.Pipeline(p => p.Cmd()));
    }

    [Fact]
    public void EmbeddedPipelineUnsupported()
    {
        using var c = KevyClient.Connect("mem://pipe-bus");
        Assert.Throws<KevyUnsupportedException>(() => c.Pipeline(p => p.Cmd("PING")));
    }

    private KevyClient Fresh()
    {
        var c = KevyClient.Connect(fx.RequireUrl());
        c.FlushAll();
        return c;
    }
}
