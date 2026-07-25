// Core string / generic-key family on BOTH backends (client-contract §3.1, §6).
package jp.golia.kevy;

import static org.junit.jupiter.api.Assertions.*;

import java.time.Duration;
import java.util.List;
import java.util.Optional;
import java.util.function.Supplier;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.MethodSource;

class CoreTest {
    static java.util.stream.Stream<org.junit.jupiter.params.provider.Arguments> backends() {
        return Harness.bothBackends();
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void setGetDelExistsIncr(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            c.set("k", "v");
            assertEquals("v", c.get("k").map(Bytes::str).orElse(null));
            assertTrue(c.get("missing").isEmpty());
            assertEquals(1, c.del("k"));
            assertEquals(0, c.exists("k"));
            c.set("a", "1");
            assertEquals(2, c.exists("a", "a")); // repeated key counts each (Redis)
            assertEquals(1, c.incr("n"));         // post-increment: 0 -> 1
        }
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void incrByAndDecr(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            assertEquals(1, c.incr("n"));
            assertEquals(11, c.incrBy("n", 10));
            assertEquals(6, c.incrBy("n", -5));
        }
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void expirePersistTtl(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            assertEquals(-2, c.ttlMs("k"));                 // no key
            c.set("k", "v");
            assertEquals(-1, c.ttlMs("k"));                 // no TTL
            assertTrue(c.expire("k", Duration.ofSeconds(100)));
            assertTrue(c.ttlMs("k") > 0);
            assertTrue(c.persist("k"));
            assertEquals(-1, c.ttlMs("k"));
        }
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void setWithTtlAtomic(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            c.setWithTtl("k", "v", Duration.ofSeconds(100));
            assertEquals("v", c.get("k").map(Bytes::str).orElse(null));
            assertTrue(c.ttlMs("k") > 0);
        }
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void typeAndDbsizeAndFlush(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            assertEquals("none", c.typeOf("k"));
            c.set("s", "v");
            c.lpush("l", "x");
            assertEquals("string", c.typeOf("s"));
            assertEquals("list", c.typeOf("l"));
            assertEquals(2, c.dbsize());
            c.flushall();
            assertEquals(0, c.dbsize());
        }
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void mgetOrderNullsAndMset(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            c.mset("a", "1", "c", "3");
            List<byte[]> got = c.mget("a", "b", "c");
            assertEquals(3, got.size());
            assertEquals("1", Bytes.str(got.get(0)));
            assertNull(got.get(1)); // missing -> null, not collapsed
            assertEquals("3", Bytes.str(got.get(2)));
        }
    }
}
