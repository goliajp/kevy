// Hash-field TTL (Redis 7.4 shape) on BOTH backends (client-contract §3.7, §6).
package jp.golia.kevy;

import static org.junit.jupiter.api.Assertions.*;

import java.time.Duration;
import java.util.function.Supplier;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.MethodSource;

class HashTtlTest {
    static java.util.stream.Stream<org.junit.jupiter.params.provider.Arguments> backends() {
        return Harness.bothBackends();
    }

    private static byte[][] fields(String... f) {
        byte[][] out = new byte[f.length][];
        for (int i = 0; i < f.length; i++) out[i] = Bytes.of(f[i]);
        return out;
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void hpexpireAndTtlCodes(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            c.hset("h", "f1", "v1");
            long[] codes = c.hpexpire(Bytes.of("h"), fields("f1", "nope"), Duration.ofSeconds(100), HExpireCond.ALWAYS);
            assertEquals(1, codes[0]);   // deadline set
            assertEquals(-2, codes[1]);  // missing field
            long[] ttl = c.hpttl(Bytes.of("h"), fields("f1"));
            assertTrue(ttl[0] > 0);
            long[] persisted = c.hpersist(Bytes.of("h"), fields("f1"));
            assertEquals(1, persisted[0]); // cleared
        }
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void hexpireWholeSecondAndHttl(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            c.hset("h", "f1", "v1");
            long[] codes = c.hexpire(Bytes.of("h"), fields("f1"), Duration.ofSeconds(100), HExpireCond.ALWAYS);
            assertEquals(1, codes[0]);
            long[] ttl = c.httl(Bytes.of("h"), fields("f1"));
            assertTrue(ttl[0] > 0 && ttl[0] <= 100);
        }
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void emptyFieldsInvalid(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            assertThrows(InvalidInputException.class,
                () -> c.httl(Bytes.of("h"), new byte[0][]));
        }
    }
}
