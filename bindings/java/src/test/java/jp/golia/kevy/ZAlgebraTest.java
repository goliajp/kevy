// Sorted-set algebra on BOTH backends (client-contract §3.6, §6).
package jp.golia.kevy;

import static org.junit.jupiter.api.Assertions.*;

import java.util.function.Supplier;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.MethodSource;

class ZAlgebraTest {
    static java.util.stream.Stream<org.junit.jupiter.params.provider.Arguments> backends() {
        return Harness.bothBackends();
    }

    private static void seed(KevyClient c) {
        c.zadd("z1", 1.0, "a");
        c.zadd("z1", 2.0, "b");
        c.zadd("z2", 3.0, "b");
        c.zadd("z2", 4.0, "c");
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void interUnionCardinality(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            seed(c);
            assertEquals(1, c.zinterstore(Bytes.of("di"), Bytes.of("z1"), Bytes.of("z2"))); // {b}
            assertEquals(3, c.zunionstore(Bytes.of("du"), Bytes.of("z1"), Bytes.of("z2"))); // {a,b,c}
        }
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void weightsAndAggregate(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            seed(c);
            byte[][] keys = {Bytes.of("z1"), Bytes.of("z2")};
            assertEquals(1, c.zinterstoreWith(Bytes.of("d"), keys, new double[]{2, 3}, ZAggregate.MAX));
            // b: max(1*2, 3*3) = 9
            assertEquals(9.0, c.zscore("d", "b").orElseThrow(), 1e-9);
        }
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void intercardWithAndWithoutLimit(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            seed(c);
            byte[][] keys = {Bytes.of("z1"), Bytes.of("z2")};
            assertEquals(1, c.zintercard(keys, null));
            assertEquals(1, c.zintercard(keys, 5L));
        }
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void emptyKeysInvalid(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            assertThrows(InvalidInputException.class, () -> c.zinterstore(Bytes.of("d")));
            assertThrows(InvalidInputException.class, () -> c.zintercard(new byte[0][], null));
        }
    }
}
