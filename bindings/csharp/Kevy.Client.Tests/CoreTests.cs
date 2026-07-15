// Core KV on both backends (contract §6 "Core KV", §3.1). Also asserts the
// sync and async faces agree (§1.4).

using Kevy;
using Xunit;

namespace Kevy.Tests;

[Collection("server")]
public class CoreTests(ServerFixture fx)
{
    [Fact]
    public void SetGetDelExistsIncr() => H.Both(fx, c =>
    {
        c.Set("k", "v");
        Assert.Equal("v", H.Str(c.Get("k")));
        Assert.Null(c.Get("missing"));
        Assert.Equal(1, c.Exists("k"));
        Assert.Equal(2, c.Exists("k", "k")); // repeated key counts each time
        Assert.Equal(1, c.Del("k"));
        Assert.Equal(0, c.Exists("k"));

        Assert.Equal(1, c.Incr("n"));
        Assert.Equal(6, c.IncrBy("n", 5));
        Assert.Equal(1, c.IncrBy("n", -5));
    });

    [Fact]
    public void ExpirePersistTtl() => H.Both(fx, c =>
    {
        c.Set("k", "v");
        Assert.Equal(-1, c.TtlMs("k"));          // no TTL
        Assert.Equal(-2, c.TtlMs("nope"));       // no key
        Assert.True(c.Expire("k", TimeSpan.FromSeconds(100)));
        Assert.InRange(c.TtlMs("k"), 1, 100_000);
        Assert.True(c.Persist("k"));
        Assert.Equal(-1, c.TtlMs("k"));
        Assert.False(c.Expire("gone", TimeSpan.FromSeconds(1)));
    });

    [Fact]
    public void SetWithTtlIsAtomic() => H.Both(fx, c =>
    {
        c.SetWithTtl("s", "v", TimeSpan.FromSeconds(50));
        Assert.Equal("v", H.Str(c.Get("s")));
        Assert.InRange(c.TtlMs("s"), 1, 50_000);
    });

    [Fact]
    public void TypeOfDbSizeFlush() => H.Both(fx, c =>
    {
        c.FlushAll();
        c.Set("str", "v");
        c.LPush("lst", "a");
        c.HSet("h", "f", "v");
        c.SAdd("st", "m");
        c.ZAdd("z", new ZMember(1, H.B("m")));
        Assert.Equal("string", c.TypeOf("str"));
        Assert.Equal("list", c.TypeOf("lst"));
        Assert.Equal("hash", c.TypeOf("h"));
        Assert.Equal("set", c.TypeOf("st"));
        Assert.Equal("zset", c.TypeOf("z"));
        Assert.Equal("none", c.TypeOf("absent"));
        Assert.Equal(5, c.DbSize());
        c.FlushAll();
        Assert.Equal(0, c.DbSize());
    });

    [Fact]
    public void MGetOrderAndMSet() => H.Both(fx, c =>
    {
        c.MSet("a", "1", "b", "2");
        var got = c.MGet("a", "missing", "b");
        Assert.Equal(3, got.Count);
        Assert.Equal("1", H.Str(got[0]));
        Assert.Null(got[1]);
        Assert.Equal("2", H.Str(got[2]));
    });

    [Fact]
    public async Task SyncAndAsyncAgree() => await H.BothAsync(fx, async c =>
    {
        await c.SetAsync("k", "async");
        Assert.Equal("async", H.Str(await c.GetAsync("k")));
        Assert.Equal(1, await c.IncrAsync("ctr"));
        Assert.Equal(3, await c.IncrByAsync("ctr", 2));
        // The sync face over the same client sees the async writes.
        Assert.Equal("async", H.Str(c.Get("k")));
    });

    [Fact]
    public void PingWorks() => H.Both(fx, c => c.Ping());
}
