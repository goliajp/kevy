// Collections + sorted-set algebra + hash-field TTL on both backends
// (contract §6 "Collections"/"Sorted-set algebra"/"Hash-field TTL",
// §3.2–§3.7).

using Kevy;
using Xunit;

namespace Kevy.Tests;

[Collection("server")]
public class CollectionsTests(ServerFixture fx)
{
    [Fact]
    public void Hash() => H.Both(fx, c =>
    {
        Assert.Equal(2, c.HSet("h", "f1", "v1", "f2", "v2"));
        Assert.Equal(0, c.HSet("h", "f1", "v1b")); // overwrite, not newly-added
        Assert.Equal("v1b", H.Str(c.HGet("h", "f1")));
        Assert.Null(c.HGet("h", "nope"));
        Assert.Equal(2, c.HLen("h"));
        Assert.Equal(4, c.HGetAll("h").Count); // flat [f,v,f,v]
        Assert.Equal(2, c.HKeys("h").Count);
        Assert.Equal(2, c.HVals("h").Count);
        Assert.Equal(1, c.HDel("h", "f1"));
        Assert.Equal(1, c.HLen("h"));
    });

    [Fact]
    public void List() => H.Both(fx, c =>
    {
        Assert.Equal(2, c.LPush("l", "b", "a")); // a is now head (LPUSH b then a)
        Assert.Equal(3, c.RPush("l", "c"));
        Assert.Equal(3, c.LLen("l"));
        var range = c.LRange("l", 0, -1);
        Assert.Equal(3, range.Count);
        var head = c.LPop("l", 1);
        Assert.Single(head);
        var tail = c.RPop("l", 5);
        Assert.NotEmpty(tail);
        Assert.Empty(c.LPop("l", 1)); // drained
    });

    [Fact]
    public void Set() => H.Both(fx, c =>
    {
        Assert.Equal(3, c.SAdd("s", "a", "b", "c"));
        Assert.Equal(0, c.SAdd("s", "a")); // already present
        Assert.Equal(3, c.SCard("s"));
        Assert.True(c.SIsMember("s", "a"));
        Assert.False(c.SIsMember("s", "z"));
        Assert.Equal(3, c.SMembers("s").Count);
        Assert.Equal(1, c.SRem("s", "a"));

        c.SAdd("s1", "a", "b", "c");
        c.SAdd("s2", "b", "c", "d");
        Assert.Equal(2, c.SInter("s1", "s2").Count);      // b, c
        Assert.Equal(4, c.SUnion("s1", "s2").Count);      // a,b,c,d
        Assert.Single(c.SDiff("s1", "s2"));               // a
    });

    [Fact]
    public void SortedSet() => H.Both(fx, c =>
    {
        Assert.Equal(3, c.ZAdd("z",
            new ZMember(1, H.B("a")), new ZMember(3, H.B("c")), new ZMember(2, H.B("b"))));
        Assert.Equal(2.0, c.ZScore("z", "b"));
        Assert.Null(c.ZScore("z", "nope"));
        Assert.Equal(3, c.ZCard("z"));
        var asc = c.ZRange("z", 0, -1);
        Assert.Equal("a", H.Str(asc[0]));
        Assert.Equal("b", H.Str(asc[1]));
        Assert.Equal("c", H.Str(asc[2]));
        Assert.Equal(1, c.ZRem("z", "a"));
    });

    [Fact]
    public void ZAlgebra() => H.Both(fx, c =>
    {
        c.ZAdd("z1", new ZMember(1, H.B("a")), new ZMember(2, H.B("b")));
        c.ZAdd("z2", new ZMember(10, H.B("b")), new ZMember(20, H.B("c")));
        Assert.Equal(1, c.ZInterStore("dInter", "z1", "z2")); // just b
        Assert.Equal(3, c.ZUnionStore("dUnion", "z1", "z2")); // a,b,c
        Assert.Equal(1, c.ZInterCard(new KevyBytes[] { "z1", "z2" }, null));
        Assert.Equal(12, c.ZScore("dInter", "b")); // 2 + 10 SUM
        // AGGREGATE MAX
        c.ZInterStoreWith("dMax", new KevyBytes[] { "z1", "z2" }, null, ZAggregate.Max);
        Assert.Equal(10, c.ZScore("dMax", "b"));
        // WEIGHTS
        c.ZUnionStoreWith("dW", new KevyBytes[] { "z1", "z2" }, new double[] { 2, 1 }, ZAggregate.Sum);
        Assert.Equal(2, c.ZScore("dW", "a")); // a only in z1: 1*2
        Assert.Throws<KevyInvalidInputException>(() => c.ZInterStore("d"));
    });

    [Fact]
    public void HashFieldTtl() => H.Both(fx, c =>
    {
        c.HSet("h", "f1", "v1", "f2", "v2");
        var codes = c.HPExpire("h", new KevyBytes[] { "f1", "nope" },
            TimeSpan.FromSeconds(100), HExpireCond.Always);
        Assert.Equal(2, codes.Count);
        Assert.Equal(1, codes[0]);   // deadline set
        Assert.Equal((sbyte)-2, codes[1]); // missing field
        var ttls = c.HPTtl("h", "f1");
        Assert.InRange(ttls[0], 1, 100_000);
        var secs = c.HTtl("h", "f1");
        Assert.InRange(secs[0], 1, 100);
        var cleared = c.HPersist("h", "f1");
        Assert.Equal(1, cleared[0]);
        Assert.Throws<KevyInvalidInputException>(() =>
            c.HPExpire("h", Array.Empty<KevyBytes>(), TimeSpan.FromSeconds(1), HExpireCond.Always));
    });
}
