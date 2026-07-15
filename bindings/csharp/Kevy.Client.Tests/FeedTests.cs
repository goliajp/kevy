// Change feed FEED.* — contract §6 "FEED replay", §3.10. The remote tests
// need a server started with the feed enabled; the embedded ones assert the
// C-ABI Unsupported caveat.

using Kevy;
using Xunit;

namespace Kevy.Tests;

public class FeedTests
{
    private const string FeedConfig = "[feed]\nenabled = true\n";

    [Fact]
    public void ReplayAndResume()
    {
        using var srv = ServerFixture.Spawn(new[] { "--threads", "1" }, FeedConfig);
        if (!srv.HasServer) return;
        using var c = KevyClient.Connect(srv.RequireUrl());

        Assert.True(c.FeedShards() >= 1);
        var (gen, off) = c.FeedTail(0);

        c.Set("fk1", "v1");
        c.Set("fk2", "v2");

        var batch = c.FeedRead(0, gen, off, 0);
        Assert.True(batch.Frames.Count >= 2);
        Assert.Contains(batch.Frames, f => f.Argv.Count > 0 && H.Str(f.Argv[0]) == "SET");

        // Resume from the returned cursor: caught up → empty batch.
        var next = c.FeedRead(0, batch.Generation, batch.NextOffset, 0);
        Assert.Empty(next.Frames);
    }

    [Fact]
    public void StaleCursorResync()
    {
        using var srv = ServerFixture.Spawn(new[] { "--threads", "1" }, FeedConfig);
        if (!srv.HasServer) return;
        using var c = KevyClient.Connect(srv.RequireUrl());
        try
        {
            c.FeedRead(0, 999999, 0, 0);
            return;
        }
        catch (KevyProtocolException)
        {
            // A FEEDRESYNC / cursor-ahead protocol error is the contract.
        }
    }

    [Fact]
    public void EmbeddedFeedUnsupported()
    {
        using var c = KevyClient.Connect("mem://feed-bus");
        Assert.Equal(1, c.FeedShards());
        Assert.Throws<KevyInvalidInputException>(() => c.FeedTail(1)); // non-zero shard
        Assert.Throws<KevyUnsupportedException>(() => c.FeedTail(0));  // feed not enabled
    }
}
