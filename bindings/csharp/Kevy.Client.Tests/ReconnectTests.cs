// Reconnect / robustness (contract §6 "Reconnect / robustness"): a dropped
// remote connection surfaces Closed/Io, and a fresh connect resumes work.

using Kevy;
using Xunit;

namespace Kevy.Tests;

public class ReconnectTests
{
    [Fact]
    public void DroppedConnectionThenReconnect()
    {
        var srv1 = ServerFixture.Spawn(Array.Empty<string>());
        if (!srv1.HasServer) return;

        var c1 = KevyClient.Connect(srv1.RequireUrl());
        c1.Set("k", "v");
        Assert.Equal("v", H.Str(c1.Get("k")));

        // Kill the server under the client → the next command fails with a
        // transport/closed error (not a hang, not a bogus value).
        srv1.Dispose();
        Assert.ThrowsAny<KevyException>(() => { for (var i = 0; i < 5; i++) c1.Get("k"); });
        c1.Dispose();

        // A fresh connect to a new server resumes commands.
        using var srv2 = ServerFixture.Spawn(Array.Empty<string>());
        using var c2 = KevyClient.Connect(srv2.RequireUrl());
        c2.Set("k2", "v2");
        Assert.Equal("v2", H.Str(c2.Get("k2")));
    }
}
