// Cluster client CRC16 routing, remote (--cluster server) (client-contract §3.15, §6).
package jp.golia.kevy;

import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Assumptions;
import org.junit.jupiter.api.Test;

class ClusterTest {
    @Test
    void routingDelExistsDbsize() {
        Assumptions.assumeTrue(Harness.remoteAvailable());
        // --cluster with 4 shards: main port P, shard ports P+1..P+4.
        try (Harness.Server s = Harness.spawn(null, "--cluster", "--threads", "4");
             ClusterClient cc = ClusterClient.connect("127.0.0.1", s.port)) {
            assertEquals(4, cc.shardCount());

            // Keys spanning slots: each routes to its owner shard, no -MOVED.
            String[] keys = {"k0", "k1", "user:42", "rate:10.0.0.1", "gl:abc", "alpha", "beta", "gamma"};
            for (int i = 0; i < keys.length; i++) {
                byte[] val = Bytes.of("v" + i);
                cc.set(Bytes.of(keys[i]), val);
                assertArrayEquals(val, cc.get(Bytes.of(keys[i])).orElse(null), "key " + keys[i] + " routed wrong (-MOVED?)");
            }
            assertEquals(1, cc.incr(Bytes.of("counter")));
            cc.ping();

            // del/exists route per key and sum across shards.
            assertEquals(3, cc.del(Bytes.of("k0"), Bytes.of("k1"), Bytes.of("user:42"), Bytes.of("absent")));
            assertEquals(3, cc.exists(Bytes.of("alpha"), Bytes.of("beta"), Bytes.of("gamma")));

            assertTrue(cc.dbsize() >= 1); // whole-cluster (server fans out)
            cc.flushall();
        }
    }
}
