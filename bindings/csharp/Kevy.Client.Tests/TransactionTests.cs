// Transactions (remote-only) — contract §6 "Transactions", §3.12.

using Kevy;
using Xunit;

namespace Kevy.Tests;

[Collection("server")]
public class TransactionTests(ServerFixture fx)
{
    [Fact]
    public void ExecReturnsNRepliesInOrder()
    {
        if (!fx.HasServer) return;
        using var c = Fresh();
        var txn = c.Multi();
        txn.Set("a", "1").Incr("n").Get("a");
        var replies = txn.Exec();
        Assert.Equal(3, replies.Count);
        Assert.Equal(ReplyKind.Simple, replies[0].Kind);
        Assert.Equal(1, replies[1].Integer);
        Assert.Equal("1", replies[2].Str());
    }

    [Fact]
    public void TypedCursorExtractors()
    {
        if (!fx.HasServer) return;
        using var c = Fresh();
        var txn = c.Multi();
        txn.Set("a", "v").Incr("ctr").Get("a");
        var cur = txn.ExecTyped();
        cur.NextOk();
        Assert.Equal(1, cur.NextInt());
        Assert.Equal("v", H.Str(cur.NextBulk()));
        cur.ExpectEmpty(); // arity gate
    }

    [Fact]
    public void WatchAbort()
    {
        if (!fx.HasServer) return;
        using var c = Fresh();
        using var other = KevyClient.Connect(fx.RequireUrl());

        c.Watch("wk");
        other.Set("wk", "changed"); // concurrent modify
        var txn = c.Multi();
        txn.Set("wk", "mine");
        Assert.Null(txn.ExecWatched()); // aborted

        c.Watch("wk");
        other.Set("wk", "again");
        var txn2 = c.Multi();
        txn2.Incr("wk2");
        Assert.Throws<KevyProtocolException>(() => txn2.ExecTyped());
    }

    [Fact]
    public void ImplicitDiscardOnDispose()
    {
        if (!fx.HasServer) return;
        using var c = Fresh();
        using (var txn = c.Multi())
            txn.Set("x", "1"); // abandoned without Exec/Discard → Dispose DISCARDs
        // Socket is usable for ordinary commands.
        c.Set("y", "2");
        Assert.Equal("2", H.Str(c.Get("y")));
        Assert.Null(c.Get("x")); // abandoned write must not have applied
    }

    [Fact]
    public void EmbeddedTransactionsUnsupported()
    {
        using var c = KevyClient.Connect("mem://txn-bus");
        Assert.Throws<KevyUnsupportedException>(() => c.Multi());
        Assert.Throws<KevyUnsupportedException>(() => c.Watch("k"));
    }

    private KevyClient Fresh()
    {
        var c = KevyClient.Connect(fx.RequireUrl());
        c.FlushAll();
        return c;
    }
}
