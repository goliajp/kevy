// Cluster CRC16 routing (remote-only, cluster mode) — contract §6
// "Cluster", §3.15. Correct routing means -MOVED never fires.

using Kevy;
using Xunit;

namespace Kevy.Tests;

public class ClusterTests
{
    [Fact]
    public void RoutingDelExistsDbSize()
    {
        using var srv = ServerFixture.Spawn(new[] { "--cluster", "--threads", "4" });
        if (!srv.HasServer) return;
        using var cc = ClusterClient.Connect("127.0.0.1", (ushort)srv.Port);

        Assert.Equal(4, cc.ShardCount);

        var keys = new[] { "k0", "k1", "user:42", "rate:10.0.0.1", "gl:abc", "alpha", "beta", "gamma" };
        for (var i = 0; i < keys.Length; i++)
        {
            var val = $"v{i}";
            cc.Set(keys[i], val); // routed; a -MOVED would surface as a protocol error
            Assert.Equal(val, H.Str(cc.Get(keys[i])));
        }

        Assert.Equal(1, cc.Incr("counter"));
        cc.Ping();

        // del/exists route per key and sum across shards.
        Assert.Equal(3, cc.Del("k0", "k1", "user:42", "absent"));
        Assert.Equal(3, cc.Exists("alpha", "beta", "gamma"));

        Assert.True(cc.DbSize() >= 1);
        cc.FlushAll();
    }
}
