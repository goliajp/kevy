// The kevy-embedded store contract (contract §6 "Embedded store contract",
// §5.2): open/openMem/close, cmd(argv) → parseable RESP, scalar get/set
// fast paths, subscribe poll + blocking wait, and persistence replay.

using Kevy;
using KEmb = Kevy.Embedded;
using Xunit;

namespace Kevy.Tests;

public class EmbeddedTests
{
    [Fact]
    public void CmdRawAndScalarFastPaths()
    {
        using var db = KEmb.KevyDb.OpenInMemory();

        // cmd(argv) runs an arbitrary verb and returns parseable RESP.
        var raw = db.CmdRaw(new[] { H.B("SET"), H.B("k"), H.B("v") });
        Assert.Equal(ReplyKind.Simple, Reply.Decode(raw).Kind);

        var got = Reply.Decode(db.CmdRaw(new[] { H.B("GET"), H.B("k") }));
        Assert.Equal("v", got.Str());

        // Scalar fast paths (no argv/RESP framing).
        db.Set("s", H.B("scalar"), ttlMs: 60_000);
        Assert.Equal("scalar", H.Str(db.Get("s")));
        Assert.Null(db.Get("absent"));
    }

    [Fact]
    public void SubscribePollAndWait()
    {
        using var db = KEmb.KevyDb.OpenInMemory();
        using var sub = db.OpenRawSub(H.B("room"), pattern: false);

        db.Publish("room", H.B("hello"));

        // Blocking wait yields the RESP frames the server would push — a
        // subscribe ack first, then the message. Skip acks to the delivery.
        var (chan, payload) = NextMessage(sub);
        Assert.Equal("room", H.Str(chan));
        Assert.Equal("hello", H.Str(payload));

        // Poll drains non-blocking (empty now → null).
        Assert.Null(sub.PollRaw());
    }

    [Fact]
    public void PersistenceSurvivesReopen()
    {
        var dir = Directory.CreateTempSubdirectory("kevy-emb-persist-").FullName;
        using (var db = KEmb.KevyDb.Open(dir))
            db.Set("durable", H.B("x"));
        // Reopen the same dir → snapshot + AOF replay.
        using var db2 = KEmb.KevyDb.Open(dir);
        Assert.Equal("x", H.Str(db2.Get("durable")));
    }

    private static (byte[] channel, byte[] payload) NextMessage(KEmb.KevyRawSub sub)
    {
        var deadline = DateTime.UtcNow.AddSeconds(2);
        while (DateTime.UtcNow < deadline)
        {
            var frame = sub.WaitRaw(2000);
            if (frame is null) continue;
            var r = Reply.Decode(frame);
            if (r.Kind == ReplyKind.Array && r.Items.Count == 3 && r.Items[0].Str() == "message")
                return (r.Items[1].Bytes, r.Items[2].Bytes);
        }
        throw new Xunit.Sdk.XunitException("no message frame");
    }
}
