// Declarative indexes IDX.* (remote-only) — contract §6 "IDX query", §3.8.

using Kevy;
using Xunit;

namespace Kevy.Tests;

[Collection("server")]
public class IndexTests(ServerFixture fx)
{
    [Fact]
    public void RangePagingEqAndList()
    {
        if (!fx.HasServer) return;
        using var c = Fresh();
        c.IdxCreateRange("byage", "user:", "age", IdxType.I64);
        var ages = new[] { "21", "22", "23", "24", "25" };
        for (var i = 0; i < ages.Length; i++)
            c.HSet($"user:{(char)('a' + i)}", "age", ages[i]);
        WaitReady(c, "byage");

        // IdxInfo parse (incl. unknown-label skip).
        var infos = c.IdxList();
        var info = Assert.Single(infos, x => H.Str(x.Name) == "byage");
        Assert.Equal("range", info.Kind);

        // Range paging: LIMIT 2 pages through 5 rows.
        var seen = 0;
        byte[]? cursor = null;
        do
        {
            var page = c.IdxQueryRange("byage", "0", "100", 2, cursor);
            seen += page.Rows.Count;
            cursor = page.Cursor;
        } while (cursor is not null && seen <= 10);
        Assert.Equal(5, seen);

        // EQ point lookup.
        var eq = c.IdxQueryEq("byage", "23", 10);
        Assert.Single(eq.Rows);
        Assert.Equal("23", H.Str(eq.Rows[0].Value));

        // Drop reports existed, then gone.
        Assert.True(c.IdxDrop("byage"));
        Assert.False(c.IdxDrop("byage"));
    }

    [Fact]
    public void EmbeddedIdxUnsupported()
    {
        using var c = KevyClient.Connect("mem://idx-bus");
        Assert.Throws<KevyUnsupportedException>(() => c.IdxList());
        Assert.Throws<KevyUnsupportedException>(() => c.IdxCreateRange("i", "u:", "age", IdxType.I64));
    }

    private static void WaitReady(KevyClient c, string name)
    {
        var deadline = DateTime.UtcNow.AddSeconds(5);
        while (DateTime.UtcNow < deadline)
        {
            if (c.IdxList().Any(i => H.Str(i.Name) == name && i.State == "ready")) return;
            Thread.Sleep(20);
        }
        throw new InvalidOperationException($"index {name} never became ready");
    }

    private KevyClient Fresh()
    {
        var c = KevyClient.Connect(fx.RequireUrl());
        c.FlushAll();
        return c;
    }
}
