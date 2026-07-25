// Blocking pops on BOTH backends (client-contract §3.14, §6). Embedded is
// poll-emulated; remote parks server-side — same observable contract.
package jp.golia.kevy;

import static org.junit.jupiter.api.Assertions.*;

import java.time.Duration;
import java.util.Optional;
import java.util.function.Supplier;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.MethodSource;

class BlockingTest {
    static java.util.stream.Stream<org.junit.jupiter.params.provider.Arguments> backends() {
        return Harness.bothBackends();
    }

    private static byte[][] keys(String... k) {
        byte[][] out = new byte[k.length][];
        for (int i = 0; i < k.length; i++) out[i] = Bytes.of(k[i]);
        return out;
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void blpopImmediateHit(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            c.rpush("l", "a", "b");
            Optional<KeyValue> hit = c.blpop(keys("l"), Duration.ofSeconds(1));
            assertTrue(hit.isPresent());
            assertEquals("l", hit.get().keyStr());
            assertEquals("a", hit.get().valueStr());
        }
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void blpopTimesOutEmpty(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            assertTrue(c.blpop(keys("empty"), Duration.ofMillis(200)).isEmpty());
        }
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void bzpopminLowest(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            c.zadd("z", 2.0, "b");
            c.zadd("z", 1.0, "a");
            Optional<ZPopHit> hit = c.bzpopmin(keys("z"), Duration.ofSeconds(1));
            assertTrue(hit.isPresent());
            assertEquals("a", hit.get().memberStr());
            assertEquals(1.0, hit.get().score(), 1e-9);
        }
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void invalidTimeoutAndKeys(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            assertThrows(InvalidInputException.class, () -> c.blpop(keys("l"), Duration.ZERO));
            assertThrows(InvalidInputException.class, () -> c.blpop(new byte[0][], Duration.ofSeconds(1)));
        }
    }
}
