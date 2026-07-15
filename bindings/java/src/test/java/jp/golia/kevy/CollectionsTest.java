// Hash / list / set / sorted-set families on BOTH backends (§3.2–3.5, §6).
package jp.golia.kevy;

import static org.junit.jupiter.api.Assertions.*;

import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.function.Supplier;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.MethodSource;

class CollectionsTest {
    static java.util.stream.Stream<org.junit.jupiter.params.provider.Arguments> backends() {
        return Harness.bothBackends();
    }

    private static Set<String> strs(List<byte[]> l) {
        Set<String> s = new HashSet<>();
        for (byte[] b : l) s.add(Bytes.str(b));
        return s;
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void hash(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            assertEquals(2, c.hset("h", "f1", "v1", "f2", "v2")); // newly-added count
            assertEquals(0, c.hset("h", "f1", "x"));              // overwrite, not new
            assertEquals("x", c.hget("h", "f1").map(Bytes::str).orElse(null));
            assertEquals(2, c.hlen("h"));
            assertEquals(4, c.hgetall("h").size());               // flat [f,v,f,v]
            assertEquals(Set.of("f1", "f2"), strs(c.hkeys("h")));
            assertEquals(1, c.hdel("h", "f1"));
        }
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void list(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            assertEquals(3, c.rpush("l", "a", "b", "c"));
            assertEquals(4, c.lpush("l", "z"));                   // new length
            assertEquals(4, c.llen("l"));
            assertEquals(List.of("z", "a"), List.of(Bytes.str(c.lrange("l", 0, 1).get(0)), Bytes.str(c.lrange("l", 0, 1).get(1))));
            assertEquals("c", Bytes.str(c.lrange("l", -1, -1).get(0))); // negative index
            assertEquals("z", Bytes.str(c.lpop("l", 1).get(0)));
            assertEquals("c", Bytes.str(c.rpop("l", 1).get(0)));
        }
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void set(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            assertEquals(3, c.sadd("s", "a", "b", "c"));
            assertEquals(1, c.srem("s", "a"));
            assertEquals(2, c.scard("s"));
            assertTrue(c.sismember("s", "b"));
            assertFalse(c.sismember("s", "a"));
            assertEquals(Set.of("b", "c"), strs(c.smembers("s")));
        }
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void setCombine(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            c.sadd("s1", "a", "b", "c");
            c.sadd("s2", "b", "c", "d");
            assertEquals(Set.of("b", "c"), strs(c.sinter("s1", "s2")));
            assertEquals(Set.of("a", "b", "c", "d"), strs(c.sunion("s1", "s2")));
            assertEquals(Set.of("a"), strs(c.sdiff("s1", "s2")));
        }
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void zset(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            assertEquals(1, c.zadd("z", 1.0, "a"));
            c.zadd("z", 3.0, "c");
            c.zadd("z", 2.0, "b");
            assertEquals(3, c.zcard("z"));
            assertEquals(2.0, c.zscore("z", "b").orElseThrow(), 1e-9);
            assertTrue(c.zscore("z", "missing").isEmpty());
            List<byte[]> asc = c.zrange("z", 0, -1);
            assertEquals("a", Bytes.str(asc.get(0)));
            assertEquals("c", Bytes.str(asc.get(2)));
            assertEquals(1, c.zrem("z", "b"));
        }
    }
}
