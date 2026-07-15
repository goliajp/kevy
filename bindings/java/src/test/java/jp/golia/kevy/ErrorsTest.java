// Error-as-value / exception mapping (client-contract §2, §6). Store-semantic
// errors stay structured on both backends; a generic -ERR is a Protocol error;
// the raw path returns an -ERR frame as DATA (not thrown).
package jp.golia.kevy;

import static org.junit.jupiter.api.Assertions.*;

import java.util.function.Supplier;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.MethodSource;

class ErrorsTest {
    static java.util.stream.Stream<org.junit.jupiter.params.provider.Arguments> backends() {
        return Harness.bothBackends();
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void wrongTypeIsStructured(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            c.set("k", "v");
            KevyException e = assertThrows(KevyException.class, () -> c.lpush("k", "x"));
            assertEquals(ErrorKind.STORE, e.kind());
            assertEquals(StoreError.WRONG_TYPE, e.storeError());
        }
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void incrOnNonNumberIsNotInteger(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            c.set("k", "abc");
            KevyException e = assertThrows(KevyException.class, () -> c.incr("k"));
            assertEquals(StoreError.NOT_INTEGER, e.storeError());
        }
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("backends")
    void rawPathReturnsErrorAsData(Supplier<KevyClient> f) {
        try (KevyClient c = f.get()) {
            Reply r = c.execute("SET", "onlyonearg"); // wrong arity -> -ERR, but as DATA
            assertTrue(r.isError());
        }
    }

    @Test
    void embeddedRemoteOnlyFamiliesUnsupported() {
        try (KevyClient c = Harness.newEmbedded()) {
            assertThrows(UnsupportedException.class, c::idxList);
            assertThrows(UnsupportedException.class, c::multi);
            assertThrows(UnsupportedException.class, () -> c.pipeline(p -> p.cmd("PING")));
        }
    }

    @Test
    void wireTextClassification() {
        assertInstanceOf(StoreException.class, Errors.fromReplyText("WRONGTYPE Operation against a key"));
        assertEquals(StoreError.OUT_OF_MEMORY, ((StoreException) Errors.fromReplyText("OOM command not allowed")).storeError());
        assertInstanceOf(ProtocolException.class, Errors.fromReplyText("ERR some verb-specific error"));
        assertInstanceOf(ReadOnlyException.class, Errors.fromReplyText("READONLY can't write to replica"));
    }
}
