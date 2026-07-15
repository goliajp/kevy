// Pub/sub round-trip on both backends (contract §6 "Pub/sub round-trip",
// §3.11).

using Kevy;
using Xunit;

namespace Kevy.Tests;

[Collection("server")]
public class PubsubTests(ServerFixture fx)
{
    [Fact]
    public void MessageRoundTrip()
    {
        foreach (var url in H.PubsubUrls(fx))
        {
            using var sub = Subscriber.ConnectChannels(url, "news");
            using var pub = KevyClient.Connect(url);
            sub.SetReadTimeout(TimeSpan.FromSeconds(3));
            DrainSubscribeAck(sub, url);
            pub.Publish("news", "hello");
            var (chan, payload) = sub.RecvMessage();
            Assert.Equal("news", H.Str(chan));
            Assert.Equal("hello", H.Str(payload));
        }
    }

    [Fact]
    public void PatternMessage()
    {
        foreach (var url in H.PubsubUrls(fx))
        {
            using var sub = Subscriber.Connect(url);
            sub.SetReadTimeout(TimeSpan.FromSeconds(3));
            sub.Psubscribe("ne*");
            using var pub = KevyClient.Connect(url);
            if (H.IsRemote(url)) { var ack = sub.Recv(); Assert.Equal(PubsubKind.Psubscribe, ack.Kind); }
            else Thread.Sleep(20);
            pub.Publish("news", "pat");
            var (chan, payload) = sub.RecvMessage();
            Assert.Equal("news", H.Str(chan));
            Assert.Equal("pat", H.Str(payload));
        }
    }

    [Fact]
    public void AckDeliveredViaRecvSkippedByRecvMessage()
    {
        if (!fx.HasServer) return;
        using var sub = Subscriber.Connect(fx.RequireUrl());
        sub.SetReadTimeout(TimeSpan.FromSeconds(3));
        sub.Subscribe("c1");
        var ack = sub.Recv();                    // recv sees the ack
        Assert.Equal(PubsubKind.Subscribe, ack.Kind);
        Assert.Equal("c1", H.Str(ack.Channel));
    }

    [Fact]
    public void AnonymousMemSubscriberRejected() =>
        Assert.Throws<KevyUnsupportedException>(() => Subscriber.Connect("mem://"));

    [Fact]
    public void Hello3RemoteOnly()
    {
        if (!fx.HasServer) return;
        using var sub = Subscriber.Connect(fx.RequireUrl());
        var ev = sub.Hello3();
        Assert.Equal(PubsubKind.Subscribe, ev.Kind); // synthetic marker
        // RESP3 push delivery still works after HELLO 3.
        sub.SetReadTimeout(TimeSpan.FromSeconds(3));
        sub.Subscribe("c3");
        sub.Recv(); // subscribe ack
        using var pub = KevyClient.Connect(fx.RequireUrl());
        pub.Publish("c3", "v3");
        var (_, payload) = sub.RecvMessage();
        Assert.Equal("v3", H.Str(payload));
    }

    [Fact]
    public void ReadTimeoutBoundsRecv()
    {
        using var sub = Subscriber.ConnectChannels(H.Mem(), "quiet");
        sub.SetReadTimeout(TimeSpan.FromMilliseconds(150));
        Assert.Throws<KevyTimedOutException>(() => sub.RecvMessage());
    }

    private static void DrainSubscribeAck(Subscriber sub, string url)
    {
        if (H.IsRemote(url)) { var ack = sub.Recv(); Assert.Equal(PubsubKind.Subscribe, ack.Kind); }
        else Thread.Sleep(20);
    }
}
