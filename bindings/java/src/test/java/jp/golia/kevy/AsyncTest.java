// The sync and async faces exist on ONE client and agree (client-contract
// §1.4, §6) — on BOTH backends (Java has blocking sockets).
package jp.golia.kevy;

import static org.junit.jupiter.api.Assertions.*;

import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.function.Supplier;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.MethodSource;

class AsyncTest {
    static java.util.stream.Stream<org.junit.jupiter.params.provider.Arguments> backends() {
        return Harness.bothBackends();
    }

    private static <T> T await(java.util.concurrent.CompletableFuture<T> fut) throws Exception {
        return fut.get(3, TimeUnit.SECONDS);
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void asyncRoundTrip(Supplier<KevyClient> f) throws Exception {
        try (KevyClient c = f.get()) {
            KevyAsyncClient a = c.async();
            await(a.set(Bytes.of("k"), Bytes.of("v")));
            assertEquals("v", Bytes.str(await(a.get(Bytes.of("k"))).orElse(null)));
            assertEquals(1L, await(a.incr(Bytes.of("n"))));
        }
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void facesAgree(Supplier<KevyClient> f) throws Exception {
        try (KevyClient c = f.get()) {
            c.set("k", "sync-write");                       // sync write
            assertEquals("sync-write", Bytes.str(await(c.async().get(Bytes.of("k"))).orElse(null))); // async reads it
            await(c.async().set(Bytes.of("k2"), Bytes.of("async-write"))); // async write
            assertEquals("async-write", c.get("k2").map(Bytes::str).orElse(null));               // sync reads it
        }
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void errorsPropagateExceptionally(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            c.set("k", "v");
            ExecutionException ee = assertThrows(ExecutionException.class,
                () -> c.async().lpush(Bytes.of("k"), Bytes.of("x")).get(3, TimeUnit.SECONDS));
            assertInstanceOf(StoreException.class, ee.getCause());
        }
    }
}
