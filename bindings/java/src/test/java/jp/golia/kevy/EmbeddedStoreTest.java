// kevy-embedded contract (client-contract §5.2, §6): open/openMem/close,
// cmd(argv), scalar get/set, subscribe poll, and persistence replay.
package jp.golia.kevy;

import static org.junit.jupiter.api.Assertions.*;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import org.junit.jupiter.api.Test;

class EmbeddedStoreTest {
    @Test
    void cmdAndScalarFastPaths() {
        try (EmbeddedDb db = EmbeddedDb.openMem()) {
            Reply ok = db.cmd(List.of(Bytes.of("SET"), Bytes.of("k"), Bytes.of("v")));
            assertInstanceOf(Reply.Simple.class, ok);
            assertArrayEquals(Bytes.of("v"), db.get(Bytes.of("k")));  // scalar GET
            db.set(Bytes.of("k2"), Bytes.of("v2"), 0);                // scalar SET
            assertArrayEquals(Bytes.of("v2"), db.get(Bytes.of("k2")));
            assertNull(db.get(Bytes.of("missing")));
            Reply g = db.cmd(List.of(Bytes.of("GET"), Bytes.of("k")));
            assertArrayEquals(Bytes.of("v"), g.payload());
        }
    }

    @Test
    void subscribePollYieldsFrame() {
        try (EmbeddedDb db = EmbeddedDb.openMem()) {
            long sub = db.subscribeHandle(Bytes.of("ch"), false);
            db.cmd(List.of(Bytes.of("PUBLISH"), Bytes.of("ch"), Bytes.of("hi")));
            // The bus emits a `subscribe` ack frame first (contract §3.11/§5.1),
            // then the message — drain until we see the delivery.
            Reply message = null;
            for (int i = 0; i < 200 && message == null; i++) {
                byte[] frame = KevyNative.subNext(sub);
                if (frame == null) {
                    Harness.sleep(5);
                    continue;
                }
                Reply r = RespParser.decode(frame);
                if ("message".equals(Bytes.str(r.items().get(0).payload()))) message = r;
            }
            KevyNative.subClose(sub);
            assertNotNull(message, "expected a delivered pub/sub message frame");
            assertEquals("ch", Bytes.str(message.items().get(1).payload()));
            assertEquals("hi", Bytes.str(message.items().get(2).payload()));
        }
    }

    @Test
    void persistenceSurvivesReopen() throws Exception {
        Path dir = Files.createTempDirectory("kevy-persist-it");
        String url = "file://" + dir;
        try (KevyClient c = Kevy.connect(url)) {
            c.set("durable", "yes");
        } // close flushes AOF + drops the last lease -> store closes
        try (KevyClient c = Kevy.connect(url)) {
            assertEquals("yes", c.get("durable").map(Bytes::str).orElse(null)); // replayed
        }
    }

    @Test
    void versionReported() {
        assertFalse(Kevy.embeddedVersion().isEmpty());
    }
}
