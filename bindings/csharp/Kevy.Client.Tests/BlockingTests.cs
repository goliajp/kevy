// Blocking pops on both backends (contract §6 "Blocking pops", §3.14).

using Kevy;
using Xunit;

namespace Kevy.Tests;

[Collection("server")]
public class BlockingTests(ServerFixture fx)
{
    [Fact]
    public void ImmediateHitsAndTimeout() => H.Both(fx, c =>
    {
        c.RPush("q", "a", "b");
        var d = TimeSpan.FromSeconds(1);
        var hit = c.BLPop(new KevyBytes[] { "q" }, d);
        Assert.NotNull(hit);
        Assert.Equal("q", H.Str(hit!.Value.Key));
        Assert.Equal("a", H.Str(hit.Value.Value));

        var tail = c.BRPop(new KevyBytes[] { "q" }, d);
        Assert.Equal("b", H.Str(tail!.Value.Value));

        // Empty list with a short timeout → null.
        Assert.Null(c.BLPop(new KevyBytes[] { "empty" }, TimeSpan.FromMilliseconds(60)));

        c.ZAdd("z", new ZMember(2, H.B("hi")), new ZMember(1, H.B("lo")));
        var zh = c.BZPopMin(new KevyBytes[] { "z" }, d);
        Assert.NotNull(zh);
        Assert.Equal("lo", H.Str(zh!.Value.Member));
        Assert.Equal(1.0, zh.Value.Score);
    });

    [Fact]
    public void InvalidArgs() => H.Both(fx, c =>
    {
        Assert.Throws<KevyInvalidInputException>(() => c.BLPop(new KevyBytes[] { "q" }, TimeSpan.Zero));
        Assert.Throws<KevyInvalidInputException>(() => c.BLPop(Array.Empty<KevyBytes>(), TimeSpan.FromSeconds(1)));
    });

    [Fact]
    public void WakesOnConcurrentPush()
    {
        foreach (var url in H.PubsubUrls(fx))
        {
            using var consumer = KevyClient.Connect(url);
            using var producer = KevyClient.Connect(url);
            var task = Task.Run(() => consumer.BLPop(new KevyBytes[] { "bq" }, TimeSpan.FromSeconds(3)));
            Thread.Sleep(80);
            producer.RPush("bq", "payload");
            var hit = task.GetAwaiter().GetResult();
            Assert.NotNull(hit);
            Assert.Equal("payload", H.Str(hit!.Value.Value));
        }
    }
}
